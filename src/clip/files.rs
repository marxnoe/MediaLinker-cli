//! Copied files on the way out (several files or a folder become one zip) and on the way in (into a
//! folder of their own, ready to be pasted).

use std::{
    fs::{self, File},
    io::{self, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

/// Where `ml clip` keeps files it received and zips it is sending.
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("medialinker")
        .join("clip")
}

pub fn incoming_dir() -> PathBuf {
    cache_dir().join("in")
}

pub fn outgoing_dir() -> PathBuf {
    cache_dir().join("out")
}

/// Total bytes under `paths`, stopping early once past `limit`. Symlinks count as nothing and are skipped.
pub fn total_size(paths: &[PathBuf], limit: u64) -> io::Result<u64> {
    let mut total = 0;
    let mut stack: Vec<PathBuf> = paths.to_vec();
    while let Some(path) = stack.pop() {
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            for entry in fs::read_dir(&path)? {
                stack.push(entry?.path());
            }
        } else if meta.is_file() {
            total += meta.len();
        }
        if total > limit {
            break;
        }
    }
    Ok(total)
}

pub fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Zips `paths` into `out`, each under its own name at the top level. Returns those names.
pub fn bundle(paths: &[PathBuf], out: &Path) -> io::Result<Vec<String>> {
    let mut zip = ZipWriter::new(BufWriter::new(File::create(out)?));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(1));
    let mut names = Vec::new();
    for path in paths {
        let name = display_name(path);
        add(&mut zip, path, &name, options)?;
        names.push(name);
    }
    zip.finish()?.flush()?;
    Ok(names)
}

fn add(zip: &mut ZipWriter<BufWriter<File>>, path: &Path, name: &str, options: SimpleFileOptions) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    #[cfg(unix)]
    let options = {
        use std::os::unix::fs::PermissionsExt;
        options.unix_permissions(meta.permissions().mode() & 0o777)
    };
    if meta.is_dir() {
        zip.add_directory(format!("{name}/"), options)?;
        let mut entries: Vec<_> = fs::read_dir(path)?.collect::<io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let child = format!("{name}/{}", entry.file_name().to_string_lossy());
            add(zip, &entry.path(), &child, options)?;
        }
    } else if meta.is_file() {
        zip.start_file(name, options)?;
        io::copy(&mut File::open(path)?, zip)?;
    }
    Ok(())
}

/// Unpacks a zip from [`bundle`] into `dir`, refusing paths that would land outside it and links.
/// Returns the top-level entries, in the order `names` gives them.
pub fn unbundle(zip: &Path, dir: &Path, names: &[String]) -> io::Result<Vec<PathBuf>> {
    let mut archive = ZipArchive::new(File::open(zip)?)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if entry.is_symlink() {
            continue;
        }
        // Not `enclosed_name`: on Windows it drops any name with a `:`, which Linux and macOS allow.
        // The name is split by hand instead, and every part cleaned of what Windows refuses.
        let name = entry.name()?.into_owned();
        let parts: Vec<&str> = name.split(['/', '\\']).filter(|p| !p.is_empty() && *p != ".").collect();
        if parts.is_empty() || parts.contains(&"..") {
            continue;
        }
        let target = parts
            .iter()
            .fold(dir.to_path_buf(), |path, part| path.join(safe_component(part)));
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&target)?;
        io::copy(&mut entry, &mut out)?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&target, fs::Permissions::from_mode(mode & 0o777));
        }
    }
    let mut top: Vec<PathBuf> = names
        .iter()
        .map(|n| dir.join(safe_name(n)))
        .filter(|p| p.exists())
        .collect();
    if top.is_empty() {
        // Not from `ml`: put whatever the zip held at the top level on the clipboard.
        top = fs::read_dir(dir)?.filter_map(|e| e.ok().map(|e| e.path())).collect();
        top.sort();
    }
    Ok(top)
}

/// Never trust a name from another device with the filesystem.
pub fn safe_name(name: &str) -> String {
    safe_component(name.rsplit(['/', '\\']).next().unwrap_or_default())
}

