//! Unpacking engine archives: zip and tar.gz, with `strip`, Unix modes (from
//! zip external attributes and tar headers) and zip-slip protection.
//!
//! Every entry name must be a plain relative path: no absolute path, drive
//! letter, `..`, `:` (drive or NTFS stream) or component made only of dots and
//! spaces (Windows trims those to `..` or nothing). One unsafe entry fails the
//! whole archive. Symbolic links are created on Unix only when their target is
//! relative and has no `..`, so every link stays inside the target folder and
//! nothing can be written through a link to the outside. On Windows a link
//! becomes a copy of its target. Existing files are replaced, never written
//! through.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use flate2::read::MultiGzDecoder;
use tokio_util::sync::CancellationToken;

use super::{DlError, IoAt};
use crate::catalog::Unpack;

/// Unpacks on a blocking thread.
pub(super) async fn run(
    archive: PathBuf,
    dest: PathBuf,
    kind: Unpack,
    strip: u32,
    cancel: CancellationToken,
) -> Result<(), DlError> {
    tokio::task::spawn_blocking(move || unpack(&archive, &dest, kind, strip, &cancel))
        .await
        .map_err(|e| DlError::Invalid(format!("the unpack task failed: {e}")))?
}

/// Unpacks `archive` into `dest`, dropping `strip` leading path components.
pub(super) fn unpack(
    archive: &Path,
    dest: &Path,
    kind: Unpack,
    strip: u32,
    cancel: &CancellationToken,
) -> Result<(), DlError> {
    fs::create_dir_all(dest).at(dest)?;
    match kind {
        Unpack::Zip => unzip(archive, dest, strip, cancel),
        Unpack::TarGz => untar(archive, dest, strip, cancel),
    }
}

fn bad(error: impl std::fmt::Display) -> DlError {
    DlError::BadArchive(error.to_string())
}

fn unzip(
    archive: &Path,
    dest: &Path,
    strip: u32,
    cancel: &CancellationToken,
) -> Result<(), DlError> {
    let file = File::open(archive).at(archive)?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file)).map_err(bad)?;
    for index in 0..zip.len() {
        if cancel.is_cancelled() {
            return Err(DlError::Canceled);
        }
        let mut entry = zip.by_index(index).map_err(bad)?;
        let name = entry.name().map_err(bad)?.into_owned();
        let Some(rel) = entry_path(&name, strip)? else {
            continue;
        };
        let out = dest.join(&rel);
        if entry.is_dir() {
            fs::create_dir_all(&out).at(&out)?;
        } else if entry.is_symlink() {
            let mut target = String::new();
            entry.read_to_string(&mut target).map_err(bad)?;
            link(&out, &target)?;
        } else {
            // Modes count only from archives made on Unix; for others the zip
            // crate invents one from the DOS attributes.
            let mode = entry
                .unix_mode()
                .filter(|_| entry.system() == zip::System::Unix);
            write_entry(&out, &mut entry, mode)?;
        }
    }
    Ok(())
}

fn untar(
    archive: &Path,
    dest: &Path,
    strip: u32,
    cancel: &CancellationToken,
) -> Result<(), DlError> {
    let file = File::open(archive).at(archive)?;
    let mut tar = tar::Archive::new(MultiGzDecoder::new(BufReader::new(file)));
    for entry in tar.entries().map_err(bad)? {
        if cancel.is_cancelled() {
            return Err(DlError::Canceled);
        }
        let mut entry = entry.map_err(bad)?;
        let name = utf8(&entry.path_bytes())?;
        let Some(rel) = entry_path(&name, strip)? else {
            continue;
        };
        let out = dest.join(&rel);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            fs::create_dir_all(&out).at(&out)?;
        } else if kind.is_file() || kind.is_contiguous() {
            let mode = entry.header().mode().ok();
            write_entry(&out, &mut entry, mode)?;
        } else if kind.is_symlink() {
            let target = entry.link_name_bytes().map(|b| utf8(&b)).transpose()?;
            link(&out, target.as_deref().unwrap_or_default())?;
        } else if kind.is_hard_link() {
            let target = entry.link_name_bytes().map(|b| utf8(&b)).transpose()?;
            let source = target
                .as_deref()
                .map(|t| entry_path(t, strip))
                .transpose()?
                .flatten()
                .ok_or_else(|| DlError::UnsafePath(format!("{name} (hard link)")))?;
            hard_link(&dest.join(source), &out)?;
        } else {
            tracing::debug!(entry = %name, kind = ?kind, "skipping an archive entry of this kind");
        }
    }
    Ok(())
}

fn utf8(bytes: &[u8]) -> Result<String, DlError> {
    String::from_utf8(bytes.to_vec())
        .map_err(|_| DlError::BadArchive("an entry name is not UTF-8".into()))
}

/// The safe relative path for an archive entry, after dropping `strip`
/// leading components (`.` and empty components do not count). `Ok(None)`
/// when nothing is left, such as the top folder itself. An unsafe name is an
/// error.
pub(super) fn entry_path(name: &str, strip: u32) -> Result<Option<PathBuf>, DlError> {
    let parts = safe_parts(name).ok_or_else(|| DlError::UnsafePath(name.to_owned()))?;
    let strip = usize::try_from(strip).unwrap_or(usize::MAX);
    if parts.len() <= strip {
        return Ok(None);
    }
    Ok(Some(parts[strip..].iter().collect()))
}

