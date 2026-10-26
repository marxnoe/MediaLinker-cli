//! `ml clip`: one clipboard shared by your devices, end-to-end encrypted, through a MediaLinker server.

mod board;
mod files;
mod keys;
#[cfg(target_os = "linux")]
mod linux_files;
mod net;
mod sync;

use std::{
    env,
    fs::{self, File},
    io::IsTerminal,
    path::PathBuf,
    process::Command,
    thread,
    time::Duration,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::{Subcommand, ValueEnum};
use keys::{Code, Pake, RoomKey};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sync::log;

use crate::{Result, e2e, qr_code};

#[derive(Subcommand)]
pub enum ClipCommand {
    /// Keep this device's clipboard in sync. The first time it starts a room and shows a pairing
    /// code; with a code from another device it joins that device's room instead
    Sync {
        /// A pairing code like K7X-42PQ (or the link from its QR code)
        code: Option<String>,
        /// What other devices call this one (default: the computer's name)
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Keep syncing after the terminal closes; output goes to a log file
        #[arg(long)]
        background: bool,
        /// Don't print a QR code with the pairing code
        #[arg(long)]
        no_qr: bool,
    },
    /// Show a pairing code that adds another device (a phone, too) to this room
    Pair {
        /// Don't print a QR code
        #[arg(long)]
        no_qr: bool,
    },
    /// Show the room, this device, and whether syncing is running
    Status,
    /// Leave the room on this device (the other devices stay paired)
    Forget,
    /// Start syncing in the background whenever you log in
    Autostart { state: Toggle },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Toggle {
    On,
    Off,
}

/// What a device remembers about its room, in `clip.json` next to the saved server.
#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    server: String,
    #[serde(with = "key_string")]
    key: RoomKey,
    device: String,
    /// The last clip this device took from the room, so a restart doesn't paste it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seen: Option<String>,
}

mod key_string {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    use super::RoomKey;

    pub fn serialize<S: Serializer>(key: &RoomKey, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&key.encode())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<RoomKey, D::Error> {
        RoomKey::parse(&String::deserialize(d)?).ok_or_else(|| D::Error::custom("not a room key"))
    }
}

fn config_dir() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .ok_or("cannot find a config directory")?
        .join("medialinker"))
}

impl Config {
    fn path() -> Result<PathBuf> {
        Ok(config_dir()?.join("clip.json"))
    }

    fn load() -> Result<Option<Self>> {
        let path = Self::path()?;
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
                format!(
                    "{} is damaged ({e}); run `ml clip forget` and pair again",
                    path.display()
                )
            }),
            Err(_) => Ok(None),
        }
    }

    fn save(&self) -> Result<()> {
        let path = Self::path()?;
        fs::create_dir_all(path.parent().unwrap()).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        let tmp = path.with_extension("json.tmp");
        let write = || -> std::io::Result<()> {
            let mut options = fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            // The room key opens every clip, so only this user may read it.
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            std::io::Write::write_all(&mut options.open(&tmp)?, &serde_json::to_vec_pretty(self)?)?;
            fs::rename(&tmp, &path)
        };
        write().map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
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
    match command {
        ClipCommand::Sync {
            code,
            name,
            background,
            no_qr,
        } => sync_command(server, code, name, background, !no_qr),
        ClipCommand::Pair { no_qr } => {
            let config = Config::load()?.ok_or("this device isn't in a room yet: run `ml clip sync` first")?;
            host_pairing(&config, !no_qr, false)
        }
        ClipCommand::Status => status(),
        ClipCommand::Forget => {
            let path = Config::path()?;
            if !path.exists() {
                eprintln!("This device isn't in a clipboard room");
                return Ok(());
            }
            if lock().is_none() {
                return Err("`ml clip sync` is running on this device; stop it first".into());
            }
            fs::remove_file(&path).map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
            let _ = fs::remove_dir_all(files::cache_dir());
            eprintln!("Left the room. Other devices stay paired with each other.");
            Ok(())
        }
        ClipCommand::Autostart { state } => autostart(state),
    }
}

