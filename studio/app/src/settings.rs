//! Settings helpers shared by the CLI and the GUI: the config with its
//! environment override, the bind-address rule (DESIGN.md section 3), the URL
//! a client on this PC uses, what a settings change has to restart, and when
//! a restart would break work in progress.
//!
//! `JAI_PORT` overrides `server.port` for one run only. The GUI's settings
//! form therefore shows and saves `config.json` as stored, and a save that
//! leaves the port alone keeps the port in use ([`applied`]).

use std::net::{IpAddr, SocketAddr};

use jai_core::{Config, Paths};
use jai_engines::{check_extra_args, EngineKind};

/// Overrides `server.port` (DESIGN.md section 0).
pub const PORT_ENV: &str = "JAI_PORT";

/// `config.json`, with `JAI_PORT` applied.
pub fn load_config(paths: &Paths) -> Config {
    let mut cfg = Config::load(paths);
    if let Some(port) = std::env::var(PORT_ENV).ok().as_deref().and_then(parse_port) {
        cfg.server.port = port;
    }
    cfg
}

fn parse_port(s: &str) -> Option<u16> {
    s.trim().parse::<u16>().ok().filter(|p| *p != 0)
}

fn bare_host(host: &str) -> &str {
    let h = host.trim();
    h.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(h)
}

/// `localhost` or a loopback IP address.
pub fn is_loopback(host: &str) -> bool {
    let h = bare_host(host);
    h.eq_ignore_ascii_case("localhost") || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The server binds a loopback address, or an IP address with LAN access
/// allowed.
pub fn check_bind(host: &str, allow_lan: bool) -> Result<(), String> {
    let h = bare_host(host);
    if h.is_empty() {
        return Err("the server address is empty; use 127.0.0.1".into());
    }
    if is_loopback(h) {
        return Ok(());
    }
    if h.parse::<IpAddr>().is_err() {
        return Err(format!(
            "{h} is not an IP address; use 127.0.0.1, or an address of this PC with LAN access allowed"
        ));
    }
    if !allow_lan {
        return Err(format!(
            "{h} is not a loopback address: allow LAN access (--allow-lan, or allowLan in the settings) \
             to accept connections from other devices; the token is still required"
        ));
    }
    Ok(())
}

/// Checks settings before they are saved or used to bind, including the
/// engines' extra arguments, which may not take over what Studio sets.
pub fn validate(cfg: &Config) -> Result<(), String> {
    if cfg.server.port == 0 {
        return Err("the port must be from 1 to 65535".into());
    }
    check_bind(&cfg.server.host, cfg.server.allow_lan)?;
    let layers = cfg.llm.gpu_layers.trim();
    if layers != "all" && layers.parse::<u32>().is_err() {
        return Err(format!(
            "gpuLayers must be \"all\" or a number of layers, not {layers:?}"
        ));
    }
    check_extra_args(EngineKind::Llm, &cfg.llm.extra_args)?;
    check_extra_args(EngineKind::Image, &cfg.image.extra_args)?;
    Ok(())
}

#[cfg(any(feature = "gui", test))]
/// The settings form as the GUI saves it, with the engines' extra arguments
/// as `config.json` holds them: the form has no field for them, and the
/// webview shows untrusted documents, so it may not change them.
pub fn from_form(mut form: Config, stored: &Config) -> Config {
    form.llm.extra_args.clone_from(&stored.llm.extra_args);
    form.image.extra_args.clone_from(&stored.image.extra_args);
    form
}

/// The URL a client on this PC uses for a server bound to `host:port`: a
/// wildcard address becomes loopback, and IPv6 addresses get brackets.
pub fn client_url(host: &str, port: u16) -> String {
    let h = bare_host(host);
    let h = match h.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) if ip.is_unspecified() => "127.0.0.1".to_string(),
        Ok(IpAddr::V6(ip)) if ip.is_unspecified() => "[::1]".to_string(),
        Ok(IpAddr::V4(ip)) => ip.to_string(),
        Ok(IpAddr::V6(ip)) => format!("[{ip}]"),
        Err(_) => h.to_string(),
    };
    format!("http://{h}:{port}")
}

