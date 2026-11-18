//! Public posts from the terminal, like the site's Public page: browse and open them, read and
//! post comments, and like them. Posting is `ml public` in `main.rs`.
//!
//! Public posts aren't shares: they have their own IDs (7 characters, or 5 for posts from before
//! they were split off shares), their own page (`/p/ID`) and their own API under `/public/`.

use std::{fs, io::IsTerminal, path::PathBuf};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::json;

use crate::{Result, agent, check, config_dir, human_size, net_err, now, plural, print_text, safe_name, save};

/// The length of a new post's ID. Posts from before the split keep their 5-character codes.
pub const ID_LEN: usize = 7;
const OLD_ID_LEN: usize = 5;
const PER_PAGE: u32 = 20;
/// How much of a text post's first line a list row shows.
const TITLE_CHARS: usize = 60;

/// A public post a command was pointed at: its ID, and the server if it came as a link.
pub struct PostRef {
    pub server: Option<String>,
    pub id: String,
}

impl PostRef {
    /// Takes an ID, a post link (`/p/ID`), or a link a post had before the split
    /// (`/receive?code=ID`, `/?code=ID`).
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        let (server, id) = if input.starts_with("http://") || input.starts_with("https://") {
            let rest = input.split('#').next().unwrap_or_default();
            let scheme_end = rest.find("://").unwrap() + 3;
            let host_end = rest[scheme_end..]
                .find(['/', '?'])
                .map_or(rest.len(), |i| scheme_end + i);
            let (path, query) = rest[host_end..].split_once('?').unwrap_or((&rest[host_end..], ""));
            if path.starts_with("/f/") || path.starts_with("/d/") {
                return Err("that's a direct link, not a public post".into());
            }
            let id = path
                .strip_prefix("/p/")
                .or_else(|| path.strip_prefix("/public/"))
                .map(|p| p.split('/').next().unwrap_or_default())
                .or_else(|| query.split('&').find_map(|p| p.strip_prefix("code=")))
                .ok_or("that link has no public post in it")?;
            (Some(rest[..host_end].to_string()), id)
        } else {
            (None, input)
        };
        let id = id.to_ascii_uppercase();
        if ![ID_LEN, OLD_ID_LEN].contains(&id.len()) || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(format!("{id:?} is not a public post ID"));
        }
        Ok(Self { server, id })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Item {
    /// Servers from before the split call it `code`.
    id: Option<String>,
    code: Option<String>,
    kind: String,
    snippet: Option<String>,
    name: Option<String>,
    size: u64,
    created_at: u64,
    likes: u64,
    comments: u64,
}

#[derive(Deserialize)]
struct List {
    total: u64,
    items: Vec<Item>,
}

/// Lists one page of public posts, most liked or newest first, optionally filtered by `search`.
pub fn browse(server: &str, search: &str, new: bool, page: u32) -> Result<()> {
    let page = page.max(1);
    let mut req = agent()
        .get(format!("{server}/public/list"))
        .query("sort", if new { "new" } else { "likes" })
        .query("limit", PER_PAGE.to_string())
        .query("offset", ((page - 1) * PER_PAGE).to_string());
    if !search.trim().is_empty() {
        req = req.query("q", search.trim());
    }
    let list: List = check(req.call().map_err(net_err(server))?)?
        .body_mut()
        .read_json()
        .map_err(|e| format!("unexpected answer from server: {e}"))?;
    if list.items.is_empty() {
        eprintln!(
            "{}",
            if page > 1 {
                "No more public posts"
            } else {
                "No public posts found"
            }
        );
        return Ok(());
    }
    for i in &list.items {
        println!(
            "{:<7}  {:>9}  {:>12}  {:>8}  {}",
            i.id.as_deref().or(i.code.as_deref()).unwrap_or_default(),
            plural(i.likes, "like"),
            plural(i.comments, "comment"),
            ago(i.created_at),
            title(i)
        );
    }
    let pages = list.total.div_ceil(PER_PAGE as u64);
    let more = if (page as u64) < pages {
        format!(" · --page {} for more", page + 1)
    } else {
        String::new()
    };
    eprintln!(
        "Page {page} of {pages}, {} · ml view ID opens one{more}",
        plural(list.total, "post")
    );
    Ok(())
}

