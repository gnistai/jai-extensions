# JAI Studio: design and contracts

JAI Studio is one executable, `jai` (`jai.exe` on Windows). It is:

- the JAI command-line tool (validate, render, encode, plus model and server commands);
- a desktop app for viewing `.jai` files and encoding images into `.jai` documents;
- a tray service that runs a local model server;
- a local model server on 127.0.0.1 with two OpenAI-style endpoints: LLM chat
  completions and image generations.

Behind the server, Studio supervises two upstream engines:

- llama.cpp's `llama-server`, running Qwen3.8-27B (GGUF UD-IQ3_XXS plus its vision projector);
- stable-diffusion.cpp's `sd-server`, running FLUX.1 schnell.

The engines and the models are downloaded on first run, after the user clicks a button.

This document is the contract between the parts. Each crate, module and file below has
one owner. Public signatures written here are fixed: change them only by changing this
document first.

Layers, as in the rest of jai-tools:

| Layer | What lives here |
|---|---|
| Spec | Nothing. Studio is an app; it implements no new normative behaviour. |
| Library | The TypeScript JAI library (`src/jai`). Unchanged. Studio runs it inside an embedded JavaScript engine. |
| Engines | TypeScript: the encoder engine plus new local providers (`src/renderers/local.ts`, `src/encoder/local-providers.ts`, `src/providers/openai-chat.ts`). Rust: scheduler, process supervision, downloader, JS host. |
| Apps | The `jai` executable (CLI, GUI, tray, server), the webview UI, and the Node CLI's new `--local` flag. |

## 0. Names and constants

- Binary name: `jai`. Product name: JAI Studio. Tauri identifier: `ai.gnist.jai`.
- Default server address: `127.0.0.1:5240`. If 5240 is busy, try 5241 to 5249. A server on
  127.0.0.1 also listens on `[::1]` at the same port (and one on ::1 on 127.0.0.1), so that
  `localhost` reaches it whichever address it resolves to and no other process can listen there;
  a port where either address is taken counts as busy. A system without that address is fine.
- Environment overrides: `JAI_HOME` (data root), `JAI_PORT`, `JAI_LOG` (tracing filter).
- Model ids in the API: `qwen3.8-27b` (LLM) and `flux-schnell` (image).
- The document model id stays `black-forest-labs/flux-schnell`, the same as the Replicate
  renderer, so `.jai` files encoded locally name the same model.
- Rust: edition 2021, stable toolchain (1.99), MSRV not pinned.
- TypeScript: as in jai-tools (ES2022, strict).

## 1. Repository layout

```
jai-tools/
  src/
    providers/openai-chat.ts        OpenAIChatClient (fetch-based, injectable fetch)
    renderers/local.ts              LocalRenderer implements Renderer
    encoder/local-providers.ts      LocalAnalyst implements Analyst
    studio/host-entry.ts            entry bundled to dist/studio/host.mjs (runs inside QuickJS)
    studio/ui/                      webview UI: index.html, main.ts, api.ts, *.ts, style.css
  scripts/build.mjs                 also builds dist/studio/host.mjs, dist/studio/ui/*, dist/studio/conformance.mjs
  test/local.test.mjs               local providers with mocked fetch
  test/studio-host.test.mjs         host-entry functions in Node with mocks
  bin/jai.mjs                       Node CLI; gains --local
  studio/
    DESIGN.md                       this file
    README.md                       user and developer guide, API reference, security, licences
    Cargo.toml                      workspace
    catalog.json                    pinned downloadable components (embedded by jai-core)
    crates/
      jai-core/                     paths, config, token, catalog, downloader, LM Studio reuse, GPU info
      jai-engines/                  engine processes, launch commands, scheduler
      jai-server/                   axum HTTP server, auth, discovery, the shared Core
      jai-js/                       rquickjs host: web APIs, image ops, validate/render/encode
    app/                            the `jai` binary: CLI (clap), GUI (Tauri), tray, Windows integration
      build.rs, tauri.conf.json, jai.manifest, icons/, capabilities/
    scripts/
      build-windows.sh              cross-compile jai.exe from Linux/WSL (cargo-xwin)
      e2e-windows.sh                GPU end-to-end checks driving jai.exe
      notices.sh, notices/          third-party licence notices (cargo-about config and template)
```

Build outputs:
- `jai-tools/dist/studio/host.mjs`: the QuickJS bundle, embedded by jai-js with `include_str!`.
- `jai-tools/dist/studio/ui/`: the webview assets, embedded by Tauri through `frontendDist`.
- `jai-tools/dist/studio/conformance.mjs`: the conformance harness bundle; tests only.
- `jai-tools/dist/studio/jai.exe`: copied there by `scripts/build-windows.sh`.

Run `npm run build` in jai-tools before any cargo build. The Rust build fails with a clear
message if `dist/studio/host.mjs` is missing.

## 2. Data directory (jai-core::paths)

Where the root is:
- Windows: `%LOCALAPPDATA%\JAI`.
- Linux: `$XDG_DATA_HOME/jai`, or `~/.local/share/jai` when that is unset.
- `JAI_HOME` overrides both.

```
<root>/config.json              settings (section 3)
<root>/token                    bearer token, created on first use, user-only permissions
<root>/server.json              discovery for a running shared server: {"version","pid","port","host","started"}
<root>/gpu.lock                 exclusive file lock held by the one process that owns the engines (Unix: mode 0600)
<root>/models/<file>            model files (catalog `file`)
<root>/models/.verified.json    {file: {size, sha256, mtimeMs}}, so 30 GB is not re-hashed at each start
<root>/engines/<dir>/...        unpacked engine builds; an engine and its CUDA runtime share one dir
<root>/downloads/<file>.part    partial downloads (resumable)
<root>/logs/jai.log, llm.log, image.log    jai.log (GUI and tray) becomes jai.log.1 past 10 MB
<root>/cache/renders/<sha256>.png    viewer render cache, keyed by document identity + seed + size
<root>/cache/notification-icon.png   Windows: the icon of the notification identity (section 10)
<root>/cache/no-ffmpeg/          an empty folder, llama-server's ffmpeg folder (section 6)
<root>/encodings/<yyyymmdd-hhmmss>-<name>/   GUI encode jobs: renders/, report.json, index.html, result .jai
```

Token: `"jai_" + base64url(32 random bytes)`. It is regenerated only on request, or when
other accounts could read it (below). On Unix the file mode is 0600; on Windows it inherits
the root's private access list.

The root is private, wherever `JAI_HOME` puts it (`Paths::resolve` and `Paths::ensure_dirs`):
- Unix: owned by the user, mode 0700; Studio's folders are created 0700.
- Windows: owned by the user, SYSTEM or Administrators, and no allow entry for anyone else. A new
  root gets a protected access list granting full control to exactly the user, SYSTEM and
  Administrators, inherited by everything inside.
- An existing root that other accounts can open is made private when it holds only Studio's
  own entries (the names above, temporary files, `config.json.invalid-*`); a token that other
  accounts could read is then deleted, so a new one is made. Any other such folder is refused
  with a `PermissionDenied` error naming the reason (for example `JAI_HOME` set to a drive root).
- `models/`, `engines/` and `downloads/` may link to another disk, but not to a folder that other
  accounts can change.

## 3. Config (jai-core::config)

`config.json` is camelCase, and every field has a default. Unknown fields are ignored.

```json
{
  "version": 1,
  "server": { "host": "127.0.0.1", "port": 5240, "allowLan": false },
  "idleUnloadMinutes": 15,
  "backend": "auto",
  "llm": { "ctx": 20480, "gpuLayers": "all", "mtp": true, "parallel": 1, "imageMaxTokens": 1536, "extraArgs": [] },
  "image": { "steps": 4, "extraArgs": [] },
  "app": { "closeToTray": true, "startAtLogin": false, "fileAssociation": false }
}
```

- `backend` is `auto | cuda | vulkan | cpu`. `auto` means CUDA when an NVIDIA GPU that can run
  the CUDA 13 llama.cpp builds is detected (Turing or newer, compute capability 7.5, with an R580
  driver or newer; `gpu::cuda_unsupported` says why not), otherwise Vulkan.
- Saving keeps keys Studio does not know. A `config.json` that is not a JSON object (a hand edit
  with a syntax error) loads as the defaults, `Config::load_checked` reports why, and the next
  save first renames it to `config.json.invalid-<unix time>`.
- `llm.extraArgs` and `image.extraArgs` may not set what Studio sets (model, address, key) or
  make an engine serve or write files or reach the network (`jai_engines::check_extra_args`,
  also run by the app's settings check, so `jai serve`, a command's private server and the GUI
  refuse such a `config.json` with the reason). The GUI never takes them from the webview: a
  settings save keeps the values `config.json` holds (`settings::from_form`).
- `server.host` other than a loopback address requires `allowLan: true`. Without it the server
  refuses to start and says why.

## 4. Catalog (studio/catalog.json, jai-core::catalog)

The catalog lists every downloadable component, pinned by URL, size and SHA-256.

```json
{
  "version": 1,
  "components": [
    {
      "id": "llm-model",
      "role": "llm-model",
      "label": "Qwen3.8 27B (UD-IQ3_XXS)",
      "file": "Qwen3.8-27B-UD-IQ3_XXS.gguf",
      "url": "https://huggingface.co/unsloth/Qwen3.8-27B-GGUF/resolve/<commit>/Qwen3.8-27B-UD-IQ3_XXS.gguf",
      "size": 10934860704,
      "sha256": "c0b7…",
      "license": "Apache-2.0",
      "source": "unsloth/Qwen3.8-27B-GGUF"
    },
    {
      "id": "llama-win-cuda",
      "role": "llm-engine",
      "os": "windows",
      "backend": "cuda",
      "file": "llama-b11669-bin-win-cuda-13.4-x64.zip",
      "unpack": "zip",
      "dir": "llama-b11669-win-cuda",
      "exe": "llama-server.exe",
      "url": "…",
      "size": 0,
      "sha256": "…",
      "license": "MIT",
      "source": "ggml-org/llama.cpp b11669"
    }
  ]
}
```

Roles:

| Role | Meaning |
|---|---|
| `llm-model` | the Qwen model |
| `llm-projector` | the Qwen vision projector (mmproj) |
| `image-diffusion` | the FLUX diffusion model |
| `image-vae` | the FLUX VAE |
| `image-clip-l` | the CLIP-L text encoder |
| `image-t5xxl` | the T5-XXL text encoder |
| `llm-engine` | llama.cpp binaries |
| `llm-runtime` | CUDA runtime DLLs for llama.cpp; unpacks into the same `dir` |
| `image-engine` | stable-diffusion.cpp binaries |
| `image-runtime` | CUDA runtime DLLs for stable-diffusion.cpp; unpacks into the same `dir` |

Model components have no `os` or `backend`; they are needed everywhere.

Fields used only for archives:
- `unpack` is `zip` or `tar.gz`.
- `strip` is the number of leading path components to drop, for tar archives that have a top folder.
- `dir` is the folder under `engines/` to unpack into.
- `exe` is the engine executable inside `dir`.

How components are selected for an OS and backend:
- All model components.
- For each engine role, the component matching that OS and backend.
- The image engine falls back when there is no exact match. On Linux there is no CUDA build of
  stable-diffusion.cpp, so the order there is Vulkan, then CPU. Every OS ends with CPU.
- The same fallback applies to the LLM engine.

Pinned set v1 (sizes in bytes; sha256 values are in catalog.json):

| Component | Source | Size |
|---|---|---|
| LLM model | unsloth/Qwen3.8-27B-GGUF @4ca720788d1e01f1bff70c033e0d0028fd02e502, Qwen3.8-27B-UD-IQ3_XXS.gguf | 10934860704 |
| LLM projector | same repo, mmproj-F16.gguf | 927607488 |
| Image diffusion | leejet/FLUX.1-schnell-gguf @c7f665ddaf9f197ff493f41fc211f5480f5f19ac, flux1-schnell-q8_0.gguf | 12801721248 |
| Image VAE | second-state/FLUX.1-schnell-GGUF @8c45a2ba25e2d02bd34230989fb54983f39e44ec, ae.safetensors | 335304388 |
| CLIP-L | comfyanonymous/flux_text_encoders @6af2a98e3f615bdfa612fbd85da93d1ed5f69ef5, clip_l.safetensors | 246144152 |
| T5-XXL | city96/t5-v1_1-xxl-encoder-gguf @005a6ea51a7d0b84d677b3e633bb52a8c85a83d9, t5-v1_1-xxl-encoder-Q8_0.gguf | 5061584064 |
| llama.cpp | ggml-org/llama.cpp b11669: win-cuda-13.4 + cudart-13.4; win-vulkan; win-cpu; ubuntu-cuda-13.4 + cudart; ubuntu-vulkan; ubuntu-x64 | |
| stable-diffusion.cpp | leejet/stable-diffusion.cpp master-970-2f016e8: win-cuda12 + cudart-sd-cu12; win-vulkan; win-cpu; Linux Vulkan; Linux CPU | |

## 5. jai-core

```rust
// paths.rs
#[derive(Clone, Debug)]
pub struct Paths { pub root: PathBuf }
impl Paths {
    pub fn resolve() -> anyhow::Result<Paths>;          // JAI_HOME or the platform default; creates root, private (section 2)
    pub fn at(root: impl Into<PathBuf>) -> Paths;       // for tests
    pub fn config_file(&self) -> PathBuf;  pub fn token_file(&self) -> PathBuf;
    pub fn server_file(&self) -> PathBuf;  pub fn gpu_lock(&self) -> PathBuf;
    pub fn models(&self) -> PathBuf;       pub fn engines(&self) -> PathBuf;
    pub fn downloads(&self) -> PathBuf;    pub fn logs(&self) -> PathBuf;
    pub fn render_cache(&self) -> PathBuf; pub fn encodings(&self) -> PathBuf;
    pub fn ensure_dirs(&self) -> std::io::Result<()>;   // private root and folders, or PermissionDenied (section 2)
}

// config.rs  (serde camelCase; Default implements the values in section 3)
pub struct Config { pub version: u32, pub server: ServerSettings, pub idle_unload_minutes: u32,
                    pub backend: BackendPref, pub llm: LlmSettings, pub image: ImageSettings, pub app: AppSettings }
pub struct ServerSettings { pub host: String, pub port: u16, pub allow_lan: bool }
pub enum BackendPref { Auto, Cuda, Vulkan, Cpu }         // serde lowercase
pub struct LlmSettings { pub ctx: u32, pub gpu_layers: String, pub mtp: bool, pub parallel: u32,
                         pub image_max_tokens: u32, pub extra_args: Vec<String> }
pub struct ImageSettings { pub steps: u32, pub extra_args: Vec<String> }
pub struct AppSettings { pub close_to_tray: bool, pub start_at_login: bool, pub file_association: bool }
impl Config { pub fn load(paths: &Paths) -> Config; pub fn save(&self, paths: &Paths) -> anyhow::Result<()>;
              pub fn load_checked(paths: &Paths) -> (Config, Option<String>) }   // + what was wrong with config.json, for the UI

// token.rs
pub fn load_or_create_token(paths: &Paths) -> anyhow::Result<String>;
pub fn regenerate_token(paths: &Paths) -> anyhow::Result<String>;

// catalog.rs
#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Component { pub id: String, pub role: Role, pub label: String, pub file: String, pub url: String,
    pub size: u64, pub sha256: String, pub license: String, pub source: String,
    pub os: Option<Os>, pub backend: Option<Backend>, pub unpack: Option<Unpack>, pub strip: Option<u32>,
    pub dir: Option<String>, pub exe: Option<String> }
pub enum Role { LlmModel, LlmProjector, ImageDiffusion, ImageVae, ImageClipL, ImageT5xxl,
                LlmEngine, LlmRuntime, ImageEngine, ImageRuntime }          // serde kebab-case
pub enum Os { Windows, Linux }  pub enum Backend { Cuda, Vulkan, Cpu }  pub enum Unpack { Zip, TarGz }
pub struct Catalog { pub version: u32, pub components: Vec<Component> }
pub fn catalog() -> &'static Catalog;                       // parsed once from include_str!("../../../catalog.json")
pub fn current_os() -> Os;
pub fn select(catalog: &Catalog, os: Os, backend: Backend) -> Selection;
pub struct Selection { pub components: Vec<Component>, pub llm_backend: Backend, pub image_backend: Backend }

// gpu.rs
pub struct GpuInfo { pub vendor: GpuVendor, pub name: String, pub vram_total_mb: u64, pub vram_used_mb: u64, pub driver: String,
                     pub compute_cap: Option<String> /* "8.9" */ }
pub enum GpuVendor { Nvidia, Amd, Intel, Unknown }
pub fn detect() -> Option<GpuInfo>;            // nvidia-smi --query-gpu=name,memory.total,memory.used,driver_version,compute_cap --format=csv,noheader,nounits
                                               // (without compute_cap on drivers that reject it)
pub fn effective_backend(pref: BackendPref, gpu: Option<&GpuInfo>) -> Backend;
pub fn cuda_unsupported(gpu: &GpuInfo) -> Option<String>;   // why auto picks Vulkan for this GPU: not NVIDIA, driver < 580, compute capability < 7.5

// install.rs: state of the selected components on disk
pub enum InstallState { Installed, Partial { bytes: u64 }, Missing, Reusable { path: PathBuf } }
pub struct ComponentStatus { pub component: Component, pub state: InstallState, pub path: PathBuf }
pub fn status(paths: &Paths, sel: &Selection) -> Vec<ComponentStatus>;   // cheap: size + .verified.json, no hashing
pub fn ready(paths: &Paths, sel: &Selection) -> bool;                     // all Installed
pub fn model_path(paths: &Paths, sel: &Selection, role: Role) -> Option<PathBuf>;
pub fn engine_exe(paths: &Paths, sel: &Selection, role: Role) -> Option<PathBuf>;   // LlmEngine | ImageEngine
pub fn reuse_links(paths: &Paths, candidate: &Path) -> bool;  // a Reusable file is hard-linked (same disk, no other account can change it), else copied

// download.rs
pub struct Progress { pub id: String, pub state: DlState, pub bytes: u64, pub total: u64, pub speed_bps: u64, pub error: Option<String> }
pub enum DlState { Queued, Reusing, Downloading, Verifying, Unpacking, Done, Failed, Canceled }   // serde lowercase
pub struct DownloadOptions { pub reuse: bool, pub concurrency: usize /* 3 */ }
pub async fn install(paths: &Paths, components: &[Component], opts: DownloadOptions,
                     progress: impl Fn(Progress) + Send + Sync + 'static,
                     cancel: tokio_util::sync::CancellationToken) -> anyhow::Result<()>;
pub fn disk_free(paths: &Paths) -> Option<u64>;
pub fn bytes_needed(paths: &Paths, sel: &Selection) -> u64;

// reuse.rs (LM Studio and similar)
pub fn find_reusable(component: &Component) -> Option<PathBuf>;   // same file name + size in LM Studio's folders:
    // its settings' downloadsFolder, <its home>/models (~/.lmstudio, or ~/.lmstudio-home-pointer), ~/.cache/lm-studio/models
```

How `install` works:
- Components that are already installed are skipped.
- Reuse comes first, when `reuse` is set and `find_reusable` finds a candidate:
  1. When `reuse_links` allows a link: hash the candidate with progress, in state `reusing`, and
     if it matches, hard-link it into `models/` (copy when the link fails). Otherwise (another
     disk, or a file other accounts could change after it was hashed) copy it through the part
     file, hashing what is copied, and keep it only if it matches.
  2. Record it in `.verified.json`.
- Otherwise, download to `downloads/<file>.part`:
  - Resume with `Range: bytes=<len>-`. A 206 response appends; a 200 restarts the file.
  - Hash incrementally. On resume, hash the existing part first.
  - Retry three times with backoff.
  - Send `User-Agent: jai-studio/<version>` and nothing that identifies the user.
- Then verify the size and SHA-256. On a mismatch, delete the part file and fail that component.
- Models: rename into `models/<file>` and record them in `.verified.json`.
- Archives: unpack into `engines/<dir>/`.
  - Reject any entry with an absolute path, a drive letter, `..`, or a path that would leave
    the folder (zip-slip).
  - Apply `strip`.
  - Keep Unix modes from tar.
  - Write `engines/<dir>/.ok-<id>` holding the sha256, and delete the archive.
- Before starting, check free disk space against the remaining bytes plus 1 GiB. If short, fail
  with a clear message.

## 6. jai-engines