/// [`client_url`] for a bound socket address.
pub fn addr_url(addr: SocketAddr) -> String {
    client_url(&addr.ip().to_string(), addr.port())
}

#[cfg(any(feature = "gui", test))]
/// The engine launcher and the scheduler take a snapshot of these at start,
/// so a change needs a new `Core`. The idle-unload time is not among them:
/// the running scheduler takes a new one (`Scheduler::set_idle_unload`).
pub fn engines_changed(old: &Config, new: &Config) -> bool {
    old.backend != new.backend || old.llm != new.llm || old.image != new.image
}

#[cfg(any(feature = "gui", test))]
/// Host, port or LAN access changed: the server must bind again.
pub fn server_changed(old: &Config, new: &Config) -> bool {
    old.server != new.server
}

#[cfg(any(feature = "gui", test))]
/// The settings to run with after the GUI saves `form`, the settings form as
/// `config.json` will now hold it: the form's values, except that a port left
/// as it was stored keeps the port in use (which `JAI_PORT` may have set). A
/// port the user changed applies.
pub fn applied(form: &Config, stored: &Config, running: &Config) -> Config {
    let mut cfg = form.clone();
    if form.server.port == stored.server.port {
        cfg.server.port = running.server.port;
    }
    cfg
}

#[cfg(any(feature = "gui", test))]
/// Why restarting the server or the engines now would break work in
/// progress, if it would. An encode keeps the server address and token it
/// started with for its whole run, and requests that are queued or running
/// would be cut off.
pub fn busy_reason(encodes: usize, requests: usize) -> Option<String> {
    match (encodes, requests) {
        (0, 0) => None,
        (1, _) => Some("an encode is running".into()),
        (n @ 2.., _) => Some(format!("{n} encodes are running")),
        (0, 1) => Some("the models are working on a request".into()),
        (0, n) => Some(format!("the models are working on {n} requests")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jai_core::BackendPref;

    #[test]
    fn ports() {
        assert_eq!(parse_port("5300"), Some(5300));
        assert_eq!(parse_port(" 5241 "), Some(5241));
        assert_eq!(parse_port("0"), None);
        assert_eq!(parse_port("70000"), None);
        assert_eq!(parse_port("x"), None);
    }

    #[test]
    fn loopback_and_bind_rule() {
        for h in [
            "127.0.0.1",
            "localhost",
            "LOCALHOST",
            "::1",
            "[::1]",
            "127.0.0.2",
        ] {
            assert!(is_loopback(h), "{h}");
            assert!(check_bind(h, false).is_ok(), "{h}");
        }
        assert!(!is_loopback("0.0.0.0"));
        let e = check_bind("0.0.0.0", false).unwrap_err();
        assert!(e.contains("--allow-lan"), "{e}");
        assert!(check_bind("0.0.0.0", true).is_ok());
        assert!(check_bind("192.168.1.20", true).is_ok());
        assert!(check_bind("example.com", true).is_err());
        assert!(check_bind("", true).is_err());
    }

    #[test]
    fn settings_validation() {
        let mut c = Config::default();
        assert!(validate(&c).is_ok());
        c.server.port = 0;
        assert!(validate(&c).is_err());
        c.server.port = 5240;
        c.llm.gpu_layers = "40".into();
        assert!(validate(&c).is_ok());
        c.llm.gpu_layers = "most".into();
        assert!(validate(&c).is_err());
        c.llm.gpu_layers = "all".into();
        c.server.host = "0.0.0.0".into();
        assert!(validate(&c).is_err());
        c.server.allow_lan = true;
        assert!(validate(&c).is_ok());
        // Extra arguments may tune an engine, not take it over.
        c.llm.extra_args = vec!["--threads".into(), "8".into()];
        c.image.extra_args = vec!["--vae-tiling".into()];
        assert!(validate(&c).is_ok());
        c.llm.extra_args = vec!["--host".into(), "0.0.0.0".into()];
        assert!(validate(&c).unwrap_err().contains("llm.extraArgs"));
        c.llm.extra_args.clear();
        c.image.extra_args = vec!["--listen-ip=0.0.0.0".into()];
        assert!(validate(&c).unwrap_err().contains("image.extraArgs"));
    }

    #[test]
    fn the_form_cannot_change_extra_args() {
        let mut stored = Config::default();
        stored.llm.extra_args = vec!["--threads".into(), "8".into()];
        let mut form = Config {
            idle_unload_minutes: 5,
            ..Config::default()
        };
        form.llm.extra_args = vec!["--media-path".into(), "C:\\".into()];
        form.image.extra_args = vec!["--serve-html-path".into(), "/".into()];
        let saved = from_form(form, &stored);
        assert_eq!(saved.idle_unload_minutes, 5, "the form's own fields apply");
        assert_eq!(saved.llm.extra_args, stored.llm.extra_args);
        assert!(saved.image.extra_args.is_empty());
    }

    #[test]
    fn client_urls() {
        assert_eq!(client_url("127.0.0.1", 5240), "http://127.0.0.1:5240");
        assert_eq!(client_url("0.0.0.0", 5241), "http://127.0.0.1:5241");
        assert_eq!(client_url("::", 5240), "http://[::1]:5240");
        assert_eq!(client_url("::1", 5240), "http://[::1]:5240");
        assert_eq!(client_url("[::1]", 5240), "http://[::1]:5240");
        assert_eq!(client_url("localhost", 5240), "http://localhost:5240");
        assert_eq!(client_url("192.168.1.20", 5240), "http://192.168.1.20:5240");
        let addr: SocketAddr = "0.0.0.0:5242".parse().unwrap();
        assert_eq!(addr_url(addr), "http://127.0.0.1:5242");
    }

    #[test]
    fn jai_port_is_not_saved() {
        // config.json says 5240; JAI_PORT made this run use 5300.
        let stored = Config::default();
        let mut running = stored.clone();
        running.server.port = 5300;
        // The form shows the stored settings; an unrelated change keeps
        // running on 5300, and 5240 is what gets saved.
        let mut form = stored.clone();
        form.app.close_to_tray = false;
        let run = applied(&form, &stored, &running);
        assert_eq!(run.server.port, 5300);
        assert!(!run.app.close_to_tray);
        assert_eq!(form.server.port, 5240);
        assert!(!server_changed(&running, &run));
        // A port the user typed in applies, and is saved.
        form.server.port = 5400;
        assert_eq!(applied(&form, &stored, &running).server.port, 5400);
        // Without JAI_PORT the running port is the stored one.
        assert_eq!(applied(&stored, &stored, &stored), stored);
    }

    #[test]
    fn restarts_wait_for_work_in_progress() {
        assert_eq!(busy_reason(0, 0), None);
        assert_eq!(busy_reason(1, 0).as_deref(), Some("an encode is running"));
        assert_eq!(busy_reason(1, 3).as_deref(), Some("an encode is running"));
        assert_eq!(busy_reason(2, 0).as_deref(), Some("2 encodes are running"));
        assert_eq!(
            busy_reason(0, 1).as_deref(),
            Some("the models are working on a request")
        );
        assert_eq!(
            busy_reason(0, 4).as_deref(),
            Some("the models are working on 4 requests")
        );
    }

    #[test]
    fn what_a_change_restarts() {
        let old = Config::default();
        let mut new = old.clone();
        assert!(!engines_changed(&old, &new) && !server_changed(&old, &new));
        new.app.close_to_tray = false;
        assert!(!engines_changed(&old, &new) && !server_changed(&old, &new));
        new.server.port = 5300;
        assert!(server_changed(&old, &new) && !engines_changed(&old, &new));
        let mut new = old.clone();
        new.idle_unload_minutes = 5;
        assert!(
            !engines_changed(&old, &new) && !server_changed(&old, &new),
            "the running scheduler takes a new idle-unload time"
        );
        let mut new = old.clone();
        new.backend = BackendPref::Vulkan;
        assert!(engines_changed(&old, &new));
        let mut new = old.clone();
        new.llm.ctx = 8192;
        assert!(engines_changed(&old, &new));
    }
}
