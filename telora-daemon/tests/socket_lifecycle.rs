//! The daemon socket end to end: a real [`SocketServer`] and the
//! shared protocol client, with the event loop played by the test.
//!
//! The env-dependent test serialises on [`ENV_LOCK`] and holds the
//! guard across `.await` on purpose: `XDG_RUNTIME_DIR` must stay put
//! for the whole async body.
#![allow(clippy::await_holding_lock)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use telora_common::protocol::{self, Error, ErrorCode, Reply, StatusResponse};
use telora_daemon::{Command, SocketServer};

/// Process-global mutex serialising every env-dependent test in
/// this file. Dispatch tests do not touch env vars and can run
/// concurrently.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    // Recover from poisoning — the only state we mutate is the env
    // var itself, which the per-test [`EnvRestore`] always restores.
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

fn unique_tempdir() -> TempDir {
    tempfile::tempdir().expect("tempdir")
}

/// Restore the previous `XDG_RUNTIME_DIR` value (or remove the var)
/// on drop, regardless of how the test exits.
fn make_env_restore() -> impl Drop {
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", v) },
                None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
            }
        }
    }
    let prev = std::env::var("XDG_RUNTIME_DIR").ok();
    Restore(prev)
}

/// A running [`SocketServer`]; dropping it aborts the server task,
/// whose own `Drop` unlinks the socket. `_tmp` is `None` when the
/// caller owns the directory.
struct TestServer {
    _tmp: Option<TempDir>,
    join: JoinHandle<()>,
    sock_path: PathBuf,
}

impl TestServer {
    fn sock_path(&self) -> &Path {
        &self.sock_path
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.join.abort();
    }
}

/// Bind a [`SocketServer`] in a fresh directory and run it. The
/// channel holds one command, so the server waits for the test to
/// take it before answering.
fn spawn_server() -> (TestServer, mpsc::Receiver<Command>) {
    let tmp = unique_tempdir();
    let sock_path = tmp.path().join("telora/daemon.sock");
    let (cmd_tx, cmd_rx) = mpsc::channel(1);
    let server = SocketServer::bind(&sock_path, cmd_tx, false).expect("bind SocketServer");
    let join = tokio::spawn(async move {
        server.run().await;
    });
    (
        TestServer {
            _tmp: Some(tmp),
            join,
            sock_path,
        },
        cmd_rx,
    )
}

/// Like [`spawn_server`], at a path whose directory the caller owns.
fn spawn_server_at(sock_path: &Path) -> (TestServer, mpsc::Receiver<Command>) {
    let (cmd_tx, cmd_rx) = mpsc::channel(1);
    let server = SocketServer::bind(sock_path, cmd_tx, false).expect("bind SocketServer");
    let join = tokio::spawn(async move {
        server.run().await;
    });
    (
        TestServer {
            _tmp: None,
            join,
            sock_path: sock_path.to_path_buf(),
        },
        cmd_rx,
    )
}

/// Send raw bytes, optionally half-close, and decode the reply.
async fn raw(sock_path: &Path, payload: &[u8], half_close: bool) -> Reply {
    let mut client = UnixStream::connect(sock_path).await.expect("connect");
    client.write_all(payload).await.expect("write");
    if half_close {
        client.shutdown().await.expect("half-close");
    }
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut buf))
        .await
        .expect("the server must answer within 2 s")
        .expect("read");
    protocol::decode_reply(buf.strip_suffix(b"\n").expect("one line")).expect("a v1 reply")
}

async fn call(sock_path: PathBuf, cmd: &'static str) -> Reply {
    tokio::time::timeout(Duration::from_secs(2), protocol::call(&sock_path, cmd))
        .await
        .expect("the server must answer within 2 s")
        .expect("transport")
}

fn error_code(reply: Reply) -> ErrorCode {
    reply.expect_err("expected an error reply").code
}

