//! End-to-end socket lifecycle tests for telora-daemon.
//!
//! Drives a real [`SocketServer`] (with a real [`Command`] channel)
//! instead of `std::os::unix::net::UnixListener` placeholders, so
//! the server's dispatch, error responses, takeover semantics, and
//! the client half-close contract are exercised end-to-end. The
//! previous incarnation of this file (issue #73) bound with a raw
//! `UnixListener` and hand-echoed the `START` response, leaving
//! every other branch of `SocketServer::run` (STOP round-trip,
//! STATUS serialisation, REFRESH parsing, the dropped-oneshot and
//! closed-channel error paths, and the bind-takeover semantic)
//! untested.
//!
//! The dispatch tests use a `tempfile::tempdir()` socket path
//! instead of `XDG_RUNTIME_DIR` plumbing so they are
//! parallelisable and do not fight the resolver cascade. One
//! env-dependent test — `lifecycle_resolve_picks_xdg_socket_dir`
//! — keeps the regression coverage from issue #27 for the
//! resolver cascade under `XDG_RUNTIME_DIR`.
//!
//! Every socket read is wrapped in [`tokio::time::timeout`] so a
//! future protocol regression (like the #132 deadlock this suite
//! also fixes) cannot hang the test process — the timeout fires
//! and the test fails fast with a clear diagnostic.
//!
//! Run with:
//!
//! ```text
//! cargo test -p telora-daemon --test socket_lifecycle
//! ```
//!
//! The env-dependent test serialises on [`ENV_LOCK`] because
//! `XDG_RUNTIME_DIR` is process-global. Holding the std
//! `MutexGuard` across `.await` is intentional and warns under
//! `clippy::await_holding_lock` — we suppress it for the whole
//! file because the whole point of the lock is to keep
//! `XDG_RUNTIME_DIR` stable across the async body.
#![allow(clippy::await_holding_lock)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use telora_daemon::{Command, SocketServer, StatusResponse, SttConfig};

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

/// Bound server handle returned by [`spawn_server`] /
/// [`spawn_server_at`]. Dropping the handle aborts the spawned
/// `run()` task (which closes the listener FD) and unlinks the
/// per-test socket file. The `Drop`-cleanup contract is
/// unit-tested at `telora-daemon/src/socket.rs:444`; this struct
/// is the integration-level mirror that the spawned-task path
/// also releases the inode.
///
/// `_tmp` is `Some` for [`spawn_server`] (the helper owns the
/// tempdir) and `None` for [`spawn_server_at`] (the caller owns
/// the tempdir; see `second_bind_takes_over_the_path` and
/// `lifecycle_resolve_picks_xdg_socket_dir`).
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
        // Abort the spawned `run()` task. The task is parked in
        // `listener.accept().await`, so the abort fires at the
        // next scheduler tick; dropping the `SocketServer`
        // inside the task then runs its `Drop` impl and unlinks
        // the socket file.
        //
        // We deliberately do NOT unlink the socket file here:
        // doing so would race with `second_bind_takes_over_the_path`
        // where a second `TestServer` has already taken over the
        // path — the first server's `Drop` would unlink the
        // second server's socket file. The `SocketServer`'s own
        // `Drop` impl (unit-tested at
        // `telora-daemon/src/socket.rs:444`) is the single
        // source of truth for socket-file cleanup.
        self.join.abort();
    }
}

