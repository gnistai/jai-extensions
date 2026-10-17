//! State of the selected components on disk (DESIGN.md section 5).
//!
//! How a state is decided, cheaply (file sizes, small marker files and
//! `models/.verified.json`; no hashing):
//! - A model is **Installed** when `models/<file>` has the catalog size and a
//!   `.verified.json` entry with the catalog sha256, the same size and the
//!   same modification time.
//! - A model whose `models/<file>` has the full size but no matching entry
//!   (copied in by hand, or interrupted before it was recorded) is reported
//!   as **Partial** with `bytes` equal to the size: nothing is left to
//!   download, but it is not verified yet. `download::install` verifies it by
//!   hashing, without downloading, and downloads it again only on a mismatch.
//! - Otherwise a model is **Reusable** when `reuse::find_reusable` finds a
//!   candidate (same name and size).
//! - An archive is **Installed** when `engines/<dir>/.ok-<id>` holds the
//!   catalog sha256 and, for engines, the executable is found in `<dir>`.
//! - Anything else with a `downloads/<file>.part` of 1 to `size` bytes is
//!   **Partial**; with no part file (or an oversized one) it is **Missing**.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::catalog::{Component, Role, Selection};
use crate::fsutil;
use crate::paths::Paths;
use crate::perms;
use crate::reuse;
use crate::verified::Verified;

