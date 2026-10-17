//! Process supervision against a fake engine. The fake is this test binary
//! started again with `JAI_ENGINES_FAKE_ENGINE` set: `fake_engine_main` then
//! prints a few lines and serves `GET /health`, 503 until it is ready.

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::time::Instant;

use super::*;
use crate::launch::engine_addr;
use crate::EngineKind;

const HELPER_ENV: &str = "JAI_ENGINES_FAKE_ENGINE";

/// Not a test: the fake engine's body when this binary runs as the helper.
#[test]
fn fake_engine_main() {
    if let Ok(options) = std::env::var(HELPER_ENV) {
        fake_engine(&options);
    }
}

fn fake_engine(options: &str) -> ! {
    let option = |key: &str| {
        options
            .split(';')
            .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
            .map(str::to_owned)
    };
    let addr: SocketAddr = option("addr").and_then(|a| a.parse().ok()).expect("addr");
    let delay = Duration::from_millis(option("delay_ms").map_or(0, |d| d.parse().expect("delay")));
    // End libtest's "test fake_engine_main ... " line.
    println!();
    println!("fake engine: starting");
    eprintln!("fake engine: loading model");
    let mut stdout = std::io::stdout();
    stdout.write_all(b"bytes \xff end\r\n").expect("stdout");
    stdout.flush().expect("stdout");
    if let Some(code) = option("exit") {
        std::thread::sleep(Duration::from_millis(200));
        eprintln!("fatal: model file not found");
        std::process::exit(code.parse().expect("exit code"));
    }
    #[cfg(unix)]
    if option("ignore_term").is_some() {
        // SAFETY: setting a signal disposition before any other thread exists.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
    }
    if option("no_listen").is_some() {
        // Loads forever and never binds, like sd-server with a big model.
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    let listener = TcpListener::bind(addr).expect("bind");
    let ready_at = Instant::now() + delay;
    println!("fake engine: listening on {addr}");
    let mut said_ready = false;
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let ready = Instant::now() >= ready_at;
        if ready && !said_ready {
            println!("fake engine: ready");
            said_ready = true;
        }
        let _ = answer(&mut stream, ready);
    }
    std::process::exit(0)
}

