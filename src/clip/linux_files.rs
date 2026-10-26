//! Copied files on Linux. File managers disagree on the format: Dolphin and newer Nautilus read
//! `text/uri-list`, while Nautilus, Nemo, Caja and Thunar look for `x-special/gnome-copied-files`.
//! arboard only offers the first, so `ml` offers both, plus the paths as text for terminals and
//! editors. On Wayland that goes through the data-control protocol; elsewhere, or where the
//! compositor lacks it, `ml` owns the X11 clipboard itself until something else is copied.

use std::{path::Path, thread};

use wl_clipboard_rs::copy::{MimeSource, MimeType, Options, Source};
use x11rb::{
    COPY_DEPTH_FROM_PARENT, CURRENT_TIME,
    connection::Connection,
    protocol::{
        Event,
        xproto::{
            AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, SELECTION_NOTIFY_EVENT,
            SelectionNotifyEvent, SelectionRequestEvent, WindowClass,
        },
    },
    wrapper::ConnectionExt as _,
};

const URI_LIST: &str = "text/uri-list";
const GNOME: &str = "x-special/gnome-copied-files";

fn uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The formats to offer, as (MIME type, bytes).
fn formats(paths: &[impl AsRef<Path>]) -> Vec<(&'static str, Vec<u8>)> {
    let uris: Vec<String> = paths.iter().map(|p| uri(p.as_ref())).collect();
    let text: Vec<String> = paths
        .iter()
        .map(|p| p.as_ref().to_string_lossy().into_owned())
        .collect();
    vec![
        (URI_LIST, (uris.join("\r\n") + "\r\n").into_bytes()),
        (GNOME, format!("copy\n{}", uris.join("\n")).into_bytes()),
        ("UTF8_STRING", text.join("\n").into_bytes()),
        ("text/plain;charset=utf-8", text.join("\n").into_bytes()),
    ]
}

pub fn set(paths: &[impl AsRef<Path>]) -> Result<(), String> {
    let formats = formats(paths);
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        let sources = formats
            .iter()
            .filter(|(mime, _)| *mime != "UTF8_STRING")
            .map(|(mime, bytes)| MimeSource {
                source: Source::Bytes(bytes.clone().into_boxed_slice()),
                mime_type: MimeType::Specific((*mime).into()),
            })
            .collect();
        if Options::new().copy_multi(sources).is_ok() {
            return Ok(());
        }
    }
    x11_set(formats).map_err(|e| format!("cannot put the files on the clipboard: {e}"))
}

fn x11_set(formats: Vec<(&'static str, Vec<u8>)>) -> Result<(), Box<dyn std::error::Error>> {
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    let window = conn.generate_id()?;
    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        window,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new(),
    )?;
    let atom = |name: &str| -> Result<u32, Box<dyn std::error::Error>> {
        Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
    };
    let clipboard = atom("CLIPBOARD")?;
    let targets = atom("TARGETS")?;
    let mut offered = Vec::new();
    for (mime, bytes) in formats {
        offered.push((atom(mime)?, bytes));
    }
    conn.set_selection_owner(window, clipboard, CURRENT_TIME)?;
    if conn.get_selection_owner(clipboard)?.reply()?.owner != window {
        return Err("another program holds the clipboard".into());
    }
    conn.flush()?;
    thread::Builder::new()
        .name("x11-file-clipboard".into())
        .spawn(move || {
            // Serve until another program takes the clipboard over.
            while let Ok(event) = conn.wait_for_event() {
                match event {
                    Event::SelectionRequest(req) => {
                        let _ = answer(&conn, &req, targets, &offered);
                    }
                    Event::SelectionClear(_) => break,
                    _ => {}
                }
            }
        })?;
    Ok(())
}

fn answer(
    conn: &impl Connection,
    req: &SelectionRequestEvent,
    targets: u32,
    offered: &[(u32, Vec<u8>)],
) -> Result<(), Box<dyn std::error::Error>> {
    // Old clients leave the property empty and mean the target.
    let property = if req.property == u32::from(AtomEnum::NONE) {
        req.target
    } else {
        req.property
    };
    let served = if req.target == targets {
        let mut list = vec![targets];
        list.extend(offered.iter().map(|(a, _)| *a));
        conn.change_property32(PropMode::REPLACE, req.requestor, property, AtomEnum::ATOM, &list)?;
        true
    } else if let Some((target, bytes)) = offered.iter().find(|(a, _)| *a == req.target) {
        conn.change_property8(PropMode::REPLACE, req.requestor, property, *target, bytes)?;
        true
    } else {
        false
    };
    let notify = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: req.time,
        requestor: req.requestor,
        selection: req.selection,
        target: req.target,
        property: if served { property } else { AtomEnum::NONE.into() },
    };
    conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify)?;
    conn.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_become_file_uris_in_every_format() {
        let f = formats(&[Path::new("/home/me/My File#1.txt"), Path::new("/tmp/ä")]);
        assert_eq!(
            String::from_utf8(f[0].1.clone()).unwrap(),
            "file:///home/me/My%20File%231.txt\r\nfile:///tmp/%C3%A4\r\n"
        );
        assert_eq!(
            String::from_utf8(f[1].1.clone()).unwrap(),
            "copy\nfile:///home/me/My%20File%231.txt\nfile:///tmp/%C3%A4"
        );
    }
}
