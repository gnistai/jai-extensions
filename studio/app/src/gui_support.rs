//! The pure parts of the GUI commands, compiled without Tauri so they are
//! unit-tested on every OS: data URLs, what `save_file` writes, dialog
//! filters, JPEG sizes, the identity of a document for the render cache,
//! what a second launch asks for and when it may act, the tail of a log
//! file, and how often download progress reaches the UI.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _};
use base64::Engine as _;
use clap::Parser as _;
use jai_core::download::DlState;
use jai_js::ValidateResult;
use sha2::{Digest, Sha256};

use crate::{Cli, Command};

/// `data:<mime>;base64,<bytes>`.
pub fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

/// The bytes of a base64 `data:` URL.
pub fn data_url_bytes(url: &str) -> anyhow::Result<Vec<u8>> {
    let rest = url
        .strip_prefix("data:")
        .ok_or_else(|| anyhow!("not a data: URL"))?;
    let (meta, data) = rest
        .split_once(',')
        .ok_or_else(|| anyhow!("a data: URL without data"))?;
    if !meta.ends_with(";base64") {
        return Err(anyhow!("only base64 data: URLs are supported"));
    }
    base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .context("the data: URL is not valid base64")
}

/// What `save_file` writes: exactly one of a data URL (a rendered image) or
/// base64 bytes (a `.jai` file).
pub fn save_payload(data_url: Option<&str>, bytes_b64: Option<&str>) -> anyhow::Result<Vec<u8>> {
    match (data_url, bytes_b64) {
        (Some(url), None) => data_url_bytes(url),
        (None, Some(b64)) => base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .context("bytesB64 is not valid base64"),
        _ => Err(anyhow!("give exactly one of dataUrl and bytesB64")),
    }
}

/// The save dialog's filter for a suggested file name.
pub fn save_filter(name: &str) -> Option<(&'static str, &'static [&'static str])> {
    let ext = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "jai" => ("JAI image", &["jai"]),
        "png" => ("PNG image", &["png"]),
        "jpg" | "jpeg" => ("JPEG image", &["jpg", "jpeg"]),
        _ => return None,
    })
}

/// The open dialog's filter: `jai` or `image`.
pub fn open_filter(image: bool) -> (&'static str, &'static [&'static str]) {
    if image {
        ("Images", &["png", "jpg", "jpeg", "webp", "gif"])
    } else {
        ("JAI images", &["jai"])
    }
}

/// Width and height from a JPEG's frame header.
pub fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.get(..2)? != [0xff, 0xd8] {
        return None;
    }
    let mut i = 2;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xff {
            return None;
        }
        let marker = bytes[i + 1];
        if marker == 0xff {
            i += 1;
            continue;
        }
        // Markers without a length.
        if marker == 0xd8 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            i += 2;
            continue;
        }
        let len = usize::from(u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]));
        let is_frame = matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc);
        if is_frame {
            let seg = bytes.get(i + 4..i + 2 + len)?;
            let height = u32::from(u16::from_be_bytes([*seg.get(1)?, *seg.get(2)?]));
            let width = u32::from(u16::from_be_bytes([*seg.get(3)?, *seg.get(4)?]));
            return Some((width, height));
        }
        i += 2 + len;
    }
    None
}

/// What the render cache keys a `.jai` file by: its canonical text, else a
/// hash of its bytes.
pub fn input_identity(validated: &ValidateResult, bytes: &[u8]) -> String {
    if let Some(text) = validated.text.as_ref().filter(|t| !t.is_empty()) {
        return text.clone();
    }
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("raw:{hex}")
}

/// What a second launch asks the running instance for.
#[derive(Debug, PartialEq, Eq)]
pub enum Forwarded {
    /// Show the window.
    Show,
    /// Show the window and open this document.
    Open(PathBuf),
    /// `jai tray`: already running, nothing to do.
    Tray,
}

/// Reads the arguments a second launch passes (its argv, starting with the
/// exe) against its working directory.
pub fn forwarded(args: &[String], cwd: &Path) -> Forwarded {
    let Ok(cli) = Cli::try_parse_from(args) else {
        return Forwarded::Show;
    };
    let path = match cli.command {
        Some(Command::Tray) => return Forwarded::Tray,
        Some(Command::Open { path }) => Some(path),
        None => cli.path,
        Some(_) => None,
    };
    match path {
        Some(p) if p.is_absolute() => Forwarded::Open(p),
        Some(p) => Forwarded::Open(cwd.join(p)),
        None => Forwarded::Show,
    }
}