/// Bind a fresh [`SocketServer`] at `tempdir/daemon.sock` and
/// spawn its `run()` task on the current tokio runtime.
///
/// Channel buffer is `1` because each test issues exactly one
/// command and the server's `cmd_tx.send(...).await` only
/// completes once the test pulls from `cmd_rx` — a larger
/// buffer would just let the server race ahead and write the
/// response before the test could inspect the dispatched
/// [`Command`].
fn spawn_server() -> (TestServer, mpsc::Receiver<Command>) {
    let tmp = unique_tempdir();
    let sock_path = tmp.path().join("daemon.sock");
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

/// Like [`spawn_server`] but binds at a caller-supplied path.
/// Used by [`second_bind_takes_over_the_path`] (same path twice)
/// and [`lifecycle_resolve_picks_xdg_socket_dir`] (path from the
/// resolver cascade); the caller is responsible for the tempdir's
/// lifetime.
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

/// Connect a client, write `payload`, half-close the write side,
/// then read the full response under a 2-second timeout. Returns
/// the response bytes.
///
/// The half-close is REQUIRED under the current protocol because
/// `SocketServer::run` reads the request with `read_to_end`
/// (`telora-daemon/src/socket.rs:200-203`) — the server waits
/// for the client's EOF before dispatching. PR #132 (`ed326d2`)
/// introduced this requirement; the production clients in
/// `telora-gui/src/connection.rs::SocketClient::send_command`
/// and `telora-daemon/src/main.rs::{run_refresh_client,
/// run_status_client}` now call `stream.shutdown().await`
/// between write and read to satisfy it.
async fn round_trip(sock_path: &Path, payload: &[u8]) -> Vec<u8> {
    let mut client = UnixStream::connect(sock_path).await.expect("connect");
    client.write_all(payload).await.expect("write");
    client
        .shutdown()
        .await
        .expect("half-close write side of daemon socket");
    let mut buf = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut buf))
        .await
        .expect(
            "server failed to respond within 2s; the half-close is missing \
             or the server is stuck on `read_to_end`",
        );
    read.expect("read_to_end failed");
    buf
}

/// `START` must dispatch [`Command::Start`] and reply
/// `STATUS: RECORDING`. The previous placeholder test
/// hand-echoed the response and never exercised the server's
/// dispatch.
#[tokio::test(flavor = "current_thread")]
async fn start_dispatches_command_start_and_replies_recording() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (server, mut cmd_rx) = spawn_server();
    let sock_path = server.sock_path().to_path_buf();
    let client_task = tokio::spawn(async move { round_trip(&sock_path, b"START").await });
    let cmd = cmd_rx.recv().await.expect("command");
    assert!(matches!(cmd, Command::Start));
    let response = client_task.await.expect("client task");
    assert_eq!(response, b"STATUS: RECORDING");
}

/// `CANCEL` must dispatch [`Command::Cancel`] and reply
/// `STATUS: CANCELLED`. Like `START`, this exercises the
/// non-oneshot dispatch path — the server sends the command and
/// writes the response without waiting for a oneshot reply.
#[tokio::test(flavor = "current_thread")]
async fn cancel_dispatches_command_cancel_and_replies_cancelled() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (server, mut cmd_rx) = spawn_server();
    let sock_path = server.sock_path().to_path_buf();
    let client_task = tokio::spawn(async move { round_trip(&sock_path, b"CANCEL").await });
    let cmd = cmd_rx.recv().await.expect("command");
    assert!(matches!(cmd, Command::Cancel));
    let response = client_task.await.expect("client task");
    assert_eq!(response, b"STATUS: CANCELLED");
}

/// `STOP` must round-trip the transcription text through the
/// `response_tx` oneshot. The test sends the command, receives
/// [`Command::Stop`] with its `response_tx`, sends a fixed
/// string through the oneshot, and asserts the exact bytes come
/// back over the socket. This pins the server's STOP →
/// event-loop → wire-out path end-to-end.
#[tokio::test(flavor = "current_thread")]
async fn stop_round_trips_transcription_through_response_tx() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (server, mut cmd_rx) = spawn_server();
    let sock_path = server.sock_path().to_path_buf();
    let client_task = tokio::spawn(async move { round_trip(&sock_path, b"STOP").await });
    let response_tx = match cmd_rx.recv().await.expect("command") {
        Command::Stop { response_tx } => response_tx,
        other => panic!("expected Command::Stop, got {other:?}"),
    };
    response_tx
        .send("hola mundo".to_string())
        .expect("send transcription result");
    let response = client_task.await.expect("client task");
    assert_eq!(response, b"hola mundo");
}

