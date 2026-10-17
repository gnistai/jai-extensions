//! `jai`: the JAI command-line tool, the JAI Studio desktop app and tray, and
//! the local model server, in one executable. See `studio/DESIGN.md` section 10.
//!
//! On Windows this is a console-subsystem program, so it deliberately has no
//! `windows_subsystem` attribute: `jai.manifest` sets
//! consoleAllocationPolicy=detached, and a GUI launch from a terminal relaunches
//! itself detached (`win::prepare_gui_launch`).

use std::future::Future;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};

// First, so its print macros reach every module after it.
#[macro_use]
mod term;

mod cli;
mod encoding;
#[cfg(feature = "gui")]
mod gui;
#[cfg(any(feature = "gui", test))]
mod gui_support;
#[cfg(any(feature = "gui", test))]
mod integration;
mod logging;
mod output;
#[cfg(any(feature = "gui", test))]
mod render_cache;
mod settings;
#[cfg(all(windows, any(feature = "gui", test)))]
mod win;
#[cfg(any(all(windows, feature = "gui"), test))]
mod winvalues;

/// JAI Studio: view and encode .jai images, and run local models.
#[derive(Debug, Parser)]
#[command(
    name = "jai",
    version,
    after_help = "`jai licenses` prints the licences of the open-source software in JAI Studio."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// A .jai file to open in the viewer
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,

    /// Set on the detached copy that a terminal launch starts for the GUI
    #[arg(long, global = true, hide = true)]
    gui_child: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Open a .jai file in the viewer (forwarded to a running instance)
    Open {
        /// The .jai file
        path: PathBuf,
    },
    /// Start in the tray only (autostart uses this)
    Tray,
    /// Run the model server in the foreground; Ctrl+C stops it
    Serve(ServeArgs),
    /// Download, check and locate the models and engines
    Models {
        #[command(subcommand)]
        command: ModelsCommand,
    },
    /// Generate images with the local image model
    Generate(GenerateArgs),
    /// Ask the local LLM
    Chat(ChatArgs),
    /// Render a JAI document with the local image model
    Render(RenderArgs),
    /// Validate a JAI document
    Validate(ValidateArgs),
    /// Encode an image into a JAI document with the local models
    Encode(EncodeArgs),
    /// Show server and engine status
    Status,
    /// Print the server URL and token, for clients of the HTTP API
    Token(TokenArgs),
    /// Print the licences of the open-source software in JAI Studio
    Licenses,
}

#[derive(Debug, Args)]
struct ServeArgs {
    /// Address to bind (default from the settings: 127.0.0.1)
    #[arg(long, value_name = "H")]
    host: Option<String>,
    /// Port to bind (default from the settings: 5240)
    #[arg(long, value_name = "P", value_parser = clap::value_parser!(u16).range(1..))]
    port: Option<u16>,
    /// Allow a non-loopback address; the token is still required
    #[arg(long)]
    allow_lan: bool,
}

#[derive(Debug, Subcommand)]
enum ModelsCommand {
    /// Show which models and engines are installed
    Status,
    /// Download the models and engines
    Download(DownloadArgs),
    /// Check the installed files against their SHA-256
    Verify,
    /// Print where the models are stored
    Path,
}

#[derive(Debug, Args)]
struct DownloadArgs {
    /// Do not ask for confirmation
    #[arg(long)]
    yes: bool,
    /// Do not reuse matching files found on this PC (such as LM Studio's)
    #[arg(long)]
    no_reuse: bool,
    /// Only these component ids, comma-separated
    #[arg(long, value_name = "IDS", value_delimiter = ',')]
    only: Vec<String>,
}

