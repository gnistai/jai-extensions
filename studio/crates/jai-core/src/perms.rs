//! Who can open Studio's folders and files (DESIGN.md section 2).
//!
//! The data root holds the server token, the engines Studio runs and the
//! models they parse, so only the user may open it: on Unix it is owned by
//! the user with mode 0700; on Windows it is owned by the user (or SYSTEM or
//! Administrators) and its access list grants nobody else anything. A root
//! Studio creates gets that from the start (on Windows a protected access
//! list for the user, SYSTEM and Administrators, inherited by everything
//! inside). An existing root that other accounts can open is repaired when
//! it holds only Studio's own files, and refused otherwise.

use std::fs;
use std::io;
use std::path::Path;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(unix)]
use unix as sys;
#[cfg(windows)]
use windows as sys;

#[cfg(test)]
mod tests;

/// What another account on this PC can do with a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    /// Only the user (and, on Windows, SYSTEM and Administrators).
    Private,
    /// Other accounts can open it, or change it: why, for messages.
    Shared(String),
}

/// What [`sys::inspect`] looks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Check {
    /// Any access by another account, even reading.
    Any,
    /// Access that lets another account change the contents.
    Write,
}

/// Makes sure the data root exists and only the user can open it. A missing
/// root is created private. An existing one that other accounts can open is
/// made private when it holds only Studio's own entries (a token that other
/// accounts could read is deleted, so a new one is made); otherwise this
/// fails and changes nothing.
pub(crate) fn ensure_private_root(root: &Path) -> io::Result<()> {
    match fs::metadata(root) {
        Ok(meta) if !meta.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} is not a folder", root.display()),
            ))
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = root.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::create_dir_all(parent)?;
            }
            match sys::create_private(root) {
                Ok(()) => return Ok(()),
                // Another process made it just now: check it like any other.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(e) => return Err(e),
    }
    let reason = match inspect(root, Check::Any)? {
        Access::Private => return Ok(()),
        Access::Shared(reason) => reason,
    };
    if !is_studio_root(root) {
        return Err(not_private(root, &reason));
    }
    let token = root.join("token");
    let token_was_shared =
        token.is_file() && !matches!(sys::inspect(&token, Check::Any), Ok(Access::Private));
    tracing::warn!(path = %root.display(), %reason, "other accounts could open the data folder; making it private");
    sys::make_private(root).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot make {} private ({reason}): {e}", root.display()),
        )
    })?;
    if let Access::Shared(reason) = inspect(root, Check::Any)? {
        return Err(not_private(root, &reason));
    }
    if token_was_shared {
        tracing::warn!("other accounts could read the server token; replacing it (paired clients need the new one)");
        fs::remove_file(&token)?;
    }
    Ok(())
}

/// Creates a folder inside the private root, with its missing parents. On
/// Unix they get mode 0700; on Windows they inherit the root's access list.
pub(crate) fn create_dir(dir: &Path) -> io::Result<()> {
    sys::create_dir_all(dir)
}

/// Fails when `dir` resolves to a folder outside `root` (a link to another
/// disk) that other accounts can change: Studio runs and parses what is in
/// `engines`, `models` and `downloads`. Only the folder itself is checked,
/// not its parents.
pub(crate) fn check_linked(root: &Path, dir: &Path) -> io::Result<()> {
    let (Ok(root), Ok(target)) = (fs::canonicalize(root), fs::canonicalize(dir)) else {
        return Ok(());
    };
    if target.starts_with(&root) {
        return Ok(());
    }
    match inspect(&target, Check::Write)? {
        Access::Private => Ok(()),
        Access::Shared(reason) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} links to {}, which other accounts on this PC can change ({reason}). \
                 JAI Studio runs and loads what is in it, so only you may be able to change it: \
                 make {} private, or remove the link",
                dir.display(),
                target.display(),
                target.display()
            ),
        )),
    }
}

/// True when no other account can change the file: safe to hard-link into
/// `models`, where it then counts as verified. False when unsure.
pub(crate) fn only_user_can_write(file: &Path) -> bool {
    matches!(sys::inspect(file, Check::Write), Ok(Access::Private))
}

/// [`sys::inspect`], its errors naming the path.
fn inspect(path: &Path, check: Check) -> io::Result<Access> {
    sys::inspect(path, check).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot check who can open {}: {e}", path.display()),
        )
    })
}

fn not_private(root: &Path, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "other accounts on this PC can open {} ({reason}). JAI Studio keeps its token, \
             engines and models there, so the folder must be private: set JAI_HOME to a new \
             folder, or remove the other accounts' access to this one",
            root.display()
        ),
    )
}

/// The root holds nothing but Studio's own entries, so its permissions are
/// Studio's to set. An empty folder counts.
fn is_studio_root(root: &Path) -> bool {
    match fs::read_dir(root) {
        Ok(entries) => entries
            .into_iter()
            .all(|e| e.is_ok_and(|e| studio_entry(&e.file_name().to_string_lossy()))),
        Err(_) => false,
    }
}

/// The names Studio creates in the data root, its temporary and backup files,
/// and files that file browsers add.
fn studio_entry(name: &str) -> bool {
    const NAMES: &[&str] = &[
        "config.json",
        "token",
        "server.json",
        "gpu.lock",
        "models",
        "engines",
        "downloads",
        "logs",
        "cache",
        "encodings",
        "desktop.ini",
        "Thumbs.db",
        ".DS_Store",
    ];
    let same = |a: &str, b: &str| {
        if cfg!(windows) {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    };
    NAMES.iter().any(|n| same(name, n))
        || (name.starts_with('.') && name.ends_with(".tmp"))
        || name.starts_with(crate::config::INVALID_PREFIX)
}
