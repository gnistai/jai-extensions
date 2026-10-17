//! Finding a running server through `<root>/server.json` (DESIGN.md sections 2
//! and 7). The server there must prove that it holds the token (the
//! `/health` proof) before a client sends it the token: the file outlives a
//! server that crashed, and another local user may then listen on its port.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::Context;
use jai_core::Paths;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How long `find` waits for `GET /health`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// The largest `/health` answer `find` reads.
const PROBE_MAX_BYTES: usize = 64 * 1024;

/// The contents of `server.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub version: String,
    pub pid: u32,
    /// The address local clients connect to: 127.0.0.1 unless the server is
    /// bound to one LAN address.
    pub host: String,
    pub port: u16,
    /// Unix time in seconds.
    pub started: u64,
}

impl ServerInfo {
    /// `http://<host>:<port>`, with brackets around an IPv6 host.
    pub fn url(&self) -> String {
        match self.host.parse::<IpAddr>() {
            Ok(IpAddr::V6(ip)) => format!("http://[{ip}]:{}", self.port),
            _ => format!("http://{}:{}", self.host, self.port),
        }
    }
}

/// Writes `server.json` atomically (a temporary file, then a rename).
pub fn write(paths: &Paths, info: &ServerInfo) -> anyhow::Result<()> {
    std::fs::create_dir_all(&paths.root)
        .with_context(|| format!("could not create {}", paths.root.display()))?;
    let file = paths.server_file();
    let tmp = paths
        .root
        .join(format!("server.json.{}.tmp", std::process::id()));
    let json = serde_json::to_vec_pretty(info).context("could not encode server.json")?;
    std::fs::write(&tmp, json).with_context(|| format!("could not write {}", tmp.display()))?;
    if let Err(err) = std::fs::rename(&tmp, &file) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("could not write {}", file.display()));
    }
    Ok(())
}

/// Removes `server.json`; a missing file is fine.
pub fn remove(paths: &Paths) {
    match std::fs::remove_file(paths.server_file()) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => tracing::warn!(error = %err, "could not remove server.json"),
    }
}

/// `server.json` plus the token, once the server there has proved that it
/// holds this token: `GET /health?nonce=<random>` (2 s timeout) must answer
/// `name: "jai"` and the proof of that nonce for the port in the file. Only
/// the nonce is sent. None when there is no file or no token, or when what
/// answers there cannot prove it (nothing, an engine, a process that took
/// the port of a server that crashed, another user's server).
pub async fn find(paths: &Paths) -> Option<(ServerInfo, String)> {
    let mut info: ServerInfo =
        serde_json::from_slice(&tokio::fs::read(paths.server_file()).await.ok()?).ok()?;
    // Only an address, never a host name.
    info.host.parse::<IpAddr>().ok()?;
    let token = tokio::fs::read_to_string(paths.token_file())
        .await
        .ok()?
        .trim()
        .to_owned();
    if token.is_empty() {
        return None;
    }
    let nonce = crate::proof::nonce().ok()?;
    let body = probe(&format!("{}/health?nonce={nonce}", info.url())).await?;
    // Something else on that port, such as an engine's own /health.
    if body.get("name").and_then(Value::as_str) != Some("jai") {
        return None;
    }
    let expected = crate::proof::proof(&token, info.port, &nonce);
    let shown = body
        .get("proof")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !crate::auth::constant_time_eq(shown.as_bytes(), expected.as_bytes()) {
        tracing::warn!(
            port = info.port,
            "the server named in server.json did not prove that it holds this user's token; not using it"
        );
        return None;
    }
    // A server that started on the same port after one was killed proves the
    // token before it rewrites server.json; report the process that answered.
    if let Some(pid) = body
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|p| u32::try_from(p).ok())
    {
        info.pid = pid;
    }
    Some((info, token))
}

/// The JSON answer to a GET: no proxy, no redirects, at most
/// `PROBE_MAX_BYTES`.
async fn probe(url: &str) -> Option<Value> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(PROBE_TIMEOUT)
        .build()
        .ok()?;
    let mut res = client.get(url).send().await.ok()?;
    if !res.status().is_success() {
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = res.chunk().await.ok()? {
        if body.len() + chunk.len() > PROBE_MAX_BYTES {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).ok()
}

/// The current `server.json`, if it parses.
pub(crate) fn read(paths: &Paths) -> Option<ServerInfo> {
    let bytes = std::fs::read(paths.server_file()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Removes `server.json` if this process on this port wrote it, so a server
/// that stops does not remove another one's file.
pub(crate) fn remove_if_ours(paths: &Paths, port: u16) {
    if read(paths).is_some_and(|info| info.pid == std::process::id() && info.port == port) {
        remove(paths);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(port: u16) -> ServerInfo {
        ServerInfo {
            version: "0.1.0".into(),
            pid: std::process::id(),
            host: "127.0.0.1".into(),
            port,
            started: 1,
        }
    }

    #[test]
    fn write_read_remove() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("root"));
        write(&paths, &info(5240)).unwrap();
        assert_eq!(read(&paths), Some(info(5240)));
        let text = std::fs::read_to_string(paths.server_file()).unwrap();
        for key in ["version", "pid", "host", "port", "started"] {
            assert!(text.contains(&format!("\"{key}\"")), "{key}");
        }
        remove_if_ours(&paths, 5241);
        assert!(read(&paths).is_some());
        remove_if_ours(&paths, 5240);
        assert!(read(&paths).is_none());
        remove(&paths);
    }

    #[test]
    fn urls() {
        assert_eq!(info(5240).url(), "http://127.0.0.1:5240");
        let v6 = ServerInfo {
            host: "::1".into(),
            ..info(5241)
        };
        assert_eq!(v6.url(), "http://[::1]:5241");
    }
}
