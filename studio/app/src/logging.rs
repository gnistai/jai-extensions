//! tracing setup: stderr for CLI commands, `<root>/logs/jai.log` for the GUI
//! and the tray, which have no console. `JAI_LOG` sets the filter (DESIGN.md
//! section 0), for example `JAI_LOG=debug` or `JAI_LOG=jai_server=trace`.
//!
//! `jai.log` is rotated by size while it is written: past 10 MB it becomes
//! `jai.log.1` and a new file starts. Only the primary instance rotates (from
//! [`allow_rotation`], called once setup runs): every launch logs a line or
//! two before tauri-plugin-single-instance hands its arguments over, and such
//! a short-lived second launch must not rename the file under the running
//! one.

use tracing_subscriber::EnvFilter;

#[cfg(feature = "gui")]
use rotating::RotatingLog;

/// The environment variable with the tracing filter.
pub const LOG_ENV: &str = "JAI_LOG";

fn filter(default: &str) -> EnvFilter {
    EnvFilter::try_from_env(LOG_ENV).unwrap_or_else(|_| EnvFilter::new(default))
}

/// Logs to stderr, at `default` unless `JAI_LOG` says otherwise. Colours
/// only on a Unix terminal: Windows consoles without VT mode print the codes.
pub fn init_stderr(default: &str) {
    use std::io::IsTerminal as _;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter(default))
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(cfg!(unix) && std::io::stderr().is_terminal())
        .try_init();
}

/// The GUI's log file, once [`init_file`] opened it.
#[cfg(feature = "gui")]
static LOG_FILE: std::sync::OnceLock<std::sync::Arc<RotatingLog>> = std::sync::OnceLock::new();

/// Logs to `logs/jai.log` (rotated by size, see the module documentation),
/// and logs panics there too.
#[cfg(feature = "gui")]
pub fn init_file(paths: &jai_core::Paths) {
    let dir = paths.logs();
    let _ = std::fs::create_dir_all(&dir);
    let Ok(log) = RotatingLog::open(dir.join("jai.log"), rotating::MAX_LOG) else {
        init_stderr("info");
        return;
    };
    let _ = LOG_FILE.set(log.clone());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter("info"))
        .with_writer(move || rotating::LogWriter(log.clone()))
        .with_ansi(false)
        .try_init();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        default_hook(info);
    }));
}

/// This process is the primary instance: it may rotate `jai.log` from now on.
#[cfg(feature = "gui")]
pub fn allow_rotation() {
    if let Some(log) = LOG_FILE.get() {
        log.allow_rotation();
    }
}

#[cfg(any(feature = "gui", test))]
mod rotating {
    use std::fs::{File, OpenOptions};
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    /// Size at which `jai.log` is rotated.
    pub const MAX_LOG: u64 = 10_000_000;

    /// A log file that is renamed to `<name>.1` and started afresh once it
    /// passes its size limit, if rotation is allowed.
    pub struct RotatingLog {
        file: Mutex<LogFile>,
    }

    struct LogFile {
        path: PathBuf,
        file: Option<File>,
        /// Bytes in the file, as far as this process knows.
        size: u64,
        rotate: bool,
        /// Rotate once `size` reaches this: the limit, or a little more after a
        /// rotation failed (the file was busy), so it is not retried on every
        /// line.
        rotate_at: u64,
        max: u64,
    }

    impl RotatingLog {
        /// Opens `path` for appending; rotation starts switched off.
        pub fn open(path: PathBuf, max: u64) -> io::Result<Arc<RotatingLog>> {
            let file = open_append(&path)?;
            let size = file.metadata().map(|m| m.len()).unwrap_or(0);
            Ok(Arc::new(RotatingLog {
                file: Mutex::new(LogFile {
                    path,
                    file: Some(file),
                    size,
                    rotate: false,
                    rotate_at: max,
                    max,
                }),
            }))
        }

        pub fn allow_rotation(&self) {
            self.lock().rotate = true;
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, LogFile> {
            self.file.lock().unwrap_or_else(|e| e.into_inner())
        }

        fn write_all(&self, buf: &[u8]) -> io::Result<()> {
            let mut f = self.lock();
            if f.rotate && f.size >= f.rotate_at {
                f.rotate_now();
            }
            let Some(file) = f.file.as_mut() else {
                return Ok(());
            };
            file.write_all(buf)?;
            f.size += buf.len() as u64;
            Ok(())
        }
    }

    impl LogFile {
        /// Renames the file to `<name>.1` (replacing an older one) and opens a
        /// new one. When the rename fails, keeps appending to the old file.
        fn rotate_now(&mut self) {
            drop(self.file.take());
            let mut old = self.path.clone().into_os_string();
            old.push(".1");
            let renamed = std::fs::rename(&self.path, PathBuf::from(old)).is_ok();
            self.file = open_append(&self.path).ok();
            self.size = self
                .file
                .as_ref()
                .and_then(|f| f.metadata().ok())
                .map_or(0, |m| m.len());
            self.rotate_at = if renamed {
                self.max
            } else {
                self.size + self.max / 10
            };
        }
    }

    fn open_append(path: &Path) -> io::Result<File> {
        OpenOptions::new().create(true).append(true).open(path)
    }

    /// What tracing writes one event through.
    pub struct LogWriter(pub Arc<RotatingLog>);

    impl Write for LogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write_all(buf)?;
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn write(log: &Arc<RotatingLog>, text: &str) {
            LogWriter(log.clone()).write_all(text.as_bytes()).unwrap();
        }

        #[test]
        fn rotates_by_size_once_allowed() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("jai.log");
            let rotated = dir.path().join("jai.log.1");
            let log = RotatingLog::open(path.clone(), 100).unwrap();
            // A second launch, before it hands over: never rotates.
            write(&log, &"a".repeat(150));
            write(&log, "b\n");
            assert!(!rotated.exists());
            // The primary instance rotates as it writes.
            log.allow_rotation();
            write(&log, "c\n");
            assert_eq!(std::fs::read_to_string(&rotated).unwrap().len(), 152);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "c\n");
            write(&log, &"d".repeat(120));
            write(&log, "e\n");
            // The older rotation is replaced.
            assert!(std::fs::read_to_string(&rotated)
                .unwrap()
                .starts_with("c\nddd"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "e\n");
            // The default limit is 10 MB.
            assert_eq!(MAX_LOG, 10_000_000);
        }

        #[test]
        fn a_large_log_at_start_rotates_when_allowed() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("jai.log");
            std::fs::write(&path, "x".repeat(500)).unwrap();
            let log = RotatingLog::open(path.clone(), 100).unwrap();
            write(&log, "starting\n");
            assert_eq!(std::fs::read_to_string(&path).unwrap().len(), 509);
            log.allow_rotation();
            write(&log, "primary\n");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "primary\n");
            assert_eq!(
                std::fs::read_to_string(dir.path().join("jai.log.1"))
                    .unwrap()
                    .len(),
                509
            );
        }
    }
}
