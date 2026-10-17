//! Terminal output for CLI commands.
//!
//! - [`safe`] makes text from documents, models, engines and files harmless on
//!   a terminal: their control characters become visible escapes.
//! - [`outln!`] and [`errln!`] print lines through it. When the reader of a
//!   pipe goes away (`jai models status | head -1`) the process ends quietly
//!   with exit code 0, where `println!` would panic.
//! - [`Live`] holds one status line on stderr that is redrawn in place when
//!   stderr is a terminal. It only uses `\r` and spaces, so it works in every
//!   Windows console without VT mode.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex, MutexGuard};

/// Longest status line; longer text is cut so the line never wraps on an
/// 80-column terminal.
const MAX_STATUS: usize = 78;

/// A character that could steer a terminal or reorder what it shows: C0
/// controls other than tab and newline, DEL, C1 controls (some terminals read
/// U+009B as CSI), and the bidirectional embedding, override and isolate
/// characters.
fn steers_terminal(c: char) -> bool {
    matches!(
        c,
        '\u{0}'..='\u{8}'
            | '\u{b}'..='\u{1f}'
            | '\u{7f}'..='\u{9f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

/// Text from a document, a model, an engine or a file name, safe to print on
/// a terminal: every character that could move the cursor, retitle the
/// window, hide the lines after it, set the clipboard or reorder the line
/// becomes a visible escape such as `\x1b` or `\u{202e}`. Tabs and newlines
/// stay.
pub fn safe(text: &str) -> Cow<'_, str> {
    if !text.chars().any(steers_terminal) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        if !steers_terminal(c) {
            out.push(c);
        } else if (c as u32) < 0x100 {
            let _ = write!(out, "\\x{:02x}", c as u32);
        } else {
            let _ = write!(out, "\\u{{{:x}}}", c as u32);
        }
    }
    Cow::Owned(out)
}

