//! Engine commands built from Paths, Config and Selection (DESIGN.md section 6,
//! "llama-server command" and "sd-server command").

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use jai_core::install::{engine_exe, model_path};
use jai_core::{Config, Paths, Role, Selection};

use crate::launcher::{InvalidSettings, NotInstalled};
use crate::EngineKind;

/// llama-server reads `--api-key` from this variable. The key goes there
/// rather than on the command line, which every account can read on Linux.
pub const LLAMA_API_KEY_ENV: &str = "LLAMA_API_KEY";

/// llama-server answers 200 here once the model is loaded and 503 while it
/// loads. It needs no API key.
pub const LLAMA_HEALTH_PATH: &str = "/health";

/// llama-server reads `--video-ffmpeg-dir` from this variable. Studio sets it
/// in the environment rather than on the command line: a build whose server
/// has no such option ignores the variable, where the flag would stop it.
pub const LLAMA_FFMPEG_DIR_ENV: &str = "LLAMA_ARG_VIDEO_FFMPEG_DIR";

/// The empty folder llama-server is told holds ffmpeg and ffprobe
/// ([`LLAMA_FFMPEG_DIR_ENV`]), under the private data root. It would otherwise
/// run whatever `ffmpeg` it finds on PATH for media its own decoder rejects.
pub fn no_ffmpeg_dir(paths: &Paths) -> PathBuf {
    paths.root.join("cache").join("no-ffmpeg")
}

/// sd-server only listens once its context exists, and `--eager-load` loads
/// the weights before that, so a 200 here means ready.
pub const SD_HEALTH_PATH: &str = "/v1/models";

/// sd-server's `--max-vram`: a negative value keeps that many GiB of VRAM free.
/// Without it the FLUX weights (12.2 GB in Q8) stay on the GPU and leave too
/// little for a render's 1.27 GB working buffer at 1 megapixel once the desktop
/// and other programs take 2 to 3 GB: every render then fails ("generate_image
/// returned no results"). With 3 GiB kept free the weights wait in RAM and move
/// to the GPU as far as the budget allows. Measured on a 16 GB RTX 5070 Ti at
/// 1216x832: about 6.3 s a render instead of 5.5 s, and still working with
/// another 3.3 GB taken by a second program after the engine loaded.
pub const SD_VRAM_RESERVE_GIB: &str = "-3";

/// Everything needed to start and supervise one engine process.
#[derive(Clone)]
pub struct LaunchSpec {
    pub kind: EngineKind,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// The engine dir, so its DLLs are found.
    pub cwd: PathBuf,
    /// Added to the environment Studio runs in. llama-server's API key is
    /// here, as `LLAMA_API_KEY`.
    pub env: Vec<(String, String)>,
    /// A random loopback address and a free port on it (see [`engine_addr`]).
    pub addr: SocketAddrV4,
    /// llama-server's random API key; sd-server has none.
    pub api_key: Option<String>,
    /// `/health` for llama-server, `/v1/models` for sd-server.
    pub health_path: &'static str,
    /// `logs/llm.log` or `logs/image.log`.
    pub log_file: PathBuf,
}

impl fmt::Debug for LaunchSpec {
    /// Like a derived Debug, with the API key left out wherever it appears.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let env: Vec<(&str, &str)> = self
            .env
            .iter()
            .map(|(name, value)| {
                let secret = name == LLAMA_API_KEY_ENV || self.api_key.as_deref() == Some(value);
                (
                    name.as_str(),
                    if secret { REDACTED } else { value.as_str() },
                )
            })
            .collect();
        f.debug_struct("LaunchSpec")
            .field("kind", &self.kind)
            .field("program", &self.program)
            .field("args", &redacted_args(&self.args))
            .field("cwd", &self.cwd)
            .field("env", &env)
            .field("addr", &self.addr)
            .field("api_key", &self.api_key.as_ref().map(|_| REDACTED))
            .field("health_path", &self.health_path)
            .field("log_file", &self.log_file)
            .finish()
    }
}