async fn no_dispatch(cmd_rx: &mut mpsc::Receiver<Command>) {
    let spurious = tokio::time::timeout(Duration::from_millis(100), cmd_rx.recv()).await;
    assert!(
        spurious.is_err(),
        "the request must not reach the event loop"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn start_reaches_the_event_loop_and_its_answer_comes_back() {
    let (server, mut cmd_rx) = spawn_server();
    let client = tokio::spawn(call(server.sock_path().to_path_buf(), "start"));
    match cmd_rx.recv().await.expect("command") {
        Command::Start { response_tx } => response_tx.send(Ok(())).unwrap(),
        other => panic!("expected Start, got {other:?}"),
    }
    assert!(client.await.unwrap().unwrap().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn a_refused_start_keeps_its_error_code() {
    let (server, mut cmd_rx) = spawn_server();
    let client = tokio::spawn(call(server.sock_path().to_path_buf(), "start"));
    match cmd_rx.recv().await.expect("command") {
        Command::Start { response_tx } => response_tx
            .send(Err(Error::new(ErrorCode::NoModel, "cargando")))
            .unwrap(),
        other => panic!("expected Start, got {other:?}"),
    }
    assert_eq!(
        client.await.unwrap(),
        Err(Error::new(ErrorCode::NoModel, "cargando"))
    );
}

// The old protocol sent errors as text, so a dictation that began with
// "ERROR:" was thrown away by the GUI.
#[tokio::test(flavor = "current_thread")]
async fn a_transcription_that_starts_with_error_is_delivered_as_text() {
    let (server, mut cmd_rx) = spawn_server();
    let client = tokio::spawn(call(server.sock_path().to_path_buf(), "stop"));
    match cmd_rx.recv().await.expect("command") {
        Command::Stop { response_tx } => response_tx
            .send(Ok("ERROR: no compila".to_string()))
            .unwrap(),
        other => panic!("expected Stop, got {other:?}"),
    }
    let fields = client.await.unwrap().expect("a successful reply");
    assert_eq!(fields["text"], "ERROR: no compila");
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_is_acknowledged() {
    let (server, mut cmd_rx) = spawn_server();
    let client = tokio::spawn(call(server.sock_path().to_path_buf(), "cancel"));
    assert!(matches!(cmd_rx.recv().await, Some(Command::Cancel)));
    assert!(client.await.unwrap().is_ok());
}

#[tokio::test(flavor = "current_thread")]
async fn status_carries_the_status_object() {
    let (server, mut cmd_rx) = spawn_server();
    let client = tokio::spawn(call(server.sock_path().to_path_buf(), "status"));
    let status = StatusResponse {
        pid: 4242,
        model_id: "test/model.bin".to_string(),
        model_kind: "whisper".to_string(),
        model_path: String::new(),
        language: "es".to_string(),
        max_recording_seconds: 600,
        state: "Idle".to_string(),
        engine: "loading".to_string(),
    };
    match cmd_rx.recv().await.expect("command") {
        Command::GetStatus { response_tx } => response_tx.send(status.clone()).unwrap(),
        other => panic!("expected GetStatus, got {other:?}"),
    }
    let mut fields = client.await.unwrap().unwrap();
    let got: StatusResponse = serde_json::from_value(fields.remove("status").unwrap()).unwrap();
    assert_eq!(got, status);
}

#[tokio::test(flavor = "current_thread")]
async fn refresh_answers_and_errors_are_forwarded() {
    let (server, mut cmd_rx) = spawn_server();
    let sock = server.sock_path().to_path_buf();
    let client = tokio::spawn(call(sock.clone(), "refresh"));
    match cmd_rx.recv().await.expect("command") {
        Command::Refresh { response_tx } => response_tx.send(Ok("recargada".into())).unwrap(),
        other => panic!("expected Refresh, got {other:?}"),
    }
    assert_eq!(client.await.unwrap().unwrap()["message"], "recargada");

    let client = tokio::spawn(call(sock, "refresh"));
    match cmd_rx.recv().await.expect("command") {
        Command::Refresh { response_tx } => response_tx
            .send(Err(Error::new(ErrorCode::InvalidConfig, "línea 2")))
            .unwrap(),
        other => panic!("expected Refresh, got {other:?}"),
    }
    assert_eq!(error_code(client.await.unwrap()), ErrorCode::InvalidConfig);
}

#[tokio::test(flavor = "current_thread")]
async fn an_unanswered_command_is_an_internal_error() {
    let (server, mut cmd_rx) = spawn_server();
    let client = tokio::spawn(call(server.sock_path().to_path_buf(), "stop"));
    drop(cmd_rx.recv().await.expect("command"));
    assert_eq!(error_code(client.await.unwrap()), ErrorCode::Internal);
}

#[tokio::test(flavor = "current_thread")]
async fn a_closed_event_loop_is_an_internal_error() {
    let (server, cmd_rx) = spawn_server();
    drop(cmd_rx);
    let reply = call(server.sock_path().to_path_buf(), "start").await;
    assert_eq!(error_code(reply), ErrorCode::Internal);
}

// AUTO_STOP belongs to the GUI's control socket, not to the daemon's.
#[tokio::test(flavor = "current_thread")]
async fn unknown_commands_are_rejected_without_dispatch() {
    let (server, mut cmd_rx) = spawn_server();
    let reply = call(server.sock_path().to_path_buf(), "auto_stop").await;
    assert_eq!(error_code(reply), ErrorCode::UnknownCommand);
    no_dispatch(&mut cmd_rx).await;
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_requests_are_rejected_without_dispatch() {
    let (server, mut cmd_rx) = spawn_server();
    let sock = server.sock_path();
    for (payload, code) in [
        (&b""[..], ErrorCode::BadRequest),
        (b"STATUS", ErrorCode::BadRequest),
        (
            b"{\"v\":2,\"cmd\":\"status\"}\n",
            ErrorCode::UnsupportedVersion,
        ),
    ] {
        assert_eq!(error_code(raw(sock, payload, true).await), code);
    }
    no_dispatch(&mut cmd_rx).await;
}

// The old server read up to 64 KiB, closed, and the client saw a reset.
#[tokio::test(flavor = "current_thread")]
async fn an_oversized_request_gets_an_error_reply() {
    let (server, mut cmd_rx) = spawn_server();
    let payload = vec![b' '; protocol::MAX_MESSAGE_BYTES * 2];
    let reply = raw(server.sock_path(), &payload, true).await;
    assert_eq!(error_code(reply), ErrorCode::BadRequest);
    no_dispatch(&mut cmd_rx).await;
}

// The old server waited for EOF, so a client that kept its write side
// open never got an answer.
#[tokio::test(flavor = "current_thread")]
async fn a_client_that_does_not_half_close_gets_its_reply() {
    let (server, mut cmd_rx) = spawn_server();
    let sock = server.sock_path().to_path_buf();
    let client =
        tokio::spawn(async move { raw(&sock, &protocol::encode_request("cancel"), false).await });
    assert!(matches!(cmd_rx.recv().await, Some(Command::Cancel)));
    assert!(client.await.unwrap().is_ok());
}

#[tokio::test(flavor = "current_thread")]
async fn a_request_split_across_writes_is_reassembled() {
    let (server, mut cmd_rx) = spawn_server();
    let sock = server.sock_path().to_path_buf();
    let client = tokio::spawn(async move {
        let mut stream = UnixStream::connect(&sock).await.unwrap();
        stream.write_all(b"{\"v\":1,").await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        stream.write_all(b"\"cmd\":\"cancel\"}\n").await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        buf
    });
    assert!(matches!(cmd_rx.recv().await, Some(Command::Cancel)));
    assert_eq!(client.await.unwrap(), b"{\"ok\":true}\n");
}

// The old bind took over the path, leaving the running daemon unreachable.
#[tokio::test(flavor = "current_thread")]
async fn a_second_daemon_is_rejected_and_the_first_keeps_serving() {
    let (first, mut cmd_rx) = spawn_server();
    let sock_path = first.sock_path().to_path_buf();
    let (cmd_tx2, _cmd_rx2) = mpsc::channel::<Command>(1);
    let err = SocketServer::bind(&sock_path, cmd_tx2, false)
        .expect_err("a live socket must not be taken over");
    assert!(
        format!("{err:#}").contains("ya hay un telora-daemon"),
        "{err:#}"
    );

    let client = tokio::spawn(call(sock_path, "cancel"));
    assert!(matches!(cmd_rx.recv().await, Some(Command::Cancel)));
    assert!(client.await.unwrap().is_ok());
}

/// Aborting the server task drops the `SocketServer`, which must
/// unlink the socket it bound.
#[tokio::test(flavor = "current_thread")]
async fn socket_file_is_unlinked_after_run_task_is_aborted() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (server, _cmd_rx) = spawn_server();
    let sock_path = server.sock_path().to_path_buf();
    assert!(
        sock_path.exists(),
        "socket file should exist after bind (found at {})",
        sock_path.display()
    );
    drop(server);
    // The abort fires at the next scheduler tick (the run loop
    // is parked in `listener.accept().await`); poll for the
    // unlink with a 2-second deadline so a scheduler stall
    // surfaces as a test failure instead of a flake.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while sock_path.exists() {
        if std::time::Instant::now() >= deadline {
            panic!(
                "socket file {} was not unlinked within 2s after abort",
                sock_path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Env-dependent regression test from issue #27: under
/// `XDG_RUNTIME_DIR`, `paths::resolve` must pick
/// `$XDG_RUNTIME_DIR/telora/` as the socket directory, and a
/// real [`SocketServer`] bound at the resolved daemon socket
/// must complete a full `START` round-trip.
///
/// All other dispatch tests use a `tempfile::tempdir()` socket
/// path so they can run in parallel without touching
/// process-global env state. This test is the only one that
/// exercises the resolver cascade end-to-end against a real
/// server, so it is the canary for any change to
/// `telora-common/src/paths.rs::resolve` that misroutes the
/// daemon socket under `XDG_RUNTIME_DIR`.
#[tokio::test(flavor = "current_thread")]
async fn lifecycle_resolve_picks_xdg_socket_dir() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _env_guard = lock_env();
    let _restore = make_env_restore();

    let tmp = unique_tempdir();
    let xdg = tmp.path().to_str().unwrap().to_string();
    // SAFETY: serialised by `_env_guard`; `_restore` returns
    // the previous value on drop.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", &xdg) };

    let cfg = telora_daemon::paths::PathsConfig::default();
    let resolved = telora_daemon::paths::resolve(&cfg).expect("resolve");
    assert!(
        resolved.socket_dir.starts_with(tmp.path()),
        "resolver picked {} outside the temp XDG dir",
        resolved.socket_dir.display()
    );

    // Bind a real `SocketServer` at the resolved path and drive
    // a full START round-trip through it.
    let (server, mut cmd_rx) = spawn_server_at(&resolved.daemon_sock);
    let sock_path = resolved.daemon_sock.clone();
    let client_task = tokio::spawn(call(sock_path, "start"));
    match cmd_rx.recv().await.expect("command") {
        Command::Start { response_tx } => response_tx.send(Ok(())).expect("ack start"),
        other => panic!("expected Command::Start, got {other:?}"),
    }
    assert!(client_task.await.expect("client task").is_ok());

    drop(server);
    // `tmp` drops at end of scope and removes the tempdir
    // (including `resolved.socket_dir` and its socket file).
}