```rust
pub enum EngineKind { Llm, Image }                        // serde lowercase
pub enum EngineState { Stopped, Starting, Ready, Stopping, Failed }

// launch.rs: building engine commands from Paths + Config + Selection
pub struct LaunchSpec { pub kind: EngineKind, pub program: PathBuf, pub args: Vec<String>, pub cwd: PathBuf,
                        pub env: Vec<(String, String)>, pub addr: SocketAddrV4, pub api_key: Option<String>,
                        pub health_path: &'static str, pub log_file: PathBuf }
pub fn llama_spec(paths: &Paths, cfg: &Config, sel: &Selection, addr: SocketAddrV4, api_key: &str, mtp: bool) -> anyhow::Result<LaunchSpec>;
pub fn sd_spec(paths: &Paths, cfg: &Config, sel: &Selection, addr: SocketAddrV4) -> anyhow::Result<LaunchSpec>;
pub fn engine_addr() -> std::io::Result<SocketAddrV4>;  // random 127.x.y.z (24 bits, never 127.0.0.1) + free port: bind :0, read, drop
pub fn check_extra_args(kind: EngineKind, args: &[String]) -> Result<(), String>;   // see "extraArgs" below

// process.rs: supervision
pub struct EngineProcess { /* child, job object (Windows), log ring, address, api key */ }
impl EngineProcess {
    pub async fn start(spec: LaunchSpec) -> anyhow::Result<EngineProcess>;
    pub async fn wait_ready(&mut self, timeout: Duration) -> anyhow::Result<()>;   // fails early if the child exits
                                                                                  // or another process answered
    pub fn base_url(&self) -> String;  pub fn api_key(&self) -> Option<String>;
    pub fn exited(&mut self) -> bool;  pub fn log_tail(&self, n: usize) -> Vec<String>;
    pub async fn stop(self);
}

// Launcher abstraction, so the scheduler can be tested with fake engines
#[async_trait::async_trait]
pub trait Launcher: Send + Sync + 'static {
    async fn launch(&self, kind: EngineKind, attempt: LaunchAttempt) -> anyhow::Result<Box<dyn RunningEngine>>;
}
pub struct LaunchAttempt { pub mtp: bool, pub cancel: tokio_util::sync::CancellationToken }   // Clone, Debug, Default
// The scheduler cancels `cancel` when it gives up on a start (shutdown, start_timeout); the launcher then
// stops what it started and returns. 20 s after the cancel the launch future is dropped (which must kill).
pub struct NotInstalled(pub String);  pub struct InvalidSettings(pub String);  pub struct StartError { pub message: String, pub log: Vec<String> }
#[async_trait::async_trait]
pub trait RunningEngine: Send + Sync {
    fn base_url(&self) -> String;  fn api_key(&self) -> Option<String>;
    fn exited(&mut self) -> bool;  fn log_tail(&self, n: usize) -> Vec<String>;
    async fn stop(self: Box<Self>);
}
pub struct ProcessLauncher { /* paths, config snapshot, selection */ }
impl ProcessLauncher { pub fn new(paths: Paths, cfg: Config, sel: Selection) -> Self }

// scheduler.rs
pub struct SchedulerConfig { pub max_llm: usize /*1*/, pub max_image: usize /*1*/, pub max_batch_wait: Duration /*120 s*/,
                             pub idle_unload: Option<Duration>, pub start_timeout: Duration /*240 s*/, pub mtp: bool }
pub struct Scheduler { /* actor handle */ }
impl Scheduler {
    pub fn new(launcher: Arc<dyn Launcher>, cfg: SchedulerConfig) -> Arc<Scheduler>;
    pub async fn acquire(self: &Arc<Self>, kind: EngineKind) -> Result<Lease, EngineError>;   // cancel-safe
    pub fn status(&self) -> SchedulerStatus;
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<SchedulerStatus>;
    pub async fn unload(&self);       // stop the loaded engine once active leases end; waiters restart it
    pub async fn shutdown(&self);     // stop everything, fail waiters, no further starts; returns once the engine
                                      // process is gone, a start in progress included
    pub fn set_idle_unload(&self, idle: Option<Duration>);   // counts from when the engine became idle
    pub fn logs(&self, kind: EngineKind, n: usize) -> Vec<String>;
}
pub struct Lease { pub kind: EngineKind, pub base_url: String, pub api_key: Option<String> /* + release-on-drop guard */ }
#[derive(Serialize, Clone)] #[serde(rename_all = "camelCase")]
pub struct SchedulerStatus { pub loaded: Option<EngineKind>, pub state: EngineState, pub active: usize,
                             pub waiting_llm: usize, pub waiting_image: usize, pub mtp: bool,
                             pub last_error: Option<String>, pub swaps: u64, pub since_ms: u64 }
pub enum EngineError { NotInstalled(String), StartFailed { kind: EngineKind, message: String, log: Vec<String> }, ShuttingDown }
```

### llama-server command

```
LLAMA_API_KEY=<random> LLAMA_ARG_VIDEO_FFMPEG_DIR=<root>/cache/no-ffmpeg
llama-server -m <models>/Qwen3.8-27B-UD-IQ3_XXS.gguf --mmproj <models>/mmproj-F16.gguf
  -ngl <gpuLayers: "all" → 999> -c <ctx> -fa on -np <parallel> --no-webui
  --host <127.x.y.z> --port <port> --image-max-tokens <imageMaxTokens>
  [--spec-type draft-mtp]  <extraArgs…>
```

- The working directory is the engine dir, so its DLLs are found there.
- The API key (32 random bytes, hex) goes in the environment, which only the user can read; on
  Linux every account can read a process's command line.
- VRAM check (CUDA only), before every LLM start: the free VRAM, sampled up to three times
  0.7 s apart (an engine that just stopped takes a moment to release it), against the measured
  need: 10,915 MiB + KV (ctx/16 MiB at f16, two thirds of that at q8_0) + 870 MiB for MTP +
  a 500 MiB margin. Enough for MTP: as configured. Enough only with a q8_0 cache: add
  `-ctk q8_0 -ctv q8_0` (unless extraArgs set a cache type). Not enough for MTP: fail the start
  with `NoRoomForMtp`, which the scheduler answers with one start without MTP (rule 4). Not
  enough even without MTP: start anyway and log a warning, because the Windows driver spills the
  excess into shared memory instead of failing, which works but is many times slower.
- `<127.x.y.z>:<port>` comes from `engine_addr`: a random loopback address, so web pages cannot
  find the engine by scanning 127.0.0.1 or rebinding a domain to it.
