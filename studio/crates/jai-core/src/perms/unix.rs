//! Unix: private means owned by the user, with no group or other bits.

use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::Path;

use super::{Access, Check};

/// The mode of Studio's folders.
const PRIVATE_DIR: u32 = 0o700;

pub(super) fn inspect(path: &Path, check: Check) -> io::Result<Access> {
    let meta = fs::metadata(path)?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    // Root can change anything anyway; trusting it matters for read-only
    // places such as /opt, never for the data root, which needs `Any`.
    let owner_ok = meta.uid() == me || (check == Check::Write && meta.uid() == 0);
    if !owner_ok {
        return Ok(Access::Shared(format!(
            "it belongs to another account (user id {})",
            meta.uid()
        )));
    }
    let mode = meta.mode() & 0o777;
    let others = match check {
        Check::Any => 0o077,
        Check::Write => 0o022,
    };
    if mode & others != 0 {
        return Ok(Access::Shared(format!("its permissions are {mode:04o}")));
    }
    Ok(Access::Private)
}

/// Creates one folder, mode 0700. Its parent must exist.
pub(super) fn create_private(path: &Path) -> io::Result<()> {
    DirBuilder::new().mode(PRIVATE_DIR).create(path)
}

/// Creates a folder and its missing parents, mode 0700.
pub(super) fn create_dir_all(path: &Path) -> io::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(PRIVATE_DIR)
        .create(path)
}

pub(super) fn make_private(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, Permissions::from_mode(PRIVATE_DIR))
}
