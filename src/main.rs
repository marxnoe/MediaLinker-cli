//! `ml`: share files and text through a MediaLinker server with 5-character codes.

use std::{
    env,
    fs::{self, File},
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use qrcode::{QrCode, render::unicode::Dense1x2};
use serde::Deserialize;
use ureq::{Agent, SendBody, http::Response};

#[derive(Parser)]
#[command(version, about = "Share files and text through MediaLinker with short codes")]
struct Cli {
    /// MediaLinker server (overrides ML_SERVER and `ml server`; default https://medialinker.noecore.com)
    #[arg(long, short, global = true, value_name = "URL")]
    server: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Share a file or some text and print its code
    Send {
        /// A file path, some text, or `-` to read stdin (stdin is also read when nothing is given)
        input: Vec<String>,
        /// Send the input as text even if it names a file
        #[arg(long, short)]
        text: bool,
        /// Send stdin as a file with this name instead of as text
        #[arg(long, short, value_name = "NAME")]
        name: Option<String>,
        /// How long the share lives
        #[arg(long, value_enum, default_value_t = Ttl::Day)]
        ttl: Ttl,
        /// Don't print a QR code for the receive page
        #[arg(long)]
        no_qr: bool,
    },
    /// Fetch a share: print text, or download a file into the current directory
    Get {
        code: String,
        /// Where to save a file (`-` writes it to stdout)
        #[arg(long, short, value_name = "PATH")]
        output: Option<PathBuf>,
        /// Overwrite an existing file
        #[arg(long, short)]
        force: bool,
    },
    /// Show the saved server, or save a new one
    Server { url: Option<String> },
    /// Print a shell completion script, e.g. `ml completions zsh > ~/.zfunc/_ml`
    Completions { shell: Shell },
}

#[derive(Clone, Copy, ValueEnum)]
enum Ttl {
    #[value(name = "1h")]
    Hour,
    #[value(name = "24h")]
    Day,
    #[value(name = "7d")]
    Week,
}

impl Ttl {
    fn as_str(self) -> &'static str {
        match self {
            Ttl::Hour => "1h",
            Ttl::Day => "24h",
            Ttl::Week => "7d",
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Created {
    code: String,
    expires_at: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Share {
    code: String,
    kind: String,
    text: Option<String>,
    name: Option<String>,
    size: u64,
}

#[derive(Deserialize)]
struct ApiError {
    error: String,
}

type Result<T> = std::result::Result<T, String>;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ml: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Server { url: None } => {
            println!("{}", server(cli.server)?);
            Ok(())
        }
        Command::Server { url: Some(url) } => save_server(&url),
        Command::Send {
            input,
            text,
            name,
            ttl,
            no_qr,
        } => send(&server(cli.server)?, input, text, name, ttl, !no_qr),
        Command::Get { code, output, force } => get(&server(cli.server)?, &code, output, force),
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "ml", &mut io::stdout());
            Ok(())
        }
    }
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("medialinker").join("server"))
}

fn normalize_server(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        return Err("server URL is empty".into());
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(url.to_string())
    } else {
        Ok(format!("https://{url}"))
    }
}

const DEFAULT_SERVER: &str = "https://medialinker.noecore.com";

fn server(flag: Option<String>) -> Result<String> {
    let saved = || config_path().and_then(|p| fs::read_to_string(p).ok());
    let url = flag.or_else(|| env::var("ML_SERVER").ok()).or_else(saved);
    normalize_server(url.as_deref().unwrap_or(DEFAULT_SERVER))
}

fn save_server(url: &str) -> Result<()> {
    let url = normalize_server(url)?;
    let path = config_path().ok_or("cannot find a config directory")?;
    fs::create_dir_all(path.parent().unwrap()).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    fs::write(&path, &url).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    eprintln!("Saved server {url}");
    Ok(())
}

