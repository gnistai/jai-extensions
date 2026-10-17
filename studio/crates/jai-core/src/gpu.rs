//! GPU detection and the effective backend (DESIGN.md sections 3 and 5).

use std::io::Read as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::catalog::Backend;
use crate::config::BackendPref;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuInfo {
    pub vendor: GpuVendor,
    pub name: String,
    pub vram_total_mb: u64,
    pub vram_used_mb: u64,
    pub driver: String,
    /// The CUDA compute capability, such as `"8.9"`; None when the driver is
    /// too old to report it.
    pub compute_cap: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    Unknown,
}

/// The query, with the compute capability.
const QUERY: &str = "--query-gpu=name,memory.total,memory.used,driver_version,compute_cap";
/// The query for drivers too old to know `compute_cap` (before R510).
const QUERY_OLD: &str = "--query-gpu=name,memory.total,memory.used,driver_version";
const FORMAT: &str = "--format=csv,noheader,nounits";

/// How long `nvidia-smi` may take before it is killed.
const TIMEOUT: Duration = Duration::from_secs(3);

/// The CUDA llama.cpp builds in the catalog are CUDA 13, which needs an R580
/// driver or newer ...
const MIN_CUDA_DRIVER: u32 = 580;
/// ... and a Turing or newer GPU: CUDA 13 dropped Maxwell, Pascal and Volta.
const MIN_COMPUTE_CAP: (u32, u32) = (7, 5);

/// Runs `nvidia-smi --query-gpu=name,memory.total,memory.used,driver_version,compute_cap
/// --format=csv,noheader,nounits` (without `compute_cap` if the driver is too
/// old for it), killing it after 3 seconds. With several GPUs it reports the
/// one with the most memory. None when there is no NVIDIA GPU or driver. It
/// blocks, so async code should call it through `spawn_blocking`.
///
/// On Windows, `nvidia-smi.exe` is looked up on PATH, then in System32 (where
/// current drivers put it), then in `Program Files\NVIDIA Corporation\NVSMI`.
pub fn detect() -> Option<GpuInfo> {
    let exe = nvidia_smi()?;
    let query = |fields: &str| {
        let mut command = Command::new(&exe);
        command.args([fields, FORMAT]);
        run_with_timeout(command, TIMEOUT)
    };
    let (output, with_cap) = match query(QUERY) {
        Ok(output) => (output, true),
        // An old driver rejects the compute_cap field; ask without it.
        Err(Run::Failed) => (query(QUERY_OLD).ok()?, false),
        Err(Run::TimedOut | Run::NotStarted) => return None,
    };
    let gpu = parse_smi(&output, with_cap);
    if gpu.is_none() {
        tracing::debug!(output = %output.trim(), "nvidia-smi reported no usable GPU");
    }
    gpu
}

/// `auto` is CUDA with an NVIDIA GPU that can run the CUDA builds (see
/// [`cuda_unsupported`]), otherwise Vulkan. Explicit choices win.
pub fn effective_backend(pref: BackendPref, gpu: Option<&GpuInfo>) -> Backend {
    match pref {
        BackendPref::Cuda => Backend::Cuda,
        BackendPref::Vulkan => Backend::Vulkan,
        BackendPref::Cpu => Backend::Cpu,
        BackendPref::Auto if gpu.is_some_and(|g| cuda_unsupported(g).is_none()) => Backend::Cuda,
        BackendPref::Auto => Backend::Vulkan,
    }
}

/// Why `auto` does not pick CUDA for this GPU, as a sentence for the UI; None
/// when it does. The CUDA llama.cpp builds are CUDA 13: they need an NVIDIA
/// GPU of the Turing generation or newer (compute capability 7.5) and an R580
/// driver or newer.
pub fn cuda_unsupported(gpu: &GpuInfo) -> Option<String> {
    if gpu.vendor != GpuVendor::Nvidia {
        return Some(format!("{} is not an NVIDIA GPU", gpu.name));
    }
    match driver_branch(&gpu.driver) {
        Some(branch) if branch >= MIN_CUDA_DRIVER => {}
        _ => {
            return Some(format!(
                "the NVIDIA driver {} is older than {MIN_CUDA_DRIVER}, the first that runs the CUDA 13 engines; \
                 update the driver, or JAI Studio uses Vulkan",
                gpu.driver
            ))
        }
    }
    let (major, minor) = MIN_COMPUTE_CAP;
    match gpu.compute_cap.as_deref().and_then(compute_cap) {
        Some(cap) if cap >= MIN_COMPUTE_CAP => None,
        Some(_) | None => Some(format!(
            "{} (compute capability {}) is older than the Turing generation ({major}.{minor}) that the CUDA 13 engines need, \
             so JAI Studio uses Vulkan",
            gpu.name,
            gpu.compute_cap.as_deref().unwrap_or("unknown")
        )),
    }
}

/// `580` for `"580.65.06"` or `"580.88"`.
fn driver_branch(driver: &str) -> Option<u32> {
    driver.trim().split('.').next()?.parse().ok()
}