/// The llama-server command. The API key goes into the environment as
/// `LLAMA_API_KEY`, not onto the command line, and so does the empty ffmpeg
/// folder ([`LLAMA_FFMPEG_DIR_ENV`]).
pub fn llama_spec(
    paths: &Paths,
    cfg: &Config,
    sel: &Selection,
    addr: SocketAddrV4,
    api_key: &str,
    mtp: bool,
) -> anyhow::Result<LaunchSpec> {
    anyhow::ensure!(
        !api_key.is_empty(),
        "llama-server needs an API key, or any local program could use it"
    );
    let llm = &cfg.llm;
    check_extra_args(EngineKind::Llm, &llm.extra_args).map_err(InvalidSettings)?;
    let layers = gpu_layers(&llm.gpu_layers).map_err(InvalidSettings)?;
    let program = installed_engine(paths, sel, Role::LlmEngine, "the llama.cpp engine")?;
    let model = installed_model(paths, sel, Role::LlmModel, "the Qwen model")?;
    let projector = installed_model(paths, sel, Role::LlmProjector, "the Qwen vision projector")?;
    let no_ffmpeg = no_ffmpeg_dir(paths);
    std::fs::create_dir_all(&no_ffmpeg)
        .with_context(|| format!("could not create {}", no_ffmpeg.display()))?;
    let mut args = strings(&["-m", path_arg(&model)?, "--mmproj", path_arg(&projector)?]);
    args.extend([
        "-ngl".into(),
        layers,
        "-c".into(),
        llm.ctx.to_string(),
        "-fa".into(),
        "on".into(),
        "-np".into(),
        llm.parallel.to_string(),
        "--no-webui".into(),
        "--host".into(),
        addr.ip().to_string(),
        "--port".into(),
        addr.port().to_string(),
        "--image-max-tokens".into(),
        llm.image_max_tokens.to_string(),
    ]);
    if mtp {
        args.extend(strings(&["--spec-type", "draft-mtp"]));
    }
    args.extend(llm.extra_args.iter().cloned());
    Ok(LaunchSpec {
        kind: EngineKind::Llm,
        cwd: engine_dir(&program)?,
        program,
        args,
        env: vec![
            (LLAMA_API_KEY_ENV.into(), api_key.into()),
            (LLAMA_FFMPEG_DIR_ENV.into(), path_arg(&no_ffmpeg)?.into()),
        ],
        addr,
        api_key: Some(api_key.into()),
        health_path: LLAMA_HEALTH_PATH,
        log_file: paths.logs().join("llm.log"),
    })
}

/// The sd-server command.
pub fn sd_spec(
    paths: &Paths,
    cfg: &Config,
    sel: &Selection,
    addr: SocketAddrV4,
) -> anyhow::Result<LaunchSpec> {
    check_extra_args(EngineKind::Image, &cfg.image.extra_args).map_err(InvalidSettings)?;
    let program = installed_engine(
        paths,
        sel,
        Role::ImageEngine,
        "the stable-diffusion.cpp engine",
    )?;
    let diffusion = installed_model(paths, sel, Role::ImageDiffusion, "the FLUX model")?;
    let vae = installed_model(paths, sel, Role::ImageVae, "the FLUX autoencoder")?;
    let clip_l = installed_model(paths, sel, Role::ImageClipL, "the CLIP-L text encoder")?;
    let t5xxl = installed_model(paths, sel, Role::ImageT5xxl, "the T5-XXL text encoder")?;
    let mut args = strings(&[
        "--diffusion-model",
        path_arg(&diffusion)?,
        "--vae",
        path_arg(&vae)?,
        "--clip_l",
        path_arg(&clip_l)?,
        "--t5xxl",
        path_arg(&t5xxl)?,
        "--cfg-scale",
        "1.0",
    ]);
    args.extend([
        "--steps".into(),
        cfg.image.steps.to_string(),
        "--sampling-method".into(),
        "euler".into(),
        "--eager-load".into(),
        "--max-vram".into(),
        SD_VRAM_RESERVE_GIB.into(),
        "-l".into(),
        addr.ip().to_string(),
        "--listen-port".into(),
        addr.port().to_string(),
    ]);
    args.extend(cfg.image.extra_args.iter().cloned());
    Ok(LaunchSpec {
        kind: EngineKind::Image,
        cwd: engine_dir(&program)?,
        program,
        args,
        env: Vec::new(),
        addr,
        api_key: None,
        health_path: SD_HEALTH_PATH,
        log_file: paths.logs().join("image.log"),
    })
}