/// One file or folder name that every system can create: no characters Windows refuses, no
/// trailing dots or spaces, and no Windows device names like `CON` or `nul.txt`.
fn safe_component(name: &str) -> String {
    let clean: String = name
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .collect();
    let clean = clean.trim().trim_start_matches('.').trim_end_matches(['.', ' ']);
    if clean.is_empty() {
        return "file".into();
    }
    let stem = clean.split('.').next().unwrap_or_default().to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    if device { format!("_{clean}") } else { clean.into() }
}

/// Keeps the newest few received clips, so pasting an older one still works for a while, and drops the rest.
pub fn prune_incoming(keep: usize) {
    let Ok(entries) = fs::read_dir(incoming_dir()) else {
        return;
    };
    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.0));
    for (_, path) in dirs.into_iter().skip(keep) {
        let _ = if path.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
    }
}

/// Reads all of `reader` but at most `limit` bytes.
pub fn read_limited(reader: &mut dyn Read, limit: u64) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    reader.take(limit + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > limit {
        return Err(io::Error::other("the clip is larger than it said"));
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_and_files_survive_a_round_trip() {
        let root = std::env::temp_dir().join(format!("ml-clip-files-{}", std::process::id()));
        let src = root.join("src");
        fs::create_dir_all(src.join("folder/sub")).unwrap();
        fs::write(src.join("folder/sub/a.txt"), "hello").unwrap();
        fs::write(src.join("b.bin"), vec![1u8; 100_000]).unwrap();
        let paths = vec![src.join("folder"), src.join("b.bin")];
        assert_eq!(total_size(&paths, u64::MAX).unwrap(), 100_005);

        let zip = root.join("x.zip");
        let names = bundle(&paths, &zip).unwrap();
        assert_eq!(names, ["folder", "b.bin"]);
        let out = root.join("out");
        fs::create_dir_all(&out).unwrap();
        let top = unbundle(&zip, &out, &names).unwrap();
        assert_eq!(top, [out.join("folder"), out.join("b.bin")]);
        assert_eq!(fs::read_to_string(out.join("folder/sub/a.txt")).unwrap(), "hello");
        assert_eq!(fs::read(out.join("b.bin")).unwrap().len(), 100_000);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn remote_names_stay_in_their_folder() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("C:\\x\\a:b.txt"), "ab.txt");
        assert_eq!(safe_name(".."), "file");
    }

    #[test]
    fn names_work_on_windows() {
        assert_eq!(safe_name("con.txt"), "_con.txt");
        assert_eq!(safe_name("LPT1"), "_LPT1");
        assert_eq!(safe_name("console.txt"), "console.txt");
        assert_eq!(safe_name("draft. "), "draft");
    }

    #[test]
    fn folders_with_odd_names_unpack() {
        let root = std::env::temp_dir().join(format!("ml-clip-names-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        // Written as a zip, not on disk: Windows can't create these names, but a Mac can send them.
        let zip = root.join("x.zip");
        let mut writer = ZipWriter::new(BufWriter::new(File::create(&zip).unwrap()));
        let options = SimpleFileOptions::default();
        writer.add_directory("notes: 2026/", options).unwrap();
        writer.add_directory("notes: 2026/aux/", options).unwrap();
        writer.start_file("notes: 2026/aux/what?.txt", options).unwrap();
        writer.write_all(b"x").unwrap();
        writer.start_file("../evil.txt", options).unwrap();
        writer.write_all(b"x").unwrap();
        writer.finish().unwrap().flush().unwrap();
        let names = ["notes: 2026".to_string()];
        let out = root.join("out");
        fs::create_dir_all(&out).unwrap();
        let top = unbundle(&zip, &out, &names).unwrap();
        assert_eq!(top, [out.join("notes 2026")]);
        assert_eq!(fs::read_to_string(out.join("notes 2026/_aux/what.txt")).unwrap(), "x");
        assert!(!root.join("evil.txt").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
