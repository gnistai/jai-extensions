//! The registry values JAI writes on Windows (DESIGN.md section 10, "Windows
//! integration"), as plain data so they are tested on every OS. `win.rs`
//! writes them.

// Off Windows this module is only built for its tests.
#![cfg_attr(not(all(windows, feature = "gui")), allow(dead_code))]

/// Per-user file classes, merged into HKEY_CLASSES_ROOT.
pub const CLASSES: &str = r"Software\Classes";
/// The ProgID of `.jai` files.
pub const PROG_ID: &str = "JAI.Document";
/// Per-user programs started at login.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// The value name under [`RUN_KEY`].
pub const RUN_NAME: &str = "JAI";
/// The AppUserModelID JAI Studio's notifications are shown under: the Tauri
/// identifier, which tauri-plugin-notification uses for an exe outside
/// `target\debug` and `target\release`.
pub const APP_ID: &str = "ai.gnist.jai";
/// Where unpackaged programs register an AppUserModelID.
pub const APP_ID_KEY: &str = r"Software\Classes\AppUserModelId";
/// The name notifications carry.
pub const APP_NAME: &str = "JAI Studio";

/// One string value under HKEY_CURRENT_USER. An empty name is the key's
/// (Default) value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegValue {
    pub key: String,
    pub name: String,
    pub data: String,
}

fn value(key: String, name: &str, data: String) -> RegValue {
    RegValue {
        key,
        name: name.to_string(),
        data,
    }
}

/// The file association for `exe` (a full path), in the order to write it.
pub fn association_values(exe: &str) -> Vec<RegValue> {
    let ext = format!(r"{CLASSES}\.jai");
    let prog = format!(r"{CLASSES}\{PROG_ID}");
    vec![
        value(ext.clone(), "", PROG_ID.to_string()),
        value(format!(r"{ext}\OpenWithProgids"), PROG_ID, String::new()),
        value(prog.clone(), "", "JAI image".to_string()),
        value(format!(r"{prog}\DefaultIcon"), "", format!("\"{exe}\",0")),
        value(
            format!(r"{prog}\shell\open\command"),
            "",
            format!("\"{exe}\" \"%1\""),
        ),
    ]
}

/// The notification identity, with `icon` (a full path to a PNG).
pub fn app_id_values(icon: &str) -> Vec<RegValue> {
    let key = format!(r"{APP_ID_KEY}\{APP_ID}");
    vec![
        value(key.clone(), "DisplayName", APP_NAME.to_string()),
        value(key, "IconUri", icon.to_string()),
    ]
}

/// The Run value: the exe in quotes, then `tray`.
pub fn run_value(exe: &str) -> String {
    format!("\"{exe}\" tray")
}

/// The VC++ runtime is new enough: msvcp140.dll's file version, whose high
/// word is the major and low word the minor version, is at least 14.40.
pub fn vc_version_ok(file_version_ms: u32) -> bool {
    let major = file_version_ms >> 16;
    let minor = file_version_ms & 0xffff;
    (major, minor) >= (14, 40)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn association() {
        let v = association_values(r"C:\Users\a b\JAI\jai.exe");
        let pairs: Vec<(&str, &str, &str)> = v
            .iter()
            .map(|r| (r.key.as_str(), r.name.as_str(), r.data.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                (r"Software\Classes\.jai", "", "JAI.Document"),
                (r"Software\Classes\.jai\OpenWithProgids", "JAI.Document", ""),
                (r"Software\Classes\JAI.Document", "", "JAI image"),
                (
                    r"Software\Classes\JAI.Document\DefaultIcon",
                    "",
                    r#""C:\Users\a b\JAI\jai.exe",0"#
                ),
                (
                    r"Software\Classes\JAI.Document\shell\open\command",
                    "",
                    r#""C:\Users\a b\JAI\jai.exe" "%1""#
                ),
            ]
        );
    }

    #[test]
    fn notification_identity() {
        let v = app_id_values(r"C:\Users\a\AppData\Local\JAI\cache\notification-icon.png");
        assert_eq!(v.len(), 2);
        assert!(v
            .iter()
            .all(|r| r.key == r"Software\Classes\AppUserModelId\ai.gnist.jai"));
        assert_eq!(
            (v[0].name.as_str(), v[0].data.as_str()),
            ("DisplayName", "JAI Studio")
        );
        assert_eq!(v[1].name, "IconUri");
        // The plugin shows toasts under the Tauri identifier.
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(conf["identifier"], APP_ID);
    }

    #[test]
    fn run_at_login() {
        assert_eq!(run_value(r"C:\Tools\jai.exe"), r#""C:\Tools\jai.exe" tray"#);
        assert_eq!(RUN_KEY, r"Software\Microsoft\Windows\CurrentVersion\Run");
    }

    #[test]
    fn vc_runtime_versions() {
        let v = |major: u32, minor: u32| (major << 16) | minor;
        assert!(vc_version_ok(v(14, 40)));
        assert!(vc_version_ok(v(14, 44)));
        assert!(vc_version_ok(v(15, 0)));
        assert!(!vc_version_ok(v(14, 38)));
        assert!(!vc_version_ok(v(12, 99)));
    }
}