fn agent() -> Agent {
    Agent::config_builder()
        .http_status_as_error(false)
        .user_agent(concat!("ml/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// Turns a non-2xx response into the server's `{"error": ...}` message.
fn check(mut res: Response<ureq::Body>) -> Result<Response<ureq::Body>> {
    if res.status().is_success() {
        return Ok(res);
    }
    let status = res.status();
    match res.body_mut().read_json::<ApiError>() {
        Ok(e) => Err(e.error),
        Err(_) => Err(format!("server answered {status}")),
    }
}

fn net_err(server: &str) -> impl Fn(ureq::Error) -> String + '_ {
    move |e| format!("cannot reach {server}: {e}")
}

enum Payload {
    Text(String),
    File { name: String, reader: Box<dyn Read> },
}

fn read_stdin(name: Option<String>) -> Result<Payload> {
    let mut stdin = io::stdin();
    if let Some(name) = name {
        return Ok(Payload::File {
            name,
            reader: Box::new(stdin),
        });
    }
    let mut text = String::new();
    stdin
        .read_to_string(&mut text)
        .map_err(|_| "stdin is not UTF-8 text; pass --name <NAME> to send it as a file")?;
    Ok(Payload::Text(text))
}

fn payload(input: Vec<String>, text: bool, name: Option<String>) -> Result<Payload> {
    if input.is_empty() || input == ["-"] {
        if io::stdin().is_terminal() {
            return Err("nothing to send: give a file, some text, or pipe something in".into());
        }
        return read_stdin(name);
    }
    if name.is_some() {
        return Err("--name only applies when reading stdin".into());
    }
    let joined = input.join(" ");
    let path = Path::new(&joined);
    if !text && input.len() == 1 && path.is_file() {
        let file = File::open(path).map_err(|e| format!("cannot open {joined}: {e}"))?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        return Ok(Payload::File {
            name,
            reader: Box::new(file),
        });
    }
    Ok(Payload::Text(joined))
}

/// Quotes a multipart header value; the server keeps only the base name anyway.
fn quote(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '"' | '\\' | '\r' | '\n' => '_',
            c => c,
        })
        .collect()
}

fn send(server: &str, input: Vec<String>, text: bool, name: Option<String>, ttl: Ttl, qr: bool) -> Result<()> {
    let boundary = format!("ml-{:016x}{:016x}", rand_u64(), rand_u64());
    let (head, body): (String, Box<dyn Read>) = match payload(input, text, name)? {
        Payload::Text(t) => {
            if t.trim().is_empty() {
                return Err("text is empty".into());
            }
            (
                "Content-Disposition: form-data; name=\"text\"\r\n\r\n".into(),
                Box::new(io::Cursor::new(t.into_bytes())),
            )
        }
        Payload::File { name, reader } => (
            format!(
                "Content-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\n\
                 Content-Type: application/octet-stream\r\n\r\n",
                quote(&name)
            ),
            reader,
        ),
    };
    let mut multipart = io::Cursor::new(format!("--{boundary}\r\n{head}"))
        .chain(body)
        .chain(io::Cursor::new(format!("\r\n--{boundary}--\r\n")));

    let res = agent()
        .post(format!("{server}/share?ttl={}", ttl.as_str()))
        .header("Content-Type", format!("multipart/form-data; boundary={boundary}"))
        .send(SendBody::from_reader(&mut multipart))
        .map_err(net_err(server))?;
    let created: Created = check(res)?
        .body_mut()
        .read_json()
        .map_err(|e| format!("unexpected answer from server: {e}"))?;
    println!("{}", created.code);
    if io::stderr().is_terminal() {
        let link = format!("{server}/?code={}", created.code);
        if qr && let Some(code) = qr_code(&link) {
            eprint!("{code}");
        }
        eprintln!("Open {link}");
        eprintln!("Expires in {} ({})", ttl.as_str(), expiry(created.expires_at));
    }
    Ok(())
}

/// Renders `link` as a QR code in half-block characters, dark on light so it scans on any terminal theme.
fn qr_code(link: &str) -> Option<String> {
    let code = QrCode::new(link).ok()?;
    let art = code.render::<Dense1x2>().quiet_zone(true).build();
    if env::var_os("NO_COLOR").is_some() {
        return Some(format!("{art}\n"));
    }
    Some(art.lines().map(|l| format!("\x1b[30;107m{l}\x1b[0m\n")).collect())
}

