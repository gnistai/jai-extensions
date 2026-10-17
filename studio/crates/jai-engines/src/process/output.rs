//! Engine output: a ring of the last 1000 lines, and the engine's log file.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt as _, AsyncRead, BufReader};
use tracing::{trace, warn};

use crate::EngineKind;

/// Lines kept in memory.
pub(crate) const RING_LINES: usize = 1000;
/// Longer lines are split; the engines print progress dots without newlines.
const MAX_LINE_BYTES: usize = 8 * 1024;
/// A log file bigger than this at start moves to `<name>.1`.
const ROTATE_BYTES: u64 = 8 * 1024 * 1024;

/// Where an engine's output goes. Shared by the stdout and stderr readers.
pub(crate) struct LogSink {
    kind: EngineKind,
    inner: Mutex<Inner>,
}

struct Inner {
    ring: VecDeque<String>,
    /// None when the file could not be opened or a write failed.
    file: Option<File>,
}

impl LogSink {
    /// Appends to the log file, rotating it first when it is large. Without a
    /// usable file the ring still works.
    pub(crate) fn open(kind: EngineKind, path: &Path) -> LogSink {
        let file = match open_log(path) {
            Ok(file) => Some(file),
            Err(err) => {
                warn!(path = %path.display(), "cannot write the {kind} engine log: {err}");
                None
            }
        };
        LogSink {
            kind,
            inner: Mutex::new(Inner {
                ring: VecDeque::with_capacity(RING_LINES),
                file,
            }),
        }
    }

    /// One line of engine output: into the ring and the file.
    pub(crate) fn line(&self, line: String) {
        trace!(target: "jai_engines::output", engine = %self.kind, "{line}");
        let mut inner = self.lock();
        inner.write(&line);
        if inner.ring.len() == RING_LINES {
            inner.ring.pop_front();
        }
        inner.ring.push_back(line);
    }

    /// A line from Studio itself, file only: `--- <UTC time> <text>`.
    pub(crate) fn note(&self, text: &str) {
        self.lock().write(&format!("--- {} {text}", utc_now()));
    }

    /// The last `n` lines, oldest first.
    pub(crate) fn tail(&self, n: usize) -> Vec<String> {
        let inner = self.lock();
        let skip = inner.ring.len().saturating_sub(n);
        inner.ring.iter().skip(skip).cloned().collect()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Inner {
    fn write(&mut self, line: &str) {
        if let Some(file) = self.file.as_mut() {
            let mut bytes = Vec::with_capacity(line.len() + 1);
            bytes.extend_from_slice(line.as_bytes());
            bytes.push(b'\n');
            if let Err(err) = file.write_all(&bytes) {
                warn!("stopped writing an engine log: {err}");
                self.file = None;
            }
        }
    }
}

fn open_log(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    if fs::metadata(path).is_ok_and(|m| m.len() > ROTATE_BYTES) {
        let mut rotated = PathBuf::from(path).into_os_string();
        rotated.push(".1");
        if let Err(err) = fs::rename(path, &rotated) {
            warn!(path = %path.display(), "cannot rotate the engine log: {err}");
        }
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// Reads a stream line by line into the sink until it ends. Bytes that are not
/// UTF-8 become U+FFFD; a trailing CR is dropped.
pub(crate) async fn pump<R: AsyncRead + Unpin>(stream: R, sink: Arc<LogSink>) {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        let buf = match reader.fill_buf().await {
            Ok(buf) if !buf.is_empty() => buf,
            _ => break,
        };
        let newline = buf.iter().position(|&b| b == b'\n');
        let end = newline.unwrap_or(buf.len());
        let take = end.min(MAX_LINE_BYTES - line.len());
        line.extend_from_slice(&buf[..take]);
        let complete = newline.is_some() && take == end;
        reader.consume(if complete { take + 1 } else { take });
        if complete || line.len() >= MAX_LINE_BYTES {
            sink.line(decode(&line));
            line.clear();
        }
    }
    if !line.is_empty() {
        sink.line(decode(&line));
    }
}

fn decode(bytes: &[u8]) -> String {
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

/// `2026-10-17 09:30:00 UTC`
fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    utc_text(secs)
}

fn utc_text(unix_secs: u64) -> String {
    let (days, rest) = (unix_secs / 86_400, unix_secs % 86_400);
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink(dir: &Path) -> Arc<LogSink> {
        Arc::new(LogSink::open(
            EngineKind::Llm,
            &dir.join("logs").join("llm.log"),
        ))
    }

    #[tokio::test]
    async fn splits_lines_and_keeps_the_last_1000() {
        let dir = tempfile::tempdir().unwrap();
        let sink = sink(dir.path());
        let mut input = Vec::new();
        for i in 0..1200 {
            input.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        input.extend_from_slice(b"bad \xff byte\n\nno newline at the end");
        pump(&input[..], Arc::clone(&sink)).await;
        let tail = sink.tail(RING_LINES + 10);
        assert_eq!(tail.len(), RING_LINES);
        assert_eq!(tail[0], "line 203");
        assert_eq!(
            sink.tail(4),
            [
                "line 1199",
                "bad \u{fffd} byte",
                "",
                "no newline at the end"
            ]
        );
        let file = fs::read_to_string(dir.path().join("logs").join("llm.log")).unwrap();
        assert!(file.starts_with("line 0\nline 1\n"));
        assert!(file.ends_with("\n\nno newline at the end\n"));
    }

    #[tokio::test]
    async fn long_lines_are_split() {
        let dir = tempfile::tempdir().unwrap();
        let sink = sink(dir.path());
        let mut input = vec![b'.'; MAX_LINE_BYTES * 2 + 5];
        input.push(b'\n');
        pump(&input[..], Arc::clone(&sink)).await;
        let lines = sink.tail(10);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].len(), MAX_LINE_BYTES);
        assert_eq!(lines[2], ".....");
    }

    #[test]
    fn notes_go_to_the_file_only_and_big_logs_rotate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.log");
        fs::write(&path, vec![b'x'; ROTATE_BYTES as usize + 1]).unwrap();
        let sink = LogSink::open(EngineKind::Image, &path);
        sink.note("starting sd-server");
        sink.line("hello".into());
        assert_eq!(sink.tail(5), ["hello"]);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("--- "), "{text}");
        assert!(text.contains(" UTC starting sd-server\nhello\n"), "{text}");
        let rotated = fs::metadata(dir.path().join("image.log.1")).unwrap();
        assert_eq!(rotated.len(), ROTATE_BYTES + 1);
    }

    #[test]
    fn utc_dates() {
        assert_eq!(utc_text(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc_text(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(utc_text(1_792_229_405), "2026-10-17 09:30:05 UTC");
    }
}