#[derive(Debug, Args)]
struct GenerateArgs {
    /// What to draw
    prompt: String,
    /// Image size: sides are multiples of 64 from 256 to 2048 (default 1024x1024)
    #[arg(long, value_name = "WxH", value_parser = cli::generate::parse_size)]
    size: Option<String>,
    /// Seed, 0 to 4294967295 (default: random)
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(0..=4_294_967_295))]
    seed: Option<u64>,
    /// Sampling steps, 1 to 16 (default from the settings: 4)
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=16))]
    steps: Option<u32>,
    /// Number of images, 1 to 4; image i uses seed + i
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=4))]
    n: Option<u32>,
    /// Image format
    #[arg(long, value_enum, value_name = "FORMAT")]
    format: Option<ImageFormat>,
    /// Output file
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ImageFormat {
    Png,
    Jpeg,
    Webp,
}

#[derive(Debug, Args)]
struct ChatArgs {
    /// The message
    prompt: String,
    /// Attach an image (PNG, JPEG, GIF or BMP); repeat for more, at most 8
    #[arg(long = "image", value_name = "FILE")]
    images: Vec<PathBuf>,
    /// System prompt
    #[arg(long, value_name = "TEXT")]
    system: Option<String>,
    /// Let the model think before it answers
    #[arg(long)]
    think: bool,
    /// Longest answer in tokens, 1 to 8192
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=8192))]
    max_tokens: Option<u32>,
    /// Print the full JSON response
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RenderArgs {
    /// A .jai file, or JAI text
    #[arg(value_name = "FILE.jai|TEXT")]
    input: String,
    /// Seed, overriding the document's
    #[arg(long, value_name = "S")]
    seed: Option<String>,
    /// Output image file
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ValidateArgs {
    /// A .jai file, or JAI text
    #[arg(value_name = "FILE.jai|TEXT")]
    input: String,
}

#[derive(Debug, Args)]
struct EncodeArgs {
    /// The image to encode
    image: PathBuf,
    /// Generations per attempt, including the first (1 to 30)
    #[arg(long, value_name = "N", default_value_t = 8, value_parser = clap::value_parser!(u32).range(1..=30))]
    generations: u32,
    /// Candidates per generation (1 to 9)
    #[arg(long, value_name = "N", default_value_t = 4, value_parser = clap::value_parser!(u32).range(1..=9))]
    population: u32,
    /// Fixed seeds, comma-separated; every candidate renders once per seed
    #[arg(
        long,
        value_name = "LIST",
        value_delimiter = ',',
        default_value = "1,2"
    )]
    seeds: Vec<u32>,
    /// Independent attempts; the best by fitness wins (1 to 5)
    #[arg(long, value_name = "N", default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=5))]
    restarts: u32,
    /// Stop when the measured fitness reaches this, 0 to 1
    #[arg(long, value_name = "F", default_value_t = 0.9, value_parser = unit_interval)]
    goal: f64,
    /// Render size multiplier, 0.25 to 1.25
    #[arg(long, value_name = "F", default_value_t = 1.0, value_parser = render_scale)]
    scale: f64,
    /// Where the LLM thinks before answering
    #[arg(long, value_enum, value_name = "MODE", default_value_t = Thinking::Off)]
    think: Thinking,
    /// Output file (default <image>.jai)
    #[arg(long, value_name = "F.jai")]
    out: Option<PathBuf>,
    /// Report folder (default <out>.report)
    #[arg(long, value_name = "DIR")]
    report: Option<PathBuf>,
    /// Overwrite an existing output file
    #[arg(long)]
    force: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Thinking {
    /// Never
    Off,
    /// When it proposes new candidates
    Propose,
    /// When it describes and proposes; inspection never thinks
    All,
}

impl Thinking {
    /// The encoder's name for it.
    fn as_str(self) -> &'static str {
        match self {
            Thinking::Off => "off",
            Thinking::Propose => "propose",
            Thinking::All => "all",
        }
    }
}

/// A number from 0 to 1.
fn unit_interval(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .trim()
        .parse()
        .map_err(|_| format!("{s:?} is not a number"))?;
    if (0.0..=1.0).contains(&v) {
        Ok(v)
    } else {
        Err("must be from 0 to 1".into())
    }
}

