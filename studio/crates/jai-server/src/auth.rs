//! The rules for every request: the Host allow-list, Origin rejection and the
//! bearer token for `/v1/` (DESIGN.md section 7, "Rules for every request").

use std::net::{IpAddr, SocketAddr};

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;
use crate::server::Shared;

/// Origins that may call the API: browser extensions. Web pages may not.
const EXTENSION_ORIGINS: [&str; 3] = [
    "chrome-extension://",
    "moz-extension://",
    "safari-web-extension://",
];

/// The one path that needs no token.
const OPEN_PATH: &str = "/health";

/// Which `Host` values name this server.
#[derive(Clone, Debug)]
pub(crate) struct HostRule {
    /// `127.0.0.1:<port>`, `localhost:<port>`, `[::1]:<port>`, plus the
    /// bound address when it is another one.
    allowed: Vec<String>,
    port: u16,
    /// Bound to every interface (which needs `allowLan`): any IP literal on
    /// our port is fine, since DNS rebinding needs a host name.
    any_ip: bool,
}

impl HostRule {
    /// The rule for a server bound to `bound` on `port`. A bound address
    /// other than 127.0.0.1 and ::1 (a LAN address, or a loopback address
    /// such as 127.0.0.2) is allowed too, since clients name it.
    pub fn new(port: u16, bound: IpAddr) -> HostRule {
        let mut allowed = vec![
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("[::1]:{port}"),
        ];
        let any_ip = bound.is_unspecified();
        if !any_ip {
            let addr = SocketAddr::new(bound, port).to_string();
            if !allowed.contains(&addr) {
                allowed.push(addr);
            }
        }
        HostRule {
            allowed,
            port,
            any_ip,
        }
    }

    /// The bound port.
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn allows(&self, host: &str) -> bool {
        if self.allowed.iter().any(|a| a.eq_ignore_ascii_case(host)) {
            return true;
        }
        self.any_ip
            && host
                .parse::<SocketAddr>()
                .is_ok_and(|addr| addr.port() == self.port)
    }
}

/// Middleware that applies the rules before any handler runs.
pub(crate) async fn enforce(State(shared): State<Shared>, req: Request, next: Next) -> Response {
    match check(&shared, &req) {
        Ok(()) => next.run(req).await,
        Err(err) => {
            tracing::debug!(path = req.uri().path(), code = %err.code, "request refused");
            err.into_response()
        }
    }
}

fn check(shared: &Shared, req: &Request) -> Result<(), ApiError> {
    check_host(&shared.hosts, req)?;
    check_origin(req.headers())?;
    if req.uri().path() != OPEN_PATH {
        crate::read_lock(&shared.core.token, |token| {
            check_bearer(req.headers(), token)
        })?;
    }
    Ok(())
}

/// Exactly one `Host` header (or an absolute request URI) naming this server.
fn check_host(rule: &HostRule, req: &Request) -> Result<(), ApiError> {
    let mut hosts = req.headers().get_all(header::HOST).iter();
    let header_host = match (hosts.next(), hosts.next()) {
        (None, _) => None,
        (Some(value), None) => Some(value.to_str().map_err(|_| ApiError::bad_host())?),
        (Some(_), Some(_)) => return Err(ApiError::bad_host()),
    };
    let uri_host = req.uri().authority().map(|a| a.as_str());
    if header_host.is_none() && uri_host.is_none() {
        return Err(ApiError::bad_host());
    }
    for host in [header_host, uri_host].into_iter().flatten() {
        if !rule.allows(host) {
            return Err(ApiError::bad_host());
        }
    }
    Ok(())
}

/// No `Origin`, or a browser extension's.
fn check_origin(headers: &HeaderMap) -> Result<(), ApiError> {
    for value in headers.get_all(header::ORIGIN) {
        let origin = value.to_str().map_err(|_| ApiError::bad_origin())?;
        if !EXTENSION_ORIGINS.iter().any(|p| origin.starts_with(p)) {
            return Err(ApiError::bad_origin());
        }
    }
    Ok(())
}

/// `Authorization: Bearer <token>`, compared in constant time.
fn check_bearer(headers: &HeaderMap, token: &str) -> Result<(), ApiError> {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, rest)| rest.trim());
    match presented {
        Some(p) if !token.is_empty() && constant_time_eq(p.as_bytes(), token.as_bytes()) => Ok(()),
        _ => Err(ApiError::unauthorized()),
    }
}

