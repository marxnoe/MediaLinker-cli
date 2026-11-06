//! `ml clip`: one clipboard shared by your devices, end-to-end encrypted, through a MediaLinker server.
//!
//! A room lasts one session. The key lives only in the memory of the running `ml clip sync`, and
//! when any device stops syncing (Ctrl+C, a closed terminal, a restart) it ends the room for all of
//! them. Pair again to start a new one.

mod board;
mod files;
#[cfg(target_os = "linux")]
mod keep;
mod keys;
#[cfg(target_os = "linux")]
mod linux_files;
pub mod net;
mod sync;
#[cfg(windows)]
mod windows_files;

use std::{
    fs::{self, File},
    io::{BufRead, IsTerminal},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::Subcommand;
use keys::{Code, Pake, RoomKey};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sync::log;

use crate::{Result, e2e, qr_code};

#[derive(Subcommand)]
pub enum ClipCommand {
    /// Keep this device's clipboard in sync with your other devices until you stop it. Without a
    /// code it starts a new room and shows a pairing code; with a code it joins that room. Stopping
    /// it (Ctrl+C, closing the terminal) ends the room on every device
    Sync {
        /// A pairing code like K7X-42PQ (or the link from its QR code)
        code: Option<String>,
        /// What other devices call this one (default: the computer's name)
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Don't print a QR code with the pairing code
        #[arg(long)]
        no_qr: bool,
    },
    /// Keeps the last clip pasteable after `ml clip sync` exits (Linux; started by `ml` itself)
    #[command(hide = true)]
    Keep,
}

/// The room this run of `ml clip sync` is in. Never written to disk.
#[derive(Clone)]
pub struct Session {
    server: String,
    key: RoomKey,
    device: String,
}

fn device_name() -> String {
    let name = gethostname::gethostname().to_string_lossy().into_owned();
    let name = name.split('.').next().unwrap_or_default().trim();
    if name.is_empty() {
        "computer".into()
    } else {
        name.into()
    }
}

pub fn run(command: ClipCommand, server: Result<String>) -> Result<()> {
    let (code, name, no_qr) = match command {
        ClipCommand::Sync { code, name, no_qr } => (code, name, no_qr),
        #[cfg(target_os = "linux")]
        ClipCommand::Keep => return keep::hold(),
        #[cfg(not(target_os = "linux"))]
        ClipCommand::Keep => return Ok(()),
    };
    let qr = !no_qr;
    let device = name.unwrap_or_else(device_name);
    let Some(_lock) = lock() else {
        return Err("`ml clip sync` is already running on this device".into());
    };
    let fresh = code.is_none();
    let session = match code {
        Some(code) => {
            // A link names its own server.
            let server = code
                .split_once("/#clip=")
                .map(|(s, _)| crate::normalize_server(s))
                .unwrap_or(server)?;
            let code = Code::parse(&code).ok_or("that is not a pairing code (they look like K7X-42PQ)")?;
            let (key, host) = join_pairing(&server, &code, &device)?;
            eprintln!("Paired with {host}.");
            Session { server, key, device }
        }
        None => Session {
            server: server?,
            key: RoomKey::generate()?,
            device,
        },
    };

    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    ctrlc::set_handler(move || {
        if flag.swap(true, Ordering::SeqCst) {
            std::process::exit(130);
        }
        // The sync loop tells the room and exits; this is the fallback if it can't in time (Windows
        // ends the process soon after a console window closes).
        thread::sleep(Duration::from_secs(4));
        std::process::exit(0);
    })
    .map_err(|e| format!("cannot watch for Ctrl+C: {e}"))?;

    let pairing = Arc::new(AtomicBool::new(false));
    let offer = {
        let session = session.clone();
        let pairing = pairing.clone();
        move || {
            if pairing.swap(true, Ordering::SeqCst) {
                return;
            }
            let session = session.clone();
            let pairing = pairing.clone();
            thread::spawn(move || {
                if let Err(e) = host_pairing(&session, qr) {
                    log(format!("Pairing: {e}. Press Enter for a new code."));
                }
                pairing.store(false, Ordering::SeqCst);
            });
        }
    };
    let interactive = std::io::stdin().is_terminal();
    if fresh {
        eprintln!("Started a new clipboard room. Add your other devices with this code:");
        offer();
    } else if interactive {
        eprintln!("Press Enter to show a code for another device.");
    }
    if interactive {
        let offer = offer.clone();
        thread::spawn(move || {
            for _ in std::io::stdin().lock().lines() {
                offer();
            }
        });
    }
    sync::run(session, stop)
}

/// Holds the lock that keeps two syncs on one device from fighting over the clipboard.
fn lock() -> Option<File> {
    let dir = files::cache_dir();
    fs::create_dir_all(&dir).ok()?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("sync.lock"))
        .ok()?;
    file.try_lock().ok()?;
    Some(file)
}

#[derive(Deserialize)]
struct Plate {
    plate: String,
}

#[derive(Deserialize)]
struct Start {
    sid: String,
}

#[derive(Deserialize)]
struct Relayed {
    data: String,
}

const PAIR_WAIT: Duration = Duration::from_secs(11 * 60);
const PAIR_STEP: Duration = Duration::from_secs(30);

