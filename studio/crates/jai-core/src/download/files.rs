//! File steps of an install: name checks, part-file locks, hashing and copying
//! with progress on blocking threads, moving and removing.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::report::Reporter;
use super::{DlError, IoAt};
use crate::catalog::Component;
use crate::fsutil;
use crate::install;
use crate::paths::Paths;

/// Buffer size for hashing and copying large files.
const CHUNK: usize = 1 << 20;

/// Catalog names become file and folder names, so they must be plain names.
pub(super) fn check_names(c: &Component) -> Result<(), DlError> {
    let plain =
        |s: &str| !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', ':', '\0']);
    let names = [
        Some(c.id.as_str()),
        Some(c.file.as_str()),
        c.dir.as_deref(),
        c.exe.as_deref(),
    ];
    match names.into_iter().flatten().find(|s| !plain(s)) {
        Some(name) => Err(DlError::Invalid(format!(
            "the catalog entry {} has an unusable name: {name:?}",
            c.id
        ))),
        None => Ok(()),
    }
}

/// Bytes already in the part file, at most the component size.
pub(super) fn part_len(paths: &Paths, c: &Component) -> u64 {
    fs::metadata(install::part_path(paths, c)).map_or(0, |m| m.len().min(c.size))
}

/// Deletes a part file left over from an install that already finished.
pub(super) fn remove_stale_part(paths: &Paths, c: &Component) {
    let part = install::part_path(paths, c);
    if fs::remove_file(&part).is_ok() {
        tracing::debug!(path = %part.display(), "removed a leftover part file");
    }
}

/// Deletes a file; a file that is already gone is fine.
pub(super) fn remove_file(path: &Path) -> Result<(), DlError> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e).at(path),
        _ => Ok(()),
    }
}

/// An exclusive lock on `downloads/<file>.lock`, held while one component
/// installs. The lock file is deleted when the lock is dropped.
pub(super) struct PartLock {
    path: PathBuf,
    _file: File,
}

impl PartLock {
    /// Busy when another install holds it. Where the file system has no
    /// locks, the install goes ahead unlocked.
    pub(super) fn acquire(paths: &Paths, c: &Component) -> Result<PartLock, DlError> {
        let path = paths.downloads().join(format!("{}.lock", c.file));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .at(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(DlError::Busy),
            Err(fs::TryLockError::Error(e)) => {
                tracing::debug!(error = %e, "file locks are not available; continuing without one");
            }
        }
        Ok(PartLock { path, _file: file })
    }
}

impl Drop for PartLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Runs blocking file work on tokio's blocking pool.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, DlError> + Send + 'static,
) -> Result<T, DlError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| DlError::Invalid(format!("a background task failed: {e}")))?
}

/// Hashes a file, reporting the running byte count. Returns the hasher, so a
/// resumed download can keep feeding it, and the number of bytes read.
pub(super) async fn hash(
    path: PathBuf,
    rep: Reporter,
    cancel: CancellationToken,
) -> Result<(Sha256, u64), DlError> {
    blocking(move || {
        let mut file = File::open(&path).at(&path)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; CHUNK];
        let mut total = 0u64;
        loop {
            if cancel.is_cancelled() {
                return Err(DlError::Canceled);
            }
            let n = read_some(&mut file, &mut buf).at(&path)?;
            if n == 0 {
                return Ok((hasher, total));
            }
            hasher.update(&buf[..n]);
            total += n as u64;
            rep.progress(total);
        }
    })
    .await
}

/// The finished hash equals the catalog's hex sha256 (any case).
pub(super) fn digest_matches(hasher: Sha256, sha256: &str) -> bool {
    fsutil::hex(&hasher.finalize()).eq_ignore_ascii_case(sha256)
}

