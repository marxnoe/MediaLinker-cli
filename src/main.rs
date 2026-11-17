//! `ml`: share files and text through a MediaLinker server with 5-character codes.

mod clip;
mod e2e;
mod public;

use std::{
    env,
    fs::{self, File},
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use e2e::{Decryptor, Encryptor, Header, SecretKey};
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
    Send(SendArgs),
    /// Post a file or some text publicly: listed on the Public page for everyone, and never expires
    Public {
        /// A file path, some text, or `-` to read stdin (stdin is also read when nothing is given)
        input: Vec<String>,
        /// Post the input as text even if it names a file
        #[arg(long, short)]
        text: bool,
        /// Post stdin as a file with this name instead of as text
        #[arg(long, short, value_name = "NAME")]
        name: Option<String>,
        /// Don't print a QR code for the share's page
        #[arg(long)]
        no_qr: bool,
    },
    /// Fetch a share: print text, or download a file into the current directory
    Get {
        /// The code, CODE.KEY for an encrypted share, or a link from the web page
        code: String,
        /// The key of an encrypted share, if it isn't part of the code
        #[arg(long, short, value_name = "KEY")]
        key: Option<String>,
        /// Where to save a file (`-` writes it to stdout)
        #[arg(long, short, value_name = "PATH")]
        output: Option<PathBuf>,
        /// Overwrite an existing file
        #[arg(long, short)]
        force: bool,
    },
    /// List public shares, most liked first, like the Public page
    Browse {
        /// Only shares whose text or file name contains this
        search: Vec<String>,
        /// Newest first instead of most liked
        #[arg(long)]
        new: bool,
        /// Which page of 20 to show
        #[arg(long, default_value_t = 1, value_name = "N")]
        page: u32,
    },
    /// Read the comments on a public share, or post one
    Comments {
        /// The code of a public share, or its link
        code: String,
        /// Post this as a comment instead of reading them
        text: Vec<String>,
        /// The name to post under (Anonymous if not given); `name#secret` adds a tripcode
        #[arg(long, short, value_name = "NAME")]
        name: Option<String>,
    },
    /// Like a public share (once per machine, like once per browser on the site)
    Like {
        /// The code of a public share, or its link
        code: String,
        /// Take the like back
        #[arg(long)]
        undo: bool,
    },
    /// Keep the clipboard of your devices in sync, end-to-end encrypted
    Clip {
        #[command(subcommand)]
        command: clip::ClipCommand,
    },
    /// Show the saved server, or save a new one
    Server { url: Option<String> },
    /// Print a shell completion script, e.g. `ml completions zsh > ~/.zfunc/_ml`
    Completions { shell: Shell },
}

#[derive(Args)]
struct SendArgs {
    /// A file path, some text, or `-` to read stdin (stdin is also read when nothing is given)
    input: Vec<String>,
    /// Send the input as text even if it names a file
    #[arg(long, short)]
    text: bool,
    /// Send stdin as a file with this name instead of as text
    #[arg(long, short, value_name = "NAME")]
    name: Option<String>,
    /// How long the share lives: 1h, 24h, 7d, or your own like 90m or 1d12h (5 minutes to 30 days
    /// for codes; links can go longer, or `never`)
    #[arg(long, default_value = "24h", value_parser = Ttl::parse)]
    ttl: Ttl,
    /// Don't print a QR code
    #[arg(long)]
    no_qr: bool,
    /// Encrypt before uploading, so the server never sees the content; prints CODE.KEY
    #[arg(long, short)]
    encrypt: bool,
    /// Burn after reading: deleted the first time it's opened (short codes only)
    #[arg(long, short)]
    burn: bool,
    /// Make a direct link instead of a short code (plain links take images, audio, video and text)
    #[arg(long, short)]
    link: bool,
    /// Print an is.gd short link instead of the code or link (not for encrypted shares)
    #[arg(long)]
    short: bool,
}

/// How long a share lives, as typed, and what to send the server for it.
#[derive(Clone)]
struct Ttl {
    shown: String,
    /// `never`, or a number of seconds.
    secs: Option<u64>,
}