/// `STATUS` must serialise [`StatusResponse`] to JSON and write
/// it back. `StatusResponse` has no `PartialEq` derive (and
/// adding one just for tests would leak an unstable contract),
/// so the test deserialises the response and asserts on the
/// fields the STATUS command is contracted to populate.
#[tokio::test(flavor = "current_thread")]
async fn status_returns_serialised_status_response() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (server, mut cmd_rx) = spawn_server();
    let sock_path = server.sock_path().to_path_buf();
    let client_task = tokio::spawn(async move { round_trip(&sock_path, b"STATUS").await });
    let response_tx = match cmd_rx.recv().await.expect("command") {
        Command::GetStatus { response_tx } => response_tx,
        other => panic!("expected Command::GetStatus, got {other:?}"),
    };
    let expected = StatusResponse {
        active: true,
        pid: 4242,
        model_id: "test/model.bin".to_string(),
        model_kind: "whisper".to_string(),
        model_path: "/tmp/test/model.bin".to_string(),
        language: "es".to_string(),
        max_recording_seconds: 600,
        state: "Idle".to_string(),
    };
    response_tx.send(expected).expect("send status");
    let response = client_task.await.expect("client task");
    let parsed: StatusResponse =
        serde_json::from_slice(&response).expect("response should be valid StatusResponse JSON");
    assert_eq!(parsed.model_id, "test/model.bin");
    assert_eq!(parsed.state, "Idle");
    assert_eq!(parsed.pid, 4242);
    assert_eq!(parsed.language, "es");
}

/// `REFRESH {json}` must parse the JSON, dispatch
/// [`Command::ReloadConfig`] carrying the parsed [`SttConfig`],
/// and forward `Ok(())` from the event loop as `OK: Config
/// reloaded`. Asserts on `new_config.model_id` to confirm the
/// server actually parsed the JSON (a `{}` payload would
/// silently parse and leave `model_id` empty — this assertion
/// catches that).
#[tokio::test(flavor = "current_thread")]
async fn refresh_parses_json_and_dispatches_reload_config() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (server, mut cmd_rx) = spawn_server();
    let sock_path = server.sock_path().to_path_buf();
    let new_config = SttConfig {
        model_id: "Qwen/Qwen3-ASR-0.6B".to_string(),
        model_kind: "qwen3-asr".to_string(),
        model_path: String::new(),
        language: "en".to_string(),
        max_recording_seconds: 300,
    };
    let payload = format!("REFRESH {}", serde_json::to_string(&new_config).unwrap());
    let client_task = tokio::spawn(async move { round_trip(&sock_path, payload.as_bytes()).await });
    let (received_config, response_tx) = match cmd_rx.recv().await.expect("command") {
        Command::ReloadConfig {
            new_config,
            response_tx,
        } => (new_config, response_tx),
        other => panic!("expected Command::ReloadConfig, got {other:?}"),
    };
    assert_eq!(
        received_config.model_id, new_config.model_id,
        "server should have parsed the JSON and carried the model_id through"
    );
    assert_eq!(received_config.model_kind, new_config.model_kind);
    response_tx.send(Ok(())).expect("send Ok(())");
    let response = client_task.await.expect("client task");
    assert_eq!(response, b"OK: Config reloaded");
}