/// Copies `src` over `dst`, hashing what it copies, and syncs it. Returns the
/// hex SHA-256 of the copy.
pub(super) async fn copy_hashing(
    src: PathBuf,
    dst: PathBuf,
    rep: Reporter,
    cancel: CancellationToken,
) -> Result<String, DlError> {
    blocking(move || {
        let mut input = File::open(&src).at(&src)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&dst)
            .at(&dst)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; CHUNK];
        let mut total = 0u64;
        loop {
            if cancel.is_cancelled() {
                return Err(DlError::Canceled);
            }
            let n = read_some(&mut input, &mut buf).at(&src)?;
            if n == 0 {
                break;
            }
            output.write_all(&buf[..n]).at(&dst)?;
            hasher.update(&buf[..n]);
            total += n as u64;
            rep.progress(total);
        }
        output.sync_all().at(&dst)?;
        Ok(fsutil::hex(&hasher.finalize()))
    })
    .await
}

fn read_some(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match file.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

/// Renames `src` to `dst`, replacing it. When they are on different file
/// systems (a `models` folder linked to another disk), copies and deletes.
pub(super) async fn move_file(src: PathBuf, dst: PathBuf) -> Result<(), DlError> {
    blocking(move || match fs::rename(&src, &dst) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            let tmp = fsutil::temp_sibling(&dst);
            let copied = fs::copy(&src, &tmp)
                .and_then(|_| OpenOptions::new().write(true).open(&tmp)?.sync_all())
                .and_then(|()| fs::rename(&tmp, &dst));
            if let Err(e) = copied {
                let _ = fs::remove_file(&tmp);
                return Err(e).at(&dst);
            }
            remove_file(&src)
        }
        Err(e) => Err(e).at(&dst),
    })
    .await
}

/// Opens the part file for appending, or truncates it to start over.
pub(super) fn open_part(part: &Path, append: bool) -> Result<File, DlError> {
    let mut options = OpenOptions::new();
    if append {
        options.append(true);
    } else {
        options.write(true).create(true).truncate(true);
    }
    options.open(part).at(part)
}

/// Flushes a finished part file to disk before it is renamed or unpacked.
pub(super) fn sync(path: &Path) -> Result<(), DlError> {
    OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|f| f.sync_all())
        .at(path)
}

/// Makes an unpacked engine executable on Unix: adds the execute bit wherever
/// the read bit is set. Archives made on Windows carry no Unix modes.
pub(super) fn ensure_executable(path: &Path) -> Result<(), DlError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path).at(path)?.permissions().mode() & 0o7777;
        let wanted = mode | ((mode & 0o444) >> 2);
        if wanted != mode {
            fs::set_permissions(path, fs::Permissions::from_mode(wanted)).at(path)?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Role;

    #[test]
    fn names_must_be_plain() {
        let mut c = Component {
            id: "llm-model".into(),
            role: Role::LlmModel,
            label: "x".into(),
            file: "model.gguf".into(),
            url: "https://example.invalid/x".into(),
            size: 1,
            sha256: "0".repeat(64),
            license: "MIT".into(),
            source: "test".into(),
            os: None,
            backend: None,
            unpack: None,
            strip: None,
            dir: None,
            exe: None,
        };
        assert!(check_names(&c).is_ok());
        for bad in ["../x", "a/b", "a\\b", "C:x", "..", ""] {
            c.file = bad.into();
            assert!(check_names(&c).is_err(), "{bad:?}");
        }
        c.file = "model.gguf".into();
        c.dir = Some("..".into());
        assert!(check_names(&c).is_err());
    }

    #[test]
    fn a_held_part_lock_makes_others_busy() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        paths.ensure_dirs().unwrap();
        let c = Component {
            id: "x".into(),
            role: Role::LlmModel,
            label: "x".into(),
            file: "x.gguf".into(),
            url: String::new(),
            size: 1,
            sha256: String::new(),
            license: String::new(),
            source: String::new(),
            os: None,
            backend: None,
            unpack: None,
            strip: None,
            dir: None,
            exe: None,
        };
        let held = PartLock::acquire(&paths, &c).unwrap();
        assert!(matches!(PartLock::acquire(&paths, &c), Err(DlError::Busy)));
        drop(held);
        let again = PartLock::acquire(&paths, &c).unwrap();
        drop(again);
        assert!(
            !paths.downloads().join("x.gguf.lock").exists(),
            "the lock file is removed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn engines_become_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("llama-server");
        fs::write(&exe, b"x").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o644)).unwrap();
        ensure_executable(&exe).unwrap();
        assert_eq!(
            fs::metadata(&exe).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}
