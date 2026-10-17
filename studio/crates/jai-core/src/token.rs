//! The server's bearer token in `<root>/token` (DESIGN.md section 2).
//!
//! The token is `"jai_" + base64url(32 random bytes)` from the OS random
//! number generator. On Unix the file mode is 0600; on Windows the file
//! inherits the data root's private access list (see `Paths::ensure_dirs`),
//! wherever `JAI_HOME` puts the root. The file holds the token alone, without
//! a newline.

use std::fs;
use std::io;
use std::path::Path;

use anyhow::Context as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand::rngs::OsRng;
use rand::TryRngCore as _;
use tracing::warn;

use crate::fsutil;
use crate::paths::Paths;

const PREFIX: &str = "jai_";
/// Unix permissions of the token file: read and write for the user only.
const MODE: u32 = 0o600;

/// Reads the token. On first use, creates `"jai_" + base64url(32 random bytes)`
/// with user-only permissions. A file that does not hold a usable token (16 to
/// 512 printable ASCII characters) is replaced with a new token.
pub fn load_or_create_token(paths: &Paths) -> anyhow::Result<String> {
    let file = paths.token_file();
    match read(&file)? {
        Stored::Token(token) => {
            restrict(&file);
            Ok(token)
        }
        Stored::Missing => create(&file),
        Stored::Unusable => {
            warn!(path = %file.display(), "the token file holds no usable token; writing a new one");
            replace(&file)
        }
    }
}

/// Replaces the token with a new random one and returns it.
pub fn regenerate_token(paths: &Paths) -> anyhow::Result<String> {
    replace(&paths.token_file())
}

enum Stored {
    Missing,
    Unusable,
    Token(String),
}

fn read(file: &Path) -> anyhow::Result<Stored> {
    match fs::read(file) {
        Ok(bytes) => Ok(match std::str::from_utf8(&bytes).map(str::trim) {
            Ok(token) if usable(token) => Stored::Token(token.to_owned()),
            _ => Stored::Unusable,
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Stored::Missing),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", file.display())),
    }
}

fn usable(token: &str) -> bool {
    (16..=512).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_graphic())
}

fn generate() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|e| anyhow::anyhow!("the system random number generator failed: {e}"))?;
    Ok(format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)))
}

/// First creation. The token is written to a temporary file and hard-linked
/// into place, which fails if another process created the token first; then
/// that token wins, so two processes never hand out different tokens. Where
/// hard links are not supported, a rename is used instead.
fn create(file: &Path) -> anyhow::Result<String> {
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let token = generate()?;
    let tmp = fsutil::temp_sibling(file);
    fsutil::write_new(&tmp, token.as_bytes(), Some(MODE))
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    match fs::hard_link(&tmp, file) {
        Ok(()) => {
            let _ = fs::remove_file(&tmp);
            Ok(token)
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&tmp);
            match read(file)? {
                Stored::Token(theirs) => Ok(theirs),
                _ => replace(file),
            }
        }
        Err(_) => match fs::rename(&tmp, file) {
            Ok(()) => Ok(token),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e).with_context(|| format!("cannot write {}", file.display()))
            }
        },
    }
}

/// Writes a new token over the file atomically.
fn replace(file: &Path) -> anyhow::Result<String> {
    let token = generate()?;
    fsutil::write_atomic(file, token.as_bytes(), Some(MODE))
        .with_context(|| format!("cannot write {}", file.display()))?;
    Ok(token)
}

/// Removes group and other permissions from an existing token file (Unix).
fn restrict(file: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(file) {
            if meta.permissions().mode() & 0o077 != 0 {
                if let Err(e) = fs::set_permissions(file, fs::Permissions::from_mode(MODE)) {
                    warn!(path = %file.display(), error = %e, "cannot restrict the token file permissions");
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = file;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_token(t: &str) -> bool {
        let body = t.strip_prefix("jai_").unwrap_or("");
        body.len() == 43
            && body
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    }

    #[test]
    fn created_once_then_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("root"));
        let first = load_or_create_token(&paths).unwrap();
        assert!(is_token(&first), "{first}");
        assert_eq!(load_or_create_token(&paths).unwrap(), first);
        assert_eq!(fs::read_to_string(paths.token_file()).unwrap(), first);
        let names: Vec<_> = fs::read_dir(&paths.root).unwrap().collect();
        assert_eq!(names.len(), 1, "no temporary files are left behind");
    }

    #[test]
    fn regenerate_replaces_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        let first = load_or_create_token(&paths).unwrap();
        let second = regenerate_token(&paths).unwrap();
        assert!(is_token(&second));
        assert_ne!(first, second);
        assert_eq!(load_or_create_token(&paths).unwrap(), second);
    }

    #[test]
    fn unusable_or_custom_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        fs::write(paths.token_file(), "  \n").unwrap();
        let token = load_or_create_token(&paths).unwrap();
        assert!(is_token(&token));

        fs::write(paths.token_file(), "my-own-long-secret-token\n").unwrap();
        assert_eq!(
            load_or_create_token(&paths).unwrap(),
            "my-own-long-secret-token"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_user_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        load_or_create_token(&paths).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&paths.token_file()), 0o600);
        regenerate_token(&paths).unwrap();
        assert_eq!(mode(&paths.token_file()), 0o600);

        fs::set_permissions(paths.token_file(), fs::Permissions::from_mode(0o644)).unwrap();
        load_or_create_token(&paths).unwrap();
        assert_eq!(
            mode(&paths.token_file()),
            0o600,
            "loose permissions are fixed"
        );
    }
}
