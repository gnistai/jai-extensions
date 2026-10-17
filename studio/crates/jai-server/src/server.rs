//! Binding, serving and routing (DESIGN.md section 7).
//!
//! The server loop is hyper's HTTP/1 with a timer, not `axum::serve`, which
//! sets none: a client that never finishes a request head, or leaves a
//! keep-alive connection idle, is cut off after `HEADER_TIMEOUT`, and at most
//! `MAX_CONNECTIONS` are served at once, so other local users cannot use up
//! the process's file descriptors (which the engines, logs and models need).

use std::io::{self, ErrorKind};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use jai_core::Paths;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::auth::{self, HostRule};
use crate::discovery::{self, ServerInfo};
use crate::error::ApiError;
use crate::{chat, images, status, Core, VERSION};

/// Body limit for `/v1/chat/completions`, which carries images.
const CHAT_BODY_LIMIT: usize = 64 * 1024 * 1024;
/// Body limit for everything else.
const BODY_LIMIT: usize = 1024 * 1024;
/// The configured port and the nine after it.
const PORT_TRIES: u16 = 10;
/// How long `stop` lets running requests finish before ending them.
const GRACE: Duration = Duration::from_secs(3);
/// How long ended requests get to unwind before the server task is aborted.
const FORCE: Duration = Duration::from_secs(2);
/// Longest a client may take to send a request head. An idle keep-alive
/// connection is closed after this long too.
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
/// Connections served at once; further clients wait in the listen backlog.
const MAX_CONNECTIONS: usize = 256;
/// The pause after an accept error other than a refused, aborted or reset
/// connection (out of file descriptors, say).
const ACCEPT_BACKOFF: Duration = Duration::from_secs(1);

/// What the server loop allows each client.
#[derive(Clone, Copy, Debug)]
struct Limits {
    header_timeout: Duration,
    max_connections: usize,
}

const LIMITS: Limits = Limits {
    header_timeout: HEADER_TIMEOUT,
    max_connections: MAX_CONNECTIONS,
};

/// What every handler and middleware gets.
#[derive(Clone)]
pub(crate) struct Shared {
    pub core: Arc<Core>,
    pub hosts: Arc<HostRule>,
    /// Cancelled when `stop` stops waiting for running requests.
    pub shutdown: CancellationToken,
    /// Chat requests whose images are being decoded and encoded again at
    /// once, which bounds the memory that takes (`IMAGE_WORK`).
    pub image_work: Arc<Semaphore>,
}

/// Chat requests whose images are re-encoded at the same time.
const IMAGE_WORK: usize = 2;

/// A running server. The shutdown sender and the join handle are private.
#[derive(Debug)]
#[non_exhaustive]
pub struct ServerHandle {
    pub addr: SocketAddr,
    graceful: Option<oneshot::Sender<()>>,
    cancel: CancellationToken,
    join: Option<JoinHandle<()>>,
    paths: Paths,
}

impl ServerHandle {
    /// `http://127.0.0.1:<port>`. A wildcard bind is reached through loopback.
    pub fn url(&self) -> String {
        format!("http://{}", client_addr(self.addr))
    }

    /// Stops accepting connections, gives running requests a few seconds,
    /// ends the rest (their leases drop, queued image jobs are cancelled) and
    /// removes `server.json`.
    pub async fn stop(mut self) {
        if let Some(tx) = self.graceful.take() {
            let _ = tx.send(());
        }
        if let Some(mut join) = self.join.take() {
            let finished = match tokio::time::timeout(GRACE, &mut join).await {
                Ok(result) => Some(result),
                Err(_) => {
                    tracing::info!("ending requests that are still running");
                    self.cancel.cancel();
                    match tokio::time::timeout(FORCE, &mut join).await {
                        Ok(result) => Some(result),
                        Err(_) => {
                            join.abort();
                            None
                        }
                    }
                }
            };
            if let Some(Err(err)) = finished {
                tracing::warn!(error = %err, "the server task failed");
            }
        }
        discovery::remove_if_ours(&self.paths, self.addr.port());
        tracing::info!(addr = %self.addr, "server stopped");
    }
}

