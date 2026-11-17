//! Public shares from the terminal, like the site's Public page: browse them, read and post
//! comments, and like them. Posting a share is `ml public` in `main.rs`.

use std::fs;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::json;

use crate::{Result, agent, check, config_dir, human_size, net_err, now, plural};

const PER_PAGE: u32 = 20;
/// How much of a text share's first line a list row shows.
const TITLE_CHARS: usize = 60;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Item {
    code: String,
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

/// Lists one page of public shares, most liked or newest first, optionally filtered by `search`.
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
                "No more public shares"
            } else {
                "No public shares found"
            }
        );
        return Ok(());
    }
    for i in &list.items {
        println!(
            "{}  {:>9}  {:>12}  {:>8}  {}",
            i.code,
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
        "Page {page} of {pages}, {} · ml get CODE opens one{more}",
        plural(list.total, "share")
    );
    Ok(())
}

/// A file's name and size, or the first line of a text share.
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

/// Prints a public share's comments, or posts `text` as a new one.
pub fn comments(server: &str, code: &str, text: &str, name: Option<String>) -> Result<()> {
    let url = format!("{server}/public/{code}/comments");
    if !text.trim().is_empty() {
        let res = agent()
            .post(&url)
            .send_json(json!({ "name": name.unwrap_or_default(), "body": text }))
            .map_err(net_err(server))?;
        let c: Comment = check(res)?
            .body_mut()
            .read_json()
            .map_err(|e| format!("unexpected answer from server: {e}"))?;
        eprintln!("Posted comment #{} on {code}", c.no);
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
        eprintln!("Comments are closed on this share");
    } else if thread.comments.is_empty() {
        eprintln!("No comments yet. ml comments {code} \"your text\" posts one");
    }
    Ok(())
}

#[derive(Deserialize)]
struct Liked {
    likes: u64,
}

/// Likes a public share, or takes the like back. Counted once per machine, the way the site
/// counts once per browser: a random ID kept in the config directory stands in for its cookie.
pub fn like(server: &str, code: &str, undo: bool) -> Result<()> {
    let url = format!("{server}/public/{code}/like");
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
    eprintln!("{done} {code} ({})", plural(liked.likes, "like"));
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
            code: "K7QX2".into(),
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
    fn times_read_as_ago() {
        let t = now() * 1000;
        assert_eq!(ago(t), "just now");
        assert_eq!(ago(t - 5 * 60_000), "5m ago");
        assert_eq!(ago(t - 3 * 3_600_000), "3h ago");
        assert_eq!(ago(t - 2 * 86_400_000), "2d ago");
    }
}