/// A render size multiplier within `encoding::SCALE`.
fn render_scale(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .trim()
        .parse()
        .map_err(|_| format!("{s:?} is not a number"))?;
    let (lo, hi) = encoding::SCALE;
    if (lo..=hi).contains(&v) {
        Ok(v)
    } else {
        Err(format!("must be from {lo} to {hi}"))
    }
}

#[derive(Debug, Args)]
struct TokenArgs {
    /// Replace the token; clients that use it must be updated
    #[arg(long)]
    regenerate: bool,
}

/// A GUI mode (DESIGN.md section 10).
#[derive(Debug)]
enum GuiMode {
    /// The window, the tray and the server, optionally opening a document.
    Window { open: Option<PathBuf> },
    /// The tray and the server; no window is shown.
    Tray,
}

impl Cli {
    /// Rejects a document path together with a subcommand (clap's
    /// `args_conflicts_with_subcommands` would also stop subcommands after a
    /// leading `--gui-child`, which a relaunch may pass anywhere), and a bare
    /// word that names no file, which is a mistyped subcommand rather than a
    /// document: `jai stauts` must not open the desktop app.
    fn check(self) -> Result<Cli, clap::Error> {
        use clap::error::ErrorKind;
        match (&self.path, &self.command) {
            (Some(path), Some(_)) => Err(Cli::command().error(
                ErrorKind::ArgumentConflict,
                format!(
                    "a document path ({}) cannot be combined with a subcommand; use `jai open <PATH>`",
                    path.display()
                ),
            )),
            (Some(path), None) if is_bare_word(path) && !path.exists() => {
                let word = path.to_string_lossy();
                let mut message = format!("unrecognized subcommand '{word}'");
                if let Some(name) = similar_subcommand(&word) {
                    message.push_str(&format!("\n\n  tip: a similar subcommand exists: '{name}'"));
                }
                message.push_str(
                    "\n\n  To open a document, give its file name (such as photo.jai) or use `jai open <PATH>`.",
                );
                Err(Cli::command().error(ErrorKind::InvalidSubcommand, message))
            }
            _ => Ok(self),
        }
    }
}

/// One word with no folder and no extension, such as `stauts`. A document
/// path from Explorer or a file manager always has a folder, and one typed in
/// a terminal has an extension.
fn is_bare_word(path: &std::path::Path) -> bool {
    let mut parts = path.components();
    matches!(
        (parts.next(), parts.next()),
        (Some(std::path::Component::Normal(_)), None)
    ) && path.extension().is_none()
}

/// The subcommand `word` is most likely a typo of, if any: at most one edit
/// (insert, delete, replace or swap two neighbours) for short names, two for
/// longer ones.
fn similar_subcommand(word: &str) -> Option<String> {
    let word = word.to_lowercase();
    Cli::command()
        .get_subcommands()
        .filter(|c| !c.is_hide_set())
        .map(|c| c.get_name().to_string())
        .map(|name| (edit_distance(&word, &name), name))
        .filter(|(d, name)| *d <= if name.chars().count() > 4 { 2 } else { 1 })
        .min_by_key(|(d, _)| *d)
        .map(|(_, name)| name)
}

/// Optimal string alignment distance: insertions, deletions, replacements
/// and swaps of neighbouring characters each cost one.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[a.len()][b.len()]
}

fn main() -> ExitCode {
    let opts = Cli::parse().check().unwrap_or_else(|e| e.exit());
    match opts.command {
        None => gui_mode(GuiMode::Window { open: opts.path }, opts.gui_child),
        Some(Command::Open { path }) => {
            gui_mode(GuiMode::Window { open: Some(path) }, opts.gui_child)
        }
        Some(Command::Tray) => gui_mode(GuiMode::Tray, opts.gui_child),
        // The server logs its requests; other commands only log problems.
        Some(Command::Serve(args)) => run_cli("info", cli::serve::run(args)),
        Some(Command::Models { command }) => run_cli("warn", cli::models::run(command)),
        Some(Command::Generate(args)) => run_cli("warn", cli::generate::run(args)),
        Some(Command::Chat(args)) => run_cli("warn", cli::chat::run(args)),
        Some(Command::Render(args)) => run_cli("warn", cli::render::run(args)),
        Some(Command::Validate(args)) => run_cli("warn", cli::validate::run(args)),
        Some(Command::Encode(args)) => run_cli("warn", cli::encode::run(args)),
        Some(Command::Status) => run_cli("warn", cli::status::run()),
        Some(Command::Token(args)) => run_cli("warn", cli::token::run(args)),
        Some(Command::Licenses) => {
            term::write_raw(term::Stream::Out, THIRD_PARTY_NOTICES);
            ExitCode::SUCCESS
        }
    }
}