/// Holds second launches (and the first launch's own document) until the
/// app can act on them.
///
/// tauri-plugin-single-instance calls back from its window procedure, which
/// runs whenever the main thread pumps messages, even inside `setup` while
/// the tray or the webview is being created. Touching windows then would
/// re-enter window creation, so until setup has finished a launch is only
/// recorded. Documents also wait for the page: Tauri does not keep events
/// for listeners that are not registered yet, so they are sent once the page
/// says it listens (`ui_ready`), or, for a page that never says so, a moment
/// after it has loaded.
#[derive(Debug, Default)]
pub struct LaunchGate(Mutex<Gate>);

#[derive(Debug, Default)]
struct Gate {
    setup_done: bool,
    page_ready: bool,
    /// Counts page loads, so a late fallback for an old load does nothing.
    load: u64,
    /// A launch during setup asked for the window.
    show: bool,
    /// Documents to open, oldest first.
    paths: Vec<PathBuf>,
}

/// What to do now about a launch.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LaunchAction {
    /// Show and focus the window.
    pub show: bool,
    /// Send these documents to the page.
    pub open: Vec<PathBuf>,
}

impl LaunchGate {
    /// A gate holding the first launch's own document, if any.
    pub fn new(open: Option<PathBuf>) -> LaunchGate {
        LaunchGate(Mutex::new(Gate {
            paths: open.into_iter().collect(),
            ..Gate::default()
        }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Gate> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A second launch: what may happen now. The rest waits for
    /// [`setup_finished`](Self::setup_finished) and
    /// [`page_ready`](Self::page_ready).
    pub fn launch(&self, f: Forwarded) -> LaunchAction {
        let mut g = self.lock();
        let show = match f {
            Forwarded::Tray => false,
            Forwarded::Show => true,
            Forwarded::Open(path) => {
                g.paths.push(path);
                true
            }
        };
        if !g.setup_done {
            g.show |= show;
            return LaunchAction::default();
        }
        let open = if g.page_ready {
            std::mem::take(&mut g.paths)
        } else {
            Vec::new()
        };
        LaunchAction { show, open }
    }

    /// Whether setup has finished, so windows may be created.
    pub fn is_setup_done(&self) -> bool {
        self.lock().setup_done
    }

    /// Setup has finished (the window exists): whether a launch during it
    /// asked for the window.
    pub fn setup_finished(&self) -> bool {
        let mut g = self.lock();
        g.setup_done = true;
        std::mem::take(&mut g.show)
    }

    /// A page load started: documents wait until that page listens. Returns
    /// the load's number for [`page_loaded`](Self::page_loaded).
    pub fn page_loading(&self) -> u64 {
        let mut g = self.lock();
        g.page_ready = false;
        g.load += 1;
        g.load
    }

    /// The number of the current page load, for [`page_loaded`](Self::page_loaded).
    pub fn current_load(&self) -> u64 {
        self.lock().load
    }

    /// The page listens for `open-path`: the documents to send it.
    pub fn page_ready(&self) -> Vec<PathBuf> {
        let mut g = self.lock();
        g.page_ready = true;
        std::mem::take(&mut g.paths)
    }

    /// The fallback a while after load `load` finished, for a page that does
    /// not call `ui_ready`: the documents to send, unless the page already
    /// said it listens or another load started since.
    pub fn page_loaded(&self, load: u64) -> Vec<PathBuf> {
        let mut g = self.lock();
        if g.page_ready || g.load != load {
            return Vec::new();
        }
        g.page_ready = true;
        std::mem::take(&mut g.paths)
    }
}

/// The last `n` lines of a text file, read from its last 1 MB; an empty list
/// when the file does not exist.
pub fn tail_lines(path: &Path, n: usize) -> std::io::Result<Vec<String>> {
    const WINDOW: u64 = 1 << 20;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    let start = len.saturating_sub(WINDOW);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(WINDOW).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.lines().collect();
    // A window that starts mid-file starts mid-line.
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    let skip = lines.len().saturating_sub(n);
    Ok(lines[skip..].iter().map(|l| l.to_string()).collect())
}

/// Whether a tray icon can be shown. On Linux, Tauri's tray loads
/// libayatana-appindicator3 (or libappindicator3) at run time and panics
/// without it, so the app checks first.
pub fn tray_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        [c"libayatana-appindicator3.so.1", c"libappindicator3.so.1"]
            .iter()
            .any(|name| {
                // SAFETY: dlopen with a valid C string; the handle is closed
                // at once.
                unsafe {
                    let handle = libc::dlopen(name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
                    if handle.is_null() {
                        false
                    } else {
                        libc::dlclose(handle);
                        true
                    }
                }
            })
    }
    #[cfg(not(target_os = "linux"))]
    true
}

/// Thins out download progress for the UI: an update passes when its
/// component changed state, or `every` has passed since its last update.
pub struct ProgressThrottle {
    every: Duration,
    last: HashMap<String, (Instant, DlState)>,
}

impl ProgressThrottle {
    pub fn new(every: Duration) -> ProgressThrottle {
        ProgressThrottle {
            every,
            last: HashMap::new(),
        }
    }

    pub fn pass(&mut self, id: &str, state: DlState, now: Instant) -> bool {
        match self.last.get(id) {
            Some((at, s)) if *s == state && now.duration_since(*at) < self.every => false,
            _ => {
                self.last.insert(id.to_string(), (now, state));
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_urls() {
        let url = data_url("image/png", b"\x89PNG");
        assert_eq!(url, "data:image/png;base64,iVBORw==");
        assert_eq!(data_url_bytes(&url).unwrap(), b"\x89PNG");
        assert!(data_url_bytes("data:text/plain,hello").is_err());
        assert!(data_url_bytes("https://x/y.png").is_err());
        assert!(data_url_bytes("data:image/png;base64").is_err());
    }

    #[test]
    fn payloads() {
        assert_eq!(
            save_payload(Some("data:image/png;base64,AAE="), None).unwrap(),
            [0, 1]
        );
        assert_eq!(save_payload(None, Some("AAE=")).unwrap(), [0, 1]);
        assert!(save_payload(None, None).is_err());
        assert!(save_payload(Some("data:x;base64,AA=="), Some("AA==")).is_err());
    }

    #[test]
    fn filters() {
        assert_eq!(save_filter("castle.jai"), Some(("JAI image", &["jai"][..])));
        assert_eq!(save_filter("castle.JPG").map(|f| f.0), Some("JPEG image"));
        assert_eq!(save_filter("castle.png").map(|f| f.0), Some("PNG image"));
        assert_eq!(save_filter("castle.html"), None);
        assert_eq!(save_filter("castle"), None);
        assert_eq!(save_filter("castle.xyz"), None);
        assert!(open_filter(true).1.contains(&"webp"));
        assert_eq!(open_filter(false), ("JAI images", &["jai"][..]));
    }

    #[test]
    fn jpeg_sizes() {
        // SOI, APP0 (length 16), SOF0 for 1344x768, EOI.
        let mut j = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10];
        j.extend_from_slice(&[0; 14]);
        j.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08, 0x03, 0x00, 0x05, 0x40, 0x03]);
        j.extend_from_slice(&[0; 9]);
        j.extend_from_slice(&[0xff, 0xd9]);
        assert_eq!(jpeg_size(&j), Some((1344, 768)));
        assert_eq!(jpeg_size(b"\x89PNG\r\n\x1a\n"), None);
        assert_eq!(jpeg_size(&[0xff, 0xd8, 0xff, 0xc0, 0x00]), None);
    }

    fn validated(text: Option<&str>) -> ValidateResult {
        ValidateResult {
            ok: true,
            fatal: false,
            text: text.map(str::to_string),
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn identities() {
        let bytes = b"\x1f\x8b a castle";
        assert_eq!(
            input_identity(&validated(Some("a castle\n")), bytes),
            "a castle\n"
        );
        let raw = input_identity(&validated(None), bytes);
        assert!(raw.starts_with("raw:") && raw.len() == 68);
        assert_eq!(raw, input_identity(&validated(Some("")), bytes));
        assert_ne!(raw, input_identity(&validated(None), b"x"));
    }

    fn args(a: &[&str]) -> Vec<String> {
        std::iter::once("C:\\JAI\\jai.exe")
            .chain(a.iter().copied())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn second_launches() {
        let cwd = Path::new("/home/a");
        assert_eq!(forwarded(&args(&[]), cwd), Forwarded::Show);
        assert_eq!(forwarded(&args(&["--gui-child"]), cwd), Forwarded::Show);
        assert_eq!(
            forwarded(&args(&["castle.jai"]), cwd),
            Forwarded::Open(PathBuf::from("/home/a/castle.jai"))
        );
        assert_eq!(
            forwarded(&args(&["--gui-child", "open", "/tmp/x.jai"]), cwd),
            Forwarded::Open(PathBuf::from("/tmp/x.jai"))
        );
        assert_eq!(
            forwarded(&args(&["--gui-child", "tray"]), cwd),
            Forwarded::Tray
        );
        assert_eq!(forwarded(&args(&["--no-such-flag"]), cwd), Forwarded::Show);
    }

    #[test]
    fn launches_wait_for_setup_and_the_page() {
        let doc = |name: &str| PathBuf::from(format!("/docs/{name}.jai"));
        // The first launch's own document, then two Explorer launches during
        // setup: nothing happens until setup has finished.
        let gate = LaunchGate::new(Some(doc("first")));
        assert_eq!(
            gate.launch(Forwarded::Open(doc("a"))),
            LaunchAction::default()
        );
        assert_eq!(gate.launch(Forwarded::Tray), LaunchAction::default());
        assert_eq!(
            gate.launch(Forwarded::Open(doc("b"))),
            LaunchAction::default()
        );
        // Then the window is shown once; the documents wait for the page.
        assert!(!gate.is_setup_done());
        assert!(gate.setup_finished());
        assert!(gate.is_setup_done());
        let load = gate.page_loading();
        assert_eq!(gate.current_load(), load);
        assert_eq!(
            gate.launch(Forwarded::Show),
            LaunchAction {
                show: true,
                open: vec![]
            }
        );
        // The page listens: every document, in order, once.
        assert_eq!(gate.page_ready(), [doc("first"), doc("a"), doc("b")]);
        assert!(gate.page_loaded(load).is_empty());
        // From now on a launch acts at once.
        assert_eq!(
            gate.launch(Forwarded::Open(doc("c"))),
            LaunchAction {
                show: true,
                open: vec![doc("c")]
            }
        );
        assert_eq!(gate.launch(Forwarded::Tray), LaunchAction::default());
        // A reload holds documents again until the new page listens.
        let reload = gate.page_loading();
        assert_eq!(
            gate.launch(Forwarded::Open(doc("d"))),
            LaunchAction {
                show: true,
                open: vec![]
            }
        );
        // A fallback for the old load does nothing; the new one delivers.
        assert!(gate.page_loaded(load).is_empty());
        assert_eq!(gate.page_loaded(reload), [doc("d")]);
        assert!(gate.page_ready().is_empty());
    }

    #[test]
    fn tray_mode_shows_nothing_unless_asked() {
        let gate = LaunchGate::new(None);
        assert_eq!(gate.launch(Forwarded::Tray), LaunchAction::default());
        assert!(!gate.setup_finished());
        assert!(gate.page_ready().is_empty());
    }

    #[test]
    fn log_tails() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("llm.log");
        assert!(tail_lines(&file, 10).unwrap().is_empty());
        std::fs::write(&file, "one\ntwo\r\nthree\n").unwrap();
        assert_eq!(tail_lines(&file, 2).unwrap(), ["two", "three"]);
        assert_eq!(tail_lines(&file, 10).unwrap(), ["one", "two", "three"]);
        // A long file: only whole lines from its end.
        let long: String = (0..200_000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&file, long).unwrap();
        let tail = tail_lines(&file, 3).unwrap();
        assert_eq!(tail, ["line 199997", "line 199998", "line 199999"]);
        let many = tail_lines(&file, 1_000_000).unwrap();
        assert!(
            many.len() < 200_000 && many[0].starts_with("line "),
            "{}",
            many[0]
        );
    }

    #[test]
    fn tray_probe() {
        // Whatever this machine has, the probe answers without loading a tray.
        let available = tray_available();
        if cfg!(not(target_os = "linux")) {
            assert!(available);
        }
    }

    #[test]
    fn throttle() {
        let mut t = ProgressThrottle::new(Duration::from_millis(100));
        let t0 = Instant::now();
        assert!(t.pass("a", DlState::Downloading, t0));
        assert!(!t.pass("a", DlState::Downloading, t0 + Duration::from_millis(50)));
        assert!(t.pass("b", DlState::Downloading, t0 + Duration::from_millis(50)));
        assert!(t.pass("a", DlState::Verifying, t0 + Duration::from_millis(60)));
        assert!(!t.pass("a", DlState::Verifying, t0 + Duration::from_millis(100)));
        assert!(t.pass("a", DlState::Verifying, t0 + Duration::from_millis(200)));
    }
}