/// `(8, 9)` for `"8.9"`.
fn compute_cap(text: &str) -> Option<(u32, u32)> {
    let (major, minor) = text.trim().split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// The first `nvidia-smi` found on PATH (absolute entries only) or in the
/// platform's usual places.
fn nvidia_smi() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "nvidia-smi.exe"
    } else {
        "nvidia-smi"
    };
    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    on_path
        .into_iter()
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .chain(usual_places(name))
        .find(|p| p.is_file())
}

#[cfg(windows)]
fn usual_places(name: &str) -> Vec<PathBuf> {
    let mut places = Vec::new();
    if let Some(root) = std::env::var_os("SystemRoot").or_else(|| std::env::var_os("windir")) {
        places.push(PathBuf::from(root).join("System32").join(name));
    }
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        places.push(
            PathBuf::from(program_files)
                .join("NVIDIA Corporation")
                .join("NVSMI")
                .join(name),
        );
    }
    places
}

/// WSL exposes the Windows driver's tool under /usr/lib/wsl/lib.
#[cfg(not(windows))]
fn usual_places(name: &str) -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/lib/wsl/lib").join(name),
        PathBuf::from("/usr/bin").join(name),
    ]
}

/// Why [`run_with_timeout`] has no output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Run {
    NotStarted,
    TimedOut,
    /// It exited with an error, or its output was lost.
    Failed,
}

/// Runs a command and returns its standard output if it exits successfully
/// within `timeout`; otherwise kills it. On Windows the child gets no console
/// window.
fn run_with_timeout(mut command: Command, timeout: Duration) -> Result<String, Run> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|_| Run::NotStarted)?;
    let deadline = Instant::now() + timeout;
    // The output is read on its own thread, so a full pipe cannot stall the
    // child, and handed over through a channel, so a pipe kept open by some
    // grandchild cannot stall us.
    let (sender, receiver) = std::sync::mpsc::channel();
    if let Some(mut stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stdout.read_to_string(&mut text);
            let _ = sender.send(text);
        });
    }
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                tracing::debug!("the command did not finish in time");
                let _ = child.kill();
                let _ = child.wait();
                return Err(Run::TimedOut);
            }
        }
    };
    let wait = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(100));
    let text = receiver.recv_timeout(wait).map_err(|_| Run::Failed)?;
    if status.success() {
        Ok(text)
    } else {
        Err(Run::Failed)
    }
}

/// Parses `name, total, used, driver[, compute_cap]` lines; picks the GPU
/// with the most memory (the first one on a tie).
fn parse_smi(text: &str, with_cap: bool) -> Option<GpuInfo> {
    text.lines()
        .filter_map(|line| parse_line(line, with_cap))
        .fold(None, |best: Option<GpuInfo>, gpu| match best {
            Some(b) if b.vram_total_mb >= gpu.vram_total_mb => Some(b),
            _ => Some(gpu),
        })
}

/// One CSV line. The name may itself contain commas, so the fields are taken
/// from the right.
fn parse_line(line: &str, with_cap: bool) -> Option<GpuInfo> {
    let mut fields = line.rsplitn(if with_cap { 5 } else { 4 }, ',');
    let cap = if with_cap {
        let cap = fields.next()?.trim();
        compute_cap(cap).map(|_| cap.to_owned())
    } else {
        None
    };
    let driver = fields.next()?.trim();
    let used = fields.next()?.trim();
    let total = fields.next()?.trim();
    let name = fields.next()?.trim();
    if name.is_empty() {
        return None;
    }
    Some(GpuInfo {
        vendor: GpuVendor::Nvidia,
        name: name.to_owned(),
        vram_total_mb: megabytes(total),
        vram_used_mb: megabytes(used),
        driver: driver.to_owned(),
        compute_cap: cap,
    })
}

