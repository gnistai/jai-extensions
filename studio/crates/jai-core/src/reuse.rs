//! Reusing model files already on this PC, such as LM Studio downloads
//! (DESIGN.md section 5). Finding a candidate is cheap: the same file name and
//! size, no hashing. The installer hashes a candidate before using it.

use std::collections::VecDeque;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use crate::catalog::Component;

/// How deep below a models folder a file may sit. LM Studio uses
/// `<publisher>/<repo>/<file>`, which is depth 3.
const MAX_DEPTH: usize = 4;
/// Most folder entries one search looks at, so a models folder set to a
/// whole drive cannot make the Models tab slow.
const MAX_ENTRIES: usize = 50_000;
/// LM Studio's settings file is small; anything bigger is not it.
const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;

/// A file with the same name and size in LM Studio's model folders (see
/// [`default_dirs`]). Only model files are reused, never engine archives.
pub fn find_reusable(component: &Component) -> Option<PathBuf> {
    find_reusable_in(component, &default_dirs())
}

/// The folders [`find_reusable`] searches: LM Studio's.
pub(crate) fn default_dirs() -> Vec<PathBuf> {
    dirs::home_dir()
        .map(|home| lm_studio_dirs(&home))
        .unwrap_or_default()
}

/// LM Studio's model folders, most likely first: the downloads folder chosen
/// in its settings (`downloadsFolder` in `settings.json`), the `models`
/// folder in its home (`~/.lmstudio`, unless `~/.lmstudio-home-pointer` names
/// another), and the default and older default locations. Each appears once.
pub(crate) fn lm_studio_dirs(home: &Path) -> Vec<PathBuf> {
    let default_home = home.join(".lmstudio");
    let lm_home =
        home_pointer(&home.join(".lmstudio-home-pointer")).unwrap_or_else(|| default_home.clone());
    let legacy = home.join(".cache").join("lm-studio");
    let mut dirs = Vec::new();
    for settings in [lm_home.join("settings.json"), legacy.join("settings.json")] {
        if let Some(folder) = downloads_folder(&settings) {
            dirs.push(folder);
        }
    }
    dirs.push(lm_home.join("models"));
    dirs.push(default_home.join("models"));
    dirs.push(legacy.join("models"));
    let mut unique: Vec<PathBuf> = Vec::with_capacity(dirs.len());
    for dir in dirs {
        if !unique.iter().any(|seen| same_path(seen, &dir)) {
            unique.push(dir);
        }
    }
    unique
}

/// The folder `~/.lmstudio-home-pointer` names, if it names one.
fn home_pointer(file: &Path) -> Option<PathBuf> {
    let text = read_small(file, 4096)?;
    let path = PathBuf::from(text.trim_start_matches('\u{feff}').trim());
    (path.is_absolute() && path.is_dir()).then_some(path)
}

/// `downloadsFolder` from LM Studio's `settings.json`. The file also holds
/// tokens, so nothing else is read from it and it is never logged.
fn downloads_folder(settings: &Path) -> Option<PathBuf> {
    let text = read_small(settings, MAX_SETTINGS_BYTES)?;
    let value: serde_json::Value =
        serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    let folder = value.get("downloadsFolder")?.as_str()?.trim();
    let path = PathBuf::from(folder);
    (!folder.is_empty() && path.is_absolute()).then_some(path)
}

/// A UTF-8 file of at most `limit` bytes.
fn read_small(file: &Path, limit: u64) -> Option<String> {
    let mut text = String::new();
    fs::File::open(file)
        .ok()?
        .take(limit + 1)
        .read_to_string(&mut text)
        .ok()?;
    (text.len() as u64 <= limit).then_some(text)
}

/// Paths compare case-insensitively on Windows.
fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.as_os_str().eq_ignore_ascii_case(b.as_os_str())
    } else {
        a == b
    }
}

/// [`find_reusable`] over the given folders, in order. The result is
/// canonical, so a symbolic link resolves to the file it names.
pub(crate) fn find_reusable_in(component: &Component, dirs: &[PathBuf]) -> Option<PathBuf> {
    if component.unpack.is_some() || !component.role.is_model() {
        return None;
    }
    dirs.iter()
        .find_map(|dir| search(dir, &component.file, component.size))
}

/// Breadth-first search for `name` with `size` bytes, down to [`MAX_DEPTH`]
/// and at most [`MAX_ENTRIES`] entries. Symbolic links to files count; links
/// to folders are not followed, so the walk cannot loop. Entries are visited
/// in name order, for stable results.
fn search(root: &Path, name: &str, size: u64) -> Option<PathBuf> {
    search_within(root, name, size, MAX_ENTRIES)
}

/// [`search`] looking at no more than `budget` entries.
fn search_within(root: &Path, name: &str, size: u64, mut budget: usize) -> Option<PathBuf> {
    let mut queue = VecDeque::from([(root.to_path_buf(), 1usize)]);
    while let Some((dir, depth)) = queue.pop_front() {
        if budget == 0 {
            tracing::debug!(root = %root.display(), "stopped looking for reusable files: too many entries");
            return None;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.take(budget).flatten().collect();
        budget = budget.saturating_sub(entries.len());
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                if depth < MAX_DEPTH {
                    queue.push_back((path, depth + 1));
                }
            } else if same_name(&entry.file_name().to_string_lossy(), name) {
                let found = fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() == size);
                if found {
                    return fs::canonicalize(&path).ok();
                }
            }
        }
    }
    None
}