/// How deep below `engines/<dir>` the engine executable may sit. Some archives
/// put everything under a top folder.
pub(crate) const EXE_DEPTH: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InstallState {
    Installed,
    /// A partial download of this many bytes. A full-size model file that is
    /// not verified yet also reports here, with `bytes` equal to its size.
    Partial {
        bytes: u64,
    },
    Missing,
    /// A matching file elsewhere on this PC, such as an LM Studio download.
    Reusable {
        path: PathBuf,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentStatus {
    pub component: Component,
    pub state: InstallState,
    /// Where the component lives once installed: `models/<file>`, or
    /// `engines/<dir>` for archives.
    pub path: PathBuf,
}

/// Cheap: file sizes and `models/.verified.json`, no hashing. See the module
/// documentation for the rules.
pub fn status(paths: &Paths, sel: &Selection) -> Vec<ComponentStatus> {
    status_in(paths, sel, &reuse::default_dirs())
}

/// [`status`], looking for reusable files in `reuse_dirs`.
pub(crate) fn status_in(
    paths: &Paths,
    sel: &Selection,
    reuse_dirs: &[PathBuf],
) -> Vec<ComponentStatus> {
    let verified = Verified::load(paths);
    sel.components
        .iter()
        .map(|c| ComponentStatus {
            component: c.clone(),
            state: state_of(paths, c, &verified, reuse_dirs),
            path: install_path(paths, c),
        })
        .collect()
}

/// All selected components are Installed (and there is at least one).
pub fn ready(paths: &Paths, sel: &Selection) -> bool {
    let verified = Verified::load(paths);
    !sel.components.is_empty()
        && sel
            .components
            .iter()
            .all(|c| state_of(paths, c, &verified, &[]) == InstallState::Installed)
}

/// The model file of the selected component with this role, if it is on disk.
pub fn model_path(paths: &Paths, sel: &Selection, role: Role) -> Option<PathBuf> {
    sel.components
        .iter()
        .find(|c| c.role == role && c.unpack.is_none())
        .map(|c| paths.models().join(&c.file))
        .filter(|p| p.is_file())
}

/// The engine executable for `Role::LlmEngine` or `Role::ImageEngine`: the
/// component's `exe`, searched for in `engines/<dir>` down to three levels
/// (some archives have a top folder). None for other roles or when it is not
/// unpacked.
pub fn engine_exe(paths: &Paths, sel: &Selection, role: Role) -> Option<PathBuf> {
    if !role.is_engine() {
        return None;
    }
    let c = sel.components.iter().find(|c| c.role == role)?;
    let dir = paths.engines().join(c.dir.as_deref()?);
    find_file(&dir, c.exe.as_deref()?, EXE_DEPTH)
}

/// Where a component lives once installed.
pub(crate) fn install_path(paths: &Paths, c: &Component) -> PathBuf {
    match (&c.unpack, &c.dir) {
        (Some(_), Some(dir)) => paths.engines().join(dir),
        _ => paths.models().join(&c.file),
    }
}

/// `downloads/<file>.part`
pub(crate) fn part_path(paths: &Paths, c: &Component) -> PathBuf {
    paths.downloads().join(format!("{}.part", c.file))
}

/// The state of one component; `reuse_dirs` empty skips the reuse search.
pub(crate) fn state_of(
    paths: &Paths,
    c: &Component,
    verified: &Verified,
    reuse_dirs: &[PathBuf],
) -> InstallState {
    if c.unpack.is_some() {
        if archive_installed(paths, c) {
            return InstallState::Installed;
        }
    } else {
        let model = paths.models().join(&c.file);
        if let Ok(meta) = fs::metadata(&model) {
            if meta.is_file() && meta.len() == c.size {
                return if verified.matches(&c.file, &c.sha256, &meta) {
                    InstallState::Installed
                } else {
                    InstallState::Partial { bytes: c.size }
                };
            }
        }
        if let Some(path) = reuse::find_reusable_in(c, reuse_dirs) {
            return InstallState::Reusable { path };
        }
    }
    match fs::metadata(part_path(paths, c)) {
        Ok(meta) if meta.len() > 0 && meta.len() <= c.size => {
            InstallState::Partial { bytes: meta.len() }
        }
        _ => InstallState::Missing,
    }
}

/// Bytes an install still has to write for a component in `state`: the rest
/// of a download, or a copy of a reusable file that is not hard-linked (see
/// [`reuse_links`]). Installed and verifiable files need none.
pub(crate) fn bytes_to_write(paths: &Paths, c: &Component, state: &InstallState) -> u64 {
    match state {
        InstallState::Installed => 0,
        InstallState::Partial { bytes } => c.size.saturating_sub(*bytes),
        InstallState::Missing => c.size,
        InstallState::Reusable { path } => {
            if reuse_links(paths, path) {
                0
            } else {
                c.size
            }
        }
    }
}

/// Whether installing the reusable file at `candidate` hard-links it into
/// `models/` (taking no space) rather than copying it. A link needs the same
/// disk, and a file no other account can change: the link shares the file,
/// which then counts as verified.
pub fn reuse_links(paths: &Paths, candidate: &Path) -> bool {
    fsutil::same_device(candidate, &paths.models()) && perms::only_user_can_write(candidate)
}

/// `engines/<dir>/.ok-<id>` holds the catalog sha256 and the engine
/// executable, if the component has one, is there.
fn archive_installed(paths: &Paths, c: &Component) -> bool {
    let Some(dir) = c.dir.as_deref() else {
        return false;
    };
    let dir = paths.engines().join(dir);
    let marker_ok = fs::read_to_string(dir.join(format!(".ok-{}", c.id)))
        .is_ok_and(|s| s.trim().eq_ignore_ascii_case(&c.sha256));
    marker_ok
        && c.exe
            .as_deref()
            .is_none_or(|exe| find_file(&dir, exe, EXE_DEPTH).is_some())
}

/// Breadth-first search for a file called `name` in `root`, down to
/// `max_depth` levels (1 is `root` itself). The shallowest match wins, then
/// the first in name order. Folder links are not followed. Names compare
/// case-insensitively on Windows.
pub(crate) fn find_file(root: &Path, name: &str, max_depth: usize) -> Option<PathBuf> {
    let mut level = VecDeque::from([root.to_path_buf()]);
    for depth in 1..=max_depth {
        let mut next = VecDeque::new();
        let mut matches = Vec::new();
        while let Some(dir) = level.pop_front() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                let path = entry.path();
                if kind.is_dir() {
                    if depth < max_depth {
                        next.push_back(path);
                    }
                } else if same_name(&entry.file_name().to_string_lossy(), name) && path.is_file() {
                    matches.push(path);
                }
            }
        }
        if let Some(found) = matches.into_iter().min() {
            return Some(found);
        }
        let mut sorted: Vec<_> = next.into_iter().collect();
        sorted.sort();
        level = sorted.into();
    }
    None
}

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
    use crate::catalog::{Backend, Os, Unpack};
    use crate::verified;

    fn model(file: &str, size: u64, sha256: &str) -> Component {
        Component {
            id: "llm-model".into(),
            role: Role::LlmModel,
            label: "Model".into(),
            file: file.into(),
            url: "https://example.invalid/m".into(),
            size,
            sha256: sha256.into(),
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

    fn engine(sha256: &str) -> Component {
        Component {
            id: "llama-test".into(),
            role: Role::LlmEngine,
            label: "Engine".into(),
            file: "llama.zip".into(),
            url: "https://example.invalid/e".into(),
            size: 100,
            sha256: sha256.into(),
            license: "MIT".into(),
            source: "test".into(),
            os: Some(Os::Linux),
            backend: Some(Backend::Cpu),
            unpack: Some(Unpack::Zip),
            strip: None,
            dir: Some("llama-test".into()),
            exe: Some("llama-server".into()),
        }
    }

    fn selection(components: Vec<Component>) -> Selection {
        Selection {
            components,
            llm_backend: Backend::Cpu,
            image_backend: Backend::Cpu,
        }
    }

    fn state(paths: &Paths, c: &Component, reuse_dirs: &[PathBuf]) -> InstallState {
        status_in(paths, &selection(vec![c.clone()]), reuse_dirs)
            .remove(0)
            .state
    }

    #[test]
    fn model_states() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("root"));
        paths.ensure_dirs().unwrap();
        let sha = "ab".repeat(32);
        let c = model("m.gguf", 10, &sha);
        let sel = selection(vec![c.clone()]);

        assert_eq!(state(&paths, &c, &[]), InstallState::Missing);
        assert!(!ready(&paths, &sel));
        assert_eq!(model_path(&paths, &sel, Role::LlmModel), None);

        fs::write(part_path(&paths, &c), b"1234").unwrap();
        assert_eq!(state(&paths, &c, &[]), InstallState::Partial { bytes: 4 });
        assert_eq!(
            bytes_to_write(&paths, &c, &InstallState::Partial { bytes: 4 }),
            6
        );

        fs::write(part_path(&paths, &c), b"0123456789abc").unwrap();
        assert_eq!(
            state(&paths, &c, &[]),
            InstallState::Missing,
            "an oversized part"
        );

        // Full size but not verified: Partial with every byte present.
        let file = paths.models().join("m.gguf");
        fs::write(&file, b"0123456789").unwrap();
        assert_eq!(state(&paths, &c, &[]), InstallState::Partial { bytes: 10 });
        assert_eq!(
            bytes_to_write(&paths, &c, &InstallState::Partial { bytes: 10 }),
            0
        );
        assert!(!ready(&paths, &sel));
        assert_eq!(model_path(&paths, &sel, Role::LlmModel), Some(file.clone()));

        verified::record(&paths, "m.gguf", &sha).unwrap();
        assert_eq!(state(&paths, &c, &[]), InstallState::Installed);
        assert!(ready(&paths, &sel));

        // A different catalog hash, or a changed file, is no longer verified.
        let newer = model("m.gguf", 10, &"cd".repeat(32));
        assert_eq!(
            state(&paths, &newer, &[]),
            InstallState::Partial { bytes: 10 }
        );
        fs::write(&file, b"9876543210").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(state(&paths, &c, &[]), InstallState::Partial { bytes: 10 });

        // A wrong-size file in models/ counts as absent.
        fs::write(&file, b"short").unwrap();
        fs::remove_file(part_path(&paths, &c)).unwrap();
        assert_eq!(state(&paths, &c, &[]), InstallState::Missing);
    }

    #[test]
    fn reusable_models() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("root"));
        paths.ensure_dirs().unwrap();
        // Inside the private root, so that only the user can change the file
        // even where the temp folder lets another group modify what is in it.
        let lm = paths.root.join("lm").join("pub").join("repo");
        fs::create_dir_all(&lm).unwrap();
        fs::write(lm.join("m.gguf"), b"0123456789").unwrap();
        let c = model("m.gguf", 10, &"ab".repeat(32));
        let dirs = vec![paths.root.join("lm")];

        let st = state(&paths, &c, &dirs);
        let expected = fs::canonicalize(lm.join("m.gguf")).unwrap();
        assert_eq!(
            st,
            InstallState::Reusable {
                path: expected.clone()
            }
        );
        assert_eq!(bytes_to_write(&paths, &c, &st), 0, "same disk: a hard link");
        assert!(reuse_links(&paths, &expected));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&expected, fs::Permissions::from_mode(0o666)).unwrap();
            assert!(!reuse_links(&paths, &expected), "others can change it");
            assert_eq!(bytes_to_write(&paths, &c, &st), 10, "copied");
        }
        assert_eq!(
            state(&paths, &c, &[]),
            InstallState::Missing,
            "no reuse folders"
        );
    }

    #[test]
    fn engine_states_and_exe_search() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("root"));
        paths.ensure_dirs().unwrap();
        let sha = "ef".repeat(32);
        let c = engine(&sha);
        let sel = selection(vec![c.clone()]);
        let edir = paths.engines().join("llama-test");

        assert_eq!(state(&paths, &c, &[]), InstallState::Missing);
        fs::create_dir_all(edir.join("llama-b1").join("bin")).unwrap();
        fs::write(
            edir.join(".ok-llama-test"),
            format!("{}\n", sha.to_uppercase()),
        )
        .unwrap();
        assert_eq!(
            state(&paths, &c, &[]),
            InstallState::Missing,
            "marker but no exe"
        );

        let exe = edir.join("llama-b1").join("bin").join("llama-server");
        fs::write(&exe, b"exe").unwrap();
        assert_eq!(state(&paths, &c, &[]), InstallState::Installed);
        assert!(ready(&paths, &sel));
        assert_eq!(engine_exe(&paths, &sel, Role::LlmEngine), Some(exe.clone()));
        assert_eq!(
            engine_exe(&paths, &sel, Role::ImageEngine),
            None,
            "not selected"
        );
        assert_eq!(
            engine_exe(&paths, &sel, Role::LlmModel),
            None,
            "not an engine role"
        );
        assert_eq!(status_in(&paths, &sel, &[])[0].path, edir);

        // The shallowest match wins.
        let top = edir.join("llama-server");
        fs::write(&top, b"exe").unwrap();
        assert_eq!(engine_exe(&paths, &sel, Role::LlmEngine), Some(top));

        fs::write(edir.join(".ok-llama-test"), "other").unwrap();
        assert_eq!(
            state(&paths, &c, &[]),
            InstallState::Missing,
            "marker for another build"
        );
    }

    #[test]
    fn exe_search_depth() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a").join("b");
        fs::create_dir_all(deep.join("c")).unwrap();
        fs::write(deep.join("tool"), b"x").unwrap();
        fs::write(deep.join("c").join("deeper"), b"x").unwrap();
        assert_eq!(find_file(dir.path(), "tool", 3), Some(deep.join("tool")));
        assert_eq!(find_file(dir.path(), "tool", 2), None);
        assert_eq!(find_file(dir.path(), "deeper", 3), None);
        assert_eq!(find_file(dir.path(), "b", 3), None, "folders are not files");
    }

    #[test]
    fn an_empty_selection_is_not_ready() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!ready(&Paths::at(dir.path()), &selection(Vec::new())));
    }
}