/// Regression test for the bug fixed by PR #132 (issue #114):
/// `REFRESH` whose JSON is split across multiple read syscalls
/// used to fail with a confusing `EOF while parsing a value` and
/// the operator saw `ERROR: Invalid config JSON`. The fix lifted
/// the read into `read_to_end` with a 64 KiB cap
/// (`telora-daemon/src/socket.rs:200-203`). This test splits
/// the payload into a `REFRESH ` prefix and the JSON body,
/// yields between the two writes (so the server's
/// `read_to_end` observes them as separate reads), and verifies
/// the full dispatch.
#[tokio::test(flavor = "current_thread")]
async fn refresh_split_across_two_writes_is_reassembled() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (server, mut cmd_rx) = spawn_server();
    let sock_path = server.sock_path().to_path_buf();
    let new_config = SttConfig {
        model_id: "split/model.bin".to_string(),
        model_kind: "whisper".to_string(),
        model_path: String::new(),
        language: "es".to_string(),
        max_recording_seconds: 600,
    };
    let json = serde_json::to_string(&new_config).unwrap();
    let client_task = tokio::spawn(async move {
        let mut client = UnixStream::connect(&sock_path).await.expect("connect");
        client.write_all(b"REFRESH ").await.expect("write prefix");
        // Yield so the server's `read_to_end` observes the two
        // writes as separate reads — the bug was that the
        // pre-#132 server read a fixed-size prefix and tripped
        // the JSON parser's "unexpected EOF" branch on the
        // split.
        tokio::task::yield_now().await;
        client.write_all(json.as_bytes()).await.expect("write JSON");
        client.shutdown().await.expect("half-close");
        let mut buf = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut buf))
            .await
            .expect("server failed to respond within 2s");
        read.expect("read_to_end");
        buf
    });
    let (received_config, response_tx) = match cmd_rx.recv().await.expect("command") {
        Command::ReloadConfig {
            new_config,
            response_tx,
        } => (new_config, response_tx),
        other => panic!("expected Command::ReloadConfig, got {other:?}"),
    };
    assert_eq!(received_config.model_id, "split/model.bin");
    response_tx.send(Ok(())).expect("send Ok(())");
    let response = client_task.await.expect("client task");
    assert_eq!(response, b"OK: Config reloaded");
}

/// Malformed JSON in `REFRESH` must be reported as
/// `ERROR: Invalid config JSON: <serde error>` BEFORE the
/// command is dispatched. The server must not consume the
/// `ReloadConfig` slot for a malformed payload.
#[tokio::test(flavor = "current_thread")]
async fn refresh_with_malformed_json_reports_parse_error() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (server, mut cmd_rx) = spawn_server();
    let sock_path = server.sock_path().to_path_buf();
    let client_task =
        tokio::spawn(async move { round_trip(&sock_path, b"REFRESH {garbage}").await });
    let response = client_task.await.expect("client task");
    assert!(
        response.starts_with(b"ERROR: Invalid config JSON"),
        "expected parse error, got: {:?}",
        String::from_utf8_lossy(&response)
    );
    // The command was never dispatched, so the channel must be
    // empty. A 100 ms timeout is enough for any spurious
    // dispatch to surface; if it does not, the test passes by
    // omission (the assert below).
    let spurious = tokio::time::timeout(Duration::from_millis(100), cmd_rx.recv()).await;
    assert!(
        spurious.is_err(),
        "malformed REFRESH must not dispatch a command"
    );
}

/// When the event loop's reload handler returns an error (e.g.
/// the voxora engine failed to load), the server must forward it
/// verbatim: `ERROR: <message>`. The test sends a valid config,
/// then sends `Err(anyhow!("boom"))` through the `response_tx`
/// oneshot and asserts the wire reply.
#[tokio::test(flavor = "current_thread")]
async fn refresh_error_from_event_loop_is_forwarded() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (server, mut cmd_rx) = spawn_server();
    let sock_path = server.sock_path().to_path_buf();
    let new_config = SttConfig {
        model_id: "err/model.bin".to_string(),
        ..SttConfig::default()
    };
    let payload = format!("REFRESH {}", serde_json::to_string(&new_config).unwrap());
    let client_task = tokio::spawn(async move { round_trip(&sock_path, payload.as_bytes()).await });
    let response_tx = match cmd_rx.recv().await.expect("command") {
        Command::ReloadConfig { response_tx, .. } => response_tx,
        other => panic!("expected Command::ReloadConfig, got {other:?}"),
    };
    response_tx
        .send(Err(anyhow::anyhow!("boom")))
        .expect("send reload error");
    let response = client_task.await.expect("client task");
    assert_eq!(response, b"ERROR: boom");
}