/// A memory figure; `[N/A]` and other non-numbers count as 0.
fn megabytes(field: &str) -> u64 {
    field
        .parse::<u64>()
        .ok()
        .or_else(|| {
            field
                .parse::<f64>()
                .ok()
                .filter(|f| f.is_finite() && *f >= 0.0)
                .map(|f| f as u64)
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_gpu() {
        let gpu = parse_smi(
            "NVIDIA GeForce RTX 5070 Ti, 16303, 1268, 610.88, 12.0\n",
            true,
        )
        .unwrap();
        assert_eq!(
            gpu,
            GpuInfo {
                vendor: GpuVendor::Nvidia,
                name: "NVIDIA GeForce RTX 5070 Ti".into(),
                vram_total_mb: 16303,
                vram_used_mb: 1268,
                driver: "610.88".into(),
                compute_cap: Some("12.0".into()),
            }
        );
        let old = parse_smi("Quadro P4000, 8192, 100, 470.82\n", false).unwrap();
        assert_eq!(old.driver, "470.82");
        assert_eq!(old.compute_cap, None);
    }

    #[test]
    fn picks_the_largest_gpu_and_tolerates_odd_fields() {
        let text = "Tesla T4, 15360, [N/A], 550.1, 7.5\nNVIDIA A100, 81920, 10, 550.1, 8.0\nNVIDIA A100, 81920, 99, 550.1, 8.0\n";
        let gpu = parse_smi(text, true).unwrap();
        assert_eq!(gpu.name, "NVIDIA A100");
        assert_eq!(gpu.vram_used_mb, 10, "the first on a tie");
        assert_eq!(
            parse_line("Odd, Name, 8192, [N/A], 1.0", false)
                .unwrap()
                .name,
            "Odd, Name"
        );
        let odd = parse_line("Odd, Name, 8192, [N/A], 1.0, [N/A]", true).unwrap();
        assert_eq!((odd.name.as_str(), odd.compute_cap), ("Odd, Name", None));
        assert_eq!(
            parse_line("Tesla T4, 15360, [N/A], 550.1", false)
                .unwrap()
                .vram_used_mb,
            0
        );
    }

    #[test]
    fn rejects_non_csv_output() {
        assert_eq!(parse_smi("", true), None);
        assert_eq!(parse_smi("No devices were found\n", true), None);
        assert_eq!(parse_smi(", 1, 2, 3", false), None);
        assert_eq!(parse_smi(", 1, 2, 3, 8.9", true), None);
    }

    fn gpu(driver: &str, cap: Option<&str>) -> GpuInfo {
        GpuInfo {
            vendor: GpuVendor::Nvidia,
            name: "GPU".into(),
            vram_total_mb: 16000,
            vram_used_mb: 0,
            driver: driver.into(),
            compute_cap: cap.map(str::to_owned),
        }
    }

    #[test]
    fn auto_prefers_cuda_on_nvidia_gpus_that_run_cuda_13() {
        let nvidia = gpu("610.88", Some("8.9"));
        let amd = GpuInfo {
            vendor: GpuVendor::Amd,
            ..nvidia.clone()
        };
        assert_eq!(
            effective_backend(BackendPref::Auto, Some(&nvidia)),
            Backend::Cuda
        );
        assert_eq!(cuda_unsupported(&nvidia), None);
        assert_eq!(
            effective_backend(BackendPref::Auto, Some(&amd)),
            Backend::Vulkan
        );
        assert_eq!(effective_backend(BackendPref::Auto, None), Backend::Vulkan);
        assert_eq!(
            effective_backend(BackendPref::Cpu, Some(&nvidia)),
            Backend::Cpu
        );
        assert_eq!(
            effective_backend(BackendPref::Vulkan, Some(&nvidia)),
            Backend::Vulkan
        );
        assert_eq!(effective_backend(BackendPref::Cuda, None), Backend::Cuda);
    }

    /// CUDA 13 needs Turing (7.5) or newer and an R580 driver; older GPUs and
    /// drivers get Vulkan unless CUDA is chosen explicitly.
    #[test]
    fn auto_avoids_cuda_where_cuda_13_cannot_run() {
        for (driver, cap) in [
            ("560.94", Some("8.9")),    // a driver before R580
            ("[N/A]", Some("8.9")),     // an unknown driver
            ("581.57", Some("6.1")),    // Pascal, GTX 1080 Ti
            ("580.65.06", Some("7.0")), // Volta
            ("581.57", None),           // no compute capability reported
        ] {
            let g = gpu(driver, cap);
            assert_eq!(
                effective_backend(BackendPref::Auto, Some(&g)),
                Backend::Vulkan,
                "{driver} {cap:?}"
            );
            assert!(cuda_unsupported(&g).is_some());
            assert_eq!(
                effective_backend(BackendPref::Cuda, Some(&g)),
                Backend::Cuda
            );
        }
        for (driver, cap) in [("580.65.06", "7.5"), ("580.88", "12.0"), ("610.88", "10.0")] {
            let g = gpu(driver, Some(cap));
            assert_eq!(
                effective_backend(BackendPref::Auto, Some(&g)),
                Backend::Cuda
            );
        }
        let pascal = cuda_unsupported(&gpu("581.57", Some("6.1"))).unwrap();
        assert!(
            pascal.contains("6.1") && pascal.contains("Turing"),
            "{pascal}"
        );
        let old = cuda_unsupported(&gpu("560.94", Some("8.9"))).unwrap();
        assert!(old.contains("560.94") && old.contains("580"), "{old}");
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_command_is_killed() {
        let mut slow = Command::new("sh");
        slow.args(["-c", "sleep 5"]);
        let started = Instant::now();
        assert_eq!(
            run_with_timeout(slow, Duration::from_millis(200)),
            Err(Run::TimedOut)
        );
        assert!(started.elapsed() < Duration::from_secs(2));

        let mut quick = Command::new("sh");
        quick.args(["-c", "echo 'GPU, 1, 2, 3'"]);
        assert_eq!(
            run_with_timeout(quick, TIMEOUT).as_deref(),
            Ok("GPU, 1, 2, 3\n")
        );

        let mut failing = Command::new("sh");
        failing.args(["-c", "echo out; exit 3"]);
        assert_eq!(run_with_timeout(failing, TIMEOUT), Err(Run::Failed));

        let missing = Command::new("/nonexistent/nvidia-smi");
        assert_eq!(run_with_timeout(missing, TIMEOUT), Err(Run::NotStarted));
    }
}
