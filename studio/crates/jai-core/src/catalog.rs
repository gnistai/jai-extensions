//! The pinned download catalog, `studio/catalog.json` (DESIGN.md section 4).

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// `studio/catalog.json`, embedded at build time.
const CATALOG_JSON: &str = include_str!("../../../catalog.json");

/// One downloadable file: a model, or an engine or runtime archive.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Component {
    pub id: String,
    pub role: Role,
    pub label: String,
    pub file: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
    pub license: String,
    pub source: String,
    /// Engines and runtimes only; models are needed on every OS.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<Os>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<Backend>,
    /// Archives only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unpack: Option<Unpack>,
    /// Leading path components to drop (tar archives with a top folder).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strip: Option<u32>,
    /// Folder under `engines/` to unpack into.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// The engine executable inside `dir`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
}

/// What a component is for.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    LlmModel,
    LlmProjector,
    ImageDiffusion,
    ImageVae,
    ImageClipL,
    ImageT5xxl,
    LlmEngine,
    LlmRuntime,
    ImageEngine,
    ImageRuntime,
}

impl Role {
    /// Model files, stored in `models/` and needed on every OS.
    pub fn is_model(self) -> bool {
        matches!(
            self,
            Role::LlmModel
                | Role::LlmProjector
                | Role::ImageDiffusion
                | Role::ImageVae
                | Role::ImageClipL
                | Role::ImageT5xxl
        )
    }

    /// The two engine executables: `LlmEngine` and `ImageEngine`.
    pub fn is_engine(self) -> bool {
        matches!(self, Role::LlmEngine | Role::ImageEngine)
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Windows,
    Linux,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Cuda,
    Vulkan,
    Cpu,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Unpack {
    #[serde(rename = "zip")]
    Zip,
    #[serde(rename = "tar.gz")]
    TarGz,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Catalog {
    pub version: u32,
    pub components: Vec<Component>,
}

/// The components for one OS and backend: every model, plus one component per
/// engine role (with fallback) and its runtime.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Selection {
    pub components: Vec<Component>,
    /// The backend of the selected LLM engine, after fallback.
    pub llm_backend: Backend,
    /// The backend of the selected image engine, after fallback.
    pub image_backend: Backend,
}

/// The embedded catalog, parsed once. A unit test keeps it valid; if it were
/// not, this logs an error and returns an empty catalog instead of panicking.
pub fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(CATALOG_JSON).unwrap_or_else(|e| {
            tracing::error!(error = %e, "the embedded download catalog is invalid");
            Catalog {
                version: 0,
                components: Vec::new(),
            }
        })
    })
}

/// The OS this binary runs on. Builds for other Unix systems count as Linux.
pub fn current_os() -> Os {
    if cfg!(windows) {
        Os::Windows
    } else {
        Os::Linux
    }
}

/// Backends to try for a preferred one, best first. Every chain ends with CPU.
fn fallback_chain(backend: Backend) -> &'static [Backend] {
    match backend {
        Backend::Cuda => &[Backend::Cuda, Backend::Vulkan, Backend::Cpu],
        Backend::Vulkan => &[Backend::Vulkan, Backend::Cpu],
        Backend::Cpu => &[Backend::Cpu],
    }
}