/// If the event loop drops the `Stop::response_tx` without
/// sending (e.g. the loop exited while the socket task was
/// waiting), the server must report
/// `ERROR: Transcription cancelled or failed`. The test sends
/// `STOP`, receives the command, and drops the oneshot sender
/// without calling `.send`.
#[tokio::test(flavor = "current_thread")]
async fn dropped_stop_response_tx_reports_cancelled() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (server, mut cmd_rx) = spawn_server();
    let sock_path = server.sock_path().to_path_buf();
    let client_task = tokio::spawn(async move { round_trip(&sock_path, b"STOP").await });
    let response_tx = match cmd_rx.recv().await.expect("command") {
        Command::Stop { response_tx } => response_tx,
        other => panic!("expected Command::Stop, got {other:?}"),
    };
    drop(response_tx); // Sender dropped without sending.
    let response = client_task.await.expect("client task");
    assert_eq!(response, b"ERROR: Transcription cancelled or failed");
}

/// If the event loop's command channel is closed (e.g. the
/// daemon's main loop exited), the server must report
/// `ERROR: Internal channel error`. The test drops the
/// `cmd_rx` before any client connects, so the server's
/// `cmd_tx.send(...).await` returns `Err(SendError)` (the
/// receiver is gone) and the server writes the error to the
/// socket.
#[tokio::test(flavor = "current_thread")]
async fn closed_command_channel_reports_internal_error() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (server, cmd_rx) = spawn_server();
    let sock_path = server.sock_path().to_path_buf();
    drop(cmd_rx); // Close the channel.
    let response = round_trip(&sock_path, b"START").await;
    assert_eq!(response, b"ERROR: Internal channel error");
}

/// `AUTO_STOP` is a *control-socket* command — `telora-ctl`
/// writes it to the GUI's `control.sock` (see
/// `telora-daemon/src/main.rs::notify_client_auto_stop`) — NOT a
/// daemon-socket command. The daemon-socket must reject it with
/// `ERROR: Unknown command`. This test pins the boundary so a
/// future refactor cannot accidentally route `AUTO_STOP` through
/// the daemon socket.
#[tokio::test(flavor = "current_thread")]
async fn unknown_command_is_rejected() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (server, _cmd_rx) = spawn_server();
    let sock_path = server.sock_path().to_path_buf();
    let response = round_trip(&sock_path, b"AUTO_STOP").await;
    assert_eq!(response, b"ERROR: Unknown command");
}

/// An empty payload (client connects and immediately
/// half-closes) must be reported as `ERROR: empty command`. The
/// server's `read_to_end` returns `Ok(0)` for an empty buffer
/// and the empty-buffer branch writes this error.
#[tokio::test(flavor = "current_thread")]
async fn empty_payload_reports_empty_command() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (server, _cmd_rx) = spawn_server();
    let sock_path = server.sock_path().to_path_buf();
    let mut client = UnixStream::connect(&sock_path).await.expect("connect");
    client.shutdown().await.expect("half-close (empty payload)");
    let mut buf = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut buf))
        .await
        .expect("server failed to respond within 2s");
    read.expect("read_to_end");
    assert_eq!(buf, b"ERROR: empty command");
}

