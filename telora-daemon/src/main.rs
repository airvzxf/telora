use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use log::{info, warn};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use telora_common::cache::resolve_voxora_cache;
use telora_common::config::Cascade;
use telora_daemon::daemon::{self, Daemon, Hooks};
use telora_daemon::{
    BridgeTranscriber, DaemonConfig, Microphone, SocketServer, StatusResponse, SttConfig,
    Transcriber, engine_family, paths, validate_stt_config,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::time::Duration;

async fn notify_client_auto_stop(control_socket: &str) {
    if let Ok(mut stream) = UnixStream::connect(control_socket).await {
        let _ = stream.write_all(b"AUTO_STOP").await;
    }
}

#[derive(Parser, Debug)]
#[command(author, version, about = "Telora Daemon - Background transcription service", long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to configuration file
    #[arg(short, long)]
    config: Option<String>,

    /// Hugging Face model id (overrides config).
    /// Example: `Qwen/Qwen3-ASR-0.6B` or
    /// `ggerganov/whisper.cpp/ggml-base.bin`.
    #[arg(long)]
    model_id: Option<String>,

    /// Engine family (`whisper` or `qwen3-asr`); overrides config.
    #[arg(long)]
    model_kind: Option<String>,

    /// Language (ISO 639-1, e.g. "es", "en"); overrides config.
    #[arg(short, long)]
    language: Option<String>,

    /// Maximum recording time in seconds (overrides config).
    #[arg(long)]
    max_recording_seconds: Option<u32>,

    /// Skip systemd socket activation and bind the daemon socket manually
    /// in `$XDG_RUNTIME_DIR/telora/daemon.sock`. Use this when running the
    /// daemon outside systemd (development, CI, ad-hoc debugging) without
    /// inheriting `LISTEN_FDS` from a parent shell.
    #[arg(long)]
    no_activation: bool,

    /// Hugging Face cache directory (overrides config).
    #[arg(long, value_name = "DIR")]
    voxora_cache: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Show daemon status and configuration
    Status,
    /// Reload configuration and restart the model if needed
    Refresh,
}

/// The shared `telora.toml` cascade plus CLI overrides, validated.
/// Any problem is fatal: running on defaults would load a different
/// model than the one configured.
fn load_config(args: &Args) -> Result<DaemonConfig> {
    let mut cfg: DaemonConfig = Cascade::standard(args.config.as_deref().map(Path::new)).load()?;

    // CLI args override
    if let Some(m) = &args.model_id {
        cfg.stt.model_id = m.clone();
    }
    if let Some(k) = &args.model_kind {
        cfg.stt.model_kind = k.clone();
    }
    if let Some(l) = &args.language {
        cfg.stt.language = l.clone();
    }
    if let Some(s) = args.max_recording_seconds {
        cfg.stt.max_recording_seconds = s;
    }

    // Filled in from the loaded engine, never from the file.
    cfg.stt.model_path = String::new();
    validate_stt_config(&cfg.stt).map_err(|e| anyhow::anyhow!("configuración inválida: {e}"))?;
    Ok(cfg)
}

async fn run_refresh_client(config: SttConfig, socket_path: &str) -> Result<()> {
    let mut stream = match UnixStream::connect(socket_path).await {
        Ok(s) => s,
        Err(e) => {
            return Err(anyhow::anyhow!(
                "Failed to connect to daemon at {}: {} (is the daemon running?)",
                socket_path,
                e
            ));
        }
    };

    let config_json = serde_json::to_string(&config)?;
    let command = format!("REFRESH {}", config_json);

    stream
        .write_all(command.as_bytes())
        .await
        .context("Failed to send refresh command to daemon")?;

    // Half-close the write side so the server's `read_to_end`
    // (telora-daemon/src/socket.rs:200-203) reaches EOF and proceeds
    // to write the response. Without this the daemon hangs forever
    // waiting for our EOF; introduced by PR #132 (ed326d2). The
    // `Refresh` subcommand would then block on `read_to_end` until
    // the operator hit Ctrl-C, masking the reload as a hang.
    stream
        .shutdown()
        .await
        .context("Failed to half-close write side of daemon socket")?;

    // Cap the response at 64 KiB to avoid an unbounded read if the
    // daemon ever leaks a non-terminating stream.
    let mut buf = Vec::new();
    let mut limited = stream.take(64 * 1024);
    limited
        .read_to_end(&mut buf)
        .await
        .context("Failed to read response from daemon")?;

    let response = String::from_utf8_lossy(&buf);
    println!("{}", response);

    Ok(())
}