/// Where an engine listens: a random address in 127.0.0.0/8 (24 random bits,
/// never 127.0.0.1) and a free port on it, found by binding port 0 there and
/// dropping the listener.
///
/// A web page that scans 127.0.0.1, or rebinds a domain to it, finds nothing:
/// sd-server has no authentication and answers any web origin, and guessing
/// the address and port together is out of reach. Other accounts on this PC
/// can still list it; see DESIGN.md section 12. Where the OS only accepts
/// 127.0.0.1, that is used, with a warning.
pub fn engine_addr() -> io::Result<SocketAddrV4> {
    for _ in 0..8 {
        match free_port_on(random_loopback()) {
            Ok(addr) => return Ok(addr),
            // This OS has only 127.0.0.1 on its loopback interface.
            Err(e) if e.kind() == io::ErrorKind::AddrNotAvailable => break,
            Err(_) => continue,
        }
    }
    tracing::warn!(
        "this system cannot listen on random loopback addresses; the engines use 127.0.0.1"
    );
    free_port_on(Ipv4Addr::LOCALHOST)
}

fn free_port_on(ip: Ipv4Addr) -> io::Result<SocketAddrV4> {
    let listener = TcpListener::bind(SocketAddrV4::new(ip, 0))?;
    match listener.local_addr()? {
        SocketAddr::V4(addr) => Ok(addr),
        SocketAddr::V6(addr) => Err(io::Error::other(format!(
            "bound {addr} for an IPv4 address"
        ))),
    }
}

/// 127.x.y.z, skipping 127.0.0.0, 127.0.0.1 and 127.255.255.255.
fn random_loopback() -> Ipv4Addr {
    loop {
        let [_, x, y, z] = rand::random::<u32>().to_be_bytes();
        if !matches!((x, y, z), (0, 0, 0 | 1) | (255, 255, 255)) {
            return Ipv4Addr::new(127, x, y, z);
        }
    }
}

/// Flags that `llm.extraArgs` may not hold: those Studio sets itself (the
/// model, the address, the key) and those that would serve or write files,
/// run tools, or reach the network. Each is refused as `--flag value` and as
/// `--flag=value`.
const LLAMA_RESERVED: &[&str] = &[
    "-m",
    "--model",
    "-mm",
    "--mmproj",
    "--host",
    "--port",
    "--api-key",
    "--api-key-file",
    "--webui",
    "--path",
    "--media-path",
    "--tools",
    "--tools-runtime",
    "--slot-save-path",
    "--log-file",
    "--video-ffmpeg-dir",
    "--rpc",
    "-hf",
    "-hfr",
    "--hf-repo",
    "-hff",
    "--hf-file",
    "-mu",
    "--model-url",
    "-mmu",
    "--mmproj-url",
    "-dr",
    "--docker-repo",
];

/// The same for `image.extraArgs` and sd-server.
const SD_RESERVED: &[&str] = &[
    "-m",
    "--model",
    "--diffusion-model",
    "--vae",
    "--clip_l",
    "--t5xxl",
    "-l",
    "--listen-ip",
    "--listen-port",
    "--serve-html-path",
    "--lora-model-dir",
    "--embd-dir",
    "-o",
    "--output",
    "--preview-path",
];

/// Checks `llm.extraArgs` or `image.extraArgs`: they may tune an engine but
/// not set what Studio sets (the model, the address, the key), serve or write
/// files, or reach the network. The error names the flag.
pub fn check_extra_args(kind: EngineKind, args: &[String]) -> Result<(), String> {
    let (reserved, setting) = match kind {
        EngineKind::Llm => (LLAMA_RESERVED, "llm.extraArgs"),
        EngineKind::Image => (SD_RESERVED, "image.extraArgs"),
    };
    for arg in args {
        let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
        if reserved.contains(&flag) {
            return Err(format!(
                "{setting} may not contain {flag}: JAI Studio sets the engine's model, address and key itself, \
                 and extra arguments may not make the engine serve or write files or reach the network"
            ));
        }
    }
    Ok(())
}

