//! The viewer's render cache (DESIGN.md section 2): `<root>/cache/renders/
//! <sha256>.png`, keyed by the document's identity (its canonical text), the
//! seed and the render size. A `<sha256>.json` sidecar keeps what the viewer
//! shows next to the image; it is written last, so an entry without it is
//! incomplete and ignored.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use jai_js::Diag;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Entries kept; the least recently used go first.
pub const MAX_ENTRIES: usize = 200;

/// Everything about a render except its pixels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedRender {
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub seed: u64,
    pub sent_prompt: String,
    pub description: String,
    pub document_text: String,
    pub diagnostics: Vec<Diag>,
}

pub struct RenderCache {
    dir: PathBuf,
}

impl RenderCache {
    pub fn new(dir: impl Into<PathBuf>) -> RenderCache {
        RenderCache { dir: dir.into() }
    }

    /// The key of a render: the canonical document text, the requested seed
    /// (`None` for the document's own) and a size tag (model, scale, format).
    /// The prefix changes whenever the renderer would draw the same key
    /// differently, so that no older render is served.
    pub fn key(identity: &str, seed: Option<&str>, size: &str) -> String {
        let mut h = Sha256::new();
        h.update(b"jai-render-v2\n");
        h.update(identity.as_bytes());
        h.update(b"\nseed=");
        h.update(seed.unwrap_or("document").as_bytes());
        h.update(b"\nsize=");
        h.update(size.as_bytes());
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    fn image_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.png"))
    }

    fn meta_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    /// A cached render, marked as recently used.
    pub fn get(&self, key: &str) -> Option<(Vec<u8>, CachedRender)> {
        let meta_path = self.meta_path(key);
        let meta: CachedRender = serde_json::from_slice(&std::fs::read(&meta_path).ok()?).ok()?;
        let bytes = std::fs::read(self.image_path(key)).ok()?;
        if let Ok(f) = std::fs::File::options().write(true).open(&meta_path) {
            let _ = f.set_modified(SystemTime::now());
        }
        Some((bytes, meta))
    }

    /// Stores a render, then drops the oldest entries beyond [`MAX_ENTRIES`].
    pub fn put(&self, key: &str, bytes: &[u8], meta: &CachedRender) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        write_atomic(&self.image_path(key), bytes)?;
        let json = serde_json::to_vec(meta).map_err(std::io::Error::other)?;
        write_atomic(&self.meta_path(key), &json)?;
        self.prune(MAX_ENTRIES);
        Ok(())
    }

    /// Keeps the `max` most recently used entries.
    pub fn prune(&self, max: usize) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut metas: Vec<(SystemTime, String)> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let key = name.strip_suffix(".json")?.to_string();
                let modified = e.metadata().ok()?.modified().ok()?;
                Some((modified, key))
            })
            .collect();
        if metas.len() <= max {
            return;
        }
        metas.sort_by_key(|m| std::cmp::Reverse(m.0));
        for (_, key) in metas.into_iter().skip(max) {
            let _ = std::fs::remove_file(self.meta_path(&key));
            let _ = std::fs::remove_file(self.image_path(&key));
        }
    }
}

/// Writes through a temporary file, so readers never see half a file.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(seed: u64) -> CachedRender {
        CachedRender {
            mime: "image/png".into(),
            width: 1344,
            height: 768,
            seed,
            sent_prompt: "A castle at dusk.".into(),
            description: "A castle at dusk.".into(),
            document_text: "a castle at dusk".into(),
            diagnostics: vec![],
        }
    }

    #[test]
    fn keys() {
        let a = RenderCache::key("a castle", None, "flux-schnell@1/png");
        assert_eq!(a.len(), 64);
        assert_eq!(a, RenderCache::key("a castle", None, "flux-schnell@1/png"));
        assert_ne!(
            a,
            RenderCache::key("a castle", Some("7"), "flux-schnell@1/png")
        );
        assert_ne!(a, RenderCache::key("a castle", None, "flux-schnell@2/png"));
        assert_ne!(a, RenderCache::key("a palace", None, "flux-schnell@1/png"));
    }

    #[test]
    fn put_get_and_prune() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RenderCache::new(dir.path().join("renders"));
        assert!(cache.get("missing").is_none());
        cache.put("k1", b"png-1", &meta(1)).unwrap();
        let (bytes, m) = cache.get("k1").unwrap();
        assert_eq!(bytes, b"png-1");
        assert_eq!(m, meta(1));

        // Without its sidecar an entry does not count.
        std::fs::write(dir.path().join("renders/k9.png"), b"half").unwrap();
        assert!(cache.get("k9").is_none());

        for (i, key) in ["k2", "k3", "k4"].iter().enumerate() {
            cache.put(key, b"png", &meta(i as u64)).unwrap();
        }
        let old = SystemTime::now() - std::time::Duration::from_secs(3600);
        for key in ["k1", "k2"] {
            let f = std::fs::File::options()
                .write(true)
                .open(dir.path().join(format!("renders/{key}.json")))
                .unwrap();
            f.set_modified(old).unwrap();
        }
        cache.prune(2);
        assert!(cache.get("k1").is_none() && cache.get("k2").is_none());
        assert!(cache.get("k3").is_some() && cache.get("k4").is_some());
        assert!(!dir.path().join("renders/k1.png").exists());
    }
}