impl Ttl {
    /// `never`, or a duration of whole numbers with units, like `90m`, `36h` or `1d12h`.
    fn parse(input: &str) -> Result<Self> {
        let shown = input.trim().to_ascii_lowercase();
        if shown == "never" {
            return Ok(Self { shown, secs: None });
        }
        let bad = || format!("{input:?} is not an expiry: use 1h, 24h, 7d, never, or a duration like 90m or 1d12h");
        let (mut secs, mut n) = (0u64, String::new());
        for c in shown.chars() {
            if c.is_ascii_digit() {
                n.push(c);
                continue;
            }
            let unit = match c {
                's' => 1,
                'm' => 60,
                'h' => 3600,
                'd' => 86_400,
                'w' => 7 * 86_400,
                _ => return Err(bad()),
            };
            let part = n
                .parse::<u64>()
                .ok()
                .and_then(|n| n.checked_mul(unit))
                .ok_or_else(bad)?;
            secs = secs.checked_add(part).ok_or_else(bad)?;
            n.clear();
        }
        if !n.is_empty() || secs == 0 {
            return Err(bad());
        }
        Ok(Self {
            shown,
            secs: Some(secs),
        })
    }

    fn is_never(&self) -> bool {
        self.secs.is_none()
    }

    /// Presets go by name, so they work on servers from before custom expiries; anything else
    /// goes as seconds.
    fn query(&self) -> String {
        match self.secs {
            _ if ["1h", "24h", "7d"].contains(&self.shown.as_str()) => self.shown.clone(),
            Some(s) => format!("{s}s"),
            None => "never".into(),
        }
    }
}

/// What the server answers to an upload: a code for shares, a URL and file name for direct links.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Created {
    code: Option<String>,
    url: Option<String>,
    name: Option<String>,
    /// `null` for public shares and direct links that never expire.
    expires_at: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Share {
    code: String,
    kind: String,
    text: Option<String>,
    name: Option<String>,
    size: u64,
    #[serde(default)]
    encrypted: bool,
    #[serde(default)]
    burn: bool,
    #[serde(default)]
    public: bool,
    #[serde(default)]
    likes: u64,
    #[serde(default)]
    comments: u64,
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
        Command::Send(args) => send(&server(cli.server)?, args),
        Command::Public {
            input,
            text,
            name,
            no_qr,
        } => public(&server(cli.server)?, input, text, name, !no_qr),
        Command::Get {
            code,
            key,
            output,
            force,
        } => {
            let target = Target::parse(&code, key)?;
            // A link names its own server, unless one was asked for explicitly.
            let server = server(cli.server.or(target.server.clone()))?;
            get(&server, &target, output, force)
        }
        Command::Browse { search, new, page } => public::browse(&server(cli.server)?, &search.join(" "), new, page),
        Command::Comments { code, text, name } => {
            let (server, code) = public_code(cli.server, &code)?;
            public::comments(&server, &code, &text.join(" "), name)
        }
        Command::Like { code, undo } => {
            let (server, code) = public_code(cli.server, &code)?;
            public::like(&server, &code, undo)
        }
        Command::Clip { command } => clip::run(command, server(cli.server)),
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "ml", &mut io::stdout());
            Ok(())
        }
    }
}

/// The server and code a public share command was pointed at, from a code or a link.
fn public_code(flag: Option<String>, input: &str) -> Result<(String, String)> {
    let target = Target::parse(input, None)?;
    if target.file.is_some() {
        return Err("that's a direct link, not a public share".into());
    }
    Ok((server(flag.or(target.server))?, target.code))
}

fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("medialinker"))
}

fn config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("server"))
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
        .tls_config(clip::net::tls_config())
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

/// The server can't check the size of encrypted text, so `ml` keeps to the same limit as plain text.
const MAX_TEXT_BYTES: usize = 1024 * 1024;