fn get(server: &str, code: &str, output: Option<PathBuf>, force: bool) -> Result<()> {
    let code = code.trim().to_ascii_uppercase();
    if code.len() != 5 || !code.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(format!("{code:?} is not a share code (codes are 5 letters and digits)"));
    }
    let agent = agent();
    let share: Share = check(
        agent
            .get(format!("{server}/share/{code}"))
            .call()
            .map_err(net_err(server))?,
    )?
    .body_mut()
    .read_json()
    .map_err(|e| format!("unexpected answer from server: {e}"))?;

    if share.kind == "text" {
        let text = share.text.unwrap_or_default();
        return match output {
            Some(path) if path != Path::new("-") => write_new(&path, force, &mut text.as_bytes()).map(|_| ()),
            _ => {
                print!("{text}");
                if !text.ends_with('\n') && io::stdout().is_terminal() {
                    println!();
                }
                Ok(())
            }
        };
    }

    let mut res = check(
        agent
            .get(format!("{server}/s/{}/file?download=1", share.code))
            .call()
            .map_err(net_err(server))?,
    )?;
    let mut body = res.body_mut().as_reader();
    if output.as_deref() == Some(Path::new("-")) {
        io::copy(&mut body, &mut io::stdout().lock()).map_err(|e| format!("download failed: {e}"))?;
        return Ok(());
    }
    let path = output.unwrap_or_else(|| PathBuf::from(safe_name(share.name.as_deref())));
    let size = write_new(&path, force, &mut body)?;
    if size != share.size {
        let _ = fs::remove_file(&path);
        return Err(format!("download incomplete ({size} of {} bytes)", share.size));
    }
    eprintln!("Saved {} ({})", path.display(), human_size(size));
    Ok(())
}

/// Writes to a temporary file next to `path` and moves it into place, so a failed download leaves nothing behind.
fn write_new(path: &Path, force: bool, body: &mut dyn Read) -> Result<u64> {
    if path.exists() && !force {
        return Err(format!("{} already exists (use --force to overwrite)", path.display()));
    }
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".part");
    let tmp = path.with_file_name(tmp_name);
    let result = (|| {
        let mut file = File::create(&tmp)?;
        let size = io::copy(body, &mut file)?;
        file.flush()?;
        fs::rename(&tmp, path)?;
        Ok::<_, io::Error>(size)
    })();
    result.map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("cannot save {}: {e}", path.display())
    })
}

/// The server already strips paths, but never trust a remote name with the filesystem.
fn safe_name(name: Option<&str>) -> String {
    let base = name.unwrap_or_default().rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .collect();
    let clean = clean.trim().trim_start_matches('.');
    if clean.is_empty() { "file".into() } else { clean.into() }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

fn expiry(expires_at_ms: u64) -> String {
    let secs = (expires_at_ms / 1000).saturating_sub(now());
    match secs {
        s if s >= 86_400 => format!("{}d {}h left", s / 86_400, s % 86_400 / 3600),
        s if s >= 3600 => format!("{}h {}m left", s / 3600, s % 3600 / 60),
        s => format!("{}m left", s / 60),
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Good enough for a multipart boundary; avoids pulling in a random number crate.
fn rand_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_get_a_scheme_and_lose_trailing_slashes() {
        assert_eq!(
            normalize_server("media.example.com/").unwrap(),
            "https://media.example.com"
        );
        assert_eq!(
            normalize_server("http://localhost:3000").unwrap(),
            "http://localhost:3000"
        );
        assert!(normalize_server("  ").is_err());
    }

    #[test]
    fn remote_names_cannot_escape_the_directory() {
        assert_eq!(safe_name(Some("../../etc/passwd")), "passwd");
        assert_eq!(safe_name(Some("C:\\x\\a:b.txt")), "ab.txt");
        assert_eq!(safe_name(Some("..")), "file");
        assert_eq!(safe_name(Some(".bashrc")), "bashrc");
        assert_eq!(safe_name(None), "file");
    }

    #[test]
    fn links_become_qr_codes() {
        let art = qr_code("https://medialinker.noecore.com/?code=K7QX2").unwrap();
        assert!(art.lines().count() > 10);
    }

    #[test]
    fn completions_cover_every_subcommand() {
        let mut out = Vec::new();
        clap_complete::generate(Shell::Bash, &mut Cli::command(), "ml", &mut out);
        let script = String::from_utf8(out).unwrap();
        for sub in ["send", "get", "server", "completions"] {
            assert!(script.contains(sub), "{sub} missing");
        }
    }

    #[test]
    fn sizes_are_readable() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }
}