impl Drop for ServerHandle {
    /// Dropped without `stop`: stop accepting, let running requests finish in
    /// the background, and remove `server.json`.
    fn drop(&mut self) {
        if let Some(tx) = self.graceful.take() {
            let _ = tx.send(());
            discovery::remove_if_ours(&self.paths, self.addr.port());
        }
    }
}

/// Binds the configured address (5240, else 5241 to 5249), writes
/// `server.json` and serves in a task. A loopback address is bound together
/// with the other loopback address on the same port (see `bind`).
pub async fn start(core: Arc<Core>) -> anyhow::Result<ServerHandle> {
    let (host, port, allow_lan) = crate::read_lock(&core.config, |c| {
        (c.server.host.clone(), c.server.port, c.server.allow_lan)
    });
    let ip = bind_ip(&host, allow_lan)?;
    let listeners = bind(ip, port).await?;
    let addr = listeners[0]
        .local_addr()
        .context("could not read the bound address")?;
    let cancel = CancellationToken::new();
    let hosts = HostRule::new(addr.port(), ip);
    let app = build(Arc::clone(&core), hosts, cancel.clone());

    discovery::write(
        &core.paths,
        &ServerInfo {
            version: VERSION.to_owned(),
            pid: std::process::id(),
            host: client_addr(addr).ip().to_string(),
            port: addr.port(),
            started: unix_now(),
        },
    )?;

    let (tx, rx) = oneshot::channel::<()>();
    let join = tokio::spawn(serve(listeners, app, rx, LIMITS));
    tracing::info!(%addr, "JAI Studio server listening");
    Ok(ServerHandle {
        addr,
        graceful: Some(tx),
        cancel,
        join: Some(join),
        paths: core.paths.clone(),
    })
}

/// The API, for tests. `port` is the bound port, which the Host check and
/// the `/health` proof use.
pub fn router(core: Arc<Core>, port: u16) -> Router {
    let (host, allow_lan) = crate::read_lock(&core.config, |c| {
        (c.server.host.clone(), c.server.allow_lan)
    });
    let ip = bind_ip(&host, allow_lan).unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    build(core, HostRule::new(port, ip), CancellationToken::new())
}

fn build(core: Arc<Core>, hosts: HostRule, shutdown: CancellationToken) -> Router {
    let shared = Shared {
        core,
        hosts: Arc::new(hosts),
        shutdown,
        image_work: Arc::new(Semaphore::new(IMAGE_WORK)),
    };
    Router::new()
        .route("/health", get(status::health))
        .route("/v1/models", get(status::models))
        .route("/v1/status", get(status::status))
        .route(
            "/v1/chat/completions",
            post(chat::completions).layer(DefaultBodyLimit::max(CHAT_BODY_LIMIT)),
        )
        .route("/v1/images/generations", post(images::generations))
        .fallback(|| async { ApiError::not_found() })
        .method_not_allowed_fallback(|| async { ApiError::method_not_allowed() })
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::from_fn_with_state(
            shared.clone(),
            end_on_shutdown,
        ))
        .layer(middleware::from_fn_with_state(
            shared.clone(),
            auth::enforce,
        ))
        .with_state(shared)
}

/// Ends a running request when the server stops waiting for it. Dropping the
/// handler drops its lease and runs its cleanup.
async fn end_on_shutdown(State(shared): State<Shared>, req: Request, next: Next) -> Response {
    let shutdown = shared.shutdown.clone();
    tokio::select! {
        res = next.run(req) => res,
        () = shutdown.cancelled() => ApiError::shutting_down().into_response(),
    }
}

/// The address to bind. Anything but loopback needs `allowLan`.
fn bind_ip(host: &str, allow_lan: bool) -> anyhow::Result<IpAddr> {
    let host = host.trim();
    let ip = if host.eq_ignore_ascii_case("localhost") {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        let bare = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        bare.parse::<IpAddr>()
            .with_context(|| format!("server.host {host:?} is not an IP address"))?
    };
    if !ip.is_loopback() && !allow_lan {
        bail!(
            "server.host is {ip}, which other devices can reach: turn on allowLan \
             (or pass --allow-lan) to serve on the network"
        );
    }
    Ok(ip)
}

