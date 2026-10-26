//! The system clipboard: one thread owns it, watches it for copies and applies clips from other
//! devices.
//!
//! Every content gets a fingerprint, and the thread remembers the fingerprint of whatever is on the
//! clipboard now. After applying a clip it reads the clipboard back and remembers what the system
//! actually stored, so its own write (converted to CRLF on Windows, re-encoded as TIFF on macOS)
//! never comes back as a new copy and no clip ever bounces between devices.
//!
//! Windows and macOS count clipboard changes, so the clipboard is only read when that count moves.
//! On Linux an X11 connection reports owner changes when there is one (Xwayland included), and a
//! slow full check covers anything it misses.

use std::{
    borrow::Cow,
    path::PathBuf,
    sync::mpsc::{self, RecvTimeoutError, Sender},
    thread,
    time::{Duration, Instant},
};

use arboard::{Clipboard, Error as ClipError, ImageData};
use sha2::{Digest, Sha256};

const POLL: Duration = Duration::from_millis(300);
/// Without change notifications, images (costly to read) and anything missed are checked this often.
const FULL_CHECK: Duration = Duration::from_secs(2);

#[derive(Clone, PartialEq, Debug)]
pub enum Content {
    /// Always with `\n` line breaks.
    Text(String),
    /// RGBA pixels.
    Image {
        width: usize,
        height: usize,
        rgba: Vec<u8>,
    },
    Files(Vec<PathBuf>),
}

pub type Fingerprint = [u8; 16];

impl Content {
    pub fn fingerprint(&self) -> Fingerprint {
        let mut h = Sha256::new();
        match self {
            Content::Text(t) => {
                h.update(b"text\0");
                h.update(t.as_bytes());
            }
            Content::Image { width, height, rgba } => {
                h.update(b"image\0");
                h.update((*width as u64).to_le_bytes());
                h.update((*height as u64).to_le_bytes());
                h.update(rgba);
            }
            Content::Files(paths) => {
                h.update(b"files\0");
                for p in paths {
                    h.update(p.to_string_lossy().as_bytes());
                    h.update(b"\0");
                }
            }
        }
        h.finalize()[..16].try_into().unwrap()
    }
}

pub fn normalize_text(text: String) -> String {
    if text.contains('\r') {
        text.replace("\r\n", "\n")
    } else {
        text
    }
}

pub enum Command {
    /// Puts a clip on the clipboard, unless something was copied here after copy number `copies`,
    /// the last one the caller had seen when it decided: then the local copy is newer and wins.
    Apply { content: Content, copies: u64 },
    /// Stops the thread and hands back the clip it last applied, if that's still on the clipboard.
    Release(Sender<Option<Content>>),
}

/// Tells the clipboard thread what to do; dropping it stops the thread.
pub struct Board {
    tx: Sender<Command>,
}

impl Board {
    pub fn apply(&self, content: Content, copies: u64) {
        let _ = self.tx.send(Command::Apply { content, copies });
    }

    /// Stops watching. Returns the clip from another device that is still on the clipboard: on
    /// Linux it disappears when this process exits, unless something keeps offering it.
    pub fn release(&self) -> Option<Content> {
        let (tx, rx) = mpsc::channel();
        self.tx.send(Command::Release(tx)).ok()?;
        rx.recv_timeout(Duration::from_secs(2)).ok().flatten()
    }
}

