//! JAI Studio core: the data directory, settings, the bearer token, the pinned
//! download catalog, GPU detection, install state, the downloader and reuse of
//! model files that are already on disk. See `studio/DESIGN.md` sections 2 to 5.

pub mod catalog;
pub mod config;
pub mod download;
pub mod gpu;
pub mod install;
pub mod paths;
pub mod reuse;
pub mod token;

mod fsutil;
mod perms;
mod verified;

pub use catalog::{Backend, Catalog, Component, Os, Role, Selection, Unpack};
pub use config::{BackendPref, Config};
pub use paths::Paths;