async fn run_status_client(socket_path: &str) -> Result<()> {
    let mut stream = match UnixStream::connect(socket_path).await {
        Ok(s) => s,
        Err(_) => {
            println!("Telora Daemon Status");
            println!(
                "{:<10} {:<10} {:<10} {:<30} {:<10} {:<10} {:<15}",
                "ACTIVE", "PID", "KIND", "MODEL", "LANG", "MAX_SEC", "STATE"
            );
            println!(
                "{:-<10} {:-<10} {:-<10} {:-<30} {:-<10} {:-<10} {:-<15}",
                "", "", "", "", "", "", ""
            );
            println!(
                "{:<10} {:<10} {:<10} {:<30} {:<10} {:<10} {:<15}",
                "NO", "-", "-", "-", "-", "-", "STOPPED"
            );
            return Ok(());
        }
    };

    if let Err(e) = stream.write_all(b"STATUS").await {
        eprintln!("Failed to send command to daemon: {}", e);
        return Ok(());
    }

    // Half-close the write side so the server's `read_to_end`
    // (telora-daemon/src/socket.rs:200-203) reaches EOF and proceeds
    // to write the response. Without this the daemon hangs forever
    // waiting for our EOF; introduced by PR #132 (ed326d2). The
    // `Status` subcommand would then block on `read_to_end` and the
    // operator would see `telora-daemon status` hang indefinitely.
    if let Err(e) = stream.shutdown().await {
        eprintln!("Failed to half-close write side of daemon socket: {}", e);
        return Ok(());
    }

    let mut buf = Vec::new();
    if let Err(e) = stream.read_to_end(&mut buf).await {
        eprintln!("Failed to read response from daemon: {}", e);
        return Ok(());
    }

    let response = String::from_utf8_lossy(&buf);

    if response.trim().is_empty() {
        eprintln!("Empty response from daemon.");
        return Ok(());
    }

    if response.starts_with("ERROR") {
        eprintln!("Daemon returned error: {}", response);
        return Ok(());
    }

    let status: StatusResponse = match serde_json::from_str(&response) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to parse response: {} (Response: {})", e, response);
            return Ok(());
        }
    };

    println!("Telora Daemon Status");
    println!(
        "{:<10} {:<10} {:<10} {:<30} {:<10} {:<10} {:<15}",
        "ACTIVE", "PID", "KIND", "MODEL", "LANG", "MAX_SEC", "STATE"
    );
    println!(
        "{:-<10} {:-<10} {:-<10} {:-<30} {:-<10} {:-<10} {:-<15}",
        "", "", "", "", "", "", ""
    );

    let model_display = if status.model_id.len() > 28 {
        format!(
            "...{}",
            &status.model_id[status.model_id.len().saturating_sub(25)..]
        )
    } else {
        status.model_id.clone()
    };

    println!(
        "{:<10} {:<10} {:<10} {:<30} {:<10} {:<10} {:<15}",
        if status.active { "YES" } else { "NO" },
        status.pid,
        status.model_kind,
        model_display,
        status.language,
        status.max_recording_seconds,
        status.state
    );

    if status.active {
        println!(
            "\nFull Model Id:   {}\nEngine Kind:     {}",
            status.model_id, status.model_kind
        );
        if !status.engine.is_empty() {
            println!("Model State:     {}", status.engine);
        }
        if !status.model_path.is_empty() {
            println!("Resolved Path:   {}", status.model_path);
        }
    }

    Ok(())
}

/// Build a transcriber for `config`; shared by startup and REFRESH.
/// Returns the engine and its on-disk model path.
async fn build_transcriber(
    config: &SttConfig,
    voxora_cache: std::path::PathBuf,
) -> Result<(Arc<dyn Transcriber>, String)> {
    let kind = engine_family(&config.model_kind).map_err(|e| anyhow::anyhow!(e))?;
    let token = std::env::var("HF_TOKEN")
        .ok()
        .or_else(|| std::env::var("HUGGING_FACE_HUB_TOKEN").ok());

    let bridge =
        BridgeTranscriber::from_id(&config.model_id, kind, Some(voxora_cache), token).await?;
    let resolved_path = bridge.resolved_path().to_string();
    Ok((Arc::new(bridge), resolved_path))
}