- `LLAMA_ARG_VIDEO_FFMPEG_DIR` (llama-server's `--video-ffmpeg-dir`) names an empty folder
  that `llama_spec` creates in the private root (`launch::no_ffmpeg_dir`). llama-server
  otherwise runs the first `ffmpeg`/`ffprobe` on PATH for media its own decoders reject; Studio
  re-encodes chat images before they get there (section 7), and this keeps that decoder out of
  reach anyway. It goes in the environment because a build whose server lacks the option
  ignores the variable, where the flag would stop it from starting.
- Health check: `GET /health` returns 200 when the model is ready and 503 while it loads. It
  needs no API key.

### sd-server command

```
sd-server --diffusion-model <flux> --vae <ae> --clip_l <clip_l> --t5xxl <t5>
  --cfg-scale 1.0 --steps <steps> --sampling-method euler --eager-load
  -l <127.x.y.z> --listen-port <port>  <extraArgs…>
```

- The working directory is the engine dir.
- Health check: `GET /v1/models` returns 200. sd-server only starts listening after its context
  exists, and `--eager-load` loads the weights before that.
- The pinned sd-server has no authentication and copies any request `Origin` into its CORS
  headers. The random loopback address keeps web pages out; other accounts on the PC can still
  find and use it while it is loaded (section 12).

### extraArgs

`llm.extraArgs` and `image.extraArgs` are appended to the commands, so they may tune an engine but
`check_extra_args` refuses flags that set what Studio sets or open the engine up, as `--flag value`
and `--flag=value`. A refused flag fails the start with `InvalidSettings` (no MTP retry).
- llama-server: `-m --model -mm --mmproj --host --port --api-key --api-key-file --webui --path
  --media-path --tools --tools-runtime --slot-save-path --log-file --video-ffmpeg-dir --rpc -hf
  -hfr --hf-repo -hff --hf-file -mu --model-url -mmu --mmproj-url -dr --docker-repo`.
- sd-server: `-m --model --diffusion-model --vae --clip_l --t5xxl -l --listen-ip --listen-port
  --serve-html-path --lora-model-dir --embd-dir -o --output --preview-path`.

### Process supervision

On Windows:
- Spawn with `CREATE_NO_WINDOW`. Otherwise a console window opens, because Studio has no console.
- Assign every child to a Job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. If jai dies,
  the engines die with it.

On Linux:
- Set `prctl(PR_SET_PDEATHSIG, SIGKILL)` in `pre_exec`, and give the child its own process group.

On both:
- Read stdout and stderr line by line into a ring of 1000 lines, and into `logs/llm.log` or
  `logs/image.log`.
- `stop`: on Windows, TerminateProcess. On Linux, SIGTERM, then SIGKILL after 3 s.
- VRAM is free when the process is gone. This was measured: immediate on Windows.
- Readiness: the health check must answer 200 (redirects are not followed), and the child itself
  must own the listening socket on its address (Windows: `GetExtendedTcpTable`, Linux:
  `/proc/<pid>/net/tcp` and `/proc/<pid>/fd`). The address is chosen before the engine starts and
  sd-server binds it only after loading its model, so another account could take it first; such a
  start fails rather than sending requests and the key to that process.
- A cancelled start (`LaunchAttempt::cancel`) stops the engine before `launch` returns.

### Scheduler policy

It is a single actor task. Lease drops send a release message. Only one engine is loaded at a
time.

1. **Grant.** If the loaded engine of kind K is Ready, waiters of kind K exist, active(K) is
   below max(K), and no swap is pending, grant the oldest waiter of kind K.
2. **Swap pending.** Mark a swap pending when waiters of the other kind K' exist and either:
   - there are no waiters of kind K; or
   - the oldest K' waiter has waited longer than `max_batch_wait`.

   While a swap is pending, grant nothing to K. Once active(K) reaches 0, stop K and start K'.
3. **First start.** When nothing is loaded and waiters exist, start the kind of the oldest waiter.
4. **Start failure.**
   - If the LLM fails to start with MTP on, retry once with MTP off. Remember that for the rest
     of the session; status reports `mtp: false`.
   - `NoRoomForMtp` (too little free VRAM, section 6) is not a failure: start once without MTP,
     report `mtp: false` while that engine runs, and try MTP again at the next start.
   - Otherwise, fail every waiter of that kind with `StartFailed`, including the last 40 log
     lines, and set state Failed. The next acquire tries again.
5. **Crash.** If a Ready engine exits, set state Failed. In-flight requests fail upstream; the
   next acquire restarts it.
6. **Idle.** When nothing is active or waiting for `idle_unload`, stop the engine.
7. **Unload.** `unload()` stops the engine once active leases end. `shutdown()` stops it and
   rejects all waiters. A start in progress is cancelled and waited for (at most 20 s, then
   dropped), so `shutdown()` returns only once no engine process holds the GPU. A start that hits
   `start_timeout` is cancelled and waited for the same way before the failure (or the MTP retry)
   goes ahead.

The encoder keeps up to four renders and up to four inspections in flight (its default
`concurrency`; each kind has its own slots). Each kind's queue stays filled until that kind of
work runs out for the generation, so a generation still costs two swaps: renders on the image
engine, then inspections and the proposal on the LLM. A request waits only for the few requests
ahead of it, never for a whole generation, because that wait would count against the client's
time limits.

## 7. jai-server: HTTP API (normative for Studio)

The full API reference also goes in `studio/README.md`.

```rust
pub struct Core { pub paths: Paths, pub config: Arc<RwLock<Config>>, pub token: Arc<RwLock<String>>,
                  pub selection: Selection, pub scheduler: Arc<Scheduler>, pub http: reqwest::Client }
impl Core { pub fn new(paths: Paths, cfg: Config) -> anyhow::Result<Arc<Core>>; }    // builds the ProcessLauncher + Scheduler
pub struct ServerHandle { pub addr: SocketAddr, /* shutdown sender, join handle */ }
impl ServerHandle { pub fn url(&self) -> String; pub async fn stop(self); }
pub async fn start(core: Arc<Core>) -> anyhow::Result<ServerHandle>;    // binds, writes server.json, serves in a task
pub fn router(core: Arc<Core>, port: u16) -> axum::Router;              // for tests
pub mod discovery {
    pub struct ServerInfo { pub version: String, pub pid: u32, pub host: String, pub port: u16, pub started: u64 }
    pub fn write(paths: &Paths, info: &ServerInfo) -> anyhow::Result<()>;
    pub fn remove(paths: &Paths);
    pub async fn find(paths: &Paths) -> Option<(ServerInfo, String /* token */)>;   // server.json + token + GET /health proof
}
pub mod gpu_lock { pub struct GpuLock; pub fn try_acquire(paths: &Paths) -> anyhow::Result<Option<GpuLock>>; }   // fs4 exclusive lock
```

### Rules for every request

- **Host header.** Must be `127.0.0.1:<port>`, `localhost:<port>` or `[::1]:<port>`, plus the
  bound address when it is another one (a LAN address with `allowLan`, or another loopback
  address such as 127.0.0.2); bound to 0.0.0.0 or `::`, any IP address on the port. Otherwise 403
  `bad_host`. This defends against DNS rebinding.
- **Connections.** HTTP/1.1. A request head must arrive within 30 s, and a keep-alive
  connection idle for 30 s is closed. At most 256 connections are served at once; others wait to
  be accepted. Other local users cannot exhaust the process's file descriptors this way.
- **Origin header.** If present, it must start with `chrome-extension://`, `moz-extension://` or
  `safari-web-extension://`. Otherwise 403 `bad_origin`. Studio never sends CORS headers; browser
  extensions with host permissions don't need them, and web pages must not get in.
- **Authentication.** Everything under `/v1/` needs `Authorization: Bearer <token>`. Otherwise
  401 `unauthorized`. `/health` is open but still checks Host.
- **Body limits.** 64 MiB for `/v1/chat/completions`, 1 MiB for everything else.
- **Errors** use OpenAI's shape:
  `{"error": {"message": "...", "type": "invalid_request_error|auth_error|server_error|unavailable_error", "code": "..."}}`.
- When the models are not installed, the response is 409 `models_missing`, with the message
  "Download the models first: open JAI Studio or run `jai models download`."

### `GET /health`

Returns 200 `{"status":"ok","name":"jai","version":"<semver>"}`.

With `?nonce=<16 to 128 base64url characters>` (once; otherwise 400 `invalid_nonce`) the answer
also carries `"proof"`: base64url, without padding, of HMAC-SHA256 under the token of
`jai-health:<port>:<nonce>`, where `<port>` is the server's own bound port. `discovery::find`
sends a fresh nonce and uses a server only when its proof matches the token file's token, so it
never sends the token to a process that took a crashed server's port, to another user's server,
or to a CLI command's private server (section 10), which has a token of its own. The proving
answer also carries `"pid"`, and `find` reports that pid: a server that took over the port
of a killed one proves the token before it rewrites `server.json`.

### `GET /v1/models`

```json
{"object":"list","data":[
  {"id":"qwen3.8-27b","object":"model","owned_by":"jai","kind":"llm","installed":true},
  {"id":"flux-schnell","object":"model","owned_by":"jai","kind":"image","installed":true,"aliases":["black-forest-labs/flux-schnell"]}]}
```

### `GET /v1/status`

```json
{"version":"…","engines":<SchedulerStatus>,"installed":true,"gpu":{"name":"…","vramUsedMb":0,"vramTotalMb":0}|null,"backend":"cuda"}
```

### `POST /v1/chat/completions`

The body is OpenAI's chat-completions body, which Studio checks and then forwards to
llama-server.

What is checked:
- `messages` is required.
- `model` must be absent, `qwen3.8-27b` or `default`.
- `n` must be absent or 1.
- `max_tokens` and `max_completion_tokens` are clamped to 8192.
- `reasoning_effort` must be one of `none`, `low`, `medium`, `high` or `xhigh`.
- Image parts must be `data:image/(png|jpeg|gif|bmp);base64,…`. Anything else is a 400: no
  remote URLs, no file paths, no WebP. At most 8 images.
- Each image must decode in full as its declared type, within 40 MB and 64 megapixels (checked
  from its header first). Otherwise 400 `invalid_image`.

How it is forwarded:
- `stream: true` is passed through as SSE.
- `chat_template_kwargs`, `response_format`, `temperature`, `top_p`, `top_k`,
  `presence_penalty`, `seed` and `stop` pass through unchanged.
- The `model` field in the response is set to `qwen3.8-27b`.
- Images go on decoded and encoded again, EXIF orientation applied: a JPEG as a JPEG (quality
  95), PNG, GIF (its first frame) and BMP as PNG. llama-server thus never sees bytes that its
  image decoder rejects, which it would hand to ffmpeg. This happens before the GPU is asked for,
  two requests at a time.
- If the stream breaks off (the engine stops, or Studio shuts down), an unfinished line is
  dropped and an unfinished event is ended, so the error event that follows parses on its own.

When the client disconnects, the GPU wait ends (the lease is dropped) and the upstream request
is aborted.

### `POST /v1/images/generations`

```json
{ "prompt": "…", "model": "flux-schnell", "n": 1, "size": "1344x768", "width": 1344, "height": 768,
  "seed": 1, "steps": 4, "output_format": "png", "output_compression": 90, "response_format": "b64_json" }
```

Request rules:
- `prompt`: 1 to 8000 characters. A prompt containing `<sd_cpp_extra_args` is a 400; that tag
  would otherwise inject engine parameters.
- `model`: absent, `flux-schnell`, or `black-forest-labs/flux-schnell`.
- Size: `size` "WxH", or `width` and `height`; the default is 1024x1024. Each side is a multiple
  of 64 from 256 to 2048, and width × height is at most 2,359,296.
- `steps`: 1 to 16. Default `config.image.steps`.
- `n`: 1 to 4.
- `seed`: 0 to 4294967295. Studio draws one with the OS RNG when it is absent, because
  sd-server's default is a fixed 42.
- `response_format`: `b64_json` only; `url` is a 400.
- `output_format`: png, jpeg or webp, default png. `output_compression`: 0 to 100, default 90.

Upstream it calls sd-server's native async API:
1. `POST /sdcpp/v1/img_gen` with
   `{prompt, width, height, seed, batch_count: n, sample_params: {sample_steps, sample_method: "euler", guidance: {txt_cfg: 1.0}}, output_format, output_compression}`.
   It returns 202 with `{id}`.
2. `GET /sdcpp/v1/jobs/{id}` every 250 ms until `completed`, `failed` or `cancelled`.
3. If the client disconnects while the job is queued, `POST /sdcpp/v1/jobs/{id}/cancel`. The
   submission runs in a task that holds the lease, so a client that leaves before sd-server has
   answered it still gets its job cancelled as soon as the id arrives; the lease is released only
   after that.

The timeout is 10 minutes from the request's arrival, waiting for the GPU and starting the
engine included (504 `timeout`).

Response:

```json
{"created":<unix>,"model":"flux-schnell","width":1344,"height":768,"steps":4,"output_format":"png",
 "data":[{"b64_json":"…","seed":1}],"timings":{"queueMs":0,"generateMs":0}}
```

In a batch, image *i* uses seed `seed + i`, which matches sd.cpp.

## 8. jai-js: embedded JavaScript host

It uses rquickjs 0.14 (QuickJS-ng). Each call creates a fresh runtime on its own OS thread,
with an 8 MiB stack, a current-thread tokio runtime, and `set_max_stack_size(4 MiB)`. It
evaluates `dist/studio/host.mjs` as an ES module.

JavaScript that runs 30 s without a pause (waiting for a fetch or a timer is a pause) is
interrupted and the call fails: a document crafted to make the library slow, such as one that
makes a regular expression backtrack, cannot hold a thread and a core. The interrupt handler
keeps the time; QuickJS calls it while regular expressions run too.

```rust
pub struct ServerRef { pub url: String, pub token: String }
pub enum DocInput { Bytes(Vec<u8>), Text(String) }        // a .jai file (the file form only), or text: a data:image/jai URL or JAI text (see validate below)
pub fn validate(input: DocInput) -> anyhow::Result<ValidateResult>;
pub async fn render(input: DocInput, opts: RenderOptions) -> anyhow::Result<RenderOutput>;
pub async fn encode(opts: EncodeOptions, sink: EncodeSink, cancel: CancellationToken) -> anyhow::Result<EncodeOutput>;
pub struct EncodeSink { pub on_event: Arc<dyn Fn(serde_json::Value) + Send + Sync>, pub render_dir: PathBuf }
// ValidateResult, RenderOptions, RenderOutput, EncodeOptions, EncodeOutput: serde mirrors of the TS types below (camelCase).
pub mod imageops {
    pub struct Thumb { pub width: u32, pub height: u32, pub data: Vec<u8> }                  // RGBA
    pub fn thumbnail(bytes: &[u8]) -> anyhow::Result<Thumb>;       // decode (PNG, JPEG, WebP, GIF), to sRGB by an embedded ICC profile, orient, centre-square crop, 16×16 (CatmullRom)
    pub struct PreparedTarget { pub jpeg: Vec<u8>, pub data_url: String, pub width: u32, pub height: u32, pub thumb: Thumb }
    pub fn prepare_target(bytes: &[u8]) -> anyhow::Result<PreparedTarget>;   // ≤ 64 MP and ≤ 40 MB in; sRGB by an embedded ICC profile; EXIF orientation; ≤ 1024 px long side; JPEG q85; no metadata
    pub fn preview_jpeg(bytes: &[u8], max_side: u32) -> anyhow::Result<Vec<u8>>;
    pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str>;
}
```

### Globals the host installs

They must behave like the WHATWG and Web versions wherever the bundles use them.

- `console.log`, `info`, `warn`, `error` and `debug` go to `tracing`.
- `setTimeout`, `clearTimeout`, `setInterval` and `clearInterval`, implemented on tokio.
- `fetch(input, init)`:
  - Only the origin of `ServerRef.url` is allowed; anything else is a TypeError "fetch blocked".
  - Methods: GET and POST. Headers can be an object, an array of pairs or a `Headers`. The body
    can be a string, a Uint8Array or an ArrayBuffer.
  - `init.signal` is honoured. On abort, it rejects with a DOMException-like error whose `name`
    is `AbortError`.
  - It returns a `Response` with `ok`, `status`, `statusText`, `headers.get()`, `text()`,
    `json()` and `arrayBuffer()`.
- `Headers`.
- `AbortController` and `AbortSignal`, with `aborted`, `reason`, `addEventListener` and
  `removeEventListener` for the "abort" event, `onabort` and `throwIfAborted`.
- `TextEncoder` and `TextDecoder`: utf-8 only. `fatal: true` throws a TypeError on malformed
  input. `ignoreBOM` is supported.
- `URL` and `URLSearchParams`, backed by the Rust `url` crate (WHATWG). They support `href`,
  `origin`, `protocol`, `username`, `password`, `host`, `hostname`, `port`, `pathname`,
  `search`, `hash`, `searchParams`, `toString` and `toJSON`, and URLSearchParams `get`,
  `getAll`, `has` and iteration.
- `crypto.getRandomValues`, `crypto.randomUUID`, and `crypto.subtle.digest('SHA-256', data)`.
- A synthetic module `node:zlib` with `deflateRawSync(bytes, {level})` and `inflateRawSync`.
  The library's gzip writer imports it when CompressionStream is absent, so `.jai` output is
  compressed.
- A synthetic module `node:crypto` with `{ webcrypto: crypto }`.
- QuickJS provides `atob`, `btoa`, `queueMicrotask` and `performance` itself.

### Host functions the bundle calls (`globalThis.__jai`)

- `thumbnail(bytes: Uint8Array) -> {width, height, data: Uint8Array}`: `imageops::thumbnail`.
- `emit(json: string)`: one encode event; Rust parses it and calls `sink.on_event`.
- `saveRender(name: string, bytes: Uint8Array)`: writes `<render_dir>/<name>`. The name must
  match `^[A-Za-z0-9._-]{1,120}$`.
- Rust calls `globalThis.__jaiAbort()` when the cancellation token fires. host-entry wires that
  to an AbortController.

### TypeScript entry (src/studio/host-entry.ts → dist/studio/host.mjs)

```ts
export const version: string;
export function validate(input: DocInput): ValidateResult;
export async function render(input: DocInput, opts: RenderOptions): Promise<RenderOutput>;
export async function encode(opts: EncodeHostOptions): Promise<EncodeOutput>;

type DocInput = { bytes: Uint8Array } | { text: string };   // a .jai file's bytes, or text: a data:image/jai URL or JAI text
interface ServerRef { url: string; token: string }
interface Diag { code: string; key?: string; requested?: string; actual?: string; reason?: string }
interface ValidateResult { ok: boolean; fatal: boolean; text: string | null; diagnostics: Diag[] }
interface RenderOptions { server: ServerRef; seed?: string | null; outputFormat?: 'png' | 'jpeg' }
interface RenderOutput { bytes: Uint8Array; mime: string; model: string; modelLabel: string; sentPrompt: string; seed: number;
                         width: number; height: number; description: string; documentText: string; diagnostics: Diag[] }
interface EncodeHostOptions { server: ServerRef; target: { url: string; width: number; height: number };
  targetThumbnail: { width: number; height: number; data: Uint8Array };
  generations: number; population: number; seeds: number[]; restarts: number; goal: number;
  timeLimitMs?: number; renderScale?: number; thinking?: 'off' | 'propose' | 'all'; sourceName?: string }
interface EncodeOutput { text: string; jai: Uint8Array; stopReason: string; seconds: number;
  error: string | null /* why a run with a result stopped early, e.g. with stopReason 'error'; else null */;
  best: { id: string; fitness: number }; report: unknown /* same shape as the Node CLI's report.json; renders by file name */ }
```

What each function does:
- `validate` mirrors `bin/jai.mjs validate`. Bytes are a `.jai` file and are decoded as the
  file form only. Text that, after trimming C0 controls and spaces (as the URL parser does),
  begins with `data:image/jai` and then `,` or `;` (ASCII case-insensitively) is a
  `data:image/jai` URL: `decodeDataURL` decodes its body as the text form, and
  `validateDataURL` adds the URL's own advisories (`data-url-parameter`,
  `unencoded-url-text`). A URL that carries no document (the data: URL processor fails: no
  `,`, or a `;base64` body that is not base64) throws "The document cannot be read: …".
  Any other text, prose that begins with "Data:" and other `data:` URLs included, is parsed
  as the text form. Nothing is sniffed between forms: uncompressed JSON or text in a file
  is `malformed-container`, and JSON in text is prose. The fatal codes are
  `malformed-container` and `invalid-json` (a `.jai` file) and `undecodable` (a `data:`
  body that is not UTF-8 text within the text bound). A fatal result has `text: null` and
  no validator diagnostics (the empty document it decoded to would only report
  `no-prompt`); otherwise `text` is the canonical text. Diagnostics come in this order: the
  decoder's, the URL's, then the validator's on the document.
