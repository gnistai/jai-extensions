//! `<root>/gpu.lock`: an exclusive file lock held by the one process that owns
//! the engines (DESIGN.md sections 2 and 10).

use std::fs::OpenOptions;

use anyhow::Context;
use jai_core::Paths;

/// Holds the lock until dropped; keep it for the life of the process. The OS
/// releases it when the process ends, however it ends.
#[derive(Debug)]
pub struct GpuLock {
    _file: std::fs::File,
}

/// Takes the lock (fs4); `Ok(None)` when another process holds it.
pub fn try_acquire(paths: &Paths) -> anyhow::Result<Option<GpuLock>> {
    std::fs::create_dir_all(&paths.root)
        .with_context(|| format!("could not create {}", paths.root.display()))?;
    let path = paths.gpu_lock();
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    // Only the user may open it, so no other account can take the lock.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options
        .open(&path)
        .with_context(|| format!("could not open {}", path.display()))?;
    // The trait method, not std's File::try_lock of the same name.
    match fs4::FileExt::try_lock(&file) {
        Ok(()) => Ok(Some(GpuLock { _file: file })),
        Err(fs4::TryLockError::WouldBlock) => Ok(None),
        Err(fs4::TryLockError::Error(err)) => {
            Err(err).with_context(|| format!("could not lock {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("root"));
        let first = try_acquire(&paths).unwrap();
        assert!(first.is_some());
        assert!(try_acquire(&paths).unwrap().is_none());
        drop(first);
        assert!(try_acquire(&paths).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn only_the_user_can_open_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("root"));
        let _lock = try_acquire(&paths).unwrap().unwrap();
        let mode = std::fs::metadata(paths.gpu_lock())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }
}