/// Opens the clipboard on its own thread and reports each new local copy, numbered from 1, through
/// `copied`.
pub fn spawn(copied: impl Fn(Content, u64) + Send + 'static) -> Result<Board, String> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || {
            let mut clipboard = match Clipboard::new() {
                Ok(c) => {
                    let _ = ready_tx.send(Ok(()));
                    c
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("cannot open the clipboard: {e}")));
                    return;
                }
            };
            let mut w = Watcher {
                changes: Changes::new(),
                last: None,
                seen: None,
                full_check: Instant::now(),
                copies: 0,
                applied: None,
            };
            // Whatever is on the clipboard at start was copied before syncing began; it isn't news.
            w.last = read(&mut clipboard, true).ok().flatten().map(|c| c.fingerprint());
            w.seen = w.changes.count();
            loop {
                match rx.recv_timeout(POLL) {
                    Ok(Command::Apply { content, copies }) => {
                        // A copy made here since the caller decided is newer; it wins, and so does one
                        // that is on the clipboard but not noticed yet.
                        if copies < w.copies || w.check(&mut clipboard, true, &copied) {
                            continue;
                        }
                        let applied = content.fingerprint();
                        if let Err(e) = write(&mut clipboard, &content) {
                            eprintln!("Cannot put the clip on the clipboard: {e}");
                            continue;
                        }
                        // Some systems hand out the new content a moment after it was set.
                        thread::sleep(Duration::from_millis(50));
                        let stored = read(&mut clipboard, true).ok().flatten();
                        w.last = Some(stored.map_or(applied, |c| c.fingerprint()));
                        w.seen = w.changes.count();
                        w.applied = Some(content);
                    }
                    Ok(Command::Release(reply)) => {
                        w.check(&mut clipboard, true, &copied);
                        let _ = reply.send(w.applied.take());
                        return;
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        w.check(&mut clipboard, false, &copied);
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        })
        .map_err(|e| format!("cannot start the clipboard thread: {e}"))?;
    ready_rx
        .recv()
        .map_err(|_| "the clipboard thread stopped".to_string())??;
    Ok(Board { tx })
}

struct Watcher {
    changes: Changes,
    /// The fingerprint of what's on the clipboard, as far as we know.
    last: Option<Fingerprint>,
    /// The change count when the clipboard was last read.
    seen: Option<u64>,
    full_check: Instant,
    copies: u64,
    /// The clip from another device on the clipboard now, until something is copied here.
    applied: Option<Content>,
}

impl Watcher {
    /// Looks for a new local copy and reports it; returns whether there was one.
    fn check(&mut self, clipboard: &mut Clipboard, now: bool, copied: &impl Fn(Content, u64)) -> bool {
        let count = self.changes.count();
        let full = now || self.full_check.elapsed() >= FULL_CHECK;
        if count.is_some() && count == self.seen && !full {
            return false;
        }
        if full {
            self.full_check = Instant::now();
        }
        // Reading an image means decoding it, so without a change count it waits for the full check.
        // Busy (Windows lets one program hold the clipboard at a time): try again next round.
        let Ok(content) = read(clipboard, count.is_some() || full) else {
            return false;
        };
        self.seen = count;
        let Some(content) = content else { return false };
        let fp = content.fingerprint();
        if self.last == Some(fp) {
            return false;
        }
        self.last = Some(fp);
        self.applied = None;
        self.copies += 1;
        copied(content, self.copies);
        true
    }
}

/// What's on the clipboard, preferring files, then text, then an image; `Err` if it couldn't be read now.
fn read(clipboard: &mut Clipboard, images: bool) -> Result<Option<Content>, ()> {
    fn absent<T>(r: Result<T, ClipError>) -> Result<Option<T>, ()> {
        match r {
            Ok(v) => Ok(Some(v)),
            Err(ClipError::ContentNotAvailable | ClipError::ConversionFailure) => Ok(None),
            Err(ClipError::ClipboardOccupied) => Err(()),
            // Anything else (an odd format, a vanished owner) is as good as empty.
            Err(_) => Ok(None),
        }
    }
    if let Some(files) = absent(clipboard.get().file_list())?
        && !files.is_empty()
    {
        return Ok(Some(Content::Files(files)));
    }
    if let Some(text) = absent(clipboard.get_text())?
        && !text.is_empty()
    {
        return Ok(Some(Content::Text(normalize_text(text))));
    }
    if images && let Some(img) = absent(clipboard.get_image())? {
        return Ok(Some(Content::Image {
            width: img.width,
            height: img.height,
            rgba: img.bytes.into_owned(),
        }));
    }
    Ok(None)
}