- `render`:
  1. Read the document as `validate` does, and refuse one with a fatal code: "The document
     cannot be read: <code> (<reason>)".
  2. Interpret the document with a one-model catalog (FLUX schnell, `default: true`) and
     `FAMILIES`.
  3. Render with `LocalRenderer`, with no input image: a `source` is never fetched, so
     `transform` renders as `generate`. A standalone document has no governing ratio, so
     the ratio is the document's `aspect`, else its `preview`'s (`previewRatio`, section
     9), else 1:1. The `seed` option (1 to 20 digits, as the `seed` key takes it) replaces
     the document's. Seeds go modulo 2^32; with no seed, the renderer picks one.
  4. Return the exact sent prompt and the diagnostics: the interpreter's, without
     `mode-degraded` (`INTERPRETER_CODES`; the renderer reports `original-not-readable`
     instead), then the renderer's.
- `encode`:
  - Runs `E.encode` with `LocalRenderer` (JPEG output) and `LocalAnalyst`, and the thumbnails
    from `__jai.thumbnail`.
  - `budgetUSD` is 0, with every cost at 0. `concurrency` is the engine's default: four renders
    and four inspections in flight (section 6, scheduler policy). `refine` is false, since there
    is one model.
  - A run that stops on an error after a candidate was scored returns its best, with the
    message in `error`; the engine also sends it as a `log` event, `Stopped: <message>`. A cancel
    during any request stops the run as `canceled`, never as `error`.
  - Each render goes through `__jai.saveRender(name, bytes)` as it completes.
  - Each engine event goes through `__jai.emit`. Renders in events are replaced by
    `{model, seed, file, error, measure, score}`.
  - The `target` event also carries `described`, the measured target in words as the optimiser
    reads it (`describeObservation`); the report carries the same text as `targetDescribed`.
    The CLI prints the one and the contact sheet shows the other; Rust formats neither.

## 9. TypeScript providers

`src/providers/openai-chat.ts`:

```ts
export interface ChatContentPart { type: 'text'; text: string } | { type: 'image_url'; image_url: { url: string } }
export interface ChatMessage { role: 'system' | 'user' | 'assistant'; content: string | ChatContentPart[] }
export interface ChatRequest { messages: ChatMessage[]; max_tokens?: number; temperature?: number; top_p?: number; top_k?: number;
  presence_penalty?: number; response_format?: unknown; chat_template_kwargs?: Record<string, unknown>; seed?: number }
export interface ChatResult { text: string; finishReason: string | null; usage: Record<string, number> | null; timings: Record<string, number> | null }
export class OpenAIChatClient {
  constructor(baseUrl: string, token: string, opts?: { fetch?: FetchLike; timeoutMs?: number });
  complete(req: ChatRequest, opts?: { signal?: AbortSignal }): Promise<ChatResult>;
}
```