/// The components of a relative path written with `/` or `\`, or None when
/// any of them is unsafe or the path is absolute.
fn safe_parts(name: &str) -> Option<Vec<&str>> {
    if name.starts_with(['/', '\\']) {
        return None;
    }
    let mut parts = Vec::new();
    for part in name.split(['/', '\\']) {
        match part {
            "" | "." => {}
            p if p.contains([':', '\0']) || p.trim_end_matches(['.', ' ']).is_empty() => {
                return None;
            }
            p => parts.push(p),
        }
    }
    Some(parts)
}

/// Writes one file, replacing whatever is at `out`, and applies the Unix
/// permission bits from the archive (no set-id or sticky bits).
fn write_entry(out: &Path, reader: &mut impl Read, mode: Option<u32>) -> Result<(), DlError> {
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).at(parent)?;
    }
    remove_existing(out)?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out)
        .at(out)?;
    let mut writer = BufWriter::with_capacity(1 << 20, file);
    io::copy(reader, &mut writer).at(out)?;
    writer.flush().at(out)?;
    #[cfg(unix)]
    if let Some(mode) = mode.map(|m| m & 0o777).filter(|m| *m != 0) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(out, fs::Permissions::from_mode(mode)).at(out)?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

/// Removes a file or link at `path` (not following it). A folder there is an
/// error: the archive wants a file where it already put a folder.
fn remove_existing(path: &Path) -> Result<(), DlError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Err(DlError::BadArchive(format!(
            "{} is both a folder and a file",
            path.display()
        ))),
        Ok(meta) => {
            #[cfg(windows)]
            if meta.permissions().readonly() {
                let mut permissions = meta.permissions();
                // Windows only: clears the read-only attribute so the file can go.
                #[allow(clippy::permissions_set_readonly_false)]
                permissions.set_readonly(false);
                let _ = fs::set_permissions(path, permissions);
            }
            #[cfg(not(windows))]
            let _ = meta;
            fs::remove_file(path).at(path)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).at(path),
    }
}

/// A symbolic link at `out` to `target`, which must be relative without `..`.
fn link(out: &Path, target: &str) -> Result<(), DlError> {
    let unsafe_link = || DlError::UnsafePath(format!("{} -> {target}", out.display()));
    let parts = safe_parts(target).ok_or_else(unsafe_link)?;
    if parts.is_empty() {
        return Err(unsafe_link());
    }
    let relative: PathBuf = parts.iter().collect();
    let parent = out.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent).at(parent)?;
    remove_existing(out)?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(&relative, out).at(out)?;
    #[cfg(not(unix))]
    {
        // Links need a privilege on Windows: copy the target if it is there.
        let source = parent.join(&relative);
        if source.is_file() {
            fs::copy(&source, out).at(out)?;
        } else {
            tracing::warn!(link = %out.display(), "skipping a link whose target is not unpacked");
        }
    }
    Ok(())
}

/// A tar hard link: `out` becomes another name for the already unpacked
/// `source`, or a copy of it where hard links do not work.
fn hard_link(source: &Path, out: &Path) -> Result<(), DlError> {
    if !source.is_file() {
        return Err(DlError::BadArchive(format!(
            "the hard link {} names a file that is not unpacked",
            out.display()
        )));
    }
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).at(parent)?;
    }
    remove_existing(out)?;
    if fs::hard_link(source, out).is_err() {
        fs::copy(source, out).at(out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str, strip: u32) -> Option<PathBuf> {
        entry_path(name, strip).unwrap()
    }

    #[test]
    fn plain_names_and_strip() {
        assert_eq!(
            path("bin/llama-server", 0),
            Some(PathBuf::from("bin").join("llama-server"))
        );
        assert_eq!(
            path("llama-b1/bin/x", 1),
            Some(PathBuf::from("bin").join("x"))
        );
        assert_eq!(
            path("./llama-b1/x", 1),
            Some(PathBuf::from("x")),
            "./ does not count"
        );
        assert_eq!(path("llama-b1/", 1), None, "the top folder itself");
        assert_eq!(path("llama-b1", 2), None);
        assert_eq!(
            path("dir\\sub\\f.dll", 0),
            Some(PathBuf::from("dir").join("sub").join("f.dll"))
        );
        assert_eq!(
            path("a//b/./c", 0),
            Some(PathBuf::from("a").join("b").join("c"))
        );
        assert_eq!(path("file.", 0), Some(PathBuf::from("file.")));
    }

    #[test]
    fn unsafe_names_are_rejected() {
        for name in [
            "../evil",
            "a/../../evil",
            "a/..",
            "/etc/passwd",
            "\\\\server\\share\\x",
            "\\x",
            "C:/Windows/x.dll",
            "C:x",
            "a/b:stream",
            "a/.../b",
            "a/.. /b",
            "a/ /b",
            "nul\0byte",
        ] {
            assert!(
                matches!(entry_path(name, 0), Err(DlError::UnsafePath(_))),
                "{name:?} must be rejected"
            );
            assert!(
                entry_path(name, 1).is_err(),
                "{name:?} is rejected before strip"
            );
        }
    }

    #[test]
    fn link_targets_must_stay_inside() {
        let dir = tempfile::tempdir().unwrap();
        for target in ["../x", "/etc/passwd", "a/../../x", "C:/x", "", "."] {
            let out = dir.path().join("link");
            assert!(
                matches!(link(&out, target), Err(DlError::UnsafePath(_))),
                "{target:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_replaced_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, b"keep").unwrap();
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        std::os::unix::fs::symlink(&outside, dest.join("f")).unwrap();
        write_entry(&dest.join("f"), &mut &b"new"[..], Some(0o644)).unwrap();
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
        assert_eq!(fs::read(dest.join("f")).unwrap(), b"new");
        assert!(!fs::symlink_metadata(dest.join("f"))
            .unwrap()
            .file_type()
            .is_symlink());
    }
}
