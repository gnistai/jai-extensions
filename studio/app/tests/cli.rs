//! The `jai` executable as a terminal or a pipe sees it: no panics when the
//! reader goes away, no terminal control from documents, and usage errors
//! for mistyped subcommands. None of these commands needs the models.

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
    // reader has gone, as with `jai validate … | head -1`.
    let doc = dir.path().join("long.jai");
    std::fs::write(&doc, "a castle at dusk, ".repeat(40_000)).unwrap();
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