Errors raise `ProviderError` with `kind` set to `auth`, `refused`, `provider`, `network`,
`timeout` or `canceled`. The client's own time limits (10 minutes for chat, 15 for images) count
the wait for the engine too, which the encoder keeps short (section 6). The time limits of an
HTTP client underneath (undici's `UND_ERR_HEADERS_TIMEOUT` and `UND_ERR_BODY_TIMEOUT`, behind
Node's fetch) are `timeout` too, not `network`.

`src/encoder/local-providers.ts`:

```ts
export class LocalAnalyst implements Analyst {
  readonly id = 'local/qwen3.8-27b'; readonly costHint = 0;
  constructor(client: OpenAIChatClient, opts?: { thinking?: 'off' | 'propose' | 'all' });
}
```

- It uses the existing prompts (SYSTEM, describePrompt, inspectSystem('xy'), inspectTargetPrompt,
  inspectRenderPrompt, proposePrompt) and parsing (parseAnswer, toDrafts, toObservation,
  toAdditions).
- Every call sends a JSON schema `response_format`:
  - describe and propose (core): `drafts` is an array of exactly n items `{prompt (≤1200 chars),
    style (≤3 strings ≤80), avoid (≤2 strings ≤80)}`, plus `notes` (≤600).
  - propose (refine): `additions`, exactly n strings ≤300, plus `notes`.
  - inspect (target): `subjects` has up to 5 items `{name ≤60, bbox_2d: 4 integers 0–1000}`
    (people are not subjects; a plain portrait may have none), `people` up to 5 items
    `{bbox_2d, face, stands_out ≤80, <trait>…}` where `face` is an enum of `FACES` and every
    trait an enum of its words in `TRAITS` (`src/encoder/people.ts`), each or null;
    `stands_out` makes the model look at the face before judging it and is never read back; `crowd` (`none`, `few`, `some`, `many` or null), `horizon_y` (integer 0–1000 or
    null), `horizon_shape` (`flat`, `curved` or null), and `camera`, `lighting` and `medium`
    (strings ≤120). `bbox_2d` is `[x1, y1, x2, y2]`, Qwen-VL's own field and order (the
    prompts' `xy` convention); the Replicate analysts use `box_2d` `[ymin, xmin, ymax, xmax]`.
  - inspect (render): `subjects` has exactly the given length; each `name` is an enum of that
    subject's name; `bbox_2d` is 4 integers or null. `people` up to 6 items (a render lists
    people down to a twentieth of the frame, so a shrunken person is still paired).
- Thinking defaults to off for every call (`chat_template_kwargs: {enable_thinking: false}`).
  - `thinking: 'propose'` enables it for propose.
  - `thinking: 'all'` enables it for describe and propose. Inspection stays off.
- Sampling:
  - inspection: temperature 0, max_tokens 4096 (the longest answer the schema allows, five
    subjects, six people with every trait and the scene, is about 1,200 tokens as compact JSON
    and about 2,050 indented);
  - describe and propose: temperature 0.7, top_p 0.8, top_k 20, presence_penalty 0.5. max_tokens
    leaves room for the longest answer the schema allows, counted at 3 characters a token plus
    60 tokens per field (n drafts or additions, plus the notes), so no valid answer is cut off.
    With thinking on it is 8192, the server's limit: reasoning counts against max_tokens and
    has no length known in advance.
- Images are sent as data URLs, one per call.

`src/renderers/local.ts`:

```ts
export const LOCAL_FLUX: ModelInfo;   // = REPLICATE flux-schnell entry's ModelInfo fields, costHint 0, same id/label/family/aliases
export class LocalRenderer implements Renderer {
  readonly id = 'local'; readonly label = 'JAI Studio';
  constructor(baseUrl: string, token: string, opts?: { fetch?: FetchLike; outputFormat?: 'png' | 'jpeg'; scale?: number; steps?: number });
  models(): ModelInfo[];
  render(req: RenderRequest): Promise<RenderResult>;
}
export const FLUX_SIZES: Record<string, [number, number]>;
// 1:1 1024x1024, 16:9 1344x768, 21:9 1536x640, 3:2 1216x832, 2:3 832x1216, 4:5 896x1088, 5:4 1088x896,
// 3:4 896x1152, 4:3 1152x896, 9:16 768x1344, 9:21 640x1536
```

- The prompt comes from `assemblePrompt(intent, LOCAL_FLUX, {foldAvoid: true})`, the same text
  Replicate would get.
- The ratio follows the spec's "Size and aspect": the governing ratio, else the intent's aspect,
  else `previewRatio`, else 1:1. It snaps to the nearest `FLUX_SIZES` key by log distance and
  reports `adjusted-value:aspect` when the aspect decided and the snap is more than 0.05 off. `scale` scales the area (width × height × scale², at most the
  server's limit). The size is picked from the sizes the server takes (multiples of 64 from 256
  to 2048): it keeps the FLUX size's ratio within 0.05 (log) and comes closest to that area,
  with a ratio error weighing four times an area error. Rounding each side by itself would
  distort small sizes (16:9 at 0.25 would become 320×256). Scale 1 is the FLUX size itself.
- Seed: `BigInt(seed) mod 2^32` with `adjusted-value:seed` when reduced. With no seed, the
  server picks one and the result reports it.
- Mode: `transform` gives `mode-degraded:mode` (or `original-not-readable:mode` without an
  input image), as the Replicate renderer does.
- The result includes `outputURL` as a `data:` URL, so the analyst needs no btoa.
- `sentInput` is `{prompt, width, height, seed, steps, output_format}`.

`src/renderers/types.ts` and `src/renderers/image-size.ts`:

```ts
interface RenderRequest { intent: RenderIntent; modelId: string; inputImage: InputImage | null;
  governingRatio: number | null; previewRatio: number | null; target: { width: number; height: number } | null; … }
export const INTERPRETER_CODES: ReadonlySet<string>;   // what a viewer shows with a render: all but mode-degraded
export function previewRatio(preview: string | null): number | null;
export function imageSize(bytes: Uint8Array): { width: number; height: number } | null;
export function sniffImage(bytes: Uint8Array): string | null;   // PNG, JPEG, WebP, GIF or AVIF by signature
```

- `previewRatio` is the width/height of a document's `preview`, a raster `data:` URL that is
  never fetched. The size comes from the image header (PNG IHDR, the JPEG frame header, WebP
  VP8/VP8L/VP8X, the GIF screen, the largest AVIF `ispe`), with no decoding and no platform
  API, so the same code runs in QuickJS, Node and browsers. Orientation metadata is not
  applied. Null when there is no preview or its size cannot be read.
- Both renderers choose the ratio in the same order: `governingRatio`, the intent's aspect,
  `previewRatio`, then the model's default.

`bin/jai.mjs` gains `--local`:
- It reads `<root>/server.json` and `<root>/token`, with the root resolved as in section 2.
- It sends the token only to a server that proves it holds it, as `discovery::find` does
  (section 7): the host must be an IP address, and `GET /health?nonce=<32 random bytes>`
  (no redirects) must return the matching `proof`, compared in constant time.
- `encode` then uses LocalRenderer plus LocalAnalyst, and `render` uses LocalRenderer.
- Their requests go over `node:http`, not Node's fetch: undici gives up after 300 s without
  reply headers, and Studio replies only once the request's engine is free and the job is done.
- `encode --local --scale` takes 0.25 to 1.25, as the app does.
- No Replicate token is needed.
- `render` (with or without `--local`) passes no input image and no governing ratio, sets
  `previewRatio` from the document's preview, and prints the interpreter's codes in
  `INTERPRETER_CODES` and then the renderer's, as the host does.

## 10. The `jai` app (studio/app)

The cargo feature `gui` is on by default; without it you get a headless CLI and server. The
`custom-protocol` feature (also default) maps to `tauri/custom-protocol`, so the exe embeds its
frontend.

### CLI (clap)

```
jai                                   GUI (window + tray + server)
jai <path.jai>  |  jai open <path>    GUI viewer on that file (forwarded to a running instance)
jai tray                              start in the tray only (autostart uses this)
jai serve [--host H] [--port P] [--allow-lan]      headless server in the foreground; Ctrl+C stops
jai models status | download [--yes] [--no-reuse] [--only <ids,…>] | verify | path
jai generate "<prompt>" [--size WxH] [--seed N] [--steps N] [--n N] [--format png|jpeg|webp] [--out FILE]
jai chat "<prompt>" [--image FILE]… [--system TEXT] [--think] [--max-tokens N] [--json]
jai render <file.jai|data:image/jai,…|text> [--seed S] [--out FILE]
jai validate <file.jai|data:image/jai,…|text>
jai encode <image> [--generations N=8] [--population N=4] [--seeds 1,2] [--restarts N=1] [--goal F=0.9]
                   [--scale F=1] [--think off|propose|all] [--out F.jai] [--report DIR] [--force]
jai status                            server and engine status
jai token [--regenerate]              print the server URL and token, for clients of the HTTP API
jai licenses                          print the licences of the open-source software in jai
jai --version | --help
```

