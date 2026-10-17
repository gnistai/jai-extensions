use std::fs;
use std::path::Path;

use super::*;

#[test]
fn a_new_root_is_private() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("a").join("jai");
    ensure_private_root(&root).unwrap();
    assert!(root.is_dir());
    assert_eq!(sys::inspect(&root, Check::Any).unwrap(), Access::Private);
    // Idempotent.
    ensure_private_root(&root).unwrap();
}

#[test]
fn a_file_is_not_a_root() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("jai");
    fs::write(&file, b"x").unwrap();
    assert!(ensure_private_root(&file).is_err());
}

#[test]
fn studio_entries() {
    for name in [
        "config.json",
        "token",
        "models",
        "cache",
        ".token.12-0.tmp",
        "config.json.invalid-1792229405",
    ] {
        assert!(studio_entry(name), "{name}");
    }
    for name in ["notes.txt", "Documents", "tokens", ".bashrc"] {
        assert!(!studio_entry(name), "{name}");
    }
}

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn new_roots_and_folders_are_0700() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("jai");
        ensure_private_root(&root).unwrap();
        assert_eq!(mode(&root), 0o700);
        create_dir(&root.join("cache").join("renders")).unwrap();
        assert_eq!(mode(&root.join("cache")), 0o700);
        assert_eq!(mode(&root.join("cache").join("renders")), 0o700);
    }

    /// A root an older version made 0755 is tightened, and a token that
    /// others could read is dropped so a new one is made.
    #[test]
    fn a_shared_studio_root_is_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("jai");
        fs::create_dir_all(root.join("models")).unwrap();
        fs::write(root.join("config.json"), b"{}").unwrap();
        fs::write(root.join("token"), b"jai_old_token_value").unwrap();
        chmod(&root, 0o755);
        chmod(&root.join("token"), 0o644);
        ensure_private_root(&root).unwrap();
        assert_eq!(mode(&root), 0o700);
        assert!(!root.join("token").exists(), "the readable token is gone");
        assert!(root.join("config.json").exists());

        // A private token in a shared root stays.
        chmod(&root, 0o755);
        fs::write(root.join("token"), b"jai_kept_token_value").unwrap();
        chmod(&root.join("token"), 0o600);
        ensure_private_root(&root).unwrap();
        assert_eq!(mode(&root), 0o700);
        assert!(root.join("token").exists());
    }

    /// JAI_HOME pointing at a folder with other things in it: refused, and
    /// left as it was.
    #[test]
    fn a_shared_foreign_folder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("home");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("notes.txt"), b"mine").unwrap();
        chmod(&root, 0o755);
        let err = ensure_private_root(&root).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let text = err.to_string();
        assert!(text.contains("JAI_HOME"), "{text}");
        assert!(text.contains("0755"), "{text}");
        assert_eq!(mode(&root), 0o755, "untouched");
        // Private, it is fine whatever it holds.
        chmod(&root, 0o700);
        ensure_private_root(&root).unwrap();
    }

    #[test]
    fn linked_folders_must_not_be_writable_by_others() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("jai");
        ensure_private_root(&root).unwrap();
        let elsewhere = dir.path().join("big-disk").join("models");
        fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("models")).unwrap();

        chmod(&elsewhere, 0o755);
        check_linked(&root, &root.join("models")).expect("others can only read it");
        chmod(&elsewhere, 0o777);
        let err = check_linked(&root, &root.join("models")).unwrap_err();
        assert!(err.to_string().contains("links to"), "{err}");
        // A folder inside the root is never checked this way.
        create_dir(&root.join("engines")).unwrap();
        chmod(&root.join("engines"), 0o777);
        check_linked(&root, &root.join("engines")).unwrap();
        // Missing folders are fine.
        check_linked(&root, &root.join("downloads")).unwrap();
    }

    #[test]
    fn files_others_can_write_are_not_private() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("m.gguf");
        fs::write(&file, b"weights").unwrap();
        chmod(&file, 0o644);
        assert!(only_user_can_write(&file));
        chmod(&file, 0o664);
        assert!(!only_user_can_write(&file));
        chmod(&file, 0o646);
        assert!(!only_user_can_write(&file));
        assert!(!only_user_can_write(&dir.path().join("missing")));
    }
}

#[cfg(windows)]
mod windows {
    use std::os::windows::ffi::OsStrExt as _;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorDacl, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR,
    };

    use super::*;

    /// Gives Everyone full control of `path`, as a shared drive would.
    fn share_with_everyone(path: &Path) {
        let sddl: Vec<u16> = "D:P(A;OICI;FA;;;WD)(A;OICI;FA;;;SY)"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: valid strings and out pointers; the descriptor is leaked,
        // which is fine in a test.
        unsafe {
            let mut sd: PSECURITY_DESCRIPTOR = null_mut();
            assert_ne!(
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut sd,
                    null_mut()
                ),
                0
            );
            let (mut present, mut defaulted) = (0, 0);
            let mut dacl = null_mut();
            assert_ne!(
                GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted),
                0
            );
            let err = SetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                dacl,
                null(),
            );
            assert_eq!(err, 0);
        }
    }

    #[test]
    fn a_shared_studio_root_is_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("jai");
        fs::create_dir_all(root.join("models")).unwrap();
        fs::write(root.join("token"), b"jai_old_token_value").unwrap();
        share_with_everyone(&root);
        // The token inherits Everyone from the root.
        share_with_everyone(&root.join("token"));
        match sys::inspect(&root, Check::Any).unwrap() {
            Access::Shared(reason) => assert!(reason.contains("Everyone"), "{reason}"),
            Access::Private => panic!("Everyone can open it"),
        }
        assert!(!only_user_can_write(&root.join("token")));
        ensure_private_root(&root).unwrap();
        assert_eq!(sys::inspect(&root, Check::Any).unwrap(), Access::Private);
        assert_eq!(
            sys::inspect(&root.join("models"), Check::Any).unwrap(),
            Access::Private,
            "inherited by what is inside"
        );
        assert!(!root.join("token").exists(), "the readable token is gone");
    }

    #[test]
    fn a_shared_foreign_folder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("notes.txt"), b"mine").unwrap();
        share_with_everyone(&root);
        let err = ensure_private_root(&root).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("Everyone"), "{err}");
        assert!(matches!(
            sys::inspect(&root, Check::Any).unwrap(),
            Access::Shared(_)
        ));
    }

    #[test]
    fn new_roots_grant_only_the_user_system_and_administrators() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("jai");
        ensure_private_root(&root).unwrap();
        create_dir(&root.join("engines")).unwrap();
        fs::write(root.join("engines").join("llama-server.exe"), b"").unwrap();
        assert_eq!(sys::inspect(&root, Check::Any).unwrap(), Access::Private);
        assert!(only_user_can_write(
            &root.join("engines").join("llama-server.exe")
        ));
    }
}