/// Binds `port`, else the next free one of the nine after it (port 0 asks
/// the OS for a free port at each try). The first listener is on `ip`.
async fn bind(ip: IpAddr, port: u16) -> anyhow::Result<Vec<TcpListener>> {
    let last = port.saturating_add(PORT_TRIES - 1);
    let mut error = None;
    for attempt in 0..PORT_TRIES {
        let p = if port == 0 {
            0
        } else {
            match port.checked_add(attempt) {
                Some(p) => p,
                None => break,
            }
        };
        match bind_port(ip, p).await {
            Ok(listeners) => return Ok(listeners),
            Err(err) => {
                tracing::debug!(port = p, error = %err, "port unavailable");
                error = Some(err);
            }
        }
    }
    let reason = error.map(|e| e.to_string()).unwrap_or_default();
    bail!("could not listen on {ip} at any port from {port} to {last}: {reason}")
}

/// `ip` on `port`, plus its `loopback_twin` on the same port: then
/// `localhost` reaches this server whichever address it resolves to, and no
/// other process can listen there and collect the token from clients that
/// resolve it that way. A port where the twin is taken counts as busy; a
/// twin the system lacks (no IPv6, say) is skipped, since nobody can listen
/// there either.
async fn bind_port(ip: IpAddr, port: u16) -> io::Result<Vec<TcpListener>> {
    let first = TcpListener::bind(SocketAddr::new(ip, port)).await?;
    let port = first.local_addr()?.port();
    let mut listeners = vec![first];
    if let Some(twin) = loopback_twin(ip) {
        match TcpListener::bind(SocketAddr::new(twin, port)).await {
            Ok(listener) => listeners.push(listener),
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::AddrInUse | ErrorKind::PermissionDenied
                ) =>
            {
                return Err(io::Error::new(
                    err.kind(),
                    format!("{} is taken: {err}", SocketAddr::new(twin, port)),
                ));
            }
            Err(err) => {
                tracing::debug!(%twin, error = %err, "the other loopback address is unavailable")
            }
        }
    }
    Ok(listeners)
}

/// The other loopback address for 127.0.0.1 and ::1; none for any other.
fn loopback_twin(ip: IpAddr) -> Option<IpAddr> {
    match ip {
        IpAddr::V4(v4) if v4 == Ipv4Addr::LOCALHOST => Some(IpAddr::V6(Ipv6Addr::LOCALHOST)),
        IpAddr::V6(v6) if v6 == Ipv6Addr::LOCALHOST => Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        _ => None,
    }
}

/// Serves `app` on every listener until `stop` fires (or its sender is
/// dropped). Then it stops accepting, asks each connection to finish its
/// request and close (idle ones close at once), and returns once all have.
async fn serve(
    listeners: Vec<TcpListener>,
    app: Router,
    mut stop: oneshot::Receiver<()>,
    limits: Limits,
) {
    let permits = Arc::new(Semaphore::new(limits.max_connections));
    // Connections hold a receiver: a send asks them to close, and `closed`
    // resolves when the last one has dropped its receiver.
    let (closing, closing_rx) = watch::channel(());
    loop {
        let permit = tokio::select! {
            biased;
            _ = &mut stop => break,
            permit = Arc::clone(&permits).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
        };
        let stream = tokio::select! {
            biased;
            _ = &mut stop => break,
            accepted = accept_any(&listeners) => match accepted {
                Ok(stream) => stream,
                Err(err) if is_connection_error(&err) => continue,
                Err(err) => {
                    tracing::warn!(error = %err, "could not accept a connection");
                    tokio::select! {
                        biased;
                        _ = &mut stop => break,
                        () = tokio::time::sleep(ACCEPT_BACKOFF) => continue,
                    }
                }
            },
        };
        tokio::spawn(connection(
            stream,
            app.clone(),
            permit,
            closing_rx.clone(),
            limits.header_timeout,
        ));
    }
    drop(listeners);
    drop(closing_rx);
    let _ = closing.send(());
    closing.closed().await;
}

/// The next connection on any of the listeners (there is at least one).
async fn accept_any(listeners: &[TcpListener]) -> io::Result<TcpStream> {
    let accepts = listeners.iter().map(|l| Box::pin(l.accept()));
    let (accepted, _, _) = futures::future::select_all(accepts).await;
    accepted.map(|(stream, _)| stream)
}