/// Shown in place of the API key in logs and Debug output.
const REDACTED: &str = "<redacted>";

/// The arguments with the value of `--api-key` (or `--api-key=…`) replaced,
/// for logs.
pub(crate) fn redacted_args(args: &[String]) -> Vec<String> {
    let mut hide_next = false;
    args.iter()
        .map(|arg| {
            let shown = if hide_next {
                REDACTED.to_string()
            } else if arg.starts_with("--api-key=") {
                format!("--api-key={REDACTED}")
            } else {
                arg.clone()
            };
            hide_next = arg == "--api-key";
            shown
        })
        .collect()
}

/// The command as one line for the engine log, with the API key hidden.
pub(crate) fn command_line(spec: &LaunchSpec) -> String {
    std::iter::once(spec.program.display().to_string())
        .chain(redacted_args(&spec.args))
        .map(|part| {
            if part.is_empty() || part.contains(char::is_whitespace) || part.contains('"') {
                format!("\"{}\"", part.replace('"', "\\\""))
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `-ngl`: "all" means every layer (999); "auto" and layer counts pass through.
fn gpu_layers(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("all") {
        Ok("999".into())
    } else if value.eq_ignore_ascii_case("auto") {
        Ok("auto".into())
    } else {
        value.parse::<u32>().map(|n| n.to_string()).map_err(|_| {
            format!("llm.gpuLayers must be \"all\" or a number of layers, not {value:?}")
        })
    }
}

/// The engine executable, which must exist.
fn installed_engine(
    paths: &Paths,
    sel: &Selection,
    role: Role,
    what: &str,
) -> anyhow::Result<PathBuf> {
    let exe = engine_exe(paths, sel, role)
        .ok_or_else(|| NotInstalled(format!("{what} is not installed")))?;
    present(exe, what)
}

/// A model file, which must exist.
fn installed_model(
    paths: &Paths,
    sel: &Selection,
    role: Role,
    what: &str,
) -> anyhow::Result<PathBuf> {
    let file = model_path(paths, sel, role)
        .ok_or_else(|| NotInstalled(format!("{what} is not installed")))?;
    present(file, what)
}

fn present(path: PathBuf, what: &str) -> anyhow::Result<PathBuf> {
    if path.is_file() {
        Ok(path)
    } else {
        Err(NotInstalled(format!(
            "{what} is not installed ({} is missing)",
            path.display()
        ))
        .into())
    }
}

/// The folder that holds the engine and its DLLs.
fn engine_dir(program: &Path) -> anyhow::Result<PathBuf> {
    program
        .parent()
        .map(Path::to_path_buf)
        .with_context(|| format!("{} has no parent folder", program.display()))
}

/// Paths become arguments as text; one that is not Unicode cannot be passed.
fn path_arg(path: &Path) -> anyhow::Result<&str> {
    path.to_str()
        .with_context(|| format!("{} is not a valid Unicode path", path.display()))
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;

    use jai_core::{Backend, Catalog, Component};

    use super::*;

    /// A data root with empty files where the selected components live.
    pub(crate) struct Fixture {
        _dir: tempfile::TempDir,
        pub(crate) paths: Paths,
        pub(crate) sel: Selection,
    }

    /// The catalog's models plus the CPU engines for this OS, laid out on disk
    /// under a temporary data root.
    pub(crate) fn fixture(create_files: bool) -> Fixture {
        let catalog: Catalog =
            serde_json::from_str(include_str!("../../../catalog.json")).expect("catalog.json");
        let (llama, sd) = if cfg!(windows) {
            ("llama-win-cpu", "sd-win-cpu")
        } else {
            ("llama-linux-cpu", "sd-linux-cpu")
        };
        let components: Vec<Component> = catalog
            .components
            .into_iter()
            .filter(|c| c.dir.is_none() || c.id == llama || c.id == sd)
            .collect();
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::at(dir.path());
        if create_files {
            fs::create_dir_all(paths.models()).expect("models dir");
            for c in &components {
                match (&c.dir, &c.exe) {
                    (Some(d), Some(exe)) => {
                        let engine_dir = paths.engines().join(d);
                        fs::create_dir_all(&engine_dir).expect("engine dir");
                        fs::write(engine_dir.join(exe), b"").expect("engine exe");
                    }
                    _ => fs::write(paths.models().join(&c.file), b"").expect("model file"),
                }
            }
        }
        let sel = Selection {
            components,
            llm_backend: Backend::Cpu,
            image_backend: Backend::Cpu,
        };
        Fixture {
            _dir: dir,
            paths,
            sel,
        }
    }

    pub(crate) fn engine_path(fx: &Fixture, id: &str) -> PathBuf {
        let c = fx
            .sel
            .components
            .iter()
            .find(|c| c.id == id)
            .expect("component");
        fx.paths
            .engines()
            .join(c.dir.as_deref().expect("dir"))
            .join(c.exe.as_deref().expect("exe"))
    }

    fn model(fx: &Fixture, file: &str) -> String {
        fx.paths
            .models()
            .join(file)
            .to_str()
            .expect("utf-8")
            .to_string()
    }

    fn at(port: u16) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::new(127, 18, 52, 86), port)
    }

    #[test]
    fn llama_command_is_the_design_command() {
        let fx = fixture(true);
        let spec = llama_spec(
            &fx.paths,
            &Config::default(),
            &fx.sel,
            at(41234),
            "k3y",
            true,
        )
        .expect("llama spec");
        let no_ffmpeg = no_ffmpeg_dir(&fx.paths);
        let expected = [
            "-m",
            &model(&fx, "Qwen3.8-27B-UD-IQ3_XXS.gguf"),
            "--mmproj",
            &model(&fx, "mmproj-F16.gguf"),
            "-ngl",
            "999",
            "-c",
            "20480",
            "-fa",
            "on",
            "-np",
            "1",
            "--no-webui",
            "--host",
            "127.18.52.86",
            "--port",
            "41234",
            "--image-max-tokens",
            "1536",
            "--spec-type",
            "draft-mtp",
        ];
        assert_eq!(spec.args, expected);
        assert!(
            no_ffmpeg.is_dir(),
            "the folder exists, empty, so ffmpeg is never found"
        );
        assert!(
            !spec.args.iter().any(|a| a.contains("k3y")),
            "the key is not on the command line, which other accounts can read"
        );
        assert_eq!(
            spec.env,
            [
                ("LLAMA_API_KEY".to_string(), "k3y".to_string()),
                (
                    "LLAMA_ARG_VIDEO_FFMPEG_DIR".to_string(),
                    no_ffmpeg.to_str().expect("utf-8").to_string()
                ),
            ]
        );
        let llama = if cfg!(windows) {
            "llama-win-cpu"
        } else {
            "llama-linux-cpu"
        };
        assert_eq!(spec.program, engine_path(&fx, llama));
        assert_eq!(Some(spec.cwd.as_path()), spec.program.parent());
        assert_eq!(spec.kind, EngineKind::Llm);
        assert_eq!(spec.addr, at(41234));
        assert_eq!(spec.api_key.as_deref(), Some("k3y"));
        assert_eq!(spec.health_path, "/health");
        assert_eq!(spec.log_file, fx.paths.logs().join("llm.log"));
    }

    #[test]
    fn llama_settings_and_extra_args() {
        let fx = fixture(true);
        let mut cfg = Config::default();
        cfg.llm.gpu_layers = "40".into();
        cfg.llm.ctx = 8192;
        cfg.llm.parallel = 2;
        cfg.llm.image_max_tokens = 1024;
        cfg.llm.extra_args = vec!["--threads".into(), "8".into()];
        let spec = llama_spec(&fx.paths, &cfg, &fx.sel, at(5000), "k", false).expect("llama spec");
        let args = spec.args.join(" ");
        assert!(
            args.contains("-ngl 40 -c 8192 -fa on -np 2 --no-webui"),
            "{args}"
        );
        assert!(args.contains("--image-max-tokens 1024"), "{args}");
        assert!(!args.contains("--spec-type"), "MTP off: {args}");
        assert!(
            args.ends_with("--image-max-tokens 1024 --threads 8"),
            "extra args last: {args}"
        );
    }

    #[test]
    fn sd_command_is_the_design_command() {
        let fx = fixture(true);
        let mut cfg = Config::default();
        cfg.image.extra_args = vec!["--vae-tiling".into()];
        let spec = sd_spec(&fx.paths, &cfg, &fx.sel, at(41235)).expect("sd spec");
        let expected = [
            "--diffusion-model",
            &model(&fx, "flux1-schnell-q8_0.gguf"),
            "--vae",
            &model(&fx, "ae.safetensors"),
            "--clip_l",
            &model(&fx, "clip_l.safetensors"),
            "--t5xxl",
            &model(&fx, "t5-v1_1-xxl-encoder-Q8_0.gguf"),
            "--cfg-scale",
            "1.0",
            "--steps",
            "4",
            "--sampling-method",
            "euler",
            "--eager-load",
            "--max-vram",
            "-3",
            "-l",
            "127.18.52.86",
            "--listen-port",
            "41235",
            "--vae-tiling",
        ];
        assert_eq!(spec.args, expected);
        let sd = if cfg!(windows) {
            "sd-win-cpu"
        } else {
            "sd-linux-cpu"
        };
        assert_eq!(spec.program, engine_path(&fx, sd));
        assert_eq!(Some(spec.cwd.as_path()), spec.program.parent());
        assert_eq!(spec.kind, EngineKind::Image);
        assert_eq!(spec.addr, at(41235));
        assert_eq!(spec.api_key, None);
        assert!(spec.env.is_empty());
        assert_eq!(spec.health_path, "/v1/models");
        assert_eq!(spec.log_file, fx.paths.logs().join("image.log"));
    }

    #[test]
    fn missing_files_are_not_installed() {
        let fx = fixture(false);
        let err = llama_spec(&fx.paths, &Config::default(), &fx.sel, at(1), "k", true)
            .expect_err("nothing on disk");
        assert!(err.downcast_ref::<NotInstalled>().is_some(), "{err:#}");
        let err =
            sd_spec(&fx.paths, &Config::default(), &fx.sel, at(1)).expect_err("nothing on disk");
        assert!(err.downcast_ref::<NotInstalled>().is_some(), "{err:#}");
    }

    #[test]
    fn empty_api_key_is_refused() {
        let fx = fixture(true);
        assert!(llama_spec(&fx.paths, &Config::default(), &fx.sel, at(1), "", true).is_err());
    }

    /// Extra arguments tune an engine; they cannot move it off its private
    /// address, replace its key, or make it serve files.
    #[test]
    fn extra_args_may_not_take_over_the_engine() {
        let ok = strings(&["--threads", "8", "--cache-type-k", "q8_0", "-b=2048"]);
        assert_eq!(check_extra_args(EngineKind::Llm, &ok), Ok(()));
        assert_eq!(
            check_extra_args(EngineKind::Image, &strings(&["--vae-tiling", "-t", "8"])),
            Ok(())
        );
        for bad in [
            &["--host", "0.0.0.0"][..],
            &["--port=8080"],
            &["--api-key", "known"],
            &["--api-key-file", "keys.txt"],
            &["--webui"],
            &["--media-path", "C:\\"],
            &["--tools", "all"],
            &["--path=/"],
            &["--rpc", "10.0.0.2:50052"],
            &["-hf", "someone/model"],
            &["--video-ffmpeg-dir", "C:\\Tools\\ffmpeg"],
        ] {
            let err = check_extra_args(EngineKind::Llm, &strings(bad)).unwrap_err();
            assert!(err.starts_with("llm.extraArgs may not contain -"), "{err}");
        }
        for bad in [
            &["-l", "0.0.0.0"][..],
            &["--listen-ip=0.0.0.0"],
            &["--listen-port", "80"],
            &["--serve-html-path", "/"],
            &["--lora-model-dir", "/"],
        ] {
            let err = check_extra_args(EngineKind::Image, &strings(bad)).unwrap_err();
            assert!(
                err.starts_with("image.extraArgs may not contain -"),
                "{err}"
            );
        }
        // "-l" is llama-server's --logit-bias, harmless there.
        assert_eq!(
            check_extra_args(EngineKind::Llm, &strings(&["-l", "15043+1"])),
            Ok(())
        );

        // The spec refuses them as a settings error, which the scheduler
        // does not retry without MTP.
        let fx = fixture(true);
        let mut cfg = Config::default();
        cfg.llm.extra_args = strings(&["--host", "0.0.0.0"]);
        let err = llama_spec(&fx.paths, &cfg, &fx.sel, at(1), "k", true).unwrap_err();
        assert!(err.downcast_ref::<InvalidSettings>().is_some(), "{err:#}");
        cfg.image.extra_args = strings(&["-l", "0.0.0.0"]);
        let err = sd_spec(&fx.paths, &cfg, &fx.sel, at(1)).unwrap_err();
        assert!(err.downcast_ref::<InvalidSettings>().is_some(), "{err:#}");
        let mut cfg = Config::default();
        cfg.llm.gpu_layers = "most".into();
        let err = llama_spec(&fx.paths, &cfg, &fx.sel, at(1), "k", true).unwrap_err();
        assert!(err.downcast_ref::<InvalidSettings>().is_some(), "{err:#}");
    }

    #[test]
    fn gpu_layer_values() {
        assert_eq!(gpu_layers("all").unwrap(), "999");
        assert_eq!(gpu_layers("ALL").unwrap(), "999");
        assert_eq!(gpu_layers(" 33 ").unwrap(), "33");
        assert_eq!(gpu_layers("auto").unwrap(), "auto");
        assert!(gpu_layers("lots").is_err());
        assert!(gpu_layers("-1").is_err());
    }

    /// Random loopback addresses, never the well-known 127.0.0.1, and free.
    #[test]
    fn engine_addresses_are_random_loopback_addresses() {
        let a = engine_addr().expect("an address");
        let b = engine_addr().expect("an address");
        for addr in [a, b] {
            assert!(addr.ip().is_loopback(), "{addr}");
            assert_ne!(addr.port(), 0);
            TcpListener::bind(addr).expect("the port is free");
        }
        // Only a system without the whole 127/8 range falls back to 127.0.0.1.
        if a.ip() != &Ipv4Addr::LOCALHOST {
            assert_ne!(a.ip(), b.ip(), "24 random bits");
        }
        for _ in 0..1000 {
            let ip = random_loopback();
            assert!(ip.is_loopback());
            assert!(![
                Ipv4Addr::new(127, 0, 0, 0),
                Ipv4Addr::LOCALHOST,
                Ipv4Addr::new(127, 255, 255, 255)
            ]
            .contains(&ip));
        }
    }

    #[test]
    fn api_key_stays_out_of_logs() {
        let spec = LaunchSpec {
            kind: EngineKind::Llm,
            program: PathBuf::from("/opt/llama server/llama-server"),
            args: strings(&["-m", "a b.gguf", "--api-key", "s3cret", "--api-key=s3cret2"]),
            cwd: PathBuf::from("/opt"),
            env: vec![
                (LLAMA_API_KEY_ENV.into(), "s3cret".into()),
                ("OTHER".into(), "s3cret".into()),
                ("CUDA_VISIBLE_DEVICES".into(), "0".into()),
            ],
            addr: at(1),
            api_key: Some("s3cret".into()),
            health_path: LLAMA_HEALTH_PATH,
            log_file: PathBuf::from("llm.log"),
        };
        let debug = format!("{spec:?}");
        assert!(!debug.contains("s3cret"), "{debug}");
        assert!(debug.contains("CUDA_VISIBLE_DEVICES"), "{debug}");
        let line = command_line(&spec);
        assert_eq!(
            line,
            "\"/opt/llama server/llama-server\" -m \"a b.gguf\" --api-key <redacted> --api-key=<redacted>"
        );
    }
}
