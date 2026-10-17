//! `models/.verified.json`: model files whose SHA-256 has been checked, keyed
//! by file name as `{file: {size, sha256, mtimeMs}}`, so multi-gigabyte files
//! are not hashed again at every start. A file counts as verified while its
//! size and modification time still match the entry.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::fsutil;
use crate::paths::Paths;

/// Serializes read-modify-write cycles within this process.
static LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Entry {
    pub size: u64,
    pub sha256: String,
    pub mtime_ms: u64,
}

#[derive(Debug, Default)]
pub(crate) struct Verified(BTreeMap<String, Entry>);

impl Verified {
    /// Reads the file; empty when it is missing or unreadable.
    pub(crate) fn load(paths: &Paths) -> Verified {
        let file = path(paths);
        match fs::read(&file) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(map) => Verified(map),
                Err(e) => {
                    tracing::warn!(path = %file.display(), error = %e, "ignoring an invalid verification record");
                    Verified::default()
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Verified::default(),
            Err(e) => {
                tracing::warn!(path = %file.display(), error = %e, "cannot read the verification record");
                Verified::default()
            }
        }
    }

    /// The model file described by `meta` is the one verified against `sha256`.
    pub(crate) fn matches(&self, file: &str, sha256: &str, meta: &fs::Metadata) -> bool {
        self.0.get(file).is_some_and(|e| {
            e.size == meta.len()
                && e.sha256.eq_ignore_ascii_case(sha256)
                && e.mtime_ms == fsutil::mtime_ms(meta)
        })
    }
}

/// Records `models/<file>` as verified against `sha256`, with its current size
/// and modification time.
pub(crate) fn record(paths: &Paths, file: &str, sha256: &str) -> io::Result<()> {
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let meta = fs::metadata(paths.models().join(file))?;
    let mut verified = Verified::load(paths);
    verified.0.insert(
        file.to_owned(),
        Entry {
            size: meta.len(),
            sha256: sha256.to_ascii_lowercase(),
            mtime_ms: fsutil::mtime_ms(&meta),
        },
    );
    save(paths, &verified)
}

/// Drops the entry for `file`, if any.
pub(crate) fn forget(paths: &Paths, file: &str) -> io::Result<()> {
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut verified = Verified::load(paths);
    if verified.0.remove(file).is_some() {
        save(paths, &verified)?;
    }
    Ok(())
}

fn path(paths: &Paths) -> PathBuf {
    paths.models().join(".verified.json")
}

fn save(paths: &Paths, verified: &Verified) -> io::Result<()> {
    let mut json = serde_json::to_vec_pretty(&verified.0).map_err(io::Error::other)?;
    json.push(b'\n');
    fsutil::write_atomic(&path(paths), &json, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_match_and_forget() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        fs::create_dir_all(paths.models()).unwrap();
        let model = paths.models().join("m.gguf");
        fs::write(&model, b"weights").unwrap();

        record(&paths, "m.gguf", "ABCD").unwrap();
        let meta = fs::metadata(&model).unwrap();
        let v = Verified::load(&paths);
        assert!(v.matches("m.gguf", "abcd", &meta));
        assert!(!v.matches("m.gguf", "abce", &meta));
        assert!(!v.matches("other.gguf", "abcd", &meta));

        let text = fs::read_to_string(path(&paths)).unwrap();
        assert!(text.contains("\"mtimeMs\""), "{text}");
        assert!(text.contains("\"sha256\": \"abcd\""), "{text}");

        fs::write(&model, b"weights, changed").unwrap();
        let meta = fs::metadata(&model).unwrap();
        assert!(!Verified::load(&paths).matches("m.gguf", "abcd", &meta));

        forget(&paths, "m.gguf").unwrap();
        assert!(Verified::load(&paths).0.is_empty());
    }

    #[test]
    fn an_invalid_record_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        fs::create_dir_all(paths.models()).unwrap();
        fs::write(path(&paths), "nonsense").unwrap();
        assert!(Verified::load(&paths).0.is_empty());
    }
}
