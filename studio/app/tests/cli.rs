//! The `jai` executable as a terminal or a pipe sees it: no panics when the
//! reader goes away, no terminal control from documents, document arguments
//! read as the Node CLI reads them, and usage errors for mistyped
//! subcommands. None of these commands needs the models.

use std::io::Write as _;
use std::process::{Command, Output, Stdio};

/// `jai` with its data folder in `home` and logging off: `$JAI_TEST_EXE`
/// (for test binaries run elsewhere, such as on Windows from WSL), else the
/// one cargo built.
fn jai(home: &std::path::Path) -> Command {
    let exe = std::env::var_os("JAI_TEST_EXE").unwrap_or_else(|| env!("CARGO_BIN_EXE_jai").into());
    let mut cmd = Command::new(exe);
    cmd.env("JAI_HOME", home).env("JAI_LOG", "off");
    cmd
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn a_closed_pipe_ends_quietly() {
    let dir = tempfile::tempdir().unwrap();
    // More output than any pipe buffer holds, so jai must write after the
    // reader has gone, as with `jai validate … | head -1`. A .jai file is one
    // gzip member of JSON.
    let doc = dir.path().join("long.jai");
    let json = format!(
        r#"{{"jai":1,"prompt":"{}"}}"#,
        "a castle at dusk, ".repeat(40_000)
    );
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(json.as_bytes()).unwrap();
    std::fs::write(&doc, gz.finish().unwrap()).unwrap();
    let mut child = jai(dir.path())
        .arg("validate")
        .arg(&doc)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    let err = text(&out.stderr);
    assert!(!err.contains("panicked"), "{err}");
    assert_eq!(out.status.code(), Some(0), "{err}");
}

#[test]
fn document_text_cannot_steer_the_terminal() {
    let dir = tempfile::tempdir().unwrap();
    // A title change, then hidden text that would also hide the diagnostics.
    let out: Output = jai(dir.path())
        .arg("validate")
        .arg("a castle \u{1b}]0;PWNED\u{7} at dusk \u{1b}[8mhidden \u{9b}2J")
        .output()
        .unwrap();
    let shown = text(&out.stdout);
    for byte in [0x1b, 0x07] {
        assert!(!out.stdout.contains(&byte), "{byte:#x} in {shown:?}");
    }
    assert!(!shown.contains('\u{9b}'), "{shown:?}");
    assert!(!text(&out.stderr).contains("panicked"));
}

#[test]
fn document_arguments_are_read_as_the_node_cli_reads_them() {
    let dir = tempfile::tempdir().unwrap();
    let validate = |arg: &str| jai(dir.path()).args(["validate", arg]).output().unwrap();
    // Prose that begins with "Data:" and other data: URLs are JAI text.
    for prose in ["Data: a bar chart of sales", "data:text/plain,A castle"] {
        let out = validate(prose);
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        assert_eq!(text(&out.stdout).lines().next(), Some(prose));
    }
    // A data:image/jai URL is read as its text body, before the .jai-file rule,
    // with the URL's own advisories.
    let out = validate("data:image/jai,photo.jai");
    assert_eq!(
        (out.status.code(), text(&out.stdout)),
        (Some(0), "photo.jai\n".into())
    );
    let out = validate("data:image/jai;base64,QSBjYXN0bGU7c3R5bGU9aW5r");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        text(&out.stdout),
        "A castle;style=ink\n  data-url-parameter\n"
    );
    // Unreadable: only the decoder's diagnostics (the empty document it
    // decoded to is not validated), exit 1.
    let out = validate("data:image/jai,%FF");
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(text(&out.stdout), "  undecodable: not-utf8\n");
    let out = validate("data:image/jai;base64");
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        text(&out.stderr)
            .contains("The document cannot be read: this data:image/jai URL carries no document"),
        "{out:?}"
    );
    // A missing .jai file is a usage error.
    let out = validate(dir.path().join("missing.jai").to_str().unwrap());
    assert_eq!(out.status.code(), Some(2), "{out:?}");
}

#[test]
fn render_refuses_an_unreadable_document_before_it_looks_for_a_server() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain.jai");
    std::fs::write(&plain, "a castle at dusk").unwrap();
    let cases = [
        (plain.to_str().unwrap(), "malformed-container (not-gzip)"),
        ("data:image/jai,%FF", "undecodable (not-utf8)"),
        ("data:image/jai;base64", "carries no document"),
    ];
    // The data folder: finding or starting a server would create it.
    let home = dir.path().join("home");
    for (arg, why) in cases {
        let out = jai(&home).args(["render", arg]).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        let err = text(&out.stderr);
        assert!(
            err.contains("The document cannot be read: ") && err.contains(why),
            "{err}"
        );
        assert!(out.stdout.is_empty(), "{out:?}");
    }
    assert!(!home.exists(), "no server was looked for");
}

#[test]
fn a_mistyped_subcommand_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let out = jai(dir.path()).arg("stauts").output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let err = text(&out.stderr);
    assert!(
        err.contains("'stauts'") && err.contains("'status'"),
        "{err}"
    );
}

#[test]
fn render_refuses_an_output_it_cannot_write() {
    let dir = tempfile::tempdir().unwrap();
    let out = jai(dir.path())
        .args(["render", "a castle at dusk", "--out"])
        .arg(dir.path().join("castle.webp"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(text(&out.stderr).contains(".png, .jpg or .jpeg"));
}

#[test]
fn licences_are_printed() {
    let dir = tempfile::tempdir().unwrap();
    let out = jai(dir.path()).arg("licenses").output().unwrap();
    assert!(out.status.success());
    assert!(!out.stdout.is_empty());
}
