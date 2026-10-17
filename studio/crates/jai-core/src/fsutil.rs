//! Small file helpers shared by the modules: atomic writes, modification
//! times, hex digests and disk identity.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

/// Writes `bytes` to `path` through a temporary file in the same folder and a
/// rename, so readers see the old content or the new, never a mix. `mode` sets
/// the Unix permissions of the new file; it is ignored elsewhere.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let tmp = temp_sibling(path);
    let result = write_new(&tmp, bytes, mode).and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Creates `path`, which must not exist, and writes and syncs `bytes`.
pub(crate) fn write_new(path: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
    let mut file = create_new(path, mode)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Opens a new file for writing, failing if it exists. `mode` applies on Unix.
pub(crate) fn create_new(path: &Path, mode: Option<u32>) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    options.open(path)
}

/// A unique hidden temporary name next to `path`.
pub(crate) fn temp_sibling(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.{}-{n}.tmp", std::process::id()))
}

/// Modification time in whole milliseconds since the Unix epoch; 0 when the
/// platform does not report one.
pub(crate) fn mtime_ms(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Lowercase hexadecimal.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

/// `path`, or its closest ancestor that exists.
pub(crate) fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut current = Some(path);
    while let Some(p) = current {
        if p.exists() {
            return Some(p.to_path_buf());
        }
        current = p.parent().filter(|q| !q.as_os_str().is_empty());
    }
    None
}

/// Whether two paths live on the same file system, so a hard link between
/// them can work. A path that does not exist yet counts by its closest
/// existing ancestor. False when unknown.
pub(crate) fn same_device(a: &Path, b: &Path) -> bool {
    match (nearest_existing(a), nearest_existing(b)) {
        (Some(a), Some(b)) => device_id(&a).is_some_and(|da| device_id(&b) == Some(da)),
        _ => false,
    }
}

#[cfg(unix)]
fn device_id(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).ok().map(|m| m.dev().to_string())
}

/// On Windows, the drive letter or UNC share of the canonical path.
#[cfg(windows)]
fn device_id(path: &Path) -> Option<String> {
    use std::path::{Component, Prefix};
    let canonical = fs::canonicalize(path).ok()?;
    match canonical.components().next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::VerbatimDisk(d) | Prefix::Disk(d) => {
                Some(char::from(d.to_ascii_uppercase()).to_string())
            }
            Prefix::VerbatimUNC(server, share) | Prefix::UNC(server, share) => Some(format!(
                "\\\\{}\\{}",
                server.to_string_lossy().to_lowercase(),
                share.to_string_lossy().to_lowercase()
            )),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(not(any(unix, windows)))]
fn device_id(_path: &Path) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }

    #[test]
    fn write_atomic_replaces_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub").join("a.json");
        write_atomic(&file, b"one", None).unwrap();
        write_atomic(&file, b"two", Some(0o600)).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"two");
        let names: Vec<_> = fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("a.json")]);
    }

    #[test]
    fn nearest_existing_walks_up() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("x").join("y");
        assert_eq!(nearest_existing(&missing).unwrap(), dir.path());
        assert!(same_device(&missing, dir.path()));
    }
}