/// A file's name and size, or the first line of a text post.
fn title(i: &Item) -> String {
    if i.kind == "file" {
        return format!("{} ({})", i.name.as_deref().unwrap_or("file"), human_size(i.size));
    }
    let snippet = i.snippet.as_deref().unwrap_or_default().trim();
    let line = snippet.lines().next().unwrap_or_default();
    let short: String = line.chars().take(TITLE_CHARS).collect();
    if short.len() < snippet.len() {
        format!("{short}…")
    } else {
        short
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Comment {
    no: u64,
    name: String,
    trip: Option<String>,
    poster: String,
    body: String,
    created_at: u64,
}

#[derive(Deserialize)]
struct Thread {
    comments: Vec<Comment>,
    locked: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Post {
    id: String,
    kind: String,
    text: Option<String>,
    name: Option<String>,
    url: Option<String>,
    size: u64,
    likes: u64,
    comments: u64,
}

/// Opens a public post: prints its text, or downloads its file, like `ml get` does for a share.
pub fn view(server: &str, id: &str, output: Option<PathBuf>, force: bool) -> Result<()> {
    let agent = agent();
    let res = agent
        .get(format!("{server}/public/{id}"))
        .call()
        .map_err(net_err(server))?;
    if res.status() == 404
        && res
            .headers()
            .get("content-type")
            .is_none_or(|t| t != "application/json")
    {
        return Err(format!(
            "{server} doesn't know public posts by ID yet: open it with ml 0.9 or older"
        ));
    }
    let post: Post = check(res)?
        .body_mut()
        .read_json()
        .map_err(|e| format!("unexpected answer from server: {e}"))?;
    if post.kind == "text" {
        print_text(post.text.unwrap_or_default(), output, force)?;
    } else {
        let url = post.url.ok_or("unexpected answer from server")?;
        let mut res = check(
            agent
                .get(format!("{server}{url}?download=1"))
                .call()
                .map_err(net_err(server))?,
        )?;
        let name = safe_name(post.name.as_deref());
        save(&mut res.body_mut().as_reader(), output, force, &name, Some(post.size))?;
    }
    if std::io::stderr().is_terminal() {
        eprintln!(
            "Public post {} · {} · {} · ml comments {}",
            post.id,
            plural(post.likes, "like"),
            plural(post.comments, "comment"),
            post.id
        );
    }
    Ok(())
}

/// Prints a public post's comments, or posts `text` as a new one.
pub fn comments(server: &str, id: &str, text: &str, name: Option<String>) -> Result<()> {
    let url = format!("{server}/public/{id}/comments");
    if !text.trim().is_empty() {
        let res = agent()
            .post(&url)
            .send_json(json!({ "name": name.unwrap_or_default(), "body": text }))
            .map_err(net_err(server))?;
        let c: Comment = check(res)?
            .body_mut()
            .read_json()
            .map_err(|e| format!("unexpected answer from server: {e}"))?;
        eprintln!("Posted comment #{} on {id}", c.no);
        return Ok(());
    }
    let thread: Thread = check(agent().get(&url).call().map_err(net_err(server))?)?
        .body_mut()
        .read_json()
        .map_err(|e| format!("unexpected answer from server: {e}"))?;
    for c in &thread.comments {
        let trip = c.trip.as_deref().map(|t| format!(" !{t}")).unwrap_or_default();
        println!("#{} {}{trip} · ID {} · {}", c.no, c.name, c.poster, ago(c.created_at));
        for line in c.body.lines() {
            println!("  {line}");
        }
        println!();
    }
    if thread.locked {
        eprintln!("Comments are closed on this post");
    } else if thread.comments.is_empty() {
        eprintln!("No comments yet. ml comments {id} \"your text\" posts one");
    }
    Ok(())
}

#[derive(Deserialize)]
struct Liked {
    likes: u64,
}

/// Likes a public post, or takes the like back. Counted once per machine, the way the site
/// counts once per browser: a random ID kept in the config directory stands in for its cookie.
pub fn like(server: &str, id: &str, undo: bool) -> Result<()> {
    let url = format!("{server}/public/{id}/like");
    let cookie = format!("ml_liker={}", liker()?);
    let res = if undo {
        agent().delete(&url).header("Cookie", &cookie).call()
    } else {
        agent().put(&url).header("Cookie", &cookie).send_empty()
    }
    .map_err(net_err(server))?;
    let liked: Liked = check(res)?
        .body_mut()
        .read_json()
        .map_err(|e| format!("unexpected answer from server: {e}"))?;
    let done = if undo { "Took back your like on" } else { "Liked" };
    eprintln!("{done} {id} ({})", plural(liked.likes, "like"));
    Ok(())
}

/// This machine's anonymous liker ID, made on first use.
fn liker() -> Result<String> {
    let path = config_dir().ok_or("cannot find a config directory")?.join("liker");
    if let Ok(id) = fs::read_to_string(&path)
        && !id.trim().is_empty()
    {
        return Ok(id.trim().to_string());
    }
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| format!("cannot make a liker ID: {e}"))?;
    let id = URL_SAFE_NO_PAD.encode(bytes);
    fs::create_dir_all(path.parent().unwrap()).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    fs::write(&path, &id).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(id)
}

/// How long ago a server timestamp (in ms) was, roughly.
fn ago(ms: u64) -> String {
    let s = now().saturating_sub(ms / 1000);
    match s {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s if s < 30 * 86_400 => format!("{}d ago", s / 86_400),
        s if s < 365 * 86_400 => format!("{}mo ago", s / (30 * 86_400)),
        s => format!("{}y ago", s / (365 * 86_400)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: &str, snippet: Option<&str>, name: Option<&str>) -> Item {
        Item {
            id: Some("K7QX2AB".into()),
            code: None,
            kind: kind.into(),
            snippet: snippet.map(Into::into),
            name: name.map(Into::into),
            size: 2048,
            created_at: 0,
            likes: 0,
            comments: 0,
        }
    }

    #[test]
    fn rows_show_a_short_title() {
        assert_eq!(title(&item("file", None, Some("cat.png"))), "cat.png (2.0 KB)");
        assert_eq!(title(&item("text", Some("hello"), None)), "hello");
        assert_eq!(title(&item("text", Some("first\nsecond"), None)), "first…");
        let long = "x".repeat(100);
        assert_eq!(title(&item("text", Some(&long), None)), format!("{}…", "x".repeat(60)));
    }

    #[test]
    fn posts_take_ids_and_links() {
        let p = PostRef::parse(" k7qx2ab ").unwrap();
        assert_eq!((p.id.as_str(), p.server), ("K7QX2AB", None));
        assert_eq!(PostRef::parse("K7QX2").unwrap().id, "K7QX2");
        let p = PostRef::parse("https://m.example.com/p/K7QX2AB").unwrap();
        assert_eq!(
            (p.id.as_str(), p.server.as_deref()),
            ("K7QX2AB", Some("https://m.example.com"))
        );
        assert_eq!(
            PostRef::parse("http://localhost:3000/public/k7qx2ab/file").unwrap().id,
            "K7QX2AB"
        );
        // Links from before the split.
        assert_eq!(
            PostRef::parse("https://m.example.com/receive?code=K7QX2").unwrap().id,
            "K7QX2"
        );
        assert_eq!(PostRef::parse("https://m.example.com/?code=K7QX2").unwrap().id, "K7QX2");
        assert!(PostRef::parse("https://m.example.com/f/AbC12.png").is_err());
        assert!(PostRef::parse("https://m.example.com/").is_err());
        assert!(PostRef::parse("K7QX").is_err());
        assert!(PostRef::parse("../../x").is_err());
    }

    #[test]
    fn lists_read_ids_from_old_servers_too() {
        let old: List = serde_json::from_str(
            r#"{"total":1,"items":[{"code":"K7QX2","kind":"text","snippet":"hi","size":2,"createdAt":0,"likes":0,"comments":0,"liked":false}]}"#,
        )
        .unwrap();
        assert_eq!(old.items[0].code.as_deref(), Some("K7QX2"));
        let new: List = serde_json::from_str(
            r#"{"total":1,"items":[{"id":"K7QX2AB","code":"K7QX2AB","kind":"text","size":2,"createdAt":0,"likes":0,"comments":0,"liked":false}]}"#,
        )
        .unwrap();
        assert_eq!(new.items[0].id.as_deref(), Some("K7QX2AB"));
    }

    #[test]
    fn times_read_as_ago() {
        let t = now() * 1000;
        assert_eq!(ago(t), "just now");
        assert_eq!(ago(t - 5 * 60_000), "5m ago");
        assert_eq!(ago(t - 3 * 3_600_000), "3h ago");
        assert_eq!(ago(t - 2 * 86_400_000), "2d ago");
    }
}