/// JSON text with every character [`safe`] escapes written as a JSON
/// `\uXXXX` escape instead, so it stays valid JSON and is harmless on a
/// terminal. (serde_json escapes the C0 controls itself; DEL, C1 and the
/// bidirectional characters can only occur inside its strings.)
pub fn json_safe(json: &str) -> Cow<'_, str> {
    if !json.chars().any(steers_terminal) {
        return Cow::Borrowed(json);
    }
    let mut out = String::with_capacity(json.len() + 16);
    for c in json.chars() {
        if steers_terminal(c) {
            let _ = write!(out, "\\u{:04x}", c as u32);
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// Where a line goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Out,
    Err,
}

impl Stream {
    pub fn is_terminal(self) -> bool {
        match self {
            Stream::Out => io::stdout().is_terminal(),
            Stream::Err => io::stderr().is_terminal(),
        }
    }
}

/// Writes `text` as it is. A reader that went away (a closed pipe) ends the
/// process quietly with 0; there is nobody left to tell. Other errors on
/// stdout are reported on stderr and end the process with 1; other errors on
/// stderr are ignored, as there is nowhere to report them.
pub fn write_raw(to: Stream, text: &str) {
    let result = match to {
        Stream::Out => {
            let mut out = io::stdout().lock();
            out.write_all(text.as_bytes()).and_then(|()| out.flush())
        }
        Stream::Err => {
            let mut err = io::stderr().lock();
            err.write_all(text.as_bytes()).and_then(|()| err.flush())
        }
    };
    match result {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(e) if to == Stream::Out => {
            let _ = writeln!(io::stderr(), "jai: cannot write the output: {e}");
            std::process::exit(1);
        }
        Err(_) => {}
    }
}

/// A line made [`safe`], then written with [`write_raw`].
pub fn line(to: Stream, text: &str) {
    let mut s = safe(text).into_owned();
    s.push('\n');
    write_raw(to, &s);
}

/// `println!`, through [`line`]: terminal-safe, and quiet when the reader of
/// a pipe is gone.
macro_rules! outln {
    () => {
        $crate::term::line($crate::term::Stream::Out, "")
    };
    ($($arg:tt)*) => {
        $crate::term::line($crate::term::Stream::Out, &format!($($arg)*))
    };
}

/// `eprintln!`, through [`line`].
macro_rules! errln {
    () => {
        $crate::term::line($crate::term::Stream::Err, "")
    };
    ($($arg:tt)*) => {
        $crate::term::line($crate::term::Stream::Err, &format!($($arg)*))
    };
}

/// Shared by the tasks of one command; clears its status line when dropped.
/// Its lines are made [`safe`]. Write errors are ignored, so a long command
/// such as an encode keeps going (and keeps writing its files) when its
/// output is cut off.
pub struct Live {
    tty: bool,
    drawn: Mutex<Drawn>,
}

#[derive(Default)]
struct Drawn {
    text: String,
    width: usize,
}

impl Live {
    pub fn new() -> Arc<Live> {
        Arc::new(Live {
            tty: io::stderr().is_terminal(),
            drawn: Mutex::default(),
        })
    }

    /// Whether the status line is shown.
    pub fn is_tty(&self) -> bool {
        self.tty
    }

    fn lock(&self) -> MutexGuard<'_, Drawn> {
        self.drawn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A permanent line on stdout.
    pub fn out(&self, line: &str) {
        let mut d = self.lock();
        erase(&mut d);
        let mut out = io::stdout().lock();
        let _ = writeln!(out, "{}", safe(line));
        let _ = out.flush();
        redraw(&mut d);
    }

    /// A permanent line on stderr.
    pub fn err(&self, line: &str) {
        let mut d = self.lock();
        erase(&mut d);
        let mut err = io::stderr().lock();
        let _ = writeln!(err, "{}", safe(line));
        let _ = err.flush();
        drop(err);
        redraw(&mut d);
    }

    /// Replaces the status line; does nothing when stderr is not a terminal.
    pub fn status(&self, text: &str) {
        if !self.tty {
            return;
        }
        let mut d = self.lock();
        let text = fit(text);
        if d.text == text && d.width > 0 {
            return;
        }
        erase(&mut d);
        d.text = text;
        redraw(&mut d);
    }

    /// Removes the status line.
    pub fn clear(&self) {
        let mut d = self.lock();
        erase(&mut d);
        d.text.clear();
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.clear();
    }
}

/// The text made [`safe`] on one line and cut to [`MAX_STATUS`] characters,
/// ending in "…" when cut.
fn fit(text: &str) -> String {
    let text = safe(&text.replace(['\n', '\r', '\t'], " ")).into_owned();
    if text.chars().count() <= MAX_STATUS {
        return text;
    }
    let mut cut: String = text.chars().take(MAX_STATUS - 1).collect();
    cut.push('…');
    cut
}

fn erase(d: &mut Drawn) {
    if d.width > 0 {
        let mut err = io::stderr().lock();
        let _ = write!(err, "\r{}\r", " ".repeat(d.width));
        let _ = err.flush();
        d.width = 0;
    }
}

fn redraw(d: &mut Drawn) {
    if !d.text.is_empty() {
        let mut err = io::stderr().lock();
        let _ = write!(err, "{}", d.text);
        let _ = err.flush();
        d.width = d.text.chars().count();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_text_fits() {
        assert_eq!(fit("short"), "short");
        assert_eq!(fit("two\nlines"), "two lines");
        let long = "x".repeat(200);
        let f = fit(&long);
        assert_eq!(f.chars().count(), MAX_STATUS);
        assert!(f.ends_with('…'));
        assert_eq!(fit("a\u{1b}[2Jb"), "a\\x1b[2Jb");
    }

    #[test]
    fn control_characters_become_visible() {
        // Plain text, tabs and newlines are untouched (and not copied).
        assert!(matches!(safe("a castle\n\tat dusk"), Cow::Borrowed(_)));
        assert!(matches!(safe("Æ ø 城堡 🏰"), Cow::Borrowed(_)));
        // A title change (OSC 0), colours, a hidden line and a clipboard write.
        assert_eq!(
            safe("a castle \u{1b}]0;PWNED\u{7} at dusk \u{1b}[31mred\u{1b}[0m \u{1b}[8mhidden"),
            "a castle \\x1b]0;PWNED\\x07 at dusk \\x1b[31mred\\x1b[0m \\x1b[8mhidden"
        );
        assert_eq!(
            safe("\u{1b}]52;c;cm0gLXJmIH4=\u{7}"),
            "\\x1b]52;c;cm0gLXJmIH4=\\x07"
        );
        // Carriage return, backspace, DEL and the 8-bit CSI.
        assert_eq!(
            safe("ok\rfail\u{8}\u{7f}\u{9b}2J"),
            "ok\\x0dfail\\x08\\x7f\\x9b2J"
        );
        // Bidirectional overrides and isolates.
        assert_eq!(
            safe("abc\u{202e}fed\u{2066}x\u{2069}"),
            "abc\\u{202e}fed\\u{2066}x\\u{2069}"
        );
        // No escape survives.
        let s = safe("\u{0}\u{1b}\u{9d}\u{85}");
        assert!(!s.chars().any(steers_terminal), "{s}");
    }

    #[test]
    fn json_stays_valid() {
        let v = serde_json::json!({"content": "a\u{1b}[31m b\u{9b}2J c\u{202e}d e\u{7f}"});
        let text = serde_json::to_string(&v).unwrap();
        let shown = json_safe(&text);
        assert!(!shown.chars().any(steers_terminal), "{shown}");
        let back: serde_json::Value = serde_json::from_str(&shown).unwrap();
        assert_eq!(back, v);
        assert!(matches!(json_safe("{\"a\":1}"), Cow::Borrowed(_)));
    }
}