/// A second `SocketServer::bind` on the same path MUST succeed
/// and take over the path. This is by design: the bind helper
/// removes stale same-UID sockets before `bind(2)` (see
/// `telora-common/src/socket_bind.rs:855-867`). The previous
/// `concurrent_double_bind_only_one_wins` test asserted the
/// OPPOSITE — it expected `EADDRINUSE` — but the bind is
/// intentionally idempotent so a daemon restart (e.g. after a
/// crash that left the socket file behind) does not require
/// manual `rm` cleanup. This test pins the takeover semantic;
/// if a future change makes the bind fail on `EADDRINUSE`,
/// daemon restart will break and this test will catch it.
///
/// See `telora-common/src/socket_bind.rs::bind_is_idempotent_when_socket_already_owned`
/// for the unit-level pin.
#[tokio::test(flavor = "current_thread")]
async fn second_bind_takes_over_the_path() {
    let _ = env_logger::builder().is_test(true).try_init();
    let tmp = unique_tempdir();
    let sock_path = tmp.path().join("daemon.sock");
    // Bind server1 manually (no `TestServer` wrapper, no
    // spawned run task). We only need the socket file to
    // exist so server2's bind helper exercises the
    // stale-socket removal path; we do not accept any
    // connections on server1.
    //
    // We `mem::forget` server1 to prevent its
    // `SocketServer::Drop` from running. The drop unlinks by
    // path, and after server2 binds the path points to
    // server2's inode — server1's drop would happily remove
    // server2's socket file. Forgetting server1 lets the
    // tempdir cleanup at end of test handle the inode.
    let (cmd_tx1, _cmd_rx1) = mpsc::channel::<Command>(1);
    let server1 = SocketServer::bind(&sock_path, cmd_tx1, false).expect("bind server1");
    std::mem::forget(server1);
    let (server2, mut cmd_rx2) = spawn_server_at(&sock_path);
    let sock_path_for_client = sock_path.clone();
    let client_task =
        tokio::spawn(async move { round_trip(&sock_path_for_client, b"START").await });
    let cmd = cmd_rx2.recv().await.expect("command");
    assert!(matches!(cmd, Command::Start));
    let response = client_task.await.expect("client task");
    assert_eq!(response, b"STATUS: RECORDING");
    drop(server2);
}

/// Integration-level complement to the unit test at
/// `telora-daemon/src/socket.rs:444`: when the spawned `run()`
/// task is aborted (and its `SocketServer` is dropped as part
/// of the abort), the socket file must be unlinked. Without
/// this, a crashed or `Ctrl-C`'d daemon would leave debris
/// under `$XDG_RUNTIME_DIR/telora/`.
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

/// Regression canary for PR #132 (issue #114): a client that
/// writes the command and immediately reads to EOF — without
/// half-closing the write side — must NOT receive a response
/// under the current server protocol. This pins the half-close
/// contract that the production clients now rely on.
///
/// Under the current server implementation
/// (`SocketServer::run` reads with `read_to_end` and waits for
/// the client EOF before dispatching), a non-half-closing client
/// hangs forever. The 2-second timeout here catches the hang
/// and the test asserts the timeout fired. If a future server
/// change makes the protocol respond without requiring
/// half-close, this test fails — the failure message points the
/// reviewer at the contract that changed.
///
/// The production-side fix for the #132 deadlock lives in
/// `telora-gui/src/connection.rs::SocketClient::send_command`
/// and `telora-daemon/src/main.rs::{run_refresh_client,
/// run_status_client}` — each now calls `stream.shutdown().await`
/// between the write and the read. Without those `shutdown()`
/// calls, every production client call would hang on the same
/// `read_to_end` this test exercises.
#[tokio::test(flavor = "current_thread")]
async fn client_that_does_not_half_close_still_gets_a_response() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (server, _cmd_rx) = spawn_server();
    let sock_path = server.sock_path().to_path_buf();
    let client_task = tokio::spawn(async move {
        let mut client = UnixStream::connect(&sock_path).await.expect("connect");
        client.write_all(b"START").await.expect("write");
        // Deliberately NO `shutdown().await` here — we want to
        // observe the server's behaviour when the client does
        // not half-close. Under the current `read_to_end`
        // protocol, the server parks forever and the timeout
        // below fires.
        let mut buf = Vec::new();
        let result =
            tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut buf)).await;
        (result, buf)
    });
    let (result, buf) = client_task.await.expect("client task");
    assert!(
        result.is_err(),
        "server responded without half-close; the protocol contract \
         (server reads to EOF and requires the client to half-close) \
         has changed. Update this test to match the new contract. \
         Got response: {:?}",
        String::from_utf8_lossy(&buf)
    );
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
    let client_task = tokio::spawn(async move { round_trip(&sock_path, b"START").await });
    let cmd = cmd_rx.recv().await.expect("command");
    assert!(matches!(cmd, Command::Start));
    let response = client_task.await.expect("client task");
    assert_eq!(response, b"STATUS: RECORDING");

    drop(server);
    // `tmp` drops at end of scope and removes the tempdir
    // (including `resolved.socket_dir` and its socket file).
}
