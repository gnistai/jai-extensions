//! Desktop integration behind the settings: start at login and the `.jai`
//! file association, plus the VC++ runtime check, the clipboard and showing a
//! file in the file manager. Windows writes the registry (`win.rs`, DESIGN.md
//! section 10); Linux writes XDG desktop entries and a MIME type, which the
//! design leaves to each OS.

// Without the GUI this module is only built for its tests.
#![cfg_attr(not(feature = "gui"), allow(dead_code))]

use std::path::Path;

/// Starts JAI Studio in the tray at login, or stops doing so.
pub fn set_start_at_login(enabled: bool) -> anyhow::Result<()> {
    #[cfg(windows)]
    return crate::win::set_start_at_login(enabled);
    #[cfg(unix)]
    return xdg::set_start_at_login(enabled);
    #[cfg(not(any(windows, unix)))]
    {
        let _ = enabled;
        anyhow::bail!("not supported on this OS")
    }
}

/// Opens `.jai` files with JAI Studio, or stops doing so.
pub fn set_file_association(enabled: bool) -> anyhow::Result<()> {
    #[cfg(windows)]
    return crate::win::set_file_association(enabled);
    #[cfg(unix)]
    return xdg::set_file_association(enabled);
    #[cfg(not(any(windows, unix)))]
    {
        let _ = enabled;
        anyhow::bail!("not supported on this OS")
    }
}

/// The VC++ runtime the engines need is installed; always true off Windows.
pub fn vc_runtime_ok() -> bool {
    #[cfg(windows)]
    return crate::win::vc_runtime_ok();
    #[cfg(not(windows))]
    true
}

/// Puts text on the clipboard.
pub fn copy_text(text: &str) -> anyhow::Result<()> {
    #[cfg(windows)]
    return crate::win::copy_text(text);
    #[cfg(unix)]
    return xdg::copy_text(text);
    #[cfg(not(any(windows, unix)))]
    {
        let _ = text;
        anyhow::bail!("no clipboard on this OS")
    }
}

/// Opens the folder in Explorer or the file manager; for a file, its folder
/// (selected, on Windows).
pub fn reveal(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        anyhow::bail!("{} does not exist", path.display());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        let mut cmd = std::process::Command::new("explorer.exe");
        if path.is_dir() {
            cmd.arg(path);
        } else {
            // Explorer parses `/select,"path"` itself; Rust's quoting would
            // put the quotes around the whole argument.
            cmd.raw_arg(format!("/select,\"{}\"", path.display()));
        }
        spawn_detached(cmd)
    }
    #[cfg(unix)]
    {
        let dir = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(path)
        };
        let mut cmd = std::process::Command::new("xdg-open");
        cmd.arg(dir);
        spawn_detached(cmd)
    }
    #[cfg(not(any(windows, unix)))]
    anyhow::bail!("cannot open a file manager on this OS")
}

