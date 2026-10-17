//! `start`, `ServerHandle::stop`, discovery through `server.json`, and
//! ending running requests on stop.

mod common;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{RawQuery, State};
use axum::http::HeaderMap;
use common::*;
use jai_server::discovery::{self, ServerInfo};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

/// What a stand-in server saw: the path and query, and any Authorization.
type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// A process that answers like JAI Studio without knowing the token: on
/// `/health` it relays to `relay_to` (a real server's port), if given, and
/// otherwise makes up an answer. Returns its port and what it saw.
async fn stand_in(relay_to: Option<u16>) -> (u16, Seen) {
    async fn any(
        State((seen, relay_to)): State<(Seen, Option<u16>)>,
        uri: axum::http::Uri,
        RawQuery(query): RawQuery,
        headers: HeaderMap,
    ) -> axum::Json<Value> {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        seen.lock().unwrap().push((uri.to_string(), auth));
        if let Some(port) = relay_to {
            let url = format!(
                "http://127.0.0.1:{port}/health?{}",
                query.unwrap_or_default()
            );
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            if let Ok(res) = client.get(url).send().await {
                if let Ok(body) = res.json::<Value>().await {
                    return axum::Json(body);
                }
            }
        }
        axum::Json(json!({ "status": "ok", "name": "jai", "version": "0.1.0", "proof": "AAAA" }))
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Seen = Arc::default();
    let app = axum::Router::new()
        .fallback(any)
        .with_state((Arc::clone(&seen), relay_to));
    tokio::spawn(async move { axum::serve(listener, app).await });
    (port, seen)
}

fn stale_info(port: u16) -> ServerInfo {
    ServerInfo {
        version: "0.1.0".into(),
        pid: 4_000_000,
        host: "127.0.0.1".into(),
        port,
        started: 1,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_writes_discovery_and_stop_removes_it() {
    within(30, async {
        let t = TestCore::new(ALL);
        let paths = t.core.paths.clone();
        std::fs::write(paths.token_file(), format!("{TOKEN}\n")).unwrap();

        let handle = jai_server::start(Arc::clone(&t.core)).await.unwrap();
        let port = handle.addr.port();
        assert_ne!(port, 0);
        assert_eq!(handle.url(), format!("http://127.0.0.1:{port}"));

        let info: ServerInfo =
            serde_json::from_slice(&std::fs::read(paths.server_file()).unwrap()).unwrap();
        assert_eq!(info.port, port);
        assert_eq!(info.host, "127.0.0.1");
        assert_eq!(info.pid, std::process::id());
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert!(info.started > 1_600_000_000);

        let (found, token) = discovery::find(&paths).await.expect("the server is found");
        assert_eq!(found, info);
        assert_eq!(token, TOKEN);

        // server.json still naming an earlier, killed server on this port: the
        // answer names the process that proved the token.
        discovery::write(
            &paths,
            &discovery::ServerInfo {
                pid: 1,
                ..info.clone()
            },
        )
        .unwrap();
        let (found, _) = discovery::find(&paths).await.expect("still found");
        assert_eq!(found.pid, std::process::id());
        discovery::write(&paths, &info).unwrap();

        // A real HTTP client, with the token.
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let models: Value = client
            .get(format!("{}/v1/models", handle.url()))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(models["data"][0]["id"], "qwen3.8-27b");

        handle.stop().await;
        assert!(!paths.server_file().exists());
        assert!(discovery::find(&paths).await.is_none());
        assert!(tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_port_moves_to_the_next() {
    within(30, async {
        let blocker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let busy = blocker.local_addr().unwrap().port();
        if busy > u16::MAX - 10 {
            return;
        }
        let t = TestCore::new(ALL);
        t.core.config.write().unwrap().server.port = busy;
        let handle = jai_server::start(Arc::clone(&t.core)).await.unwrap();
        let port = handle.addr.port();
        assert!(port > busy && port <= busy + 9, "{port}");
        handle.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lan_addresses_need_allow_lan() {
    let t = TestCore::new(ALL);
    t.core.config.write().unwrap().server.host = "0.0.0.0".to_owned();
    let err = jai_server::start(Arc::clone(&t.core)).await.unwrap_err();
    assert!(err.to_string().contains("allowLan"), "{err}");
    assert!(!t.core.paths.server_file().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_needs_a_jai_server() {
    within(30, async {
        let t = TestCore::new(ALL);
        let paths = t.core.paths.clone();
        assert!(discovery::find(&paths).await.is_none(), "no server.json");

        std::fs::write(paths.token_file(), TOKEN).unwrap();
        // Nothing listens on that port any more.
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let info = |port: u16| ServerInfo {
            version: "0.1.0".into(),
            pid: 1,
            host: "127.0.0.1".into(),
            port,
            started: 1,
        };
        discovery::write(&paths, &info(dead)).unwrap();
        let begun = Instant::now();
        assert!(discovery::find(&paths).await.is_none(), "dead port");
        assert!(begun.elapsed() < Duration::from_secs(3));

        // Something that answers /health but is not JAI (an engine, say).
        let other = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let other_port = other.local_addr().unwrap().port();
        let app = axum::Router::new().route(
            "/health",
            axum::routing::get(|| async { axum::Json(json!({ "status": "ok" })) }),
        );
        tokio::spawn(async move { axum::serve(other, app).await });
        discovery::write(&paths, &info(other_port)).unwrap();
        assert!(discovery::find(&paths).await.is_none(), "not a JAI server");

        // A host name instead of an address.
        let named = ServerInfo {
            host: "localhost".into(),
            ..info(other_port)
        };
        discovery::write(&paths, &named).unwrap();
        assert!(discovery::find(&paths).await.is_none(), "host name");

        // No token.
        let handle = jai_server::start(Arc::clone(&t.core)).await.unwrap();
        std::fs::remove_file(paths.token_file()).unwrap();
        assert!(discovery::find(&paths).await.is_none(), "no token");
        handle.stop().await;
    })
    .await;
}

/// security-3 / SERVER-JS-3: after a crash leaves `server.json` behind,
/// whatever listens on its port must prove that it holds the token before a
/// client trusts it; the token itself is never sent while finding it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_trusts_only_a_server_that_proves_the_token() {
    within(30, async {
        let t = TestCore::new(ALL);
        let paths = t.core.paths.clone();
        std::fs::write(paths.token_file(), TOKEN).unwrap();

        // A stand-in that answers {"name":"jai"} on the stale port.
        let (port, seen) = stand_in(None).await;
        discovery::write(&paths, &stale_info(port)).unwrap();
        assert!(discovery::find(&paths).await.is_none(), "a made-up answer");
        let requests = seen.lock().unwrap().clone();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert!(requests[0].0.starts_with("/health?nonce="), "{requests:?}");
        assert_eq!(requests[0].1, None, "the token was sent");

        // A stand-in that relays /health to the real server on another port:
        // the proof names the real server's port, so it does not match.
        let handle = jai_server::start(Arc::clone(&t.core)).await.unwrap();
        let real = discovery::find(&paths)
            .await
            .expect("the real server proves it");
        assert_eq!(real.0.port, handle.addr.port());
        let (relay, relayed) = stand_in(Some(handle.addr.port())).await;
        discovery::write(&paths, &stale_info(relay)).unwrap();
        assert!(discovery::find(&paths).await.is_none(), "a relayed proof");
        assert!(relayed
            .lock()
            .unwrap()
            .iter()
            .all(|(_, auth)| auth.is_none()));

        // Another user's real server holds another token.
        discovery::write(&paths, &real.0).unwrap();
        std::fs::write(paths.token_file(), "jai_someone-else-entirely-0123").unwrap();
        assert!(discovery::find(&paths).await.is_none(), "another token");
        std::fs::write(paths.token_file(), TOKEN).unwrap();
        assert!(discovery::find(&paths).await.is_some());
        handle.stop().await;
    })
    .await;
}

/// security-6 (a): a server bound to another loopback address answers
/// clients that use that address, so discovery finds it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_loopback_alias_is_reachable() {
    within(30, async {
        let t = TestCore::new(ALL);
        let paths = t.core.paths.clone();
        std::fs::write(paths.token_file(), TOKEN).unwrap();
        t.core.config.write().unwrap().server.host = "127.0.0.2".to_owned();
        // Not every system routes all of 127.0.0.0/8 to loopback.
        let Ok(handle) = jai_server::start(Arc::clone(&t.core)).await else {
            return;
        };
        assert_eq!(
            handle.url(),
            format!("http://127.0.0.2:{}", handle.addr.port())
        );
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let health: Value = client
            .get(format!("{}/health", handle.url()))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(health["name"], "jai", "{health}");
        let (found, _) = discovery::find(&paths).await.expect("found on 127.0.0.2");
        assert_eq!(found.host, "127.0.0.2");
        handle.stop().await;
    })
    .await;
}

/// security-6 (b): the default bind holds [::1] on the same port, so a
/// client that resolves localhost to ::1 reaches Studio, not a squatter.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn localhost_reaches_the_server_over_ipv6_too() {
    within(30, async {
        if tokio::net::TcpListener::bind("[::1]:0").await.is_err() {
            return;
        }
        let t = TestCore::new(ALL);
        let handle = jai_server::start(Arc::clone(&t.core)).await.unwrap();
        let port = handle.addr.port();
        assert!(
            tokio::net::TcpListener::bind(("::1", port)).await.is_err(),
            "another process could take [::1]:{port}"
        );
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let models: Value = client
            .get(format!("http://[::1]:{port}/v1/models"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(models["data"][0]["id"], "qwen3.8-27b", "{models}");
        handle.stop().await;
        assert!(tokio::net::TcpStream::connect(("::1", port)).await.is_err());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_ends_running_requests() {
    within(30, async {
        let t = TestCore::new(ALL);
        let handle = jai_server::start(Arc::clone(&t.core)).await.unwrap();
        let addr = handle.addr;

        let started = t.seen.chat_started.notified();
        let request = json!({ "messages": [{ "role": "user", "content": "hang" }] });
        let mut stream = raw_post(addr, "/v1/chat/completions", &request).await;
        started.await;

        let begun = Instant::now();
        handle.stop().await;
        assert!(
            begun.elapsed() < Duration::from_secs(8),
            "{:?}",
            begun.elapsed()
        );

        let mut answer = String::new();
        let _ =
            tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut answer)).await;
        assert!(answer.starts_with("HTTP/1.1 503"), "{answer}");
        assert!(answer.contains("shutting_down"), "{answer}");

        let seen = Arc::clone(&t.seen);
        eventually("llama-server to see the request dropped", move || {
            seen.chat_aborted.load(Ordering::SeqCst) == 1
        })
        .await;
        t.wait_idle().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_handle_stops_the_server() {
    within(30, async {
        let t = TestCore::new(ALL);
        let handle = jai_server::start(Arc::clone(&t.core)).await.unwrap();
        let port = handle.addr.port();
        drop(handle);
        assert!(!t.core.paths.server_file().exists());
        eventually("the listener to close", move || {
            std::net::TcpStream::connect(("127.0.0.1", port)).is_err()
        })
        .await;
    })
    .await;
}