fn write(clipboard: &mut Clipboard, content: &Content) -> Result<(), ClipError> {
    match content {
        Content::Text(text) => {
            if cfg!(windows) {
                clipboard.set_text(text.replace('\n', "\r\n"))
            } else {
                clipboard.set_text(text.as_str())
            }
        }
        Content::Image { width, height, rgba } => clipboard.set_image(ImageData {
            width: *width,
            height: *height,
            bytes: Cow::Borrowed(rgba),
        }),
        #[cfg(target_os = "linux")]
        Content::Files(paths) => {
            super::linux_files::set(paths, false).map_err(|description| ClipError::Unknown { description })
        }
        #[cfg(not(target_os = "linux"))]
        Content::Files(paths) => clipboard.set().file_list(paths),
    }
}

/// The system's count of clipboard changes, where there is one.
struct Changes {
    #[cfg(target_os = "linux")]
    x11: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}

#[cfg(windows)]
impl Changes {
    fn new() -> Self {
        Self {}
    }

    fn count(&self) -> Option<u64> {
        // Zero means the count isn't available to this process.
        let n = unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() };
        (n != 0).then_some(n as u64)
    }
}

#[cfg(target_os = "macos")]
impl Changes {
    fn new() -> Self {
        Self {}
    }

    fn count(&self) -> Option<u64> {
        let n = objc2_app_kit::NSPasteboard::generalPasteboard().changeCount();
        Some(n as u64)
    }
}

#[cfg(target_os = "linux")]
impl Changes {
    fn new() -> Self {
        Self { x11: x11_watch() }
    }

    fn count(&self) -> Option<u64> {
        let n = self.x11.as_ref()?.load(std::sync::atomic::Ordering::Relaxed);
        // u64::MAX marks a connection that died.
        (n != u64::MAX).then_some(n)
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
impl Changes {
    fn new() -> Self {
        Self {}
    }

    fn count(&self) -> Option<u64> {
        None
    }
}

/// Counts XFixes owner changes of the CLIPBOARD selection on a thread of its own.
#[cfg(target_os = "linux")]
fn x11_watch() -> Option<std::sync::Arc<std::sync::atomic::AtomicU64>> {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    use x11rb::{
        connection::Connection,
        protocol::{
            xfixes::{ConnectionExt as _, SelectionEventMask},
            xproto::ConnectionExt as _,
        },
    };

    let (conn, screen) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots[screen].root;
    let atom = conn.intern_atom(false, b"CLIPBOARD").ok()?.reply().ok()?.atom;
    conn.xfixes_query_version(5, 0).ok()?.reply().ok()?;
    let mask = SelectionEventMask::SET_SELECTION_OWNER
        | SelectionEventMask::SELECTION_WINDOW_DESTROY
        | SelectionEventMask::SELECTION_CLIENT_CLOSE;
    conn.xfixes_select_selection_input(root, atom, mask).ok()?;
    conn.flush().ok()?;
    let count = Arc::new(AtomicU64::new(0));
    let counter = count.clone();
    thread::Builder::new()
        .name("x11-clipboard-events".into())
        .spawn(move || {
            loop {
                match conn.wait_for_event() {
                    Ok(x11rb::protocol::Event::XfixesSelectionNotify(_)) => {
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(_) => {}
                    Err(_) => {
                        counter.store(u64::MAX, Ordering::Relaxed);
                        return;
                    }
                }
            }
        })
        .ok()?;
    Some(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_endings_do_not_change_the_fingerprint() {
        let windows = Content::Text(normalize_text("a\r\nb\r\n".into()));
        let unix = Content::Text("a\nb\n".into());
        assert_eq!(windows.fingerprint(), unix.fingerprint());
        assert_ne!(unix.fingerprint(), Content::Text("a\nb".into()).fingerprint());
        assert_ne!(
            Content::Files(vec!["/a".into()]).fingerprint(),
            Content::Text("/a".into()).fingerprint()
        );
    }
}
