//! The sync loop: keeps one connection to the room open, announces local copies, and puts clips from
//! other devices on this clipboard.
//!
//! Ordering is last writer wins. Each clip carries the moment it was copied on the server's clock;
//! the server turns away anything older than the room's latest clip and every device ignores clips
//! older than the one it holds, so all devices settle on the same clip even when two copy at once.
//!
//! Text up to 32 KB travels inside the announcement. Images and files follow as an encrypted blob:
//! the sender announces first and uploads straight after, and the other devices start downloading
//! at once, streaming the upload as it arrives. The clipboard keeps its old content until the whole
//! clip is here and checked, and a newer copy on either side cancels the older one.

use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Read, Seek, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::json;
use ureq::{Agent, SendBody};

use super::{
    Session,
    board::{self, Board, Content, normalize_text},
    files::{self, safe_name},
    keys::{self, RoomKey},
    net::{self, Frame, Socket},
};
use crate::{
    e2e::{self, Decryptor, Encryptor, Header},
    human_size,
};

/// Longer text goes as a blob, so announcements stay small.
const INLINE_TEXT: usize = 32 * 1024;
const DEFAULT_MAX_MB: u64 = 100;
const PING_EVERY: Duration = Duration::from_secs(10);
/// No word from the server for this long means the connection is dead, even if the OS hasn't noticed.
const DEAD_AFTER: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Received clips kept on disk, so pasting one a second time still works for a while.
const KEEP_RECEIVED: usize = 8;

/// What a clip is, sealed with the clip's meta key. The web page reads and writes the same JSON.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Meta {
    /// `text`, `image` or `files`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from: String,
    /// Short text, inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Bytes of content (of the zip, for several files).
    #[serde(default)]
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    /// The blob is a zip of these top-level files and folders.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<String>,
}

impl Meta {
    fn describe(&self) -> String {
        match self.kind.as_str() {
            "text" => {
                let chars = self.text.as_ref().map_or(self.size as usize, |t| t.chars().count());
                format!("Text ({chars} characters)")
            }
            "image" => format!("An image ({})", human_size(self.size)),
            _ if self.entries.len() > 1 => format!("{} files ({})", self.entries.len(), human_size(self.size)),
            _ => format!(
                "{} ({})",
                self.entries
                    .first()
                    .or(self.name.as_ref())
                    .map_or("a file", |n| n.as_str()),
                human_size(self.size)
            ),
        }
    }
}

#[derive(Clone)]
enum Blob {
    Bytes(Arc<[u8]>),
    /// A zip of several files, deleted when no longer needed.
    Zip(PathBuf),
    /// A single copied file, opened right away: if it is cut and pasted (moved) before the upload
    /// is done, the open handle still reads it.
    File(Arc<File>),
}

impl Blob {
    fn discard(self) {
        if let Blob::Zip(path) = self {
            let _ = fs::remove_file(path);
        }
    }

    fn open(&self) -> io::Result<Box<dyn Read + Send>> {
        Ok(match self {
            Blob::Bytes(b) => Box::new(io::Cursor::new(b.clone())),
            Blob::Zip(path) => Box::new(File::open(path)?),
            Blob::File(file) => {
                let mut f = file.try_clone()?;
                f.seek(io::SeekFrom::Start(0))?;
                Box::new(f)
            }
        })
    }
}

struct Outgoing {
    meta: Meta,
    blob: Option<Blob>,
}

enum Event {
    /// A local copy, when it happened, and its number.
    Copied(Content, u64, u64),
    Prepared {
        id: String,
        result: Result<Outgoing, String>,
    },
    Uploaded {
        id: String,
        result: Result<(), String>,
    },
    Downloaded {
        id: String,
        result: Result<Content, String>,
    },
}

#[derive(Deserialize, Clone)]
struct ServerClip {
    id: String,
    at: u64,
    meta: String,
    #[serde(default)]
    blob: bool,
}

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum ServerMsg {
    Hello {
        now: u64,
        clip: Option<ServerClip>,
        devices: usize,
    },
    Clip {
        clip: ServerClip,
    },
    Stale {
        id: String,
        clip: ServerClip,
    },
    Devices {
        n: usize,
    },
    Ended,
    Error {
        error: String,
    },
    #[serde(other)]
    Other,
}

/// The clip this device holds (or is about to): the one any other clip has to beat.
struct Current {
    id: String,
    at: u64,
}

