//! On Linux a clipboard lives in the program that set it, so the last clip from another device would
//! vanish when `ml clip sync` exits. A small detached `ml` takes it over instead and keeps offering
//! it until something else is copied. It gets the clip through a pipe, never through a file.

use std::{
    borrow::Cow,
    env,
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
};

use arboard::{Clipboard, ImageData, SetExtLinux};
use serde::{Deserialize, Serialize};

use super::{board::Content, linux_files, sync};
use crate::Result;

#[derive(Serialize, Deserialize, Default)]
struct Kept {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    files: Vec<PathBuf>,
    /// An image, as PNG.
    #[serde(default)]
    png: Vec<u8>,
}

/// Starts the keeper for `content`; a failure only costs the clipboard, so it isn't reported.
pub fn hand_over(content: Content) {
    let kept = match content {
        Content::Text(text) => Kept {
            text: Some(text),
            ..Kept::default()
        },
        Content::Files(files) => Kept {
            files,
            ..Kept::default()
        },
        Content::Image { width, height, rgba } => match sync::encode_png(width, height, &rgba) {
            Ok(png) => Kept { png, ..Kept::default() },
            Err(_) => return,
        },
    };
    let Ok(exe) = env::current_exe() else { return };
    let mut cmd = Command::new(exe);
    cmd.args(["clip", "keep"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Its own process group, so closing the terminal doesn't take it along.
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    if let Ok(mut child) = cmd.spawn()
        && let Some(mut stdin) = child.stdin.take()
    {
        let _ = stdin.write_all(&serde_json::to_vec(&kept).unwrap_or_default());
    }
}

/// The keeper: offers the clip from stdin until something else is copied.
pub fn hold() -> Result<()> {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).map_err(|e| e.to_string())?;
    let kept: Kept = serde_json::from_slice(&input).map_err(|e| e.to_string())?;
    if !kept.files.is_empty() {
        return linux_files::set(&kept.files, true);
    }
    let mut clipboard = Clipboard::new().map_err(|e| e.to_string())?;
    let set = clipboard.set().wait();
    let result = match (kept.text, kept.png) {
        (Some(text), _) => set.text(text),
        (None, png) if !png.is_empty() => match sync::decode_png(&png)? {
            Content::Image { width, height, rgba } => set.image(ImageData {
                width,
                height,
                bytes: Cow::Owned(rgba),
            }),
            _ => return Ok(()),
        },
        _ => return Ok(()),
    };
    result.map_err(|e| e.to_string())
}