fn answer(stream: &mut TcpStream, ready: bool) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&buf[..n]);
    }
    let (status, body) = if !head.starts_with(b"GET /health ") {
        ("404 Not Found", "{}")
    } else if ready {
        ("200 OK", r#"{"status":"ok"}"#)
    } else {
        (
            "503 Service Unavailable",
            r#"{"error":{"code":503,"message":"Loading model"}}"#,
        )
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

/// A spec that starts the fake engine with these options.
fn fake_spec(dir: &Path, options: &str) -> LaunchSpec {
    let addr = engine_addr().expect("free address");
    LaunchSpec {
        kind: EngineKind::Llm,
        program: std::env::current_exe().expect("test binary"),
        args: [
            "process::tests::fake_engine_main",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ]
        .map(String::from)
        .to_vec(),
        cwd: dir.to_path_buf(),
        env: vec![(HELPER_ENV.into(), format!("addr={addr};{options}"))],
        addr,
        api_key: Some("test-key".into()),
        health_path: "/health",
        log_file: dir.join("logs").join("llm.log"),
    }
}

/// Waits until nothing listens on the address any more.
async fn wait_until_gone(addr: SocketAddrV4) {
    let addr = SocketAddr::V4(addr);
    let deadline = Instant::now() + Duration::from_secs(15);
    while TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok() {
        assert!(Instant::now() < deadline, "the engine is still running");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Waits until the log ring holds every wanted line.
async fn wait_for_lines(engine: &EngineProcess, wanted: &[&str]) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let lines = engine.log_tail(output::RING_LINES);
        if wanted.iter().all(|w| lines.iter().any(|l| l == w)) {
            return;
        }
        assert!(Instant::now() < deadline, "missing lines; have {lines:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn waits_for_health_captures_output_and_stops() {
    let dir = tempfile::tempdir().unwrap();
    let spec = fake_spec(dir.path(), "delay_ms=800");
    let (addr, log_file) = (spec.addr, spec.log_file.clone());
    let t0 = Instant::now();
    let mut engine = EngineProcess::start(spec).await.expect("spawn");
    engine
        .wait_ready(Duration::from_secs(30))
        .await
        .expect("ready");
    assert!(
        t0.elapsed() >= Duration::from_millis(800),
        "503 while loading"
    );
    assert!(!engine.exited());
    assert_eq!(engine.base_url(), format!("http://{addr}"));
    assert!(addr.ip().is_loopback());
    assert_eq!(engine.api_key().as_deref(), Some("test-key"));
    #[cfg(unix)]
    {
        let pid = libc::pid_t::try_from(engine.child.id().expect("pid")).unwrap();
        // SAFETY: plain system call.
        assert_eq!(
            unsafe { libc::getpgid(pid) },
            pid,
            "the engine leads its own group"
        );
    }
    wait_for_lines(
        &engine,
        &[
            "fake engine: starting",
            "fake engine: loading model",
            "bytes \u{fffd} end",
            "fake engine: ready",
        ],
    )
    .await;
    engine.stop().await;
    assert!(TcpStream::connect_timeout(&SocketAddr::V4(addr), Duration::from_secs(5)).is_err());
    let file = std::fs::read_to_string(&log_file).expect("log file");
    assert!(file.contains(" UTC starting "), "{file}");
    assert!(file.contains("\nfake engine: loading model\n"), "{file}");
    assert!(file.contains(" UTC stopped ("), "{file}");
}

#[tokio::test]
async fn early_exit_fails_at_once_with_the_log_tail() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = EngineProcess::start(fake_spec(dir.path(), "exit=3"))
        .await
        .expect("spawn");
    let t0 = Instant::now();
    let err = engine
        .wait_ready(Duration::from_secs(120))
        .await
        .expect_err("it exits");
    assert!(t0.elapsed() < Duration::from_secs(10), "failed early");
    let start = err.downcast_ref::<StartError>().expect("a StartError");
    let name = program_name(&std::env::current_exe().unwrap());
    assert!(
        start
            .message
            .starts_with(&format!("{name} exited before it was ready (exit ")),
        "{}",
        start.message
    );
    assert!(
        start.message.ends_with(": fatal: model file not found"),
        "{}",
        start.message
    );
    assert!(
        start.log.iter().any(|l| l == "fake engine: loading model"),
        "{:?}",
        start.log
    );
    assert!(engine.exited());
    engine.stop().await;
}

#[tokio::test]
async fn a_slow_engine_times_out_with_the_log_tail() {
    let dir = tempfile::tempdir().unwrap();
    let spec = fake_spec(dir.path(), "delay_ms=600000");
    let addr = spec.addr;
    let mut engine = EngineProcess::start(spec).await.expect("spawn");
    let err = engine
        .wait_ready(Duration::from_millis(1500))
        .await
        .expect_err("too slow");
    let start = err.downcast_ref::<StartError>().expect("a StartError");
    assert!(
        start
            .message
            .contains(" did not become ready within 1.5 s: "),
        "{}",
        start.message
    );
    assert!(start
        .log
        .iter()
        .any(|l| l.starts_with("fake engine: listening on")));
    engine.stop().await;
    wait_until_gone(addr).await;
}

#[tokio::test]
async fn dropping_the_process_kills_it() {
    let dir = tempfile::tempdir().unwrap();
    let spec = fake_spec(dir.path(), "delay_ms=0");
    let addr = spec.addr;
    let mut engine = EngineProcess::start(spec).await.expect("spawn");
    engine
        .wait_ready(Duration::from_secs(30))
        .await
        .expect("ready");
    drop(engine);
    wait_until_gone(addr).await;
}

#[tokio::test]
async fn a_missing_program_cannot_start() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = fake_spec(dir.path(), "");
    spec.program = dir.path().join("no-such-engine");
    let err = EngineProcess::start(spec).await.err().expect("no program");
    assert!(
        format!("{err:#}").starts_with("could not start "),
        "{err:#}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_then_sigkill_after_3_s() {
    let dir = tempfile::tempdir().unwrap();
    let spec = fake_spec(dir.path(), "ignore_term=1");
    let addr = spec.addr;
    let mut engine = EngineProcess::start(spec).await.expect("spawn");
    engine
        .wait_ready(Duration::from_secs(30))
        .await
        .expect("ready");
    let t0 = Instant::now();
    engine.stop().await;
    let took = t0.elapsed();
    assert!(
        took >= Duration::from_secs(3) && took < Duration::from_secs(8),
        "{took:?}"
    );
    assert!(TcpStream::connect_timeout(&SocketAddr::V4(addr), Duration::from_secs(5)).is_err());
}

/// Started from a thread that then ends, as when jai dies: the death signal
/// kills the engine, with no `stop` and no kill on drop.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_engine_dies_with_the_thread_that_started_it() {
    let dir = tempfile::tempdir().unwrap();
    let spec = fake_spec(dir.path(), "delay_ms=0");
    let addr = spec.addr;
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut engine = EngineProcess::start(spec).await.expect("spawn");
            engine
                .wait_ready(Duration::from_secs(30))
                .await
                .expect("ready");
            std::mem::forget(engine);
        });
    })
    .join()
    .unwrap();
    wait_until_gone(addr).await;
}

