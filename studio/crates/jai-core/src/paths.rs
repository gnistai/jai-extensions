//! Locations under the JAI data root (DESIGN.md section 2).

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::Context as _;

use crate::perms;

/// Environment variable that overrides the data root.
pub const JAI_HOME: &str = "JAI_HOME";

/// The data root. Every file and folder Studio writes lives under it.
#[derive(Clone, Debug)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    /// `JAI_HOME`, else `%LOCALAPPDATA%\JAI` on Windows and `$XDG_DATA_HOME/jai`
    /// (or `~/.local/share/jai`) on Linux. Creates the root so that only the
    /// user can open it, and checks an existing one (see [`Paths::ensure_dirs`]).
    pub fn resolve() -> anyhow::Result<Paths> {
        let root = root_from(std::env::var_os(JAI_HOME))?;
        perms::ensure_private_root(&root)
            .with_context(|| format!("cannot use the data folder {}", root.display()))?;
        Ok(Paths { root })
    }

    /// A data root at a given place, for tests. Creates nothing.
    pub fn at(root: impl Into<PathBuf>) -> Paths {
        Paths { root: root.into() }
    }

    /// `<root>/config.json`
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.json")
    }

    /// `<root>/token`
    pub fn token_file(&self) -> PathBuf {
        self.root.join("token")
    }

    /// `<root>/server.json`
    pub fn server_file(&self) -> PathBuf {
        self.root.join("server.json")
    }

    /// `<root>/gpu.lock`
    pub fn gpu_lock(&self) -> PathBuf {
        self.root.join("gpu.lock")
    }

    /// `<root>/models`
    pub fn models(&self) -> PathBuf {
        self.root.join("models")
    }

    /// `<root>/engines`
    pub fn engines(&self) -> PathBuf {
        self.root.join("engines")
    }

    /// `<root>/downloads`
    pub fn downloads(&self) -> PathBuf {
        self.root.join("downloads")
    }

    /// `<root>/logs`
    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// `<root>/cache/renders`
    pub fn render_cache(&self) -> PathBuf {
        self.root.join("cache").join("renders")
    }

    /// `<root>/encodings`
    pub fn encodings(&self) -> PathBuf {
        self.root.join("encodings")
    }

    /// Creates the root and the folders above, private to the user.
    ///
    /// The root must be private: on Unix owned by the user with mode 0700, on
    /// Windows owned by the user (or SYSTEM or Administrators) with an access
    /// list that grants nobody else anything. A new root is created that way.
    /// An existing root that other accounts can open is made private when it
    /// holds only Studio's own files (and a token they could read is deleted,
    /// so a new one is made); otherwise this fails with a `PermissionDenied`
    /// error that says why. `models`, `engines` and `downloads` may link to
    /// another disk, but not to a folder that other accounts can change.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        perms::ensure_private_root(&self.root)?;
        for dir in [
            self.models(),
            self.engines(),
            self.downloads(),
            self.logs(),
            self.render_cache(),
            self.encodings(),
        ] {
            perms::create_dir(&dir)?;
        }
        for dir in [self.models(), self.engines(), self.downloads()] {
            perms::check_linked(&self.root, &dir)?;
        }
        Ok(())
    }
}

/// The root for a `JAI_HOME` value, else the platform default. A relative
/// `JAI_HOME` is made absolute, because engines run in their own folders and
/// get model paths as arguments.
fn root_from(jai_home: Option<OsString>) -> anyhow::Result<PathBuf> {
    if let Some(home) = jai_home.filter(|v| !v.is_empty()) {
        return std::path::absolute(PathBuf::from(home))
            .with_context(|| format!("{JAI_HOME} is not a usable path"));
    }
    default_root().with_context(|| format!("cannot find the user data folder; set {JAI_HOME}"))
}

#[cfg(windows)]
fn default_root() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("JAI"))
}

/// `$XDG_DATA_HOME/jai`, or `~/.local/share/jai` when that is unset.
#[cfg(not(windows))]
fn default_root() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("jai"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jai_home_wins_and_is_made_absolute() {
        let root = root_from(Some(OsString::from("some/rel"))).unwrap();
        assert!(root.is_absolute());
        assert!(root.ends_with("some/rel"));

        let dir = tempfile::tempdir().unwrap();
        let root = root_from(Some(dir.path().as_os_str().to_owned())).unwrap();
        assert_eq!(root, dir.path());
    }

    #[test]
    fn empty_jai_home_means_the_platform_default() {
        let root = root_from(Some(OsString::new())).unwrap();
        let name = root.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, if cfg!(windows) { "JAI" } else { "jai" });
    }

    #[test]
    fn layout_and_ensure_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("root"));
        assert_eq!(paths.config_file(), paths.root.join("config.json"));
        assert_eq!(
            paths.render_cache(),
            paths.root.join("cache").join("renders")
        );
        paths.ensure_dirs().unwrap();
        for d in [
            paths.models(),
            paths.engines(),
            paths.downloads(),
            paths.logs(),
            paths.render_cache(),
            paths.encodings(),
        ] {
            assert!(d.is_dir(), "{} was not created", d.display());
        }
    }

    /// `ensure_dirs` keeps the root private: it tightens Studio's own root,
    /// refuses a shared folder with other things in it, and refuses a models
    /// folder linked to a place other accounts can change.
    #[cfg(unix)]
    #[test]
    fn ensure_dirs_keeps_the_root_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let chmod = |p: &std::path::Path, m: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap()
        };
        let dir = tempfile::tempdir().unwrap();

        let paths = Paths::at(dir.path().join("jai"));
        paths.ensure_dirs().unwrap();
        assert_eq!(mode(&paths.root), 0o700);
        assert_eq!(mode(&paths.models()), 0o700);
        chmod(&paths.root, 0o755);
        paths.ensure_dirs().unwrap();
        assert_eq!(mode(&paths.root), 0o700, "an older 0755 root is tightened");

        let shared = Paths::at(dir.path().join("shared"));
        std::fs::create_dir_all(&shared.root).unwrap();
        std::fs::write(shared.root.join("notes.txt"), b"not Studio's").unwrap();
        chmod(&shared.root, 0o775);
        let err = shared.ensure_dirs().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(!shared.models().exists(), "nothing created in it");

        let linked = Paths::at(dir.path().join("linked"));
        let elsewhere = dir.path().join("open-disk");
        std::fs::create_dir_all(&elsewhere).unwrap();
        chmod(&elsewhere, 0o777);
        std::fs::create_dir_all(&linked.root).unwrap();
        chmod(&linked.root, 0o700);
        std::os::unix::fs::symlink(&elsewhere, linked.engines()).unwrap();
        let err = linked.ensure_dirs().unwrap_err();
        assert!(err.to_string().contains("links to"), "{err}");
        chmod(&elsewhere, 0o755);
        linked.ensure_dirs().unwrap();
    }
}