/// A local copy announced to the server but not accepted yet; sent again after a reconnect.
struct Pending {
    id: String,
    msg: String,
    blob: Option<(Blob, Meta)>,
}

struct Sync {
    session: Session,
    room_id: String,
    events: Sender<Event>,
    board: Board,
    current: Option<Current>,
    /// The server's clock minus ours, in milliseconds.
    offset: i64,
    devices: usize,
    pending: Option<Pending>,
    outbox: Vec<String>,
    /// The ID of the current clip, for workers to notice they've been overtaken.
    latest: Arc<Mutex<String>>,
    max_bytes: u64,
    uploads: HashMap<String, Instant>,
    /// The number of the last local copy handled.
    copies: u64,
    /// Set on Ctrl+C or when the terminal closes: end the room and stop.
    stop: Arc<AtomicBool>,
    /// The server said another device ended the room.
    ended: bool,
}

/// How a connection came to an end.
enum Closed {
    Lost(String),
    /// Another device ended the room.
    Ended,
    /// This device ended the room.
    Stopped,
}

pub fn log(msg: impl AsRef<str>) {
    eprintln!("{}", msg.as_ref());
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub fn agent() -> Agent {
    Agent::config_builder()
        .http_status_as_error(false)
        .user_agent(concat!("ml/", env!("CARGO_PKG_VERSION")))
        .tls_config(net::tls_config())
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        // A download follows the upload as it happens, so it may take as long as the upload does.
        .timeout_recv_body(Some(Duration::from_secs(30 * 60)))
        .build()
        .into()
}

/// Runs until this device or another one ends the room, reconnecting whenever the connection drops.
pub fn run(session: Session, stop: Arc<AtomicBool>) -> Result<(), String> {
    let (events, rx) = mpsc::channel();
    let copied = events.clone();
    let board = board::spawn(move |content, n| {
        let _ = copied.send(Event::Copied(content, now_ms(), n));
    })?;
    let _ = fs::remove_dir_all(files::outgoing_dir());
    let max_mb = server_limit(&session.server).unwrap_or(DEFAULT_MAX_MB);
    let mut sync = Sync {
        room_id: session.key.room_id(),
        session,
        events,
        board,
        current: None,
        offset: 0,
        devices: 0,
        pending: None,
        outbox: Vec::new(),
        latest: Arc::default(),
        max_bytes: max_mb * 1024 * 1024,
        uploads: HashMap::new(),
        copies: 0,
        stop,
        ended: false,
    };
    log(format!(
        "Syncing the clipboard of {} through {} (Ctrl+C ends the room on all devices)",
        sync.session.device, sync.session.server
    ));
    let mut backoff = Duration::from_secs(1);
    let mut reported = false;
    loop {
        let started = Instant::now();
        match net::connect(&sync.session.server, &format!("/clip/ws?room={}", sync.room_id)) {
            Ok(socket) => {
                reported = false;
                match sync.connected(socket, &rx) {
                    Closed::Stopped => {
                        log("Ended the room on all devices.");
                        sync.finish();
                        return Ok(());
                    }
                    Closed::Ended => {
                        log(
                            "Another device stopped syncing, so the room ended. Run `ml clip sync` to start a new one.",
                        );
                        sync.finish();
                        return Ok(());
                    }
                    Closed::Lost(reason) => log(format!("Disconnected: {reason}. Reconnecting…")),
                }
                if started.elapsed() > Duration::from_secs(60) {
                    backoff = Duration::from_secs(1);
                }
            }
            Err(e) => {
                if !reported {
                    log(format!("{e}. Retrying in the background…"));
                    reported = true;
                }
            }
        }
        sync.outbox.clear();
        // Local copies keep coming in while offline; the newest goes out on reconnect.
        let mut clock = WakeClock::new();
        let until = Instant::now() + jitter(backoff);
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left.min(Duration::from_secs(1))) {
                Ok(ev) => sync.handle(ev),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err("the clipboard thread stopped".into()),
            }
            if clock.woke() {
                backoff = Duration::from_secs(1);
                break;
            }
            if sync.stop.load(Ordering::SeqCst) {
                // Offline: one quick try to tell the room, so the others don't sync on without us.
                if let Ok(mut socket) = net::connect(&sync.session.server, &format!("/clip/ws?room={}", sync.room_id)) {
                    end_room(&mut socket);
                }
                log("Stopped. The other devices will see the room end once this one reaches the server.");
                sync.finish();
                return Ok(());
            }
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Tells the room this device is stopping, which ends it for everyone, and waits a moment for the
/// server to confirm.
fn end_room(socket: &mut Socket) {
    if net::send(socket, r#"{"t":"end"}"#.into()).is_err() {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match net::read(socket) {
            Ok(Frame::Text(t)) if t.contains(r#""t":"ended""#) => return,
            Ok(_) => {}
            Err(_) => return,
        }
    }
}

fn jitter(d: Duration) -> Duration {
    let spread = keys::random::<1>().map_or(0, |b| b[0] as u64);
    d + Duration::from_millis(spread * d.as_millis() as u64 / 1024)
}

/// The server's clip size limit in MB, from its `/config`.
fn server_limit(server: &str) -> Option<u64> {
    let mut res = agent().get(format!("{server}/config")).call().ok()?;
    let config: serde_json::Value = res.body_mut().read_json().ok()?;
    config["clipMaxMb"].as_u64()
}

/// Notices the computer slept: the wall clock jumps ahead while the monotonic clock stood still.
struct WakeClock {
    wall: SystemTime,
    mono: Instant,
}

impl WakeClock {
    fn new() -> Self {
        Self {
            wall: SystemTime::now(),
            mono: Instant::now(),
        }
    }

    fn woke(&mut self) -> bool {
        let wall = SystemTime::now().duration_since(self.wall).unwrap_or_default();
        let mono = self.mono.elapsed();
        *self = Self::new();
        wall > mono + Duration::from_secs(5)
    }
}

impl Sync {
    /// Talks to the server until the connection fails or the room ends.
    fn connected(&mut self, mut socket: Socket, rx: &Receiver<Event>) -> Closed {
        net::set_read_timeout(&socket, Duration::from_millis(100));
        let mut heard = Instant::now();
        let mut pinged = Instant::now();
        let mut clock = WakeClock::new();
        loop {
            match net::read(&mut socket) {
                Ok(Frame::Text(text)) => {
                    heard = Instant::now();
                    self.on_server(&text);
                }
                Ok(Frame::Other) => heard = Instant::now(),
                Ok(Frame::Idle) => {}
                Err(_) if self.ended => return Closed::Ended,
                Err(e) => return Closed::Lost(e),
            }
            if self.ended {
                return Closed::Ended;
            }
            if self.stop.load(Ordering::SeqCst) {
                end_room(&mut socket);
                return Closed::Stopped;
            }
            while let Ok(ev) = rx.try_recv() {
                self.handle(ev);
            }
            for msg in std::mem::take(&mut self.outbox) {
                if let Err(e) = net::send(&mut socket, msg) {
                    return Closed::Lost(e);
                }
            }
            if pinged.elapsed() >= PING_EVERY {
                pinged = Instant::now();
                if let Err(e) = net::send(&mut socket, r#"{"t":"ping"}"#.into()) {
                    return Closed::Lost(e);
                }
            }
            if heard.elapsed() > DEAD_AFTER {
                return Closed::Lost("the server stopped answering".into());
            }
            if clock.woke() {
                return Closed::Lost("woke from sleep".into());
            }
        }
    }

    /// Stops watching the clipboard, keeping the last clip from another device pasteable.
    fn finish(&self) {
        let kept = self.board.release();
        #[cfg(target_os = "linux")]
        if let Some(content) = kept {
            super::keep::hand_over(content);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = kept;
    }

    fn set_current(&mut self, id: &str, at: u64) {
        self.current = Some(Current { id: id.into(), at });
        *self.latest.lock().unwrap() = id.into();
        if let Some(p) = self.pending.take_if(|p| p.id != id)
            && let Some((blob, _)) = p.blob
        {
            blob.discard();
        }
    }

    fn handle(&mut self, ev: Event) {
        match ev {
            Event::Copied(content, copied_at, n) => {
                self.copies = n;
                self.on_copied(content, copied_at);
            }
            Event::Prepared { id, result } => {
                let current = self.current.as_ref().filter(|c| c.id == id).map(|c| c.at);
                match (result, current) {
                    (Ok(out), Some(at)) => self.announce(id, at, out),
                    (Ok(out), None) => {
                        if let Some(b) = out.blob {
                            b.discard();
                        }
                    }
                    (Err(e), Some(_)) => log(format!("  not syncing it: {e}")),
                    (Err(_), None) => {}
                }
            }
            Event::Uploaded { id, result } => {
                let took = self.uploads.remove(&id).map(|t| t.elapsed());
                match result {
                    Ok(()) => {
                        if took.is_some_and(|t| t > Duration::from_secs(2)) {
                            log("  uploaded");
                        }
                    }
                    Err(e) => log(format!("  upload failed: {e}")),
                }
            }
            Event::Downloaded { id, result } => {
                if self.current.as_ref().is_none_or(|c| c.id != id) {
                    return;
                }
                match result {
                    Ok(content) => {
                        if matches!(content, Content::Files(_) | Content::Image { .. }) {
                            log("  ready to paste");
                        }
                        self.board.apply(content, self.copies);
                    }
                    Err(e) => log(format!("  could not receive it: {e}")),
                }
            }
        }
    }

    fn on_copied(&mut self, content: Content, copied_at: u64) {
        let Ok(id) = keys::new_id() else { return };
        let at = (copied_at as i64 + self.offset).max(0) as u64;
        self.set_current(&id, at);
        let from = self.session.device.clone();
        if let Content::Text(text) = &content
            && text.len() <= INLINE_TEXT
        {
            log(format!("↑ Text ({} characters)", text.chars().count()));
            let meta = Meta {
                kind: "text".into(),
                from,
                size: text.len() as u64,
                text: Some(text.clone()),
                ..Meta::default()
            };
            self.announce(id, at, Outgoing { meta, blob: None });
            return;
        }
        log(match &content {
            Content::Text(t) => format!("↑ Text ({} characters)", t.chars().count()),
            Content::Image { width, height, .. } => format!("↑ An image ({width}×{height})"),
            Content::Files(paths) if paths.len() == 1 => format!("↑ {}", files::display_name(&paths[0])),
            Content::Files(paths) => format!("↑ {} files", paths.len()),
        });
        let events = self.events.clone();
        let max = self.max_bytes;
        thread::spawn(move || {
            let result = prepare(content, &id, from, max);
            let _ = events.send(Event::Prepared { id, result });
        });
    }

    fn announce(&mut self, id: String, at: u64, out: Outgoing) {
        let meta_json = serde_json::to_vec(&out.meta).unwrap_or_default();
        let sealed = e2e::seal(&self.session.key.meta_key(&id), "clip", meta_json);
        let msg = json!({
            "t": "clip",
            "id": id,
            "at": at,
            "meta": URL_SAFE_NO_PAD.encode(sealed),
            "blob": out.blob.is_some(),
        })
        .to_string();
        self.outbox.push(msg.clone());
        self.pending = Some(Pending {
            id,
            msg,
            blob: out.blob.map(|b| (b, out.meta)),
        });
    }

    fn on_server(&mut self, text: &str) {
        let Ok(msg) = serde_json::from_str::<ServerMsg>(text) else {
            return;
        };
        match msg {
            ServerMsg::Hello { now, clip, devices } => {
                self.offset = now as i64 - now_ms() as i64;
                self.devices = devices;
                log(format!("Connected · {}", devices_text(devices)));
                if let Some(c) = clip {
                    self.on_clip(c);
                }
                // Anything queued while offline is in `pending` already; send it once.
                self.outbox.clear();
                if let Some(p) = &self.pending {
                    self.outbox.push(p.msg.clone());
                }
            }
            ServerMsg::Clip { clip } => self.on_clip(clip),
            // Our clip came back as its own competition: it was sent twice and is in already.
            ServerMsg::Stale { id, clip } if clip.id == id => self.on_clip(clip),
            ServerMsg::Stale { id, clip } => {
                if self.pending.as_ref().is_some_and(|p| p.id == id) {
                    log("  another device copied something newer first");
                    // The server's word is final, whatever our clock said.
                    self.current = None;
                }
                self.on_clip(clip);
            }
            ServerMsg::Devices { n } => {
                if n != self.devices {
                    self.devices = n;
                    log(format!("  {}", devices_text(n)));
                }
            }
            ServerMsg::Ended => self.ended = true,
            ServerMsg::Error { error } => log(format!("Server: {error}")),
            ServerMsg::Other => {}
        }
    }

    fn on_clip(&mut self, c: ServerClip) {
        if let Some(cur) = &mut self.current
            && cur.id == c.id
        {
            // Our own clip, accepted: the server's time is the one everybody compares against.
            cur.at = c.at;
            if let Some(p) = self.pending.take_if(|p| p.id == c.id)
                && let Some((blob, meta)) = p.blob
            {
                self.upload(c.id, blob, meta);
            }
            return;
        }
        if let Some(cur) = &self.current
            && (c.at, c.id.as_str()) <= (cur.at, cur.id.as_str())
        {
            return;
        }
        self.set_current(&c.id, c.at);
        let meta = URL_SAFE_NO_PAD
            .decode(&c.meta)
            .ok()
            .and_then(|sealed| e2e::unseal(&self.session.key.meta_key(&c.id), "clip", sealed).ok())
            .and_then(|json| serde_json::from_slice::<Meta>(&json).ok());
        let Some(meta) = meta else {
            log("↓ A clip this device can't decrypt (was it paired with another room?)");
            return;
        };
        let from = if meta.from.is_empty() {
            "another device"
        } else {
            &meta.from
        };
        log(format!("↓ {} from {from}", meta.describe()));
        if let Some(text) = meta.text.clone().filter(|_| meta.kind == "text") {
            self.board.apply(Content::Text(normalize_text(text)), self.copies);
        } else if c.blob {
            let job = Job {
                server: self.session.server.clone(),
                room_id: self.room_id.clone(),
                key: self.session.key.clone(),
                id: c.id,
                latest: self.latest.clone(),
                events: self.events.clone(),
            };
            let max = self.max_bytes;
            thread::spawn(move || job.download(meta, max));
        }
    }

    fn upload(&mut self, id: String, blob: Blob, meta: Meta) {
        self.uploads.insert(id.clone(), Instant::now());
        let job = Job {
            server: self.session.server.clone(),
            room_id: self.room_id.clone(),
            key: self.session.key.clone(),
            id,
            latest: self.latest.clone(),
            events: self.events.clone(),
        };
        thread::spawn(move || job.upload(blob, meta));
    }
}

fn devices_text(n: usize) -> String {
    match n {
        1 => "no other device online yet".into(),
        2 => "1 other device online".into(),
        n => format!("{} other devices online", n - 1),
    }
}

/// Turns a copy into what gets uploaded: PNG for images, the file itself, or a zip of several.
fn prepare(content: Content, id: &str, from: String, max: u64) -> Result<Outgoing, String> {
    let too_big = |size: u64| {
        format!(
            "it is {} and clips are limited to {}",
            human_size(size),
            human_size(max)
        )
    };
    let (meta, blob) = match content {
        Content::Text(text) => {
            let meta = Meta {
                kind: "text".into(),
                size: text.len() as u64,
                name: Some("clipboard.txt".into()),
                mime: Some("text/plain".into()),
                ..Meta::default()
            };
            (meta, Blob::Bytes(text.into_bytes().into()))
        }
        Content::Image { width, height, rgba } => {
            let png = encode_png(width, height, &rgba)?;
            let meta = Meta {
                kind: "image".into(),
                size: png.len() as u64,
                name: Some("image.png".into()),
                mime: Some("image/png".into()),
                ..Meta::default()
            };
            (meta, Blob::Bytes(png.into()))
        }
        Content::Files(paths) => {
            let size = files::total_size(&paths, max).map_err(|e| format!("cannot read the files: {e}"))?;
            if size > max {
                return Err(too_big(size));
            }
            if let [path] = paths.as_slice()
                && path.is_file()
            {
                let meta = Meta {
                    kind: "files".into(),
                    size,
                    name: Some(files::display_name(path)),
                    ..Meta::default()
                };
                let file = File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                (meta, Blob::File(Arc::new(file)))
            } else {
                let dir = files::outgoing_dir();
                fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
                let zip = dir.join(format!("{id}.zip"));
                let entries = files::bundle(&paths, &zip).map_err(|e| {
                    let _ = fs::remove_file(&zip);
                    format!("cannot pack the files: {e}")
                })?;
                let blob = Blob::Zip(zip.clone());
                let size = fs::metadata(&zip).map(|m| m.len()).unwrap_or_default();
                if size > max {
                    blob.discard();
                    return Err(too_big(size));
                }
                let name = match entries.as_slice() {
                    [one] => format!("{one}.zip"),
                    _ => "files.zip".into(),
                };
                let meta = Meta {
                    kind: "files".into(),
                    size,
                    name: Some(name),
                    mime: Some("application/zip".into()),
                    entries,
                    ..Meta::default()
                };
                (meta, blob)
            }
        }
    };
    Ok(Outgoing {
        meta: Meta { from, ..meta },
        blob: Some(blob),
    })
}

pub(super) fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("cannot encode the image: {e}"))?;
    writer
        .write_image_data(rgba)
        .map_err(|e| format!("cannot encode the image: {e}"))?;
    writer.finish().map_err(|e| format!("cannot encode the image: {e}"))?;
    Ok(out)
}

pub(super) fn decode_png(bytes: &[u8]) -> Result<Content, String> {
    let bad = |e: png::DecodingError| format!("the image is damaged: {e}");
    let mut decoder = png::Decoder::new(io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().map_err(bad)?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("the image is too large")?];
    let info = reader.next_frame(&mut buf).map_err(bad)?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::GrayscaleAlpha => buf.chunks(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Rgb => buf.chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err("the image uses a palette ml can't read".into()),
    };
    Ok(Content::Image {
        width: info.width as usize,
        height: info.height as usize,
        rgba,
    })
}

/// An upload or download on its own thread, which gives up as soon as a newer clip takes over.
struct Job {
    server: String,
    room_id: String,
    key: RoomKey,
    id: String,
    latest: Arc<Mutex<String>>,
    events: Sender<Event>,
}

enum Failure {
    Retry(String),
    Fatal(String),
}

impl Job {
    fn current(&self) -> bool {
        *self.latest.lock().unwrap() == self.id
    }

    fn url(&self) -> String {
        format!("{}/clip/{}/blob/{}", self.server, self.room_id, self.id)
    }

    /// Calls `attempt` with growing pauses until it works, fails for good, or the clip is overtaken.
    fn retry<T>(&self, mut attempt: impl FnMut() -> Result<T, Failure>) -> Option<Result<T, String>> {
        let mut wait = Duration::from_secs(1);
        for _ in 0..8 {
            if !self.current() {
                return None;
            }
            match attempt() {
                Ok(v) => return Some(Ok(v)),
                Err(Failure::Fatal(e)) => return Some(Err(e)),
                Err(Failure::Retry(e)) => {
                    if wait >= Duration::from_secs(8) {
                        log(format!("  {e}, trying again"));
                    }
                }
            }
            thread::sleep(wait);
            wait = (wait * 2).min(MAX_BACKOFF);
        }
        Some(Err("gave up after several tries".into()))
    }

    fn upload(self, blob: Blob, meta: Meta) {
        let header = Header {
            kind: "file".into(),
            name: meta.name.clone(),
            mime: meta.mime.clone(),
        };
        let blob_key = self.key.blob_key(&self.id);
        let agent = agent();
        let result = self.retry(|| {
            let reader = blob
                .open()
                .map_err(|e| Failure::Fatal(format!("cannot read it: {e}")))?;
            let mut body = Encryptor::new(&blob_key, &header, reader);
            let res = agent
                .put(self.url())
                .header("Content-Type", "application/octet-stream")
                .send(SendBody::from_reader(&mut body))
                .map_err(|e| Failure::Retry(e.to_string()))?;
            match res.status().as_u16() {
                200..=299 => Ok(()),
                404 | 413 => Err(Failure::Fatal(server_error(res))),
                _ => Err(Failure::Retry(server_error(res))),
            }
        });
        blob.discard();
        if let Some(result) = result {
            let _ = self.events.send(Event::Uploaded { id: self.id, result });
        }
    }

    fn download(self, meta: Meta, max: u64) {
        let blob_key = self.key.blob_key(&self.id);
        let agent = agent();
        if meta.size > 4 * 1024 * 1024 {
            log(format!("  receiving {}…", human_size(meta.size)));
        }
        let result = self.retry(|| {
            let res = agent
                .get(self.url())
                .call()
                .map_err(|e| Failure::Retry(e.to_string()))?;
            match res.status().as_u16() {
                200..=299 => {}
                404 => return Err(Failure::Fatal("a newer clip replaced it on the server".into())),
                _ => return Err(Failure::Retry(server_error(res))),
            }
            let (header, mut plain) = Decryptor::open(&blob_key, Box::new(res.into_body().into_reader()))
                .map_err(|e| Failure::Retry(e.to_string()))?;
            self.receive(&meta, &header, &mut plain, max).map_err(Failure::Retry)
        });
        if let Some(result) = result {
            let _ = self.events.send(Event::Downloaded { id: self.id, result });
        }
    }

    fn receive(&self, meta: &Meta, header: &Header, plain: &mut dyn Read, max: u64) -> Result<Content, String> {
        let failed = |e: io::Error| e.to_string();
        match meta.kind.as_str() {
            "text" => {
                let bytes = files::read_limited(plain, max).map_err(failed)?;
                let text = String::from_utf8(bytes).map_err(|_| "the text is not UTF-8")?;
                Ok(Content::Text(normalize_text(text)))
            }
            "image" => decode_png(&files::read_limited(plain, max).map_err(failed)?),
            _ => {
                let incoming = files::incoming_dir();
                let dir = incoming.join(&self.id);
                let _ = fs::remove_dir_all(&dir);
                fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
                let paths = if meta.entries.is_empty() {
                    let name = safe_name(header.name.as_deref().or(meta.name.as_deref()).unwrap_or("file"));
                    let path = dir.join(&name);
                    save(plain, &path, max).map_err(failed)?;
                    vec![path]
                } else {
                    let zip = incoming.join(format!("{}.zip", self.id));
                    let unpacked = save(plain, &zip, max)
                        .and_then(|_| files::unbundle(&zip, &dir, &meta.entries))
                        .map_err(failed);
                    let _ = fs::remove_file(&zip);
                    unpacked?
                };
                files::prune_incoming(KEEP_RECEIVED);
                Ok(Content::Files(paths))
            }
        }
    }
}

/// Writes `plain` to `path` through a `.part` file, so a cut-off download never looks finished.
fn save(plain: &mut dyn Read, path: &PathBuf, max: u64) -> io::Result<()> {
    let mut part = path.as_os_str().to_os_string();
    part.push(".part");
    let part = PathBuf::from(part);
    let result = (|| {
        let mut out = File::create(&part)?;
        let n = io::copy(&mut plain.take(max + 1), &mut out)?;
        if n > max {
            return Err(io::Error::other("the clip is larger than the limit"));
        }
        out.flush()?;
        fs::rename(&part, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result
}

fn server_error(mut res: ureq::http::Response<ureq::Body>) -> String {
    let status = res.status();
    res.body_mut()
        .read_json::<serde_json::Value>()
        .ok()
        .and_then(|v| v["error"].as_str().map(String::from))
        .unwrap_or_else(|| format!("server answered {status}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_survive_png() {
        let rgba: Vec<u8> = (0..4 * 3 * 2).map(|i| i as u8).collect();
        let png = encode_png(3, 2, &rgba).unwrap();
        assert_eq!(
            decode_png(&png).unwrap(),
            Content::Image {
                width: 3,
                height: 2,
                rgba
            }
        );
    }

    #[test]
    fn meta_matches_the_web_page_format() {
        let meta = Meta {
            kind: "files".into(),
            from: "laptop".into(),
            size: 3,
            name: Some("a.zip".into()),
            mime: Some("application/zip".into()),
            entries: vec!["a".into(), "b".into()],
            ..Meta::default()
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"files","from":"laptop","size":3,"name":"a.zip","type":"application/zip","entries":["a","b"]}"#
        );
        assert_eq!(meta.describe(), "2 files (3 B)");
    }

    #[test]
    fn several_files_are_zipped_and_one_is_sent_as_is() {
        let root = std::env::temp_dir().join(format!("ml-clip-prepare-{}", std::process::id()));
        fs::create_dir_all(root.join("dir")).unwrap();
        fs::write(root.join("dir/a.txt"), "a").unwrap();
        fs::write(root.join("b.txt"), "bb").unwrap();
        let one = prepare(Content::Files(vec![root.join("b.txt")]), "x", "me".into(), 1024).unwrap();
        assert_eq!((one.meta.name.as_deref(), one.meta.size), (Some("b.txt"), 2));
        assert!(one.meta.entries.is_empty());
        let id = format!("test-{}", std::process::id());
        let two = prepare(
            Content::Files(vec![root.join("dir"), root.join("b.txt")]),
            &id,
            "me".into(),
            1 << 20,
        )
        .unwrap();
        assert_eq!(two.meta.entries, ["dir", "b.txt"]);
        two.blob.unwrap().discard();
        assert!(prepare(Content::Files(vec![root.join("b.txt")]), "x", "me".into(), 1).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
