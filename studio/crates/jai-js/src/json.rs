//! JSON text produced by JavaScript. `JSON.stringify` escapes lone surrogates
//! (`"\ud800"`), which serde_json rejects, so they become U+FFFD here: the
//! same repair a WebIDL USVString conversion makes.

use std::borrow::Cow;

use serde::de::DeserializeOwned;

/// Parses JSON from JavaScript, replacing escaped lone surrogates when needed.
pub(crate) fn from_js_text<T: DeserializeOwned>(text: &str) -> serde_json::Result<T> {
    match serde_json::from_str(text) {
        Ok(value) => Ok(value),
        Err(error) => match replace_lone_surrogates(text) {
            Cow::Owned(fixed) => serde_json::from_str(&fixed),
            Cow::Borrowed(_) => Err(error),
        },
    }
}

/// The four hex digits at `at` as a UTF-16 code unit.
fn hex4(bytes: &[u8], at: usize) -> Option<u16> {
    let digits = bytes.get(at..at + 4)?;
    let text = std::str::from_utf8(digits).ok()?;
    u16::from_str_radix(text, 16).ok()
}

/// Rewrites every `\uXXXX` escape of an unpaired surrogate as `�`.
fn replace_lone_surrogates(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            i += 1;
            continue;
        }
        let unit = if bytes.get(i + 1) == Some(&b'u') {
            hex4(bytes, i + 2)
        } else {
            None
        };
        let Some(unit) = unit else {
            // Any other escape: skip the backslash and the escaped byte.
            i += 2;
            continue;
        };
        let paired = (0xd800..=0xdbff).contains(&unit)
            && bytes.get(i + 6) == Some(&b'\\')
            && bytes.get(i + 7) == Some(&b'u')
            && hex4(bytes, i + 8).is_some_and(|low| (0xdc00..=0xdfff).contains(&low));
        if paired {
            i += 12;
        } else if (0xd800..=0xdfff).contains(&unit) {
            out.push_str(&text[copied..i]);
            out.push_str("\\ufffd");
            i += 6;
            copied = i;
        } else {
            i += 6;
        }
    }
    if copied == 0 {
        return Cow::Borrowed(text);
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lone_surrogates_become_replacement_characters() {
        let v: String = from_js_text(r#""a\ud800b\udc00c""#).unwrap();
        assert_eq!(v, "a\u{fffd}b\u{fffd}c");
        let v: String = from_js_text(r#""🏰 \\ud800""#).unwrap();
        assert_eq!(v, "\u{1f3f0} \\ud800");
        let v: String = from_js_text(r#""end\ud800""#).unwrap();
        assert_eq!(v, "end\u{fffd}");
    }

    #[test]
    fn invalid_json_still_fails() {
        assert!(from_js_text::<String>("\"open").is_err());
        assert!(from_js_text::<String>(r#""\ud800"#).is_err());
    }
}