How a CLI command finds the engines:
- Client commands (generate, chat, render, encode, status) look for a shared server (the GUI's,
  the tray's or `jai serve`) with `discovery::find`. If there is one, they use it over HTTP.
- If there isn't, they take `gpu_lock` and start a temporary server in their own process. When the
  command ends, it stops the engines and that server.
- The temporary server is private to its command: it binds `127.0.0.1` on a free port, carries a
  token of its own (in memory only), and removes its `server.json` entry once it listens. Another
  process holds the token file's token, so even while the entry exists the `/health` proof fails
  and `discovery::find` passes it over. No other command, GUI or client comes to depend on a
  server that stops when its command ends.
- If the lock is held but no shared server is found (another command's temporary server, or a
  server that is still starting), the command waits for the lock, looking again every 500 ms. It
  says once on stderr what it waits for, and keeps an elapsed-time status line on a terminal.
  Ctrl+C stops waiting.
- CLI commands never start Tauri. The single-instance plugin would otherwise forward them to the
  running GUI and exit 0.
- `serve`, the GUI and `tray` take `gpu_lock`. A second GUI launch is forwarded by
  tauri-plugin-single-instance before `gpu_lock` is tried. A second `jai serve` fails with
  "already running (pid, port)" when a shared server answers, else says that another JAI process
  is using the GPU.

Command line rules:
- A document argument (`render`, `validate`) is read as `bin/jai.mjs` reads one. After
  trimming C0 controls and spaces, one that begins with `data:image/jai` and then `,` or `;`
  (ASCII case-insensitively) is a `data:image/jai` URL, passed to the host as text, which
  decodes it as its text body (section 8, `validate`). Otherwise one line ending in `.jai`
  is a `.jai` file, decoded as the file form only; a missing one is a usage error, never a
  prompt. Any other argument, prose that begins with "Data:" and other `data:` URLs
  included, is JAI text. No other file type is read as a document.
- A document that cannot be read exits 1, for `render` as for `validate`: a fatal code
  (`malformed-container`, `invalid-json`, `undecodable`) or a `data:image/jai` URL that
  carries no document. For a fatal code, `validate` prints the decoder's diagnostics (and a
  URL's advisories) without canonical text, and does not validate the empty document they
  leave. `render` refuses any unreadable document before it looks for a server,
  with "The document cannot be read: <reason>" on stderr, which `validate` also prints for
  a URL that carries no document.
- A bare word that names no file and has no extension, given where a document path may go, is a
  mistyped subcommand: a usage error (exit 2) with a suggestion when a subcommand is one edit away
  (two for names over four letters), as `jai stauts` suggests `status`. Paths with a folder or an
  extension still go to the app: a `.jai` file opens in View, which says when it is missing, an
  image opens in Encode, and anything else is refused with a message.
- `render --out` must end in `.png`, `.jpg` or `.jpeg` (or have no extension, for PNG); anything
  else is a usage error, never PNG bytes under another type's name. `render --seed` takes 1 to
  20 digits, as the document's `seed` key does; anything else is a usage error.
- `encode` checks `--out` before it starts: not a folder, not an existing file without
  `--force`, and openable for writing (an existing file) or creatable (a new one; its folder is
  created). At the end it writes `report.json` and `index.html` first, then the `.jai`; when the
  `.jai` cannot be written, the result text is still printed and the command exits 1.
- `chat --image` takes files of at most 40,000,000 bytes, by the server's own count of
  `len / 4 * 3` of the base64 text.

Output:
- Text from documents, models, engines or file names is printed through `term::safe`: C0
  controls other than tab and newline, DEL, C1 controls and the bidirectional embedding, override
  and isolate characters become visible escapes (`\x1b`, `\u{202e}`), so a document cannot
  retitle the terminal, hide lines or set the clipboard. `chat` streams the model's text exactly
  when redirected and through `term::safe` on a terminal; `chat --json` escapes those characters
  as JSON `\uXXXX`, so it stays valid JSON.
- A closed stdout or stderr (`jai models status | head -1`) ends the command quietly with exit
  code 0. Long commands (`encode`, `models download`) keep going and keep writing their files when
  only their output is cut off.

### Windows console

- The exe is a console-subsystem program. Its manifest (`app/jai.manifest`) sets
  `consoleAllocationPolicy` to `detached`, which Windows 11 24H2 and later honour.
- The custom manifest replaces Tauri's default one, so it must keep the Common-Controls v6
  dependency and the usual Tauri manifest content. Without that dependency, startup fails with a
  TaskDialogIndirect error.
- GUI modes (no arguments, `open`, a `.jai` path, `tray`) depend on the console:
  - **No console attached** (`GetConsoleWindow()` is null): run the GUI directly.
  - **Launched from a terminal**: relaunch itself detached (`DETACHED_PROCESS`, with the same
    arguments plus the hidden flag `--gui-child`) and exit 0, so the shell prompt returns.
  - **Older Windows**, where Explorer still allocates a console: if `GetConsoleProcessList`
    returns 1, call `FreeConsole()` and run the GUI. The console flashes briefly.

### GUI (Tauri 2.12)

Window:
- "JAI Studio", 1180×800, minimum 760×560. It is not shown at all in `tray` mode.

Tray:
- The JAI icon, with tooltip "JAI Studio: LLM <state> · Image <state>", updated from
  `scheduler.subscribe()`. While another JAI process (`jai serve`) runs the shared server and the
  app uses it, the tray shows that server's engines instead, from its `GET /v1/status` every 3 s.
- Menu: Open JAI Studio; a separator; a disabled status line; Unload models; Copy server token;
  a separator; Quit. Unload models with another process's engines says so instead.
- Left click shows and focuses the window.
- On Linux, create the tray only if libayatana-appindicator3 (or libappindicator3) can be loaded.

Closing and quitting:
- If `closeToTray` is set, closing the window hides it, and the first time a notification says
  Studio is still running. Otherwise closing quits.
- Quit runs `scheduler.shutdown()`, stops the server, removes server.json and releases gpu.lock.

Notifications (Windows): tauri-plugin-notification shows toasts under the Tauri identifier
`ai.gnist.jai`, and Windows drops the toasts of an unpackaged program whose AppUserModelID is not
registered. At startup the primary instance writes `HKCU\Software\Classes\AppUserModelId\ai.gnist.jai`
(`DisplayName` "JAI Studio", `IconUri` `<root>\cache\notification-icon.png`, written from the
app icon). This is not an opt-in setting: without it no notification shows. A failed
notification is logged as a warning.

Single instance:
- `tauri-plugin-single-instance` is registered first. A second launch passes its arguments; the
  first instance shows its window and opens any `.jai` path in the viewer.
- The plugin calls back from its window procedure, which runs whenever the main thread pumps
  messages, including inside `setup` while the tray or the webview is created. So launches go
  through a `LaunchGate` (managed before any plugin): until setup has finished they are only
  recorded, and the window is shown once at its end if any asked for it; nothing creates a window
  before then. Documents (the first launch's own too, and every forwarded one, in order) are sent
  as `open-path` once the page calls `ui_ready`, or 1 s after a page load for a page that never
  does. A page reload holds them again until the new page is ready.

Log: `logs/jai.log`, rotated to `jai.log.1` past 10 MB while it is written, by the primary
instance only (a forwarded second launch logs a line before it hands over, and must not rename
the file under the running app).

Another process's server: when the app has no server of its own and uses a shared server another
process runs (`jai serve`), `status` returns that server's engines and install state (read at
most every 3 s), `unload_models` fails saying the models belong to that process, and
`engine_logs` reads the end of `logs/llm.log` or `logs/image.log`, which the engines of every
process write.

Linux, before the webview starts:
- If not already set, set `WEBKIT_DISABLE_DMABUF_RENDERER=1`. WebKitGTK has known blank-window
  bugs on NVIDIA.

### Tauri commands

The UI calls these through `window.__TAURI__.core.invoke`, with `withGlobalTauri: true`. Names
are snake_case and arguments camelCase.

| Command | Args | Returns |
|---|---|---|
| `app_info` | – | `{version, dataDir, os, backend, gpu, vcRuntimeOk, configProblem, cudaNote}`; `gpu` includes `computeCap`; `configProblem` is `Config::load_checked`'s message, `cudaNote` is `gpu::cuda_unsupported`'s when the backend is `auto` on an NVIDIA GPU (else null) |
| `server_info` | – | `{url, token, host, port, running, note?}`; `note` says why this app's server is not running, or that another JAI process runs it |
| `regenerate_token` | – | `{token}`; refused while an encode runs (it keeps the token it started with) |
| `status` | – | `{engines: SchedulerStatus, installed: bool}`, of this app's engines or of the other process's server it uses |
| `models_status` | – | `{components: [{id, role, label, size, state, partialBytes, reusablePath, reuseLinks, license, source}], totalBytes, neededBytes, diskFree}`; `reuseLinks` (reusable files only) is `install::reuse_links`: true for a hard link, false for a copy |
| `download_start` | `{reuse: bool}` | `()`; emits `download-progress` (Progress) and `download-done` `{ok, error?}` |
| `download_cancel` | – | `()` |
| `unload_models` | – | `()`; fails, saying so, when another process runs the engines |
| `engine_logs` | `{kind: 'llm'\|'image'}` | `string[]` |
| `settings_get` | – | Config as `config.json` holds it: without `JAI_PORT`, which overrides the port of one run only |
| `settings_set` | `{config}` | the saved Config. Applied: autostart, file association and the idle-unload time (`Scheduler::set_idle_unload`) at once; a port or host change restarts the server, an engine change (backend, llm, image) builds a new core. A port left as stored keeps the port in use. `llm.extraArgs` and `image.extraArgs` are kept as `config.json` holds them, whatever the webview sends, and the settings check (extraArgs included) runs before anything is saved. While an encode runs or the scheduler has active or waiting requests, a change that restarts the server or the engines is refused and nothing is saved |
| `install_vc_runtime` | – | `{outcome: 'declined'\|'finished'\|'started', exitCode}`; Windows only, downloads https://aka.ms/vc14/vc_redist.x64.exe over HTTPS only (redirects included) from a Microsoft host into a new file in a new, randomly named folder under the user's temp folder, and runs it after the user confirms, only if WinVerifyTrust accepts its Authenticode signature and the signer is Microsoft Corporation (the file is held open, read-only to others, from the check until it has started). It waits for the installer (ShellExecuteExW, then the process's exit code: 0 installed, 3010 or 1641 restart needed, 1602 canceled, 1638 another version installed); `declined` when the user said no to the question or to the UAC prompt, `started` when Windows gave no process to wait for. The UI then reads `app_info` again, and keeps doing so for a while when the runtime may still come |
| `read_document` | `{path}` | `{name, path, validate: ValidateResult}` |
| `viewer_render` | `{path, seed?}` | `{dataUrl, width, height, seed, sentPrompt, description, documentText, diagnostics, cached}` |
| `save_file` | `{dataUrl?, bytesB64?, suggestedName}` | `{path}\|null`, through a save dialog (tauri-plugin-dialog); exactly one of a rendered image's data URL and a `.jai` file's bytes |
| `open_dialog` | `{kind: 'jai'\|'image'}` | `{path}\|null` |
| `image_preview` | `{path}` | `{dataUrl, width, height}` (JPEG ≤ 1024 px) |
| `encode_start` | `{path, options: {generations, population, seeds, restarts, goal, scale, thinking}}` | `{jobId, dir}`; emits `encode-event` `{jobId, event}` and `encode-done` `{jobId, ok, text?, jaiPath?, reportDir, error?, stopReason?}`. `error` is why it failed, or, with a result, the host's `EncodeOutput.error` (why it stopped early) |
| `encode_cancel` | `{jobId}` | `()` |
| `reveal` | `{path}` | `()`, opens the folder in Explorer or the file manager |
| `ui_ready` | – | `()`; the page calls it once its `open-path` listener is registered, and the documents that waited are sent |
| `third_party_notices` | – | `string`, the licence notices `jai licenses` prints |

In `encode-event`, render references carry a `thumb` field: a data URL of a JPEG at most 384 px
on the long side, so the UI never reads files.

### Windows integration (opt-in, in settings)

File association, written per user under `HKCU\Software\Classes` with no admin rights:

```
.jai                        (Default) = JAI.Document
.jai\OpenWithProgids        JAI.Document = ""
JAI.Document                (Default) = "JAI image"
JAI.Document\DefaultIcon    (Default) = "<exe>",0
JAI.Document\shell\open\command  (Default) = "<exe>" "%1"
```

Then call `SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_FLUSH)`. Turning the option off removes
these keys.

Start at login: write `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, value `JAI` =
`"<exe>" tray`.

VC++ runtime check: read the version of `System32\msvcp140.dll`. It must be 14.40 or newer, or
the UI shows a warning and an "Install" button.

## 11. The webview UI (src/studio/ui)

Plain TypeScript and DOM, no framework, bundled by esbuild into `dist/studio/ui/main.js`.
`index.html` and `style.css` are copied alongside. All calls go through `api.ts`, a typed
wrapper over invoke and events. When `window.__TAURI__` is missing, `api.ts` uses an in-memory
mock, so the UI can be developed and screenshot in a normal browser.

Tabs: **View**, **Encode**, **Models**, **Server**. The header shows a status pill with the
LLM and Image state.

**Models** (first run opens here):
- The total ("Download models: 30.3 GB") and the free disk space.
- One row per component: label, size, state, and its source and licence.
- "Found in LM Studio: will reuse" when a reusable file exists (with "copied" when
  `reuseLinks` is false), plus a "Reuse files found on this PC" checkbox (on).
- One big button, with per-file progress bars, speed and ETA, and Cancel.
- A VC++ runtime warning with an Install button when it is missing.

**View**:
- A drop zone and an Open button (its dialog shows `.jai` files only). Command-line and
  file-association paths arrive through the `open-path` event. The page registers that
  listener first, then calls `ui_ready`. A `.jai` path goes to View and an image to Encode;
  anything else is refused with a message. `read_document` decodes the file as the file form
  only.
- The rendered image, at its natural aspect, fitted to the panel. Until the first render is
  ready, the document's `preview` stands in for it, scaled to the panel at its own ratio,
  with the description and document text beside it. If that render fails, the preview, or
  without one the description, stays in place of the image with the error; the drop zone
  returns only for a document that could not be read.
- The description text, the exact sent prompt (collapsible), the diagnostics, and the seed.
- Buttons: Render again (new seed), Save image…, Copy HTML (`<img src="name.jai" alt="…"
  width="…" height="…">` with the alt set to the description and the render's size). The src
  carrier needs a non-empty `alt`, so Copy HTML is unavailable for a document with no
  description.

**Encode**:
- A drop zone and an Open button for an image, then a preview.
- Settings: generations, population, seeds, attempts and goal, with the same defaults as the
  CLI.
- Start and Cancel.
- While it runs:
  - the target image with the measured subject and people boxes and the horizon drawn as an SVG
    overlay, and each person's traits listed below it;
  - a small fitness-per-generation chart (inline SVG);
  - the current best candidate's render thumbnails with its measured feedback;
  - a scrolling list of candidates.
- At the end: the result document text, Save .jai…, Open in viewer, and Reveal report folder.

**Server**:
- The URL and token, with Copy and Regenerate, and the state: running, or not running with
  `server_info`'s note saying why (or which other JAI process runs the server).
- Engine state, swaps and MTP, plus Unload models.
- Engine logs, with an llm/image toggle.
- A settings form: idle unload, close to tray, start at login, open .jai files with JAI, port,
  and backend. It shows and saves `config.json` (`settings_get`), so `JAI_PORT` is never saved.
  A port the user types must be 1024 to 65535; the stored port is accepted as it is. It shows
  `app_info`'s `configProblem`, and the engine card its `cudaNote`.
- Open-source licences (`third_party_notices`), read when first opened.

Style: the jai site's tokens (stone neutrals, orange accent #c2410c) as CSS custom properties,
with light and dark themes via prefers-color-scheme. Body text is 15 px system-ui. The UI is
keyboard reachable, with visible focus and labels on every control.

## 12. Security summary

- The server binds 127.0.0.1 by default, and [::1] on the same port so that nobody else can
  listen where `localhost` may lead. Binding anything else needs `--allow-lan` (or the setting),
  the token is still required, and the UI shows a warning.
- Clients (`discovery::find`, `bin/jai.mjs --local`) send the token only to a server that has
  proved it holds the token (the `/health` proof), never to whatever took the port of a server
  that crashed.
- Request heads must arrive within 30 s, idle connections close after 30 s, and at most 256
  connections are served at once.
- The bearer token is required for `/v1/*` and stored with user-only permissions.
- The data root is private to the user wherever `JAI_HOME` puts it (section 2): other accounts
  cannot read the token or replace engines, models or settings.
- Host allow-list (DNS rebinding); Origin rejection for web origins; no CORS headers.
- Engines listen on a random loopback address and port (`127.x.y.z`, 24 random bits), so web
  pages cannot find them by scanning or DNS rebinding. Readiness requires that the engine process
  itself owns the socket. llama-server gets a random API key through its environment, never its
  command line, and an empty folder as its ffmpeg folder (`LLAMA_ARG_VIDEO_FFMPEG_DIR`). Engine
  `extraArgs` cannot rebind, re-key or open up an engine, and the GUI never takes them from the
  webview. The engine HTTP client follows no redirects.
- Known gap: the pinned sd-server has no authentication and reflects any CORS origin, and other
  accounts on the same PC can list loopback listeners. While the image engine is loaded (during
  renders, then up to `idleUnloadMinutes`), another local account can submit, read or cancel
  image jobs on it directly. Closing this needs an sd-server build that requires a key.
- JS host fetch only reaches the local server's origin.
- Downloads use pinned HTTPS URLs with SHA-256 verification and zip-slip-safe extraction.
  Nothing downloaded is executed except the engines and the VC++ redistributable. That one is
  fetched over HTTPS only from a Microsoft host into a fresh private folder, runs only after an
  explicit click, and only when its Authenticode signature is valid and names Microsoft
  Corporation.
- A CLI command's temporary server is private: a token of its own and no `server.json` entry.
- Terminal output never carries a document's, a model's or an engine's control characters
  (section 10, Output).
- Image input limits: 40 MB and 64 MP. Chat images are data: URLs only, so llama-server is never
  asked to fetch a URL (no SSRF), and they are decoded and encoded again before it sees them.
- The JavaScript host interrupts JavaScript that runs 30 s without a pause.
- No telemetry. No identity in any request.

## 13. Tests

Rust, with `cargo test --workspace --exclude jai` on Linux plus `cargo xwin check` for Windows:
- **jai-core:**
  - config defaults and round-trip;
  - catalog selection per OS and backend;
  - the downloader against a local axum file server: resume (206 and 200), hash mismatch,
    zip-slip rejection, strip, cancel;
  - reuse with hard links.
- **jai-engines:**
  - the scheduler with a fake Launcher: batching, the two swaps per generation, anti-starvation,
    idle unload, MTP fallback, crash recovery, cancel-safety, and shutdown;
  - process supervision with a tiny test HTTP program (a test binary).
- **jai-server**, in process with a fake Launcher whose engines are axum mocks of llama-server
  and sd-server:
  - auth, Host and Origin checks;
  - chat validation, forwarding and streaming;
  - images, including size rules, seeds and disconnect-cancel;
  - models_missing.
- **jai-js:**
  - the spec conformance suite (`jai/tests/run.mjs`, bundled as `dist/studio/conformance.mjs`
    with node: shims backed by host functions) must pass all fixtures and the fuzz test inside
    QuickJS;
  - fetch allow-list and abort;
  - validate, render and encode against a mocked local server.

- **jai** (the app, `cargo test -p jai --no-default-features`; Windows-only parts under
  `cargo test` on Windows): CLI parsing and usage errors (mistyped subcommands), terminal-safe
  text, the private server's token, `encode --out` checks and the report-first write order,
  `models verify` (engines found as the install status finds them, damaged models removed),
  settings (JAI_PORT never saved, restarts refused while busy), the launch gate, log rotation and
  tails, the registry values, and the Authenticode check (Windows). `app/tests/cli.rs` runs the
  executable: a closed pipe, a document with terminal escapes, document arguments read as the
  Node CLI reads them, `render` refusing an unreadable document before it looks for a server,
  a mistyped subcommand, `render --out` and `licenses`.

TypeScript: `npm test`, which adds `test/local.test.mjs` and `test/studio-host.test.mjs`.

GPU end to end: `studio/scripts/e2e-windows.sh`, driving the real `jai.exe` on the RTX 5070 Ti.

## 14. Building

- Windows exe from WSL: `studio/scripts/build-windows.sh`.
  1. Run `npm run build`.
  2. With cargo-about installed, write `dist/studio/THIRD-PARTY-NOTICES.txt` (`notices.sh`) and
     set `JAI_THIRD_PARTY_NOTICES` to it; without, warn that the exe must not be given away.
  3. Run `cargo xwin build --release --target x86_64-pc-windows-msvc -p jai`, with the LLVM 23
     tools on PATH and `CARGO_TARGET_DIR=$HOME/.cache/jai-studio-target`.
  4. Copy `jai.exe` to `dist/studio/`.
- Licence notices: `studio/scripts/notices.sh <target> <file>` runs cargo-about 0.9
  (`cargo install cargo-about --locked --features cli`) with `scripts/notices/about.toml` and
  `notices.hbs` over the `jai` package's dependency graph for that target (build and dev
  dependencies left out), after jai-tools' own MIT licence. The embedded JavaScript is jai-tools'
  own code. `app/build.rs` embeds the file named by `JAI_THIRD_PARTY_NOTICES` (a missing or empty
  file fails the build); without the variable, `jai licenses` says the build has no notices.
- Linux:
  - headless: `cargo build --release -p jai --no-default-features`;
  - GUI: `cargo build --release -p jai`, which needs libwebkit2gtk-4.1-dev and
    libayatana-appindicator3-dev.
- CI (`.github/workflows/studio.yml`), both jobs installing cargo-about 0.9.2 and building with
  the notices embedded:
  - windows-latest: native MSVC build and tests; uploads jai.exe and THIRD-PARTY-NOTICES.txt;
  - ubuntu-24.04: apt dependencies, GUI and headless builds, and tests; uploads `jai-gui`,
    `jai-headless` and THIRD-PARTY-NOTICES.txt (the GUI build's graph covers both).