/// An error about one connection, not about the listener.
fn is_connection_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        ErrorKind::ConnectionRefused | ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset
    )
}

/// Serves one connection with HTTP/1 and the header timeout. The permit is
/// held until the connection closes.
async fn connection(
    stream: TcpStream,
    app: Router,
    permit: OwnedSemaphorePermit,
    mut closing: watch::Receiver<()>,
    header_timeout: Duration,
) {
    let mut http = http1::Builder::new();
    http.timer(TokioTimer::new())
        .header_read_timeout(header_timeout);
    let conn = http.serve_connection(TokioIo::new(stream), TowerToHyperService::new(app));
    let mut conn = pin!(conn);
    let result = tokio::select! {
        result = conn.as_mut() => result,
        _ = closing.changed() => {
            conn.as_mut().graceful_shutdown();
            conn.await
        }
    };
    if let Err(err) = result {
        tracing::trace!(error = %err, "connection ended with an error");
    }
    drop(closing);
    drop(permit);
}

/// Where local clients reach a server bound to `addr`.
fn client_addr(addr: SocketAddr) -> SocketAddr {
    match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port())
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), addr.port())
        }
        _ => addr,
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_addresses() {
        assert_eq!(
            bind_ip("127.0.0.1", false).unwrap(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        assert_eq!(
            bind_ip("localhost", false).unwrap(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        assert_eq!(
            bind_ip("[::1]", false).unwrap(),
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        );
        let err = bind_ip("0.0.0.0", false).unwrap_err().to_string();
        assert!(err.contains("allowLan"), "{err}");
        assert!(bind_ip("192.168.1.5", false).is_err());
        assert!(bind_ip("192.168.1.5", true).is_ok());
        assert!(bind_ip("example.com", true).is_err());
    }

    #[test]
    fn loopback_twins() {
        let v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert_eq!(loopback_twin(v4), Some(v6));
        assert_eq!(loopback_twin(v6), Some(v4));
        assert_eq!(loopback_twin("127.0.0.2".parse().unwrap()), None);
        assert_eq!(loopback_twin("192.168.1.5".parse().unwrap()), None);
        assert_eq!(loopback_twin("0.0.0.0".parse().unwrap()), None);
    }

    #[test]
    fn client_addresses() {
        let any: SocketAddr = "0.0.0.0:5240".parse().unwrap();
        assert_eq!(client_addr(any).to_string(), "127.0.0.1:5240");
        let any6: SocketAddr = "[::]:5240".parse().unwrap();
        assert_eq!(client_addr(any6).to_string(), "[::1]:5240");
        let lan: SocketAddr = "192.168.1.5:5240".parse().unwrap();
        assert_eq!(client_addr(lan), lan);
    }

    #[tokio::test]
    async fn busy_port_moves_on() {
        let first = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let busy = first.local_addr().unwrap().port();
        if busy > u16::MAX - PORT_TRIES {
            return;
        }
        let next = bind(IpAddr::V4(Ipv4Addr::LOCALHOST), busy).await.unwrap();
        let port = next[0].local_addr().unwrap().port();
        assert!(port > busy && port < busy + PORT_TRIES, "{port}");
    }

    /// Whether this system has an IPv6 loopback address.
    async fn has_ipv6() -> bool {
        TcpListener::bind("[::1]:0").await.is_ok()
    }

    /// security-6: 127.0.0.1 is bound with [::1] on the same port, so
    /// nobody else can take [::1] while the server runs.
    #[tokio::test]
    async fn loopback_binds_hold_both_addresses() {
        let listeners = bind(IpAddr::V4(Ipv4Addr::LOCALHOST), 0).await.unwrap();
        let port = listeners[0].local_addr().unwrap().port();
        if !has_ipv6().await {
            assert_eq!(listeners.len(), 1);
            return;
        }
        assert_eq!(listeners.len(), 2);
        assert_eq!(
            listeners[1].local_addr().unwrap(),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port)
        );
        assert!(TcpListener::bind(("::1", port)).await.is_err());
        assert!(TcpListener::bind(("127.0.0.1", port)).await.is_err());

        // Another loopback address is bound alone.
        if let Ok(alias) = bind("127.0.0.2".parse().unwrap(), 0).await {
            assert_eq!(alias.len(), 1);
        }
    }

    /// A port whose [::1] side another process holds counts as busy.
    #[tokio::test]
    async fn a_taken_twin_moves_on() {
        let Ok(squatter) = TcpListener::bind("[::1]:0").await else {
            return;
        };
        let taken = squatter.local_addr().unwrap().port();
        if taken > u16::MAX - PORT_TRIES {
            return;
        }
        let listeners = bind(IpAddr::V4(Ipv4Addr::LOCALHOST), taken).await.unwrap();
        let port = listeners[0].local_addr().unwrap().port();
        assert!(port > taken && port < taken + PORT_TRIES, "{port}");
        assert_eq!(listeners.len(), 2);
    }

    // ---- the server loop ----

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn hello_app() -> Router {
        Router::new().route("/", get(|| async { "hello" }))
    }

    /// Serves `hello_app` with these limits; dropping the sender stops it.
    async fn serve_hello(limits: Limits) -> (SocketAddr, oneshot::Sender<()>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();
        let join = tokio::spawn(serve(vec![listener], hello_app(), rx, limits));
        (addr, tx, join)
    }

    /// Reads until the server closes the connection, failing after `secs`.
    async fn read_until_closed(stream: &mut TcpStream, secs: u64) -> String {
        let mut out = Vec::new();
        tokio::time::timeout(Duration::from_secs(secs), stream.read_to_end(&mut out))
            .await
            .expect("the server kept the connection open")
            .ok();
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Reads until `marker` has arrived.
    async fn read_until(stream: &mut TcpStream, marker: &str) -> String {
        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        while !String::from_utf8_lossy(&out).contains(marker) {
            let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
                .await
                .expect("no answer")
                .unwrap();
            assert!(n > 0, "closed early: {}", String::from_utf8_lossy(&out));
            out.extend_from_slice(&buf[..n]);
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    const QUICK: Limits = Limits {
        header_timeout: Duration::from_millis(300),
        max_connections: 8,
    };

    /// security-9: a request head that never ends no longer holds the
    /// connection (and a file descriptor) forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unfinished_request_heads_are_cut_off() {
        let (addr, _stop, _join) = serve_hello(QUICK).await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n")
            .await
            .unwrap();
        let begun = std::time::Instant::now();
        read_until_closed(&mut stream, 5).await;
        assert!(begun.elapsed() < Duration::from_secs(3));

        // A connection that sends nothing at all is cut off as well.
        let mut silent = TcpStream::connect(addr).await.unwrap();
        read_until_closed(&mut silent, 5).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_keep_alive_connections_are_closed() {
        let (addr, _stop, _join) = serve_hello(QUICK).await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        for _ in 0..2 {
            stream
                .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .await
                .unwrap();
            let answer = read_until(&mut stream, "hello").await;
            assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        }
        let begun = std::time::Instant::now();
        read_until_closed(&mut stream, 5).await;
        assert!(begun.elapsed() < Duration::from_secs(3));
    }

    /// Past the limit, connections wait in the backlog instead of taking
    /// file descriptors; they are served once a slot is free.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connections_past_the_limit_wait() {
        let limits = Limits {
            header_timeout: Duration::from_secs(20),
            max_connections: 1,
        };
        let (addr, _stop, _join) = serve_hello(limits).await;
        let holder = TcpStream::connect(addr).await.unwrap();
        // Let the server accept the first connection.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut waiting = TcpStream::connect(addr).await.unwrap();
        waiting
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut buf = [0u8; 64];
        assert!(
            tokio::time::timeout(Duration::from_millis(500), waiting.read(&mut buf))
                .await
                .is_err(),
            "the second connection was served while the first held the only slot"
        );
        drop(holder);
        let answer = read_until(&mut waiting, "hello").await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopping_closes_idle_connections_and_the_listener() {
        let (addr, stop, join) = serve_hello(LIMITS).await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        read_until(&mut stream, "hello").await;
        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), join)
            .await
            .expect("an idle connection kept the server running")
            .unwrap();
        read_until_closed(&mut stream, 5).await;
        assert!(TcpStream::connect(addr).await.is_err());
    }
}