/// Components for an OS and backend (DESIGN.md section 4, "How components are
/// selected"): all models; for each engine role the build for this OS with the
/// first backend of the fallback chain that exists (on Linux the image engine
/// has no CUDA build, so CUDA falls back to Vulkan, then CPU); and the runtime
/// archives that match each chosen engine's backend. The effective backends
/// are recorded. Catalog order is kept.
pub fn select(catalog: &Catalog, os: Os, backend: Backend) -> Selection {
    let pick = |role: Role| -> Option<&Component> {
        fallback_chain(backend).iter().find_map(|b| {
            catalog
                .components
                .iter()
                .find(|c| c.role == role && c.os == Some(os) && c.backend == Some(*b))
        })
    };
    let llm = pick(Role::LlmEngine);
    let image = pick(Role::ImageEngine);
    let effective = |engine: Option<&Component>, role: Role| match engine.and_then(|c| c.backend) {
        Some(b) => b,
        None => {
            tracing::warn!(
                ?os,
                ?backend,
                ?role,
                "the catalog has no engine build for this OS"
            );
            Backend::Cpu
        }
    };
    let llm_backend = effective(llm, Role::LlmEngine);
    let image_backend = effective(image, Role::ImageEngine);
    let runtime_for = |engine: Option<&Component>, c: &Component, b: Backend| {
        engine.is_some() && c.os == Some(os) && c.backend == Some(b)
    };
    let wanted = |c: &Component| match c.role {
        Role::LlmModel
        | Role::LlmProjector
        | Role::ImageDiffusion
        | Role::ImageVae
        | Role::ImageClipL
        | Role::ImageT5xxl => true,
        Role::LlmEngine => llm.is_some_and(|e| e.id == c.id),
        Role::ImageEngine => image.is_some_and(|e| e.id == c.id),
        Role::LlmRuntime => runtime_for(llm, c, llm_backend),
        Role::ImageRuntime => runtime_for(image, c, image_backend),
    };
    Selection {
        components: catalog
            .components
            .iter()
            .filter(|c| wanted(c))
            .cloned()
            .collect(),
        llm_backend,
        image_backend,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The types above match the catalog file.
    #[test]
    fn embedded_catalog_parses() {
        let c: Catalog = serde_json::from_str(CATALOG_JSON).unwrap();
        assert_eq!(c.version, 1);
        assert!(c.components.iter().any(|x| x.role == Role::ImageT5xxl));
        assert!(c.components.iter().any(|x| x.unpack == Some(Unpack::TarGz)));
        assert_eq!(catalog(), &c);
    }

    /// Every entry is usable by the installer: plain names, a pinned
    /// lowercase sha256, and archives with a target folder.
    #[test]
    fn embedded_catalog_is_well_formed() {
        let plain = |s: &str| !s.is_empty() && !s.contains(['/', '\\', ':']) && s != "..";
        for c in &catalog().components {
            assert!(plain(&c.id) && plain(&c.file), "{}", c.id);
            assert!(c.size > 0, "{}", c.id);
            assert!(c.url.starts_with("https://"), "{}", c.id);
            assert_eq!(c.sha256.len(), 64, "{}", c.id);
            assert!(c
                .sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
            assert_eq!(c.role.is_model(), c.unpack.is_none(), "{}", c.id);
            assert_eq!(
                c.role.is_model(),
                c.os.is_none() && c.backend.is_none(),
                "{}",
                c.id
            );
            if c.unpack.is_some() {
                assert!(c.dir.as_deref().is_some_and(plain), "{}", c.id);
            }
            assert_eq!(c.role.is_engine(), c.exe.is_some(), "{}", c.id);
        }
    }

    fn ids(sel: &Selection) -> Vec<&str> {
        sel.components
            .iter()
            .filter(|c| !c.role.is_model())
            .map(|c| c.id.as_str())
            .collect()
    }

    fn check(os: Os, backend: Backend, engines: &[&str], llm: Backend, image: Backend) {
        let sel = select(catalog(), os, backend);
        assert_eq!(ids(&sel), engines, "{os:?} {backend:?}");
        assert_eq!(sel.llm_backend, llm, "{os:?} {backend:?}");
        assert_eq!(sel.image_backend, image, "{os:?} {backend:?}");
        let models = sel.components.iter().filter(|c| c.role.is_model()).count();
        assert_eq!(models, 6, "every model is selected");
    }

    #[test]
    fn selection_per_os_and_backend() {
        use Backend::{Cpu, Cuda, Vulkan};
        check(
            Os::Windows,
            Cuda,
            &[
                "llama-win-cuda",
                "llama-win-cuda-rt",
                "sd-win-cuda",
                "sd-win-cuda-rt",
            ],
            Cuda,
            Cuda,
        );
        check(
            Os::Windows,
            Vulkan,
            &["llama-win-vulkan", "sd-win-vulkan"],
            Vulkan,
            Vulkan,
        );
        check(Os::Windows, Cpu, &["llama-win-cpu", "sd-win-cpu"], Cpu, Cpu);
        // No CUDA build of stable-diffusion.cpp for Linux: the image engine
        // falls back to Vulkan and takes no CUDA runtime.
        check(
            Os::Linux,
            Cuda,
            &["llama-linux-cuda", "llama-linux-cuda-rt", "sd-linux-vulkan"],
            Cuda,
            Vulkan,
        );
        check(
            Os::Linux,
            Vulkan,
            &["llama-linux-vulkan", "sd-linux-vulkan"],
            Vulkan,
            Vulkan,
        );
        check(
            Os::Linux,
            Cpu,
            &["llama-linux-cpu", "sd-linux-cpu"],
            Cpu,
            Cpu,
        );
    }

    #[test]
    fn fallback_ends_with_cpu() {
        let mut cat = catalog().clone();
        cat.components
            .retain(|c| !(c.role == Role::ImageEngine && c.backend == Some(Backend::Vulkan)));
        let sel = select(&cat, Os::Linux, Backend::Cuda);
        assert_eq!(
            ids(&sel),
            ["llama-linux-cuda", "llama-linux-cuda-rt", "sd-linux-cpu"]
        );
        assert_eq!(sel.image_backend, Backend::Cpu);

        // Vulkan never falls "up" to CUDA.
        cat.components
            .retain(|c| !(c.role == Role::LlmEngine && c.backend == Some(Backend::Vulkan)));
        let sel = select(&cat, Os::Windows, Backend::Vulkan);
        assert_eq!(ids(&sel), ["llama-win-cpu", "sd-win-cpu"]);
        assert_eq!(sel.llm_backend, Backend::Cpu);
    }

    #[test]
    fn current_os_matches_the_build() {
        assert_eq!(current_os() == Os::Windows, cfg!(windows));
    }
}