fn send(server: &str, a: SendArgs) -> Result<()> {
    if a.ttl.is_never() && !a.link {
        return Err("--ttl never only works with --link: short codes need an end".into());
    }
    if a.burn && a.link {
        return Err(
            "--burn only works with short codes: link previews in chat apps would open a direct link first".into(),
        );
    }
    if a.short && a.encrypt {
        return Err("--short can't be used with --encrypt: is.gd would see the key in the link".into());
    }
    let payload = payload(a.input, a.text, a.name)?;
    let key = a.encrypt.then(SecretKey::generate).transpose()?;
    let mut query = format!("ttl={}", a.ttl.query());
    if key.is_some() {
        query.push_str("&encrypted=1");
    }
    if a.burn {
        query.push_str("&burn=1");
    }
    let endpoint = if a.link { "upload" } else { "share" };
    let created = upload(server, &format!("{endpoint}?{query}"), payload, key.as_ref()).map_err(|e| {
        if a.link && key.is_none() && e.starts_with("Unsupported file type") {
            format!("{e}: plain direct links only take images, audio, video and text; add -e or leave out --link")
        } else {
            e
        }
    })?;
    // The key only ever goes into what we print; the server got ciphertext.
    let secret = key.map(|k| k.encode());
    let unexpected = || "unexpected answer from server".to_string();
    // What goes to stdout, and the page a phone should open.
    let (mut out, mut link) = if a.link {
        let url = match &secret {
            Some(k) => format!("{server}/d/{}#k={k}", created.name.ok_or_else(unexpected)?),
            None => created.url.ok_or_else(unexpected)?,
        };
        (url.clone(), url)
    } else {
        let code = created.code.ok_or_else(unexpected)?;
        match &secret {
            Some(k) => (format!("{code}.{k}"), format!("{server}/?code={code}#k={k}")),
            None => (code.clone(), format!("{server}/?code={code}")),
        }
    };
    if a.short {
        link = shorten(&link)?;
        out = link.clone();
    }
    println!("{out}");
    if io::stderr().is_terminal() {
        if !a.no_qr
            && let Some(code) = qr_code(&link)
        {
            eprint!("{code}");
        }
        if link != out {
            eprintln!("Open {link}");
        }
        match (&secret, a.link) {
            (Some(_), false) => {
                eprintln!("End-to-end encrypted: the code alone won't open it, pass on the link or CODE.KEY")
            }
            (Some(_), true) => {
                eprintln!("End-to-end encrypted: the key is in the link after #, which never reaches the server")
            }
            _ => {}
        }
        if a.burn {
            eprintln!("Burns after reading: deleted the first time it's opened");
        }
        match created.expires_at {
            Some(t) => eprintln!("Expires in {} ({})", a.ttl.shown, expiry(t)),
            None => eprintln!("Never expires"),
        }
    }
    Ok(())
}

/// Turns a link into an is.gd short link, like the button on the site. Only called on request.
fn shorten(link: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct IsGd {
        shorturl: Option<String>,
        errormessage: Option<String>,
    }
    let mut res = agent()
        .get("https://is.gd/create.php")
        .query("format", "json")
        .query("url", link)
        .call()
        .map_err(|e| format!("cannot reach is.gd: {e}"))?;
    let body: IsGd = res
        .body_mut()
        .read_json()
        .map_err(|e| format!("unexpected answer from is.gd: {e}"))?;
    body.shorturl.ok_or_else(|| {
        body.errormessage
            .unwrap_or_else(|| "is.gd could not shorten that link".into())
    })
}

/// Posts a public share, like "Post something" on the site's Public page.
fn public(server: &str, input: Vec<String>, text: bool, name: Option<String>, qr: bool) -> Result<()> {
    let payload = payload(input, text, name)?;
    let created = upload(server, "share?public=1", payload, None)?;
    let code = created.code.ok_or("unexpected answer from server")?;
    println!("{code}");
    if io::stderr().is_terminal() {
        show_link(&format!("{server}/?code={code}"), qr);
        eprintln!("Public: anyone can see, like and comment on it at {server}/public. It never expires.");
    }
    Ok(())
}

/// Prints the QR code (if asked) and the link to a share's page.
fn show_link(link: &str, qr: bool) {
    if qr && let Some(code) = qr_code(link) {
        eprint!("{code}");
    }
    eprintln!("Open {link}");
}