/// Equality whose time depends only on the lengths.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    const LOOPBACK: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

    #[test]
    fn loopback_hosts() {
        let rule = HostRule::new(5240, LOOPBACK);
        assert_eq!(rule.port(), 5240);
        assert!(rule.allows("127.0.0.1:5240"));
        assert!(rule.allows("localhost:5240"));
        assert!(rule.allows("LOCALHOST:5240"));
        assert!(rule.allows("[::1]:5240"));
        assert!(!rule.allows("127.0.0.1:5241"));
        assert!(!rule.allows("127.0.0.1"));
        assert!(!rule.allows("localhost"));
        assert!(!rule.allows("evil.example:5240"));
        assert!(!rule.allows("localhost.:5240"));
        assert!(!rule.allows("192.168.1.5:5240"));
    }

    #[test]
    fn lan_hosts() {
        let lan: IpAddr = "192.168.1.5".parse().unwrap();
        let rule = HostRule::new(5240, lan);
        assert!(rule.allows("192.168.1.5:5240"));
        assert!(rule.allows("127.0.0.1:5240"));
        assert!(!rule.allows("192.168.1.6:5240"));
        assert!(!rule.allows("evil.example:5240"));

        let any: IpAddr = "0.0.0.0".parse().unwrap();
        let rule = HostRule::new(5240, any);
        assert!(rule.allows("192.168.1.6:5240"));
        assert!(rule.allows("[fe80::1]:5240"));
        assert!(!rule.allows("192.168.1.6:80"));
        assert!(!rule.allows("evil.example:5240"));
    }

    /// A server bound to another loopback address is reached by that name
    /// (security-6): without it every request, /health too, was bad_host.
    #[test]
    fn other_bound_addresses() {
        let alias: IpAddr = "127.0.0.2".parse().unwrap();
        let rule = HostRule::new(5240, alias);
        assert!(rule.allows("127.0.0.2:5240"));
        assert!(rule.allows("127.0.0.1:5240"));
        assert!(rule.allows("localhost:5240"));
        assert!(!rule.allows("127.0.0.3:5240"));
        assert!(!rule.allows("127.0.0.2:5241"));

        let v6: IpAddr = "::1".parse().unwrap();
        let rule = HostRule::new(5240, v6);
        assert!(rule.allows("[::1]:5240"));
        assert!(rule.allows("127.0.0.1:5240"));
        assert_eq!(rule.allowed.len(), 3, "no duplicate entry");
    }

    #[test]
    fn origins() {
        let mut h = HeaderMap::new();
        assert!(check_origin(&h).is_ok());
        h.insert(
            header::ORIGIN,
            HeaderValue::from_static("chrome-extension://abcdefgh"),
        );
        assert!(check_origin(&h).is_ok());
        h.insert(
            header::ORIGIN,
            HeaderValue::from_static("moz-extension://x"),
        );
        assert!(check_origin(&h).is_ok());
        h.insert(
            header::ORIGIN,
            HeaderValue::from_static("safari-web-extension://x"),
        );
        assert!(check_origin(&h).is_ok());
        for bad in [
            "https://evil.example",
            "null",
            "http://127.0.0.1:5240",
            "file://",
        ] {
            h.insert(header::ORIGIN, HeaderValue::from_static(bad));
            assert!(check_origin(&h).is_err(), "{bad}");
        }
        h.insert(
            header::ORIGIN,
            HeaderValue::from_static("chrome-extension://abcdefgh"),
        );
        h.append(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );
        assert!(check_origin(&h).is_err());
    }

    #[test]
    fn bearer() {
        let mut h = HeaderMap::new();
        assert!(check_bearer(&h, "jai_x").is_err());
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer jai_x"),
        );
        assert!(check_bearer(&h, "jai_x").is_ok());
        assert!(check_bearer(&h, "jai_y").is_err());
        assert!(check_bearer(&h, "").is_err());
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer jai_x"),
        );
        assert!(check_bearer(&h, "jai_x").is_ok());
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Basic jai_x"),
        );
        assert!(check_bearer(&h, "jai_x").is_err());
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer "));
        assert!(check_bearer(&h, "").is_err());
    }

    #[test]
    fn constant_time() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