/// Starts a helper program and reaps it on a thread, so it never lingers as
/// a zombie.
fn spawn_detached(mut cmd: std::process::Command) -> anyhow::Result<()> {
    use std::process::Stdio;
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot start {program}: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Linux (and other Unix) desktops: XDG autostart, a desktop entry with a
/// MIME type for `.jai`, and the clipboard tools.
#[cfg(unix)]
pub mod xdg {
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use anyhow::Context as _;

    /// The desktop entry and MIME package names (the Tauri identifier).
    pub const APP_ID: &str = "ai.gnist.jai";
    /// The MIME type of `.jai` files.
    pub const MIME: &str = "image/jai";

    fn home() -> anyhow::Result<PathBuf> {
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .context("HOME is not set")
    }

    fn config_home() -> anyhow::Result<PathBuf> {
        match std::env::var_os("XDG_CONFIG_HOME").filter(|h| !h.is_empty()) {
            Some(dir) => Ok(PathBuf::from(dir)),
            None => Ok(home()?.join(".config")),
        }
    }

    fn data_home() -> anyhow::Result<PathBuf> {
        match std::env::var_os("XDG_DATA_HOME").filter(|h| !h.is_empty()) {
            Some(dir) => Ok(PathBuf::from(dir)),
            None => Ok(home()?.join(".local").join("share")),
        }
    }

    /// One argument of a desktop entry's `Exec` key, quoted and escaped as
    /// the Desktop Entry Specification requires.
    pub fn exec_arg(arg: &str) -> String {
        let mut q = String::from("\"");
        for c in arg.chars() {
            match c {
                '"' | '`' | '$' | '\\' => {
                    q.push('\\');
                    q.push(c);
                }
                '%' => q.push_str("%%"),
                c => q.push(c),
            }
        }
        q.push('"');
        // The string-value escape applies on top of the quoting.
        q.replace('\\', "\\\\")
    }

    /// `~/.config/autostart/ai.gnist.jai.desktop`.
    pub fn autostart_entry(exe: &str) -> String {
        format!(
            "[Desktop Entry]\nType=Application\nName=JAI Studio\nComment=JAI Studio's local model server in the tray\nExec={} tray\nTerminal=false\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n",
            exec_arg(exe)
        )
    }

    /// `~/.local/share/applications/ai.gnist.jai.desktop`.
    pub fn association_entry(exe: &str) -> String {
        format!(
            "[Desktop Entry]\nType=Application\nName=JAI Studio\nComment=View and encode JAI images\nExec={} %f\nTerminal=false\nMimeType={MIME};\nCategories=Graphics;Viewer;\n",
            exec_arg(exe)
        )
    }

    /// `~/.local/share/mime/packages/ai.gnist.jai.xml`.
    pub fn mime_package() -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<mime-info xmlns=\"http://www.freedesktop.org/standards/shared-mime-info\">\n  <mime-type type=\"{MIME}\">\n    <comment>JAI image</comment>\n    <glob pattern=\"*.jai\"/>\n  </mime-type>\n</mime-info>\n"
        )
    }

    fn exe() -> anyhow::Result<String> {
        let exe = std::env::current_exe().context("cannot find the jai executable")?;
        Ok(exe.to_string_lossy().into_owned())
    }

    fn write_file(path: &Path, text: &str) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))
    }

    fn remove_file(path: &Path) -> anyhow::Result<()> {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("cannot remove {}", path.display()))
            }
            _ => Ok(()),
        }
    }

    /// Runs a desktop database tool; a missing tool is not an error.
    fn run_quietly(program: &str, args: &[&std::ffi::OsStr]) {
        let status = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Err(e) = status {
            tracing::debug!("{program} did not run: {e}");
        }
    }

    pub fn set_start_at_login(enabled: bool) -> anyhow::Result<()> {
        let path = config_home()?
            .join("autostart")
            .join(format!("{APP_ID}.desktop"));
        if enabled {
            write_file(&path, &autostart_entry(&exe()?))
        } else {
            remove_file(&path)
        }
    }

    pub fn set_file_association(enabled: bool) -> anyhow::Result<()> {
        let data = data_home()?;
        let apps = data.join("applications");
        let mime = data.join("mime");
        let entry = apps.join(format!("{APP_ID}.desktop"));
        let package = mime.join("packages").join(format!("{APP_ID}.xml"));
        if enabled {
            write_file(&package, &mime_package())?;
            write_file(&entry, &association_entry(&exe()?))?;
        } else {
            remove_file(&entry)?;
            remove_file(&package)?;
        }
        run_quietly("update-mime-database", &[mime.as_os_str()]);
        run_quietly("update-desktop-database", &[apps.as_os_str()]);
        if enabled {
            let id = format!("{APP_ID}.desktop");
            run_quietly(
                "xdg-mime",
                &["default".as_ref(), id.as_ref(), MIME.as_ref()],
            );
        }
        Ok(())
    }

    /// wl-copy (Wayland), else xclip or xsel (X11).
    pub fn copy_text(text: &str) -> anyhow::Result<()> {
        let tools: [(&str, &[&str]); 3] = [
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ];
        for (program, args) in tools {
            let Ok(mut child) = Command::new(program)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            else {
                continue;
            };
            let wrote = child
                .stdin
                .take()
                .map(|mut stdin| stdin.write_all(text.as_bytes()).is_ok())
                .unwrap_or(false);
            // xclip and xsel stay alive to serve the selection; reap later.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            if wrote {
                return Ok(());
            }
        }
        anyhow::bail!("no clipboard tool found (install wl-clipboard, xclip or xsel)")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn exec_quoting() {
            assert_eq!(exec_arg("/opt/jai/jai"), "\"/opt/jai/jai\"");
            assert_eq!(exec_arg("/home/a b/jai"), "\"/home/a b/jai\"");
            // `$` becomes `\$` (quoting), then `\\$` (string escape).
            assert_eq!(exec_arg("/a$b"), "\"/a\\\\$b\"");
            assert_eq!(exec_arg("/100%"), "\"/100%%\"");
        }

        #[test]
        fn entries() {
            let a = autostart_entry("/opt/jai/jai");
            assert!(a.starts_with("[Desktop Entry]\n"));
            assert!(a.contains("\nExec=\"/opt/jai/jai\" tray\n"));
            let e = association_entry("/opt/jai/jai");
            assert!(e.contains("\nExec=\"/opt/jai/jai\" %f\n"));
            assert!(e.contains("\nMimeType=image/jai;\n"));
            let m = mime_package();
            assert!(m.contains("<mime-type type=\"image/jai\">"));
            assert!(m.contains("<glob pattern=\"*.jai\"/>"));
        }

        #[test]
        fn autostart_file() {
            let dir = tempfile::tempdir().unwrap();
            let file = dir
                .path()
                .join("autostart")
                .join(format!("{APP_ID}.desktop"));
            write_file(&file, &autostart_entry("/x/jai")).unwrap();
            assert!(std::fs::read_to_string(&file)
                .unwrap()
                .contains("Exec=\"/x/jai\" tray"));
            remove_file(&file).unwrap();
            assert!(!file.exists());
            // Removing what is not there is fine.
            remove_file(&file).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reveal_needs_an_existing_path() {
        let e = reveal(Path::new("/definitely/not/here")).unwrap_err();
        assert!(e.to_string().contains("does not exist"));
    }

    #[test]
    fn vc_runtime_off_windows() {
        if cfg!(not(windows)) {
            assert!(vc_runtime_ok());
        }
    }
}
