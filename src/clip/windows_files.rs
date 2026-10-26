//! Copied files on Windows. arboard adds CF_HDROP to whatever is already on the clipboard, and when
//! Explorer copied a file before, its own formats (Shell IDList Array and friends) stay there and
//! Explorer pastes from those, so Ctrl+V would paste the older file. `ml` empties the clipboard
//! first and marks the files as copied, not cut.

use std::path::PathBuf;

use clipboard_win::{Clipboard, options::DoClear, raw};

/// DROPEFFECT_COPY, for Explorer's "Preferred DropEffect" format.
const DROPEFFECT_COPY: u32 = 1;

pub fn set(paths: &[PathBuf]) -> Result<(), String> {
    let failed = |e: clipboard_win::ErrorCode| format!("cannot put the files on the clipboard: {e}");
    let _open = Clipboard::new_attempts(10).map_err(failed)?;
    let paths: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    raw::set_file_list_with(&paths, DoClear).map_err(failed)?;
    if let Some(format) = raw::register_format("Preferred DropEffect") {
        raw::set_without_clear(format.get(), &DROPEFFECT_COPY.to_le_bytes()).map_err(failed)?;
    }
    Ok(())
}