fn send_data(socket: &mut net::Socket, data: &[u8]) -> Result<()> {
    net::send(
        socket,
        json!({ "t": "msg", "data": URL_SAFE_NO_PAD.encode(data) }).to_string(),
    )
}

fn recv_data(socket: &mut net::Socket) -> Result<Vec<u8>> {
    let msg: Relayed = net::expect(socket, PAIR_STEP).map_err(|e| {
        if e.contains("closed") {
            "the other device left".to_string()
        } else {
            e
        }
    })?;
    URL_SAFE_NO_PAD
        .decode(msg.data)
        .map_err(|_| "the other device sent a broken pairing message".into())
}

fn public_key(bytes: Vec<u8>) -> Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| "the other device sent a broken pairing message".into())
}

#[derive(Serialize, Deserialize)]
struct Welcome {
    key: String,
    from: String,
}

#[derive(Serialize, Deserialize)]
struct Thanks {
    name: String,
}

/// Shows a code for another device to join with, until one joins. A wrong code burns that pairing,
/// so a fresh code replaces it.
fn host_pairing(session: &Session, qr: bool) -> Result<()> {
    let mut failures = 0;
    loop {
        match host_once(session, qr) {
            Ok(name) => {
                log(format!("Paired with {name}. Press Enter to add another device."));
                return Ok(());
            }
            Err(Hosting::Expired(e)) => return Err(e),
            Err(Hosting::Failed(e)) => {
                failures += 1;
                if failures >= 3 {
                    return Err(e);
                }
                log(format!("Pairing failed: {e}. Here is a new code:"));
            }
        }
    }
}

enum Hosting {
    /// Nobody came; no point in another code.
    Expired(String),
    Failed(String),
}

fn host_once(session: &Session, qr: bool) -> std::result::Result<String, Hosting> {
    let failed = Hosting::Failed;
    let mut socket = net::connect(&session.server, "/clip/pair").map_err(Hosting::Expired)?;
    net::send(&mut socket, r#"{"t":"host"}"#.into()).map_err(failed)?;
    let plate: Plate = net::expect(&mut socket, PAIR_STEP).map_err(Hosting::Expired)?;
    let code = Code::new(&plate.plate).map_err(failed)?;
    show_code(&session.server, &code, qr);
    let start: Start = net::expect(&mut socket, PAIR_WAIT).map_err(Hosting::Expired)?;
    let pake = Pake::start(&code, &start.sid).map_err(failed)?;
    let mine = pake.public;
    send_data(&mut socket, &mine).map_err(failed)?;
    let theirs = public_key(recv_data(&mut socket).map_err(failed)?).map_err(failed)?;
    let keys = pake.finish(&mine, &theirs).map_err(failed)?;
    let welcome = Welcome {
        key: session.key.encode(),
        from: session.device.clone(),
    };
    let sealed = e2e::seal(
        &keys.to_joiner,
        "pair",
        serde_json::to_vec(&welcome).unwrap_or_default(),
    );
    send_data(&mut socket, &sealed).map_err(failed)?;
    // Only a device that knew the code can answer under the session key.
    let wrong = || Hosting::Failed("the other device entered a wrong code".into());
    let answer = recv_data(&mut socket).map_err(|_| wrong())?;
    let thanks = e2e::unseal(&keys.to_host, "pair", answer).map_err(|_| wrong())?;
    let thanks: Thanks = serde_json::from_slice(&thanks).map_err(|_| wrong())?;
    let _ = socket.close(None);
    Ok(thanks.name)
}

fn show_code(server: &str, code: &Code, qr: bool) {
    let link = format!("{server}/#clip={}", code.display());
    let art = qr
        .then(|| qr_code(&link))
        .flatten()
        .filter(|_| std::io::stderr().is_terminal());
    if let Some(art) = &art {
        eprint!("{art}");
    }
    eprintln!();
    eprintln!("    {}", code.display());
    eprintln!();
    eprintln!("On another computer: ml clip sync {}", code.display());
    let scan = if art.is_some() { "scan the QR code, or " } else { "" };
    eprintln!("On a phone: {scan}open {link}");
    eprintln!("The code works once and for 10 minutes.");
}

fn join_pairing(server: &str, code: &Code, device: &str) -> Result<(RoomKey, String)> {
    let mut socket = net::connect(server, "/clip/pair")?;
    net::send(&mut socket, json!({ "t": "join", "plate": code.plate() }).to_string())?;
    let start: Start = net::expect(&mut socket, PAIR_STEP)?;
    let pake = Pake::start(code, &start.sid)?;
    let mine = pake.public;
    send_data(&mut socket, &mine)?;
    let theirs = public_key(recv_data(&mut socket)?)?;
    let keys = pake.finish(&theirs, &mine)?;
    let wrong = "wrong code: check it and try again (the other device shows a new one)";
    let sealed = recv_data(&mut socket)?;
    let welcome = e2e::unseal(&keys.to_joiner, "pair", sealed).map_err(|_| wrong)?;
    let welcome: Welcome = serde_json::from_slice(&welcome).map_err(|_| wrong)?;
    let key = RoomKey::parse(&welcome.key).ok_or(wrong)?;
    let thanks = Thanks { name: device.into() };
    send_data(
        &mut socket,
        &e2e::seal(&keys.to_host, "pair", serde_json::to_vec(&thanks).unwrap_or_default()),
    )?;
    let _ = socket.close(None);
    Ok((key, welcome.from))
}