/// Uploads to `{server}/{path}` (with its query) as multipart, encrypting first if there is a key.
fn upload(server: &str, path: &str, payload: Payload, key: Option<&SecretKey>) -> Result<Created> {
    let boundary = format!("ml-{:016x}{:016x}", rand_u64(), rand_u64());
    if let Payload::Text(t) = &payload
        && t.trim().is_empty()
    {
        return Err("text is empty".into());
    }
    let (head, body): (String, Box<dyn Read>) = match (payload, key) {
        (Payload::Text(t), Some(key)) => {
            if t.len() > MAX_TEXT_BYTES {
                return Err("text exceeds the 1 MB limit".into());
            }
            let header = Header {
                kind: "text".into(),
                name: None,
                mime: None,
            };
            let content = Box::new(io::Cursor::new(t.into_bytes()));
            (encrypted_head(), Box::new(Encryptor::new(key, &header, content)))
        }
        (Payload::File { name, reader }, Some(key)) => {
            let header = Header {
                kind: "file".into(),
                name: Some(name),
                mime: None,
            };
            (encrypted_head(), Box::new(Encryptor::new(key, &header, reader)))
        }
        (Payload::Text(t), None) => (
            "Content-Disposition: form-data; name=\"text\"\r\n\r\n".into(),
            Box::new(io::Cursor::new(t.into_bytes())),
        ),
        (Payload::File { name, reader }, None) => (
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
        .post(format!("{server}/{path}"))
        .header("Content-Type", format!("multipart/form-data; boundary={boundary}"))
        .send(SendBody::from_reader(&mut multipart))
        .map_err(net_err(server))?;
    check(res)?
        .body_mut()
        .read_json()
        .map_err(|e| format!("unexpected answer from server: {e}"))
}

/// Encrypted shares are always an opaque file; the real name and kind are inside the ciphertext.
fn encrypted_head() -> String {
    "Content-Disposition: form-data; name=\"file\"; filename=\"encrypted.bin\"\r\n\
     Content-Type: application/octet-stream\r\n\r\n"
        .into()
}

/// What `ml get` was pointed at: a code, `CODE.KEY`, or a web page link with the key in its fragment.
struct Target {
    server: Option<String>,
    /// Empty for a direct link.
    code: String,
    key: Option<SecretKey>,
    /// The file name of a direct link (`/f/NAME`, or `/d/NAME` for an encrypted one).
    file: Option<String>,
}

impl Target {
    fn parse(input: &str, key_flag: Option<String>) -> Result<Self> {
        let input = input.trim();
        let (server, code, from_input) = if input.starts_with("http://") || input.starts_with("https://") {
            let (rest, fragment) = input.split_once('#').unwrap_or((input, ""));
            let scheme_end = rest.find("://").unwrap() + 3;
            let host_end = rest[scheme_end..]
                .find(['/', '?'])
                .map_or(rest.len(), |i| scheme_end + i);
            let server = Some(rest[..host_end].to_string());
            let key = fragment.strip_prefix("k=").filter(|k| !k.is_empty());
            let path = rest[host_end..].split('?').next().unwrap_or_default();
            if let Some(file) = path.strip_prefix("/f/").or_else(|| path.strip_prefix("/d/")) {
                if file.is_empty() || file.contains(['/', '\\']) || file.starts_with('.') {
                    return Err("that link has no file name in it".into());
                }
                return Ok(Self {
                    server,
                    code: String::new(),
                    key: parse_key(key_flag.as_deref().or(key))?,
                    file: Some(file.to_string()),
                });
            }
            let code = rest[host_end..]
                .split_once('?')
                .and_then(|(_, q)| q.split('&').find_map(|p| p.strip_prefix("code=")))
                .ok_or("that link has no share code in it")?;
            (server, code, key)
        } else {
            match input.split_once('.') {
                Some((code, key)) => (None, code, Some(key)),
                None => (None, input, None),
            }
        };
        let code = code.to_ascii_uppercase();
        if code.len() != 5 || !code.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(format!("{code:?} is not a share code (codes are 5 letters and digits)"));
        }
        Ok(Self {
            server,
            code,
            key: parse_key(key_flag.as_deref().or(from_input))?,
            file: None,
        })
    }
}

fn parse_key(key: Option<&str>) -> Result<Option<SecretKey>> {
    key.map(|k| SecretKey::parse(k).ok_or_else(|| "that key is not valid (it should be 43 characters)".to_string()))
        .transpose()
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

fn get(server: &str, target: &Target, output: Option<PathBuf>, force: bool) -> Result<()> {
    let agent = agent();
    if let Some(file) = &target.file {
        return get_direct(&agent, server, file, target.key.as_ref(), output, force);
    }
    let share: Share = check(
        agent
            .get(format!("{server}/share/{}", target.code))
            .call()
            .map_err(net_err(server))?,
    )?
    .body_mut()
    .read_json()
    .map_err(|e| format!("unexpected answer from server: {e}"))?;
    let file_url = format!("{server}/s/{}/file?download=1", share.code);

    if share.encrypted {
        let key = target
            .key
            .as_ref()
            .ok_or("this share is end-to-end encrypted: pass the link or CODE.KEY you were given, or --key KEY")?;
        get_encrypted(&agent, server, &file_url, key, output, force)?;
    } else if share.kind == "text" {
        print_text(share.text.clone().unwrap_or_default(), output, force)?;
    } else {
        let mut res = check(agent.get(&file_url).call().map_err(net_err(server))?)?;
        let name = safe_name(share.name.as_deref());
        save(&mut res.body_mut().as_reader(), output, force, &name, Some(share.size))?;
    }
    if io::stderr().is_terminal() {
        if share.burn {
            eprintln!("Burned after reading: it's gone from the server now");
        }
        if share.public {
            eprintln!(
                "Public · {} · {} · ml comments {}",
                plural(share.likes, "like"),
                plural(share.comments, "comment"),
                share.code
            );
        }
    }
    Ok(())
}

/// Fetches a direct link: plain ones are saved as they are, `/d/` ones are decrypted with the key.
fn get_direct(
    agent: &Agent,
    server: &str,
    file: &str,
    key: Option<&SecretKey>,
    output: Option<PathBuf>,
    force: bool,
) -> Result<()> {
    let url = format!("{server}/f/{file}");
    if let Some(key) = key {
        return get_encrypted(agent, server, &url, key, output, force);
    }
    if file.ends_with(".mle") {
        return Err("this link is end-to-end encrypted: pass the whole link with its #k= key, or --key KEY".into());
    }
    let res = agent.get(&url).call().map_err(net_err(server))?;
    if res.status() == 404 {
        return Err("no file at that link (it may have expired)".into());
    }
    let mut res = check(res)?;
    save(
        &mut res.body_mut().as_reader(),
        output,
        force,
        &link_file_name(file),
        None,
    )
}

/// The name to save a direct link's file under: its random ID and extension, minus the expiry.
fn link_file_name(file: &str) -> String {
    let name = match file.split_once(".x") {
        Some((id, rest)) => {
            let ext = rest.trim_start_matches(|c: char| c.is_ascii_digit());
            format!("{id}{ext}")
        }
        None => file.to_string(),
    };
    safe_name(Some(&name))
}

/// Writes a download to stdout (`-o -`) or a new file, checking its size when it is known.
fn save(body: &mut dyn Read, output: Option<PathBuf>, force: bool, name: &str, size: Option<u64>) -> Result<()> {
    if output.as_deref() == Some(Path::new("-")) {
        io::copy(body, &mut io::stdout().lock()).map_err(|e| format!("download failed: {e}"))?;
        return Ok(());
    }
    let path = output.unwrap_or_else(|| PathBuf::from(name));
    let got = write_new(&path, force, body)?;
    if let Some(size) = size
        && got != size
    {
        let _ = fs::remove_file(&path);
        return Err(format!("download incomplete ({got} of {size} bytes)"));
    }
    eprintln!("Saved {} ({})", path.display(), human_size(got));
    Ok(())
}

/// Downloads and decrypts as it goes; a wrong key or any tampering fails before anything is kept.
fn get_encrypted(
    agent: &Agent,
    server: &str,
    url: &str,
    key: &SecretKey,
    output: Option<PathBuf>,
    force: bool,
) -> Result<()> {
    let res = check(agent.get(url).call().map_err(net_err(server))?)?;
    let (header, mut plain) =
        Decryptor::open(key, Box::new(res.into_body().into_reader())).map_err(|e| format!("cannot decrypt: {e}"))?;

    if header.kind == "text" {
        let mut bytes = Vec::new();
        plain
            .read_to_end(&mut bytes)
            .map_err(|e| format!("cannot decrypt: {e}"))?;
        let text = String::from_utf8(bytes).map_err(|_| "the decrypted text is not UTF-8")?;
        return print_text(text, output, force);
    }

    if output.as_deref() == Some(Path::new("-")) {
        // Stdout can't be taken back, so check the whole share before writing any of it.
        let mut bytes = Vec::new();
        plain
            .read_to_end(&mut bytes)
            .map_err(|e| format!("cannot decrypt: {e}"))?;
        io::stdout()
            .lock()
            .write_all(&bytes)
            .map_err(|e| format!("cannot write to stdout: {e}"))?;
        return Ok(());
    }
    let path = output.unwrap_or_else(|| PathBuf::from(safe_name(header.name.as_deref())));
    let size = write_new(&path, force, &mut plain)?;
    eprintln!("Saved {} ({}, decrypted)", path.display(), human_size(size));
    Ok(())
}

/// "1 like", "3 likes".
fn plural(n: u64, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

fn print_text(text: String, output: Option<PathBuf>, force: bool) -> Result<()> {
    match output {
        Some(path) if path != Path::new("-") => write_new(&path, force, &mut text.as_bytes()).map(|_| ()),
        _ => {
            print!("{text}");
            if !text.ends_with('\n') && io::stdout().is_terminal() {
                println!();
            }
            Ok(())
        }
    }
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
        for sub in [
            "send",
            "public",
            "browse",
            "comments",
            "like",
            "get",
            "clip",
            "sync",
            "server",
            "completions",
        ] {
            assert!(script.contains(sub), "{sub} missing");
        }
    }

    #[test]
    fn targets_take_codes_keys_and_links() {
        let key = SecretKey::generate().unwrap().encode();
        let t = Target::parse(" k7qx2 ", None).unwrap();
        assert_eq!((t.code.as_str(), t.server, t.key.is_none()), ("K7QX2", None, true));

        let t = Target::parse(&format!("K7QX2.{key}"), None).unwrap();
        assert_eq!(t.key.unwrap().encode(), key);

        let link = format!("http://localhost:3000/?code=K7QX2#k={key}");
        let t = Target::parse(&link, None).unwrap();
        assert_eq!(t.server.as_deref(), Some("http://localhost:3000"));
        assert_eq!(t.code, "K7QX2");
        assert_eq!(t.key.unwrap().encode(), key);

        let t = Target::parse("https://m.example.com/?code=K7QX2", Some(key.clone())).unwrap();
        assert_eq!(t.key.unwrap().encode(), key);
        assert!(Target::parse("K7QX2.nope", None).is_err());

        let t = Target::parse("https://m.example.com/f/AbC12.x1794999763.png", None).unwrap();
        assert_eq!(t.file.as_deref(), Some("AbC12.x1794999763.png"));
        assert_eq!(t.server.as_deref(), Some("https://m.example.com"));
        let t = Target::parse(&format!("http://localhost:3000/d/AbC12.mle#k={key}"), None).unwrap();
        assert_eq!(
            (t.file.as_deref(), t.key.unwrap().encode()),
            (Some("AbC12.mle"), key.clone())
        );
        assert!(Target::parse("https://m.example.com/f/", None).is_err());
        assert!(Target::parse("https://m.example.com/f/../x", None).is_err());
        assert!(Target::parse("https://m.example.com/", None).is_err());
    }

    #[test]
    fn expiries_take_presets_and_durations() {
        let secs = |s: &str| Ttl::parse(s).map(|t| t.secs);
        assert_eq!(secs("24h"), Ok(Some(86_400)));
        assert_eq!(secs("7d"), Ok(Some(7 * 86_400)));
        assert_eq!(secs("90m"), Ok(Some(5400)));
        assert_eq!(secs("1d12h"), Ok(Some(36 * 3600)));
        assert_eq!(secs("2W"), Ok(Some(14 * 86_400)));
        assert_eq!(secs("never"), Ok(None));
        for bad in ["", "0m", "5", "5y", "h", "1.5h", "-5m", "99999999999999999999d"] {
            assert!(Ttl::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(Ttl::parse("1h").unwrap().query(), "1h");
        assert_eq!(Ttl::parse("90m").unwrap().query(), "5400s");
        assert_eq!(Ttl::parse("never").unwrap().query(), "never");
    }

    #[test]
    fn direct_link_files_lose_their_expiry() {
        assert_eq!(link_file_name("AbC12.x1794999763.png"), "AbC12.png");
        assert_eq!(link_file_name("AbC12.txt"), "AbC12.txt");
        assert_eq!(plural(1, "like"), "1 like");
        assert_eq!(plural(0, "like"), "0 likes");
    }

    #[test]
    fn sizes_are_readable() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }
}