/// The licences of the open-source software in this build, generated by
/// `studio/scripts/notices.sh` and embedded by `build.rs`
/// (`JAI_THIRD_PARTY_NOTICES`). A build without them says so.
pub const THIRD_PARTY_NOTICES: &str = include_str!(concat!(env!("OUT_DIR"), "/notices.txt"));

/// Runs a CLI command on a tokio runtime, logging to stderr at `log` unless
/// `JAI_LOG` says otherwise. CLI commands never start Tauri:
/// tauri-plugin-single-instance would forward them to a running GUI and exit 0.
fn run_cli(log: &str, command: impl Future<Output = anyhow::Result<ExitCode>>) -> ExitCode {
    logging::init_stderr(log);
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => report(runtime.block_on(command)),
        Err(e) => report(Err(e.into())),
    }
}

/// Runs a GUI mode. On Windows, a launch from a terminal first relaunches
/// itself detached so the shell prompt returns.
#[cfg(feature = "gui")]
fn gui_mode(mode: GuiMode, gui_child: bool) -> ExitCode {
    #[cfg(windows)]
    {
        match win::prepare_gui_launch(gui_child) {
            Ok(win::GuiLaunch::Here) => {}
            Ok(win::GuiLaunch::Relaunched) => return ExitCode::SUCCESS,
            Err(e) => return gui_error(e),
        }
    }
    #[cfg(not(windows))]
    let _ = gui_child;
    match gui::run(mode) {
        Ok(code) => ExitCode::from(code),
        Err(e) => gui_error(e),
    }
}

/// A GUI that cannot start says why in the log, on stderr and, on Windows,
/// in an error box (a GUI launch usually has no console).
#[cfg(feature = "gui")]
fn gui_error(e: anyhow::Error) -> ExitCode {
    tracing::error!("cannot start: {e:#}");
    errln!("jai: {e:#}");
    #[cfg(windows)]
    win::error_box("JAI Studio", &format!("JAI Studio cannot start.\n\n{e:#}"));
    ExitCode::FAILURE
}

/// A build without the `gui` feature is a headless CLI and server.
#[cfg(not(feature = "gui"))]
fn gui_mode(mode: GuiMode, _gui_child: bool) -> ExitCode {
    let what = match mode {
        GuiMode::Window { open: Some(path) } => format!("open {}", path.display()),
        GuiMode::Window { open: None } => "start the desktop app".to_string(),
        GuiMode::Tray => "start in the tray".to_string(),
    };
    errln!("jai: cannot {what}: this build has no GUI. Run `jai serve` for the server, or see `jai --help`.");
    ExitCode::from(2)
}