/// Closing the Job object, as when jai dies, kills the engine.
#[cfg(windows)]
#[tokio::test]
async fn closing_the_job_kills_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let spec = fake_spec(dir.path(), "delay_ms=0");
    let addr = spec.addr;
    let mut engine = EngineProcess::start(spec).await.expect("spawn");
    engine
        .wait_ready(Duration::from_secs(30))
        .await
        .expect("ready");
    let job = engine._job.take();
    assert!(job.is_some(), "the engine is in a job");
    drop(job);
    wait_until_gone(addr).await;
    assert!(engine.exited());
}

/// Another account's program takes the engine's address while the engine
/// loads and answers the health check: the start fails, and nothing is
/// sent to it.
#[tokio::test]
async fn a_squatter_on_the_address_is_not_taken_for_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let spec = fake_spec(dir.path(), "no_listen=1");
    let squatter = TcpListener::bind(spec.addr).expect("squat");
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in squatter.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut head = [0u8; 512];
            let n = stream.read(&mut head).unwrap_or(0);
            log.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&head[..n]).into_owned());
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
        }
    });
    let mut engine = EngineProcess::start(spec).await.expect("spawn");
    let err = engine
        .wait_ready(Duration::from_secs(30))
        .await
        .expect_err("not our engine");
    let start = err.downcast_ref::<StartError>().expect("a StartError");
    assert!(
        start.message.contains("another program answered on"),
        "{}",
        start.message
    );
    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 1, "only the health check: {requests:?}");
    assert!(!requests[0].contains("test-key"), "no key sent");
    engine.stop().await;
}

/// Answers every connection with `response`.
fn serve(listener: TcpListener, response: String) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut head = [0u8; 512];
            let _ = stream.read(&mut head);
            let _ = stream.write_all(response.as_bytes());
        }
    });
}

/// A redirect is not an answer, and is not followed (here to a server that
/// would answer 200).
#[tokio::test]
async fn a_redirect_is_not_ready() {
    let dir = tempfile::tempdir().unwrap();
    let spec = fake_spec(dir.path(), "no_listen=1");
    let elsewhere = TcpListener::bind("127.0.0.1:0").unwrap();
    let target = elsewhere.local_addr().unwrap();
    serve(
        elsewhere,
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into(),
    );
    serve(
        TcpListener::bind(spec.addr).expect("squat"),
        format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{target}/health\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        ),
    );
    let mut engine = EngineProcess::start(spec).await.expect("spawn");
    let err = engine
        .wait_ready(Duration::from_millis(1200))
        .await
        .expect_err("never ready");
    let start = err.downcast_ref::<StartError>().expect("a StartError");
    assert!(
        start.message.contains("did not become ready"),
        "{}",
        start.message
    );
    engine.stop().await;
}

/// Cancelling a start stops the engine before `start_ready` returns.
#[tokio::test]
async fn a_cancelled_start_stops_the_engine_first() {
    let dir = tempfile::tempdir().unwrap();
    let spec = fake_spec(dir.path(), "delay_ms=600000");
    let addr = spec.addr;
    let log_file = spec.log_file.clone();
    let cancel = CancellationToken::new();
    let starting = tokio::spawn({
        let cancel = cancel.clone();
        async move { start_ready(spec, Duration::from_secs(120), &cancel).await }
    });
    // Wait until the engine is up and answering 503.
    let deadline = Instant::now() + Duration::from_secs(15);
    while TcpStream::connect_timeout(&SocketAddr::V4(addr), Duration::from_secs(1)).is_err() {
        assert!(Instant::now() < deadline, "the engine never listened");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    cancel.cancel();
    let err = starting.await.unwrap().err().expect("cancelled");
    assert!(format!("{err:#}").contains("was cancelled"), "{err:#}");
    assert!(
        TcpStream::connect_timeout(&SocketAddr::V4(addr), Duration::from_secs(2)).is_err(),
        "gone when start_ready returned"
    );
    let file = std::fs::read_to_string(&log_file).expect("log file");
    assert!(file.contains(" UTC start cancelled\n"), "{file}");
    assert!(file.contains(" UTC stopped ("), "{file}");
}

#[test]
fn exit_hints() {
    assert!(ntstatus_hint(0xC000_0135).unwrap().contains("Visual C++"));
    assert_eq!(ntstatus_hint(3), None);
    assert_eq!(seconds(Duration::from_secs(240)), "240 s");
    assert_eq!(seconds(Duration::from_millis(1500)), "1.5 s");
    assert_eq!(
        program_name(Path::new("/x/llama-server.exe")),
        "llama-server"
    );
}