fn sync_command(
    server: Result<String>,
    code: Option<String>,
    name: Option<String>,
    background: bool,
    qr: bool,
) -> Result<()> {
    let existing = Config::load()?;
    let mut fresh = false;
    let mut config = match (code, existing) {
        (Some(code), existing) => {
            // A link names its own server.
            let server = code
                .split_once("/#clip=")
                .map(|(s, _)| crate::normalize_server(s))
                .unwrap_or(server)?;
            let code = Code::parse(&code).ok_or("that is not a pairing code (they look like K7X-42PQ)")?;
            let device = name
                .clone()
                .unwrap_or_else(|| existing.as_ref().map_or_else(device_name, |c| c.device.clone()));
            let (key, host) = join_pairing(&server, &code, &device)?;
            if existing.is_some_and(|c| c.key.room_id() != key.room_id()) {
                eprintln!("Left the previous room.");
            }
            eprintln!("Paired with {host}.");
            Config {
                server,
                key,
                device,
                seen: None,
            }
        }
        (None, Some(config)) => config,
        (None, None) => {
            if background {
                return Err("pair this device first: run `ml clip sync` (or `ml clip sync CODE`) in a terminal".into());
            }
            fresh = true;
            Config {
                server: server?,
                key: RoomKey::generate()?,
                device: name.clone().unwrap_or_else(device_name),
                seen: None,
            }
        }
    };
    if let Some(name) = name {
        config.device = name;
    }
    config.save()?;

    if background {
        return spawn_background();
    }
    let Some(_lock) = lock() else {
        return Err("`ml clip sync` is already running on this device".into());
    };
    if fresh {
        eprintln!("Started a new clipboard room. Add your other devices with this code:");
        let pairing = config.clone();
        thread::spawn(move || {
            if let Err(e) = host_pairing(&pairing, qr, true) {
                log(format!("Pairing: {e}. Run `ml clip pair` for a new code."));
            }
        });
    }
    sync::run(config)
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

fn log_path() -> PathBuf {
    files::cache_dir().join("sync.log")
}

fn spawn_background() -> Result<()> {
    if lock().is_none() {
        eprintln!("Already syncing on this device.");
        return Ok(());
    }
    let exe = env::current_exe().map_err(|e| format!("cannot find the ml program: {e}"))?;
    let log_file = log_path();
    let out = File::create(&log_file).map_err(|e| format!("cannot create {}: {e}", log_file.display()))?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.args(["clip", "sync"])
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    #[cfg(windows)]
    {
        const DETACHED_PROCESS: u32 = 0x8;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::os::windows::process::CommandExt::creation_flags(
            &mut cmd,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW,
        );
    }
    cmd.spawn()
        .map_err(|e| format!("cannot start syncing in the background: {e}"))?;
    eprintln!("Syncing in the background. Log: {}", log_file.display());
    Ok(())
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

/// Shows codes for other devices to join with, until one joins (`once`) or the user stops it.
/// A wrong code burns that pairing, so a fresh code replaces it.
fn host_pairing(config: &Config, qr: bool, once: bool) -> Result<()> {
    let mut failures = 0;
    loop {
        match host_once(config, qr) {
            Ok(name) => {
                log(format!("Paired with {name}."));
                failures = 0;
                if once {
                    return Ok(());
                }
                eprintln!("Another device? Here is a new code (Ctrl+C when done):");
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

fn host_once(config: &Config, qr: bool) -> std::result::Result<String, Hosting> {
    let failed = Hosting::Failed;
    let mut socket = net::connect(&config.server, "/clip/pair").map_err(Hosting::Expired)?;
    net::send(&mut socket, r#"{"t":"host"}"#.into()).map_err(failed)?;
    let plate: Plate = net::expect(&mut socket, PAIR_STEP).map_err(Hosting::Expired)?;
    let code = Code::new(&plate.plate).map_err(failed)?;
    show_code(&config.server, &code, qr);
    let start: Start = net::expect(&mut socket, PAIR_WAIT).map_err(Hosting::Expired)?;
    let pake = Pake::start(&code, &start.sid).map_err(failed)?;
    let mine = pake.public;
    send_data(&mut socket, &mine).map_err(failed)?;
    let theirs = public_key(recv_data(&mut socket).map_err(failed)?).map_err(failed)?;
    let keys = pake.finish(&mine, &theirs).map_err(failed)?;
    let welcome = Welcome {
        key: config.key.encode(),
        from: config.device.clone(),
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

fn status() -> Result<()> {
    let Some(config) = Config::load()? else {
        println!("Not in a clipboard room. Run `ml clip sync` to start one.");
        return Ok(());
    };
    println!("Room:      {}… on {}", &config.key.room_id()[..8], config.server);
    println!("Device:    {}", config.device);
    let running = lock().is_none();
    println!("Syncing:   {}", if running { "yes" } else { "no (run `ml clip sync`)" });
    println!("Autostart: {}", if autostart_path()?.exists() { "on" } else { "off" });
    if running && log_path().exists() {
        println!("Log:       {}", log_path().display());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn autostart_path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .ok_or("cannot find a config directory")?
        .join("autostart/ml-clip.desktop"))
}

#[cfg(target_os = "macos")]
fn autostart_path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .ok_or("cannot find the home directory")?
        .join("Library/LaunchAgents/com.noecore.ml-clip.plist"))
}

/// Windows keeps the setting in the registry; this file only records that it was turned on.
#[cfg(windows)]
fn autostart_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("autostart"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn autostart_path() -> Result<PathBuf> {
    Err("autostart isn't supported on this system".into())
}

fn autostart(state: Toggle) -> Result<()> {
    let path = autostart_path()?;
    let exe = env::current_exe().map_err(|e| format!("cannot find the ml program: {e}"))?;
    let exe = exe.to_string_lossy();
    match state {
        Toggle::On => {
            if Config::load()?.is_none() {
                return Err("pair this device first: run `ml clip sync` (or `ml clip sync CODE`)".into());
            }
            let running = lock().is_none();
            fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
            write_autostart(&path, &exe)?;
            eprintln!("ml clip sync now starts when you log in.");
            if cfg!(target_os = "macos") {
                // launchd starts it as soon as the agent is loaded.
                eprintln!("Log: {}", log_path().display());
            } else if !running {
                spawn_background()?;
            }
        }
        Toggle::Off => {
            remove_autostart(&path)?;
            eprintln!("ml clip sync no longer starts when you log in. A running sync keeps going until you log out.");
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn write_autostart(path: &std::path::Path, exe: &str) -> Result<()> {
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=MediaLinker clipboard sync\n\
         Exec=\"{exe}\" clip sync --background\nTerminal=false\nNoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n"
    );
    fs::write(path, entry).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(target_os = "macos")]
fn write_autostart(path: &std::path::Path, exe: &str) -> Result<()> {
    let log = log_path();
    let log = log.to_string_lossy();
    let escape = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.noecore.ml-clip</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>clip</string><string>sync</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>StandardErrorPath</key><string>{}</string>
  <key>StandardOutPath</key><string>{}</string>
</dict>
</plist>
"#,
        escape(exe),
        escape(&log),
        escape(&log)
    );
    fs::create_dir_all(files::cache_dir()).map_err(|e| e.to_string())?;
    fs::write(path, plist).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    let _ = Command::new("launchctl").arg("unload").arg(path).output();
    let ok = Command::new("launchctl")
        .args(["load", "-w"])
        .arg(path)
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        Ok(())
    } else {
        Err("launchctl could not load the login item".into())
    }
}

#[cfg(windows)]
fn write_autostart(path: &std::path::Path, exe: &str) -> Result<()> {
    let ok = Command::new("reg")
        .args([
            "add",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "MediaLinker clip",
            "/t",
            "REG_SZ",
        ])
        .args(["/d", &format!("\"{exe}\" clip sync --background"), "/f"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        return Err("could not add ml to the programs that start at login".into());
    }
    fs::write(path, "on").map_err(|e| e.to_string())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn write_autostart(_: &std::path::Path, _: &str) -> Result<()> {
    Err("autostart isn't supported on this system".into())
}

fn remove_autostart(path: &std::path::Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    if path.exists() {
        let _ = Command::new("launchctl").arg("unload").arg(path).output();
    }
    #[cfg(windows)]
    {
        let _ = Command::new("reg")
            .args([
                "delete",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "MediaLinker clip",
                "/f",
            ])
            .output();
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
    }
}