/// Prints an error with its causes; exits with 2 for usage errors, else 1.
fn report(result: anyhow::Result<ExitCode>) -> ExitCode {
    match result {
        Ok(code) => code,
        Err(e) => {
            // The message may quote a document, a model or an engine.
            errln!("jai: {e:#}");
            ExitCode::from(cli::exit_code(&e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn try_parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("jai").chain(args.iter().copied()))?.check()
    }

    fn parse(args: &[&str]) -> Cli {
        try_parse(args).unwrap()
    }

    #[test]
    fn definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn gui_modes() {
        let c = parse(&[]);
        assert!(c.command.is_none() && c.path.is_none() && !c.gui_child);
        let c = parse(&["castle.jai"]);
        assert_eq!(c.path, Some(PathBuf::from("castle.jai")));
        assert!(parse(&["castle.jai", "--gui-child"]).gui_child);
        let c = parse(&["open", "castle.jai", "--gui-child"]);
        assert!(matches!(c.command, Some(Command::Open { .. })) && c.gui_child);
        assert!(matches!(parse(&["tray"]).command, Some(Command::Tray)));
        // The hidden flag works before a subcommand too (a relaunch may put it anywhere).
        let c = parse(&["--gui-child", "tray"]);
        assert!(matches!(c.command, Some(Command::Tray)) && c.gui_child);
        let c = parse(&["--gui-child", "open", "castle.jai"]);
        assert!(matches!(c.command, Some(Command::Open { .. })) && c.path.is_none());
        // A document path and a subcommand together are a usage error.
        let e = try_parse(&["castle.jai", "status"]).unwrap_err();
        assert_eq!(e.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn flags() {
        match parse(&[
            "models",
            "download",
            "--yes",
            "--no-reuse",
            "--only",
            "llm-model,image-vae",
        ])
        .command
        {
            Some(Command::Models {
                command: ModelsCommand::Download(a),
            }) => {
                assert!(a.yes && a.no_reuse);
                assert_eq!(a.only, ["llm-model", "image-vae"]);
            }
            other => panic!("{other:?}"),
        }
        match parse(&["encode", "photo.png"]).command {
            Some(Command::Encode(a)) => {
                assert_eq!((a.generations, a.population, a.restarts), (8, 4, 1));
                assert_eq!(a.seeds, [1, 2]);
                assert_eq!((a.goal, a.scale), (0.9, 1.0));
                assert_eq!(a.think, Thinking::Off);
                assert!(a.out.is_none() && a.report.is_none() && !a.force);
            }
            other => panic!("{other:?}"),
        }
        match parse(&[
            "encode", "p.png", "--seeds", "7,8,9", "--think", "propose", "--goal", "0.8",
        ])
        .command
        {
            Some(Command::Encode(a)) => {
                assert_eq!(a.seeds, [7, 8, 9]);
                assert_eq!(a.think, Thinking::Propose);
                assert_eq!(a.goal, 0.8);
            }
            other => panic!("{other:?}"),
        }
        match parse(&[
            "chat",
            "hi",
            "--image",
            "a.png",
            "--image",
            "b.png",
            "--think",
            "--max-tokens",
            "100",
            "--json",
        ])
        .command
        {
            Some(Command::Chat(a)) => {
                assert_eq!(a.images.len(), 2);
                assert!(a.think && a.json && a.system.is_none());
                assert_eq!(a.max_tokens, Some(100));
            }
            other => panic!("{other:?}"),
        }
        match parse(&[
            "generate", "a castle", "--size", "1344x768", "--n", "2", "--format", "webp", "--seed",
            "7",
        ])
        .command
        {
            Some(Command::Generate(a)) => {
                assert_eq!(a.size.as_deref(), Some("1344x768"));
                assert_eq!(
                    (a.n, a.seed, a.format),
                    (Some(2), Some(7), Some(ImageFormat::Webp))
                );
            }
            other => panic!("{other:?}"),
        }
        match parse(&[
            "serve",
            "--host",
            "0.0.0.0",
            "--port",
            "5300",
            "--allow-lan",
        ])
        .command
        {
            Some(Command::Serve(a)) => {
                assert_eq!(
                    (a.host.as_deref(), a.port, a.allow_lan),
                    (Some("0.0.0.0"), Some(5300), true)
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse(&["token", "--regenerate"]).command,
            Some(Command::Token(TokenArgs { regenerate: true }))
        ));
        assert!(matches!(
            parse(&["render", "prompt=a castle", "--seed", "3"]).command,
            Some(Command::Render(_))
        ));
        assert!(matches!(
            parse(&["validate", "castle.jai"]).command,
            Some(Command::Validate(_))
        ));
        assert!(matches!(parse(&["status"]).command, Some(Command::Status)));
        assert!(matches!(
            parse(&["licenses"]).command,
            Some(Command::Licenses)
        ));
    }

    #[test]
    fn mistyped_subcommands_are_usage_errors() {
        use clap::error::ErrorKind;
        let e = try_parse(&["stauts"]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidSubcommand);
        assert_eq!(e.exit_code(), 2);
        let text = e.to_string();
        assert!(
            text.contains("'stauts'") && text.contains("similar subcommand exists: 'status'"),
            "{text}"
        );
        for (typo, meant) in [
            ("valdiate", "validate"),
            ("encdoe", "encode"),
            ("tokn", "token"),
            ("Modles", "models"),
        ] {
            let text = try_parse(&[typo]).unwrap_err().to_string();
            assert!(text.contains(&format!("'{meant}'")), "{typo}: {text}");
        }
        // Nothing close: still an error, without a tip.
        let text = try_parse(&["frobnicate"]).unwrap_err().to_string();
        assert!(!text.contains("tip:"), "{text}");
        // Documents still open the viewer: with an extension or a folder
        // (even when missing, so the window can say so), or when they exist.
        assert!(try_parse(&["castle.jai"]).is_ok());
        assert!(try_parse(&["photos/castle"]).is_ok());
        assert!(try_parse(&["src"]).is_ok());
        assert!(try_parse(&["open", "stauts"]).is_ok());
    }

    #[test]
    fn edit_distances() {
        assert_eq!(edit_distance("stauts", "status"), 1);
        assert_eq!(edit_distance("tokn", "token"), 1);
        assert_eq!(edit_distance("chat", "chat"), 0);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("render", "encode"), 4);
        assert_eq!(similar_subcommand("gen"), None);
        assert_eq!(similar_subcommand("serv").as_deref(), Some("serve"));
    }

    #[test]
    fn ranges_are_usage_errors() {
        use clap::error::ErrorKind;
        let kind = |args: &[&str]| try_parse(args).map(|_| ()).unwrap_err().kind();
        assert_eq!(
            kind(&["encode", "p.png", "--population", "10"]),
            ErrorKind::ValueValidation
        );
        assert_eq!(
            kind(&["encode", "p.png", "--generations", "0"]),
            ErrorKind::ValueValidation
        );
        assert_eq!(
            kind(&["encode", "p.png", "--restarts", "6"]),
            ErrorKind::ValueValidation
        );
        assert_eq!(
            kind(&["encode", "p.png", "--goal", "1.5"]),
            ErrorKind::ValueValidation
        );
        assert_eq!(
            kind(&["encode", "p.png", "--scale", "3"]),
            ErrorKind::ValueValidation
        );
        assert_eq!(
            kind(&["encode", "p.png", "--seeds", "1,x"]),
            ErrorKind::ValueValidation
        );
        assert_eq!(
            kind(&["generate", "x", "--size", "1000x768"]),
            ErrorKind::ValueValidation
        );
        assert_eq!(
            kind(&["generate", "x", "--n", "5"]),
            ErrorKind::ValueValidation
        );
        assert_eq!(
            kind(&["generate", "x", "--steps", "17"]),
            ErrorKind::ValueValidation
        );
        assert_eq!(
            kind(&["generate", "x", "--seed", "4294967296"]),
            ErrorKind::ValueValidation
        );
        assert_eq!(
            kind(&["chat", "x", "--max-tokens", "0"]),
            ErrorKind::ValueValidation
        );
        assert_eq!(kind(&["serve", "--port", "0"]), ErrorKind::ValueValidation);
        assert_eq!(
            kind(&["models"]),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        // Usage errors exit with 2.
        assert_eq!(try_parse(&["encode"]).unwrap_err().exit_code(), 2);
        match parse(&[
            "encode", "p.png", "--goal", "0", "--scale", "0.5", "--think", "all",
        ])
        .command
        {
            Some(Command::Encode(a)) => {
                assert_eq!((a.goal, a.scale), (0.0, 0.5));
                assert_eq!(a.think.as_str(), "all");
            }
            other => panic!("{other:?}"),
        }
    }
}
