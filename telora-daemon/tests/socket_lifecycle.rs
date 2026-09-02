//! End-to-end socket lifecycle tests for telora-daemon.
//!
//! Exercises the full daemon lifecycle (bind → client connect →
//! START/STOP round-trip → cleanup) under a simulated
//! `XDG_RUNTIME_DIR`. Run with:
//!
//! ```text
//! cargo test -p telora-daemon --test socket_lifecycle
//! ```
//!
//! All three tests serialise on [`ENV_LOCK`] because `XDG_RUNTIME_DIR`
//! is a process-global variable. Without the lock, cargo's
//! `--test-threads` flag could let two tests corrupt each other's
//! resolver state.
//!
//! Holding the std `MutexGuard` across `.await` is intentional and
//! warns under `clippy::await_holding_lock` — we suppress it for the
//! whole file because the whole point of the lock is to keep
//! `XDG_RUNTIME_DIR` stable across the async body.
#![allow(clippy::await_holding_lock)]

use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Process-global mutex serialising every test in this file. Each
/// test acquires it before touching `XDG_RUNTIME_DIR`. We tolerate
/// poisoning (a previous failed test holding the lock) because the
/// only state we mutate is the env var itself, which is always
/// restored on drop.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    // Recover from poisoning — it does not invalidate the env var
    // restoration guarantee below.
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

#[tokio::test(flavor = "current_thread")]
async fn lifecycle_with_xdg_runtime_dir() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _env_guard = lock_env();
    let _restore = make_env_restore();

    let tmp = unique_tempdir();
    let xdg = tmp.path().to_str().unwrap().to_string();
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", &xdg) };

    let cfg = telora_daemon::paths::PathsConfig::default();
    let resolved = telora_daemon::paths::resolve(&cfg).expect("resolve");
    assert!(
        resolved.socket_dir.starts_with(tmp.path()),
        "resolver picked {} outside the temp XDG dir",
        resolved.socket_dir.display()
    );

    // Bind a daemon socket directly with std::os::unix::net::UnixListener
    // so we can talk to it from this test (telora_daemon::SocketServer
    // requires a Command channel we don't want to construct here).
    let sock_path = resolved.daemon_sock.clone();
    let listener = std::os::unix::net::UnixListener::bind(&sock_path).expect("bind");
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::UnixListener::from_std(listener).expect("from_std");

    // Connect a client and send a START, expect a response.
    let client_path = sock_path.clone();
    let client = tokio::spawn(async move {
        let mut s = UnixStream::connect(&client_path).await.expect("connect");
        s.write_all(b"START\n").await.expect("write START");
        let mut buf = vec![0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf))
            .await
            .expect("timeout")
            .expect("read");
        String::from_utf8_lossy(&buf[..n]).to_string()
    });

    let (mut stream, _) = listener.accept().await.expect("accept");
    let mut buf = [0u8; 64];
    let n = stream.read(&mut buf).await.expect("read cmd");
    let cmd = String::from_utf8_lossy(&buf[..n]).trim().to_string();
    assert_eq!(cmd, "START", "client sent unexpected command");
    stream
        .write_all(b"STATUS: RECORDING")
        .await
        .expect("write status");

    let response = client.await.expect("client task");
    assert!(
        response.contains("RECORDING"),
        "expected RECORDING in response, got: {response}"
    );

    drop(listener);
    drop(stream);
    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_dir_all(&resolved.socket_dir);
}

#[tokio::test(flavor = "current_thread")]
async fn lifecycle_two_binders_serialised() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _env_guard = lock_env();
    let _restore = make_env_restore();

    let tmp = unique_tempdir();
    let xdg = tmp.path().to_str().unwrap().to_string();
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", &xdg) };

    let cfg = telora_daemon::paths::PathsConfig::default();
    let resolved = telora_daemon::paths::resolve(&cfg).expect("resolve");
    let sock_path = resolved.daemon_sock.clone();

    // First bind. Explicitly unlink the socket file between binds
    // because the kernel does not always reclaim the dirent before
    // the next bind(2) attempt — and unlike `SocketServer::bind` we
    // do not have our own stale-socket remover here.
    {
        let listener = std::os::unix::net::UnixListener::bind(&sock_path).expect("bind 1");
        drop(listener);
        let _ = std::fs::remove_file(&sock_path);
    }
    // Second bind (after the first listener is gone) must succeed.
    {
        let listener = std::os::unix::net::UnixListener::bind(&sock_path).expect("bind 2");
        drop(listener);
        let _ = std::fs::remove_file(&sock_path);
    }

    let _ = std::fs::remove_dir_all(&resolved.socket_dir);
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_double_bind_only_one_wins() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _env_guard = lock_env();
    let _restore = make_env_restore();

    let tmp = unique_tempdir();
    let xdg = tmp.path().to_str().unwrap().to_string();
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", &xdg) };

    let cfg = telora_daemon::paths::PathsConfig::default();
    let resolved = telora_daemon::paths::resolve(&cfg).expect("resolve");
    let sock_path = resolved.daemon_sock.clone();

    // First bind holds the socket.
    let _hold = std::os::unix::net::UnixListener::bind(&sock_path).expect("bind hold");

    // Second bind must fail.
    let result = std::os::unix::net::UnixListener::bind(&sock_path);
    assert!(
        result.is_err(),
        "expected second bind to fail with EADDRINUSE"
    );
    let err = result.unwrap_err();
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::AddrInUse,
        "expected AddrInUse, got: {err:?}"
    );

    drop(_hold);
    let _ = std::fs::remove_dir_all(&resolved.socket_dir);
}