/// File names compare case-insensitively on Windows.
fn same_name(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Role;

    fn model(file: &str, size: u64) -> Component {
        Component {
            id: "llm-model".into(),
            role: Role::LlmModel,
            label: "test".into(),
            file: file.into(),
            url: "https://example.invalid/x".into(),
            size,
            sha256: "0".repeat(64),
            license: "MIT".into(),
            source: "test".into(),
            os: None,
            backend: None,
            unpack: None,
            strip: None,
            dir: None,
            exe: None,
        }
    }

    #[test]
    fn finds_a_file_by_name_and_size() {
        let dir = tempfile::tempdir().unwrap();
        let lm = dir.path().join("lmstudio");
        let repo = lm.join("unsloth").join("Qwen-GGUF");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("q.gguf"), b"12345").unwrap();
        fs::write(lm.join("q.gguf.part"), b"12345").unwrap();

        let dirs = vec![dir.path().join("missing"), lm.clone()];
        let found = find_reusable_in(&model("q.gguf", 5), &dirs).unwrap();
        assert_eq!(found, fs::canonicalize(repo.join("q.gguf")).unwrap());
        assert_eq!(
            find_reusable_in(&model("q.gguf", 6), &dirs),
            None,
            "size differs"
        );
        assert_eq!(
            find_reusable_in(&model("r.gguf", 5), &dirs),
            None,
            "name differs"
        );

        let mut archive = model("q.gguf", 5);
        archive.unpack = Some(crate::catalog::Unpack::Zip);
        archive.role = Role::LlmEngine;
        assert_eq!(
            find_reusable_in(&archive, &dirs),
            None,
            "archives are never reused"
        );
    }

    #[test]
    fn stops_at_the_depth_limit() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a").join("b").join("c");
        fs::create_dir_all(deep.join("d")).unwrap();
        fs::write(deep.join("ok.gguf"), b"x").unwrap();
        fs::write(deep.join("d").join("deep.gguf"), b"x").unwrap();
        let dirs = vec![dir.path().to_path_buf()];
        assert!(
            find_reusable_in(&model("ok.gguf", 1), &dirs).is_some(),
            "depth 4"
        );
        assert_eq!(
            find_reusable_in(&model("deep.gguf", 1), &dirs),
            None,
            "depth 5"
        );
    }

    /// LM Studio's downloads folder can be moved in its settings, and its
    /// home with a pointer file; both are searched, before the defaults.
    #[test]
    fn lm_studio_folders_from_its_settings() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let defaults = |home: &Path| {
            vec![
                home.join(".lmstudio").join("models"),
                home.join(".cache").join("lm-studio").join("models"),
            ]
        };
        assert_eq!(lm_studio_dirs(home), defaults(home), "nothing configured");

        let big = home.join("big-disk").join("LM Models");
        fs::create_dir_all(home.join(".lmstudio")).unwrap();
        let settings = serde_json::json!({
            "downloadsFolder": big.to_str().unwrap(),
            "hfDownloadToken": "secret",
        });
        fs::write(
            home.join(".lmstudio").join("settings.json"),
            settings.to_string(),
        )
        .unwrap();
        assert_eq!(
            lm_studio_dirs(home),
            [vec![big.clone()], defaults(home)].concat()
        );

        // A moved LM Studio home: its settings and models folder count.
        let moved = home.join("apps").join("lmstudio");
        fs::create_dir_all(&moved).unwrap();
        fs::write(home.join(".lmstudio-home-pointer"), moved.to_str().unwrap()).unwrap();
        fs::write(moved.join("settings.json"), r#"{"downloadsFolder": ""}"#).unwrap();
        assert_eq!(
            lm_studio_dirs(home),
            [vec![moved.join("models")], defaults(home)].concat()
        );

        // Junk is ignored: a relative folder, invalid JSON, a missing home.
        fs::write(
            moved.join("settings.json"),
            r#"{"downloadsFolder": "models"}"#,
        )
        .unwrap();
        assert_eq!(lm_studio_dirs(home)[0], moved.join("models"));
        fs::write(moved.join("settings.json"), "{ not json").unwrap();
        assert_eq!(lm_studio_dirs(home)[0], moved.join("models"));
        fs::write(home.join(".lmstudio-home-pointer"), "/no/such/folder").unwrap();
        assert_eq!(lm_studio_dirs(home), [vec![big], defaults(home)].concat());
    }

    #[test]
    fn a_huge_folder_is_searched_only_so_far() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("z");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("m.gguf"), b"x").unwrap();
        for i in 0..20 {
            fs::write(dir.path().join(format!("f{i:02}")), b"").unwrap();
        }
        // 21 entries at the top, then one in z/.
        assert!(search_within(dir.path(), "m.gguf", 1, 22).is_some());
        assert_eq!(
            search_within(dir.path(), "m.gguf", 1, 21),
            None,
            "the budget ran out"
        );
        assert!(search(dir.path(), "m.gguf", 1).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_file_resolves_to_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        let lm = dir.path().join("lm").join("pub").join("repo");
        fs::create_dir_all(&store).unwrap();
        fs::create_dir_all(&lm).unwrap();
        fs::write(store.join("real.gguf"), b"abc").unwrap();
        std::os::unix::fs::symlink(store.join("real.gguf"), lm.join("m.gguf")).unwrap();
        let found = find_reusable_in(&model("m.gguf", 3), &[dir.path().join("lm")]).unwrap();
        assert_eq!(found, fs::canonicalize(store.join("real.gguf")).unwrap());
    }
}