/// Enforce a `0o700` mode on the voxora model-cache root so other
/// local users cannot read model weights or plant a symlink inside
/// the cache (whisper.cpp's mmap follows symlinks — see
/// `transcriber::refuse_if_symlink` for the engine-side guard).
///
/// If the directory already exists with a broader mode we log a
/// warning but DO NOT abort — the operator may have shared this
/// directory with another tool by design. If it does not exist, we
/// create it with `0o700` via `paths::ensure_dir_0700` (re-exported
/// from `telora_common`).
#[cfg(unix)]
fn secure_voxora_cache_dir(cache: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    match std::fs::metadata(cache) {
        Ok(md) if md.is_dir() => {
            let mode = md.permissions().mode();
            if mode & 0o077 != 0 {
                warn!(
                    "voxora cache directory {} has mode {:o} (world/group readable); \
                     this is a security risk in multi-user environments. \
                     Continuing — set the mode to 0o700 if no other tool needs shared access.",
                    cache.display(),
                    mode & 0o777
                );
            }
        }
        Ok(_) => {
            warn!(
                "voxora cache path {} exists but is not a directory; leaving it untouched",
                cache.display()
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Err(create_err) = paths::ensure_dir_0700(cache) {
                warn!(
                    "could not create voxora cache directory {} with mode 0o700: {create_err}; \
                     voxora-hf will create it on first download with its own (broader) mode",
                    cache.display()
                );
            }
        }
        Err(e) => {
            warn!(
                "cannot stat voxora cache directory {}: {e}; \
                 voxora-hf will create it on first download",
                cache.display()
            );
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();

    if let Some(Commands::Status) = args.command {
        // Status client best-effort: if config load fails we still
        // try to reach the daemon through the resolver's default
        // cascade (XDG_RUNTIME_DIR → /run/user/<uid>/ → /tmp/<uid>/).
        // Both errors are surfaced on stderr.
        let paths_cfg = match load_config(&args) {
            Ok(c) => paths::PathsConfig {
                socket_dir: c.paths.socket_dir.clone(),
                daemon_socket: c.paths.daemon_socket.clone(),
                control_socket: c.paths.control_socket.clone(),
            },
            Err(e) => {
                eprintln!(
                    "Error loading configuration: {}. Falling back to default socket resolver.",
                    e
                );
                paths::PathsConfig::default()
            }
        };
        let resolved = match paths::resolve(&paths_cfg) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Error resolving socket path: {}", e);
                return Ok(());
            }
        };
        let daemon_sock = resolved.daemon_sock.to_string_lossy().into_owned();
        if let Err(e) = run_status_client(&daemon_sock).await {
            eprintln!("Error querying status: {}", e);
        }
        return Ok(());
    }

    if let Some(Commands::Refresh) = args.command {
        let cfg = match load_config(&args) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Error loading configuration: {}", e);
                return Ok(());
            }
        };
        let paths_cfg = paths::PathsConfig {
            socket_dir: cfg.paths.socket_dir.clone(),
            daemon_socket: cfg.paths.daemon_socket.clone(),
            control_socket: cfg.paths.control_socket.clone(),
        };
        let resolved = paths::resolve(&paths_cfg).context("resolving daemon socket path")?;
        let daemon_sock = resolved.daemon_sock.to_string_lossy().into_owned();
        // Propagate failures so `telora-daemon refresh` exits non-zero
        // on connection / write / read errors. Hotkey wrappers and CI
        // jobs rely on the exit code to detect a successful refresh.
        run_refresh_client(cfg.stt, &daemon_sock).await?;
        return Ok(());
    }

    let daemon_cfg = match load_config(&args) {
        Ok(c) => c,
        Err(e) => {
            return Err(e.context("loading telora-daemon configuration"));
        }
    };
    let paths_config = daemon_cfg.paths.clone();
    let stt_config = daemon_cfg.stt;

    // Resolve the voxora cache root. The explicit override and the
    // `VOXORA_CACHE_DIR` env var both pin a custom location; in
    // their absence the daemon falls back to
    // `$XDG_CACHE_HOME/voxora/models/huggingface` (the legacy
    // 0.1.x layout). The `models/huggingface` suffix is
    // load-bearing: voxora-hf 0.4's default-features change enabled
    // `voxora-config`, whose `cache_root()` returns just
    // `$XDG_CACHE_HOME/voxora`. Letting `from_id` see `None` here
    // would orphan the operator's 3 GB of cached models and trigger
    // a re-download against the new (wrong) root — airvzxf/telora#79
    // took that exact shape from a different cause.
    //
    // Empty environment values fall through to the XDG default. clap rejects
    // an empty `--voxora-cache=` value before it reaches this resolver.
    // A non-empty CLI override has precedence; if it fails validation,
    // resolution goes directly to the XDG default rather than silently
    // selecting the lower-priority environment value.
    //
    // Both override sources flow through the shared resolver and its
    // traversal/symlink checks. An earlier daemon-only implementation
    // short-circuited on the raw override and accepted
    // `VOXORA_CACHE_DIR=/tmp/foo/../bar` verbatim.
    let env_cache_override = std::env::var_os("VOXORA_CACHE_DIR").map(PathBuf::from);
    let voxora_cache =
        resolve_voxora_cache(args.voxora_cache.as_deref(), env_cache_override.as_deref())
            .context("resolving voxora cache directory")?;

    // Tighten the cache directory's mode so other local users cannot
    // read model weights or plant a symlink that whisper.cpp's mmap
    // would happily follow. We do this AFTER the explicit override
    // is honoured (so operators who intentionally share a cache
    // across UIDs see a warning rather than an abort).
    #[cfg(unix)]
    secure_voxora_cache_dir(&voxora_cache);

    info!("Starting Telora Daemon...");
    info!("Model kind: {}", stt_config.model_kind);
    info!("Model id:   {}", stt_config.model_id);
    info!("Language:   {}", stt_config.language);

    let microphone = Microphone::new(daemon_cfg.audio.clone());
    let (daemon, initial_load) = Daemon::new(microphone, stt_config);

    // Socket
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    // Resolve the socket location through the [paths] cascade
    // introduced in EPIC #27. EPIC #28 lifted the resolver into
    // `telora_common::paths` so the daemon, GUI, and CLI all share
    // the same cascade.
    let paths_cfg = paths::PathsConfig {
        socket_dir: paths_config.socket_dir.clone(),
        daemon_socket: paths_config.daemon_socket.clone(),
        control_socket: paths_config.control_socket.clone(),
    };
    let resolved_paths = paths::resolve(&paths_cfg)?;
    let socket_server =
        SocketServer::bind(&resolved_paths.daemon_sock, cmd_tx, !args.no_activation)
            .context("Failed to bind socket")?;

    tokio::spawn(async move {
        socket_server.run().await;
    });

    // READY=1 goes out before the model loads: loading large models can
    // exceed systemd's start timeout, and a killed start used to leave the
    // socket unit pointing at a deleted path.
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("NOTIFY_SOCKET").is_some()
            && let Err(e) =
                libsystemd::daemon::notify(true, &[libsystemd::daemon::NotifyState::Ready])
        {
            log::warn!("sd_notify(READY=1) failed: {}", e);
        }
    }

    // Graceful shutdown on SIGTERM (systemd) and SIGINT (dev shell).
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    {
        let mut sigterm = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
        let mut sigint = signal(SignalKind::interrupt()).context("installing SIGINT handler")?;
        tokio::spawn(async move {
            tokio::select! {
                _ = sigterm.recv() => info!("Received SIGTERM; initiating graceful shutdown"),
                _ = sigint.recv()  => info!("Received SIGINT; initiating graceful shutdown"),
            }
            let _ = shutdown_tx.send(true);
        });
    }

    info!(
        "Socket ready on {}; loading model in the background",
        resolved_paths.daemon_sock.display()
    );

    let control_sock = resolved_paths.control_sock.to_string_lossy().into_owned();
    let hooks = Hooks {
        load_engine: Box::new(move |config| {
            let cache = voxora_cache.clone();
            Box::pin(async move {
                build_transcriber(&config, cache)
                    .await
                    .map_err(|e| format!("{e:#}"))
            })
        }),
        notify_auto_stop: Box::new(move || {
            let control_sock = control_sock.clone();
            tokio::spawn(async move { notify_client_auto_stop(&control_sock).await });
        }),
    };
    daemon::run(
        daemon,
        initial_load,
        cmd_rx,
        shutdown_rx,
        hooks,
        Duration::from_millis(20),
    )
    .await;

    info!("Telora daemon stopped cleanly");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn build_error(model_kind: &str) -> String {
        let cache = tempfile::tempdir().unwrap();
        let config = SttConfig {
            model_kind: model_kind.to_string(),
            model_id: "asr-1.0".to_string(),
            ..telora_daemon::socket::default_stt_config()
        };
        match build_transcriber(&config, cache.path().to_path_buf()).await {
            Ok(_) => panic!("model_kind {model_kind:?} must be rejected"),
            Err(e) => format!("{e:#}"),
        }
    }

    // A telora.toml left over from the hosted-engine era must fail with
    // a clear message instead of a confusing registry error.
    #[tokio::test]
    async fn removed_hosted_engine_is_rejected_by_name() {
        let err = build_error("minimax").await;
        assert!(err.contains("motor no soportado"), "{err}");
        assert!(err.contains("minimax"), "{err}");
    }

    #[tokio::test]
    async fn unknown_engine_is_rejected() {
        let err = build_error("parakeet").await;
        assert!(err.contains("motor no soportado"), "{err}");
    }
}
