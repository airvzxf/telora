use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::{Config, File};
use log::{error, info, warn};
use ringbuf::HeapRb;
use std::path::PathBuf;
use std::sync::Arc;
use telora_common::cache::resolve_voxora_cache;
use telora_common::env::telora_env_source;
use telora_daemon::{
    AudioEngine, BridgeTranscriber, Command, DaemonConfig, SocketServer, StatusResponse, SttConfig,
    Transcriber, paths,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{RwLock, mpsc, oneshot};
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
    /// `ggerganov/whisper.cpp/ggml-base.bin`. Ignored when
    /// `model_kind = "minimax"` (MiniMax is a hosted API; the
    /// bearer token comes from the env).
    #[arg(long)]
    model_id: Option<String>,

    /// Engine family (`whisper`, `qwen3-asr`, or `minimax`);
    /// overrides config. `minimax` requires `MINIMAX_API_KEY` (or
    /// `VOXORA_MINIMAX_API_KEY`) in the daemon's environment.
    #[arg(long)]
    model_kind: Option<String>,

    /// Path to a `.env`-style file containing the MiniMax bearer
    /// token (closes #166). Overrides the default discovery cascade
    /// (`/etc/telora/.env` → `$XDG_CONFIG_HOME/telora/.env` →
    /// `VOXORA_MINIMAX_API_KEY` → `MINIMAX_API_KEY`). Only
    /// consulted when `model_kind = "minimax"`. Useful for CI
    /// runners and one-off dev shells that keep their secret in
    /// a project-local `.env`.
    #[arg(long, value_name = "PATH")]
    minimax_env_file: Option<PathBuf>,

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

#[derive(PartialEq)]
enum State {
    Idle,
    Recording,
    Processing,
}

/// The model is loaded after the socket is ready, so the daemon can be
/// up without a usable engine; every caller must check which case applies.
enum Engine {
    Loading,
    Ready(Arc<dyn Transcriber>),
    Failed(String),
}

impl Engine {
    fn status_label(&self) -> String {
        match self {
            Engine::Loading => "loading".to_string(),
            Engine::Ready(_) => "ready".to_string(),
            Engine::Failed(e) => format!("failed: {e}"),
        }
    }

    /// Why START cannot proceed, or `None` when the engine is usable.
    fn unavailable_reason(&self) -> Option<String> {
        match self {
            Engine::Loading => Some("el modelo todavía se está cargando".to_string()),
            Engine::Ready(_) => None,
            Engine::Failed(e) => Some(format!("no se pudo cargar el modelo: {e}")),
        }
    }
}

struct DaemonState {
    engine: Engine,
    stt_config: SttConfig,
}

/// Load the engine in the background and publish the outcome into
/// `state`. Copies the engine's resolved id/path/endpoint back into the
/// config so STATUS shows what was actually loaded.
fn spawn_engine_load(
    state: Arc<RwLock<DaemonState>>,
    config: SttConfig,
    voxora_cache: PathBuf,
    minimax_env_file: Option<PathBuf>,
) {
    tokio::spawn(async move {
        state.write().await.engine = Engine::Loading;
        match build_transcriber(&config, voxora_cache, minimax_env_file.as_deref()).await {
            Ok((transcriber, model_id, path, endpoint)) => {
                let mut s = state.write().await;
                s.engine = Engine::Ready(transcriber);
                s.stt_config.model_id = model_id;
                s.stt_config.model_path = path;
                s.stt_config.endpoint = endpoint;
                info!("Model loaded; ready to transcribe.");
            }
            Err(e) => {
                let msg = format!("{e:#}");
                error!("Failed to load model: {msg}");
                state.write().await.engine = Engine::Failed(msg);
            }
        }
    });
}

/// Load and merge configuration from the four-tier cascade
/// (`/etc/telora.toml`, `~/.config/telora/config.toml`, the
/// `--config` CLI override, and `TELORA_*` env vars). Returns a
/// [`DaemonConfig`] which wraps both the STT settings and the
/// `[paths]` overrides added in sub-issue #33.
fn load_config(args: &Args) -> Result<DaemonConfig> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());

    // Load configuration from multiple sources in order of precedence (last one wins).
    let mut builder = Config::builder();

    // 1. System config (/etc/telora.toml) - Lowest priority
    builder = builder.add_source(File::with_name("/etc/telora.toml").required(false));

    // 2. User config (~/.config/telora/config.toml)
    builder = builder.add_source(
        File::with_name(&format!("{}/.config/telora/config.toml", home)).required(false),
    );

    // 3. Explicit config file via CLI --config
    if let Some(cfg_path) = &args.config {
        builder = builder.add_source(File::with_name(cfg_path));
    }

    // 4. Environment variables - Highest priority. The source
    // construction is centralised in [`telora_common::env::telora_env_source`]
    // (the daemon's `TELORA_*` environment-source helper) because
    // `config` 0.13's defaults silently drop `TELORA_PATHS__SOCKET_DIR`;
    // see that helper's rustdoc for the why. The integration test
    // `telora-daemon/tests/config_env_cascade.rs` calls the same
    // helper through `telora_daemon::telora_env_source` (a re-export
    // of the `telora-common` helper that survives the move so the
    // test does not have to change) to pin the behaviour.
    builder = builder.add_source(telora_env_source());

    let mut cfg: DaemonConfig = match builder.build() {
        Ok(c) => c
            .try_deserialize()
            .context("loading telora config (telora.toml / --config / TELORA_*)")?,
        Err(e) => {
            warn!("Configuration warning: {}. Using defaults.", e);
            DaemonConfig::default()
        }
    };

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

    // Legacy compatibility: if the user's telora.toml only supplies
    // `model_path`, treat it as a Whisper `model_id` so existing
    // configs keep working. The `model_path` field was a local file
    // path in the pre-voxora daemon; HF ids are forward-slash
    // separated, so the two are unambiguous in practice.
    if cfg.stt.model_id.is_empty() && !cfg.stt.model_path.is_empty() {
        cfg.stt.model_id = cfg.stt.model_path.clone();
    }

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
        // Closes #167: branch on whether the engine is hosted
        // (`status.endpoint` non-empty) or on-disk
        // (`status.model_path` non-empty). The two fields are
        // mutually exclusive by construction — `build_transcriber`
        // populates exactly one per engine — so printing both
        // would only confuse operators.
        println!(
            "\nFull Model Id:   {}\nEngine Kind:     {}",
            status.model_id, status.model_kind
        );
        if !status.engine.is_empty() {
            println!("Model State:     {}", status.engine);
        }
        if !status.endpoint.is_empty() {
            println!("Endpoint:        {}", status.endpoint);
        }
        if !status.model_path.is_empty() {
            println!("Resolved Path:   {}", status.model_path);
        }
    }

    Ok(())
}

/// Async constructor for a fresh [`BridgeTranscriber`] from an
/// [`SttConfig`]. Centralised so the daemon's startup and
/// `ReloadConfig` handler both go through the same path.
///
/// `minimax_env_file` (closes #166) is the optional `--minimax-env-file`
/// CLI override; `None` falls back to the default discovery
/// cascade (`/etc/telora/.env` → `$XDG_CONFIG_HOME/telora/.env`
/// → env vars) inside `BridgeTranscriber::from_id`.
///
/// Returns `(transcriber, resolved_model_id, resolved_path,
/// resolved_endpoint)` — all three are the engine's authoritative
/// values after build (closes #165, #167). For Whisper / Qwen3-ASR
/// `model_id` matches the operator's TOML input, `resolved_path`
/// is the on-disk cache path, and `endpoint` is the empty string.
/// For MiniMax `model_id` is voxora's `DEFAULT_MODEL = "asr-1.0"`
/// (or the operator's TOML override), `resolved_path` is `""`,
/// and `endpoint` is the API URL. The two callers (startup and
/// REFRESH) copy them back into `stt_config` so the status display
/// reflects what voxora actually loaded.
async fn build_transcriber(
    config: &SttConfig,
    voxora_cache: std::path::PathBuf,
    minimax_env_file: Option<&std::path::Path>,
) -> Result<(Arc<dyn Transcriber>, String, String, String)> {
    let kind = voxora_bridge::EngineFamily::from_config(&config.model_kind).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown model_kind {:?}; expected one of `whisper`, `qwen3-asr`, or `minimax`",
            config.model_kind
        )
    })?;
    let token = std::env::var("HF_TOKEN")
        .ok()
        .or_else(|| std::env::var("HUGGING_FACE_HUB_TOKEN").ok());

    let bridge = BridgeTranscriber::from_id(
        &config.model_id,
        kind,
        Some(voxora_cache),
        token,
        minimax_env_file,
    )
    .await?;
    let resolved_model_id = bridge.model_id().to_string();
    let resolved_path = bridge.resolved_path().to_string();
    let resolved_endpoint = bridge.endpoint().to_string();
    Ok((
        Arc::new(bridge),
        resolved_model_id,
        resolved_path,
        resolved_endpoint,
    ))
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

    let daemon_state = Arc::new(RwLock::new(DaemonState {
        engine: Engine::Loading,
        stt_config: stt_config.clone(),
    }));

    // Audio Engine initialization
    let rb = HeapRb::<f32>::new(16000 * 30); // 30 seconds buffer
    let (producer, mut consumer) = rb.split();

    let mut audio_engine = AudioEngine::new().context("Failed to init audio engine")?;
    audio_engine
        .start(producer, &daemon_cfg.audio)
        .context("Failed to start audio engine")?;

    // Socket
    let (cmd_tx, mut cmd_rx) = mpsc::channel(32);
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

    spawn_engine_load(
        Arc::clone(&daemon_state),
        stt_config,
        voxora_cache.clone(),
        args.minimax_env_file.clone(),
    );

    // Graceful shutdown on SIGTERM (systemd) and SIGINT (dev shell).
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
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

    let mut state = State::Idle;
    let mut audio_buffer: Vec<f32> = Vec::new();
    let mut response_tx_opt: Option<oneshot::Sender<String>> = None;
    let mut pending_result: Option<String> = None;
    // Transcription runs on a blocking thread; results carry the job id so
    // a cancelled job's late result is dropped.
    let (result_tx, mut result_rx) = mpsc::channel::<(u64, String)>(4);
    let mut job_id: u64 = 0;
    let mut audio_tick = tokio::time::interval(Duration::from_millis(20));
    audio_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                info!("Shutdown requested; exiting event loop");
                break;
            }
            Some(cmd) = cmd_rx.recv() => match cmd {
                Command::Start { response_tx } => {
                    info!("Command: START");
                    let refusal = if state == State::Processing {
                        Some("todavía se está transcribiendo la grabación anterior".to_string())
                    } else {
                        daemon_state.read().await.engine.unavailable_reason()
                    };
                    if let Some(reason) = refusal {
                        warn!("START refused: {reason}");
                        let _ = response_tx.send(Err(reason));
                    } else {
                        // Drop audio captured before START.
                        while consumer.pop().is_some() {}
                        state = State::Recording;
                        audio_buffer.clear();
                        pending_result = None;
                        let _ = response_tx.send(Ok(()));
                    }
                }
                Command::Stop { response_tx } => {
                    info!("Command: STOP");
                    match state {
                        State::Recording => {
                            drain_audio(&mut consumer, &mut audio_buffer, usize::MAX);
                            response_tx_opt = Some(response_tx);
                            job_id += 1;
                            start_transcription(
                                &daemon_state,
                                std::mem::take(&mut audio_buffer),
                                job_id,
                                result_tx.clone(),
                            )
                            .await;
                            state = State::Processing;
                        }
                        State::Processing => response_tx_opt = Some(response_tx),
                        State::Idle => {
                            let _ = response_tx.send(pending_result.take().unwrap_or_default());
                        }
                    }
                }
                Command::Cancel => {
                    info!("Command: CANCEL");
                    if state == State::Processing {
                        job_id += 1;
                        if let Some(tx) = response_tx_opt.take() {
                            let _ = tx.send("ERROR: transcripción cancelada".to_string());
                        }
                    }
                    state = State::Idle;
                    audio_buffer.clear();
                    response_tx_opt = None;
                    pending_result = None;
                }
                Command::GetStatus { response_tx } => {
                    let s = daemon_state.read().await;
                    let _ = response_tx.send(StatusResponse {
                        active: true,
                        pid: std::process::id(),
                        model_id: s.stt_config.model_id.clone(),
                        model_kind: s.stt_config.model_kind.clone(),
                        model_path: s.stt_config.model_path.clone(),
                        endpoint: s.stt_config.endpoint.clone(),
                        language: s.stt_config.language.clone(),
                        max_recording_seconds: s.stt_config.max_recording_seconds,
                        state: match state {
                            State::Idle => "Idle".to_string(),
                            State::Recording => "Recording".to_string(),
                            State::Processing => "Processing".to_string(),
                        },
                        engine: s.engine.status_label(),
                    });
                }
                Command::ReloadConfig { new_config, response_tx } => {
                    handle_reload(&daemon_state, new_config, response_tx, &voxora_cache, &args).await;
                }
            },
            Some((id, text)) = result_rx.recv() => {
                if id != job_id {
                    info!("Dropping result of cancelled transcription job {id}");
                    continue;
                }
                if let Some(tx) = response_tx_opt.take() {
                    let _ = tx.send(text);
                    pending_result = None;
                } else {
                    pending_result = Some(text);
                }
                if state == State::Processing {
                    state = State::Idle;
                }
            }
            _ = audio_tick.tick() => {
                if state != State::Recording {
                    // Keep the ring buffer from filling while idle.
                    while consumer.pop().is_some() {}
                    continue;
                }
                let max_seconds = daemon_state.read().await.stt_config.max_recording_seconds;
                let limit = 16000 * max_seconds as usize;
                drain_audio(&mut consumer, &mut audio_buffer, limit);
                if audio_buffer.len() >= limit {
                    warn!("Audio buffer limit reached ({max_seconds}s). Stopping recording automatically.");
                    job_id += 1;
                    start_transcription(
                        &daemon_state,
                        std::mem::take(&mut audio_buffer),
                        job_id,
                        result_tx.clone(),
                    )
                    .await;
                    state = State::Processing;
                    let control_sock = resolved_paths.control_sock.to_string_lossy().into_owned();
                    tokio::spawn(async move {
                        notify_client_auto_stop(&control_sock).await;
                    });
                }
            }
        }
    }

    info!("Telora daemon stopped cleanly");
    Ok(())
}

/// REFRESH: swap the engine when the model changed (or is not loaded),
/// otherwise just apply the new settings.
async fn handle_reload(
    daemon_state: &Arc<RwLock<DaemonState>>,
    new_config: SttConfig,
    response_tx: oneshot::Sender<Result<()>>,
    voxora_cache: &std::path::Path,
    args: &Args,
) {
    info!(
        "Command: REFRESH (model_kind={} model_id={})",
        new_config.model_kind, new_config.model_id
    );
    // Atomicity contract (issue #93): the engine
    // swap and the `stt_config` mutation commit
    // together under the same `RwLock` write guard.
    // Cheap path (no model change) commits the
    // config delta inline; needs-reload path
    // `tokio::spawn`s the rebuild so the main loop
    // keeps ticking through the multi-second /
    // multi-minute engine load.
    let needs_reload = {
        let s = daemon_state.read().await;
        new_config.model_id != s.stt_config.model_id
            || new_config.model_kind != s.stt_config.model_kind
            || !matches!(s.engine, Engine::Ready(_))
    };
    if !needs_reload {
        // No engine swap needed, but other fields
        // (language, max_recording_seconds) still
        // need to take effect. The new config is
        // safe to commit because no engine load
        // happened.
        let mut s = daemon_state.write().await;
        s.stt_config = new_config;
        info!("Configuration updated (no model change).");
        let _ = response_tx.send(Ok(()));
    } else {
        // Hand the rebuild off to a spawned task so
        // the event loop keeps draining commands
        // (STATUS / START / STOP) while the new
        // engine loads. The `oneshot::Sender`
        // survives the move — it is `Send + 'static`
        // — so the socket handler's `rx.await` sees
        // the result when this task eventually fires
        // `.send(Ok(()))` or drops the sender.
        let daemon_state_bg = Arc::clone(daemon_state);
        let voxora_cache_bg = voxora_cache.to_path_buf();
        // `args.minimax_env_file` is owned by
        // `Args` on the main stack; REFRESH runs on
        // a spawned task, so clone the
        // `Option<PathBuf>` and reduce to a
        // borrowed view inside the task.
        let minimax_env_file_bg = args.minimax_env_file.clone();
        tokio::spawn(async move {
            // Clone `new_config` so we can both
            // commit the metadata under the lock
            // and use the original to build the
            // new engine.
            let new_config_for_build = new_config.clone();

            // Drop the old engine before building the new
            // one so both never sit in (V)RAM at once.
            {
                let mut s = daemon_state_bg.write().await;
                s.engine = Engine::Loading;
                s.stt_config = new_config;
            }

            // Step 2: build the new engine outside
            // the lock. This is the multi-second /
            // multi-minute await we used to do on
            // the event loop — now off-loaded to a
            // worker.
            match build_transcriber(
                &new_config_for_build,
                voxora_cache_bg,
                minimax_env_file_bg.as_deref(),
            )
            .await
            {
                Ok((new_transcriber, resolved_model_id, resolved_path, resolved_endpoint)) => {
                    let mut s = daemon_state_bg.write().await;
                    s.engine = Engine::Ready(new_transcriber);
                    // Closes #165 / #167: copy the
                    // engine's authoritative
                    // model_id / resolved_path /
                    // endpoint back so the
                    // REFRESHed status display
                    // matches what voxora actually
                    // loaded.
                    s.stt_config.model_id = resolved_model_id;
                    s.stt_config.model_path = resolved_path;
                    s.stt_config.endpoint = resolved_endpoint;
                    info!("Transcriber reloaded successfully.");
                    let _ = response_tx.send(Ok(()));
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    error!("Failed to reload transcriber: {msg}");
                    daemon_state_bg.write().await.engine = Engine::Failed(msg.clone());
                    let _ = response_tx.send(Err(anyhow::anyhow!("Failed to load model: {msg}")));
                }
            }
        });
    }
}

/// Move captured samples into `buffer`, stopping at `limit` samples.
fn drain_audio(
    consumer: &mut ringbuf::Consumer<f32, Arc<HeapRb<f32>>>,
    buffer: &mut Vec<f32>,
    limit: usize,
) {
    while buffer.len() < limit {
        match consumer.pop() {
            Some(sample) => buffer.push(sample),
            None => break,
        }
    }
}

/// Transcribe `audio` on a blocking thread so the event loop keeps
/// answering STATUS and CANCEL; the result arrives on `result_tx`.
async fn start_transcription(
    daemon_state: &Arc<RwLock<DaemonState>>,
    audio: Vec<f32>,
    job_id: u64,
    result_tx: mpsc::Sender<(u64, String)>,
) {
    info!("Processing {} samples (job {job_id})...", audio.len());
    let (engine, language) = {
        let s = daemon_state.read().await;
        let engine = match &s.engine {
            Engine::Ready(t) => Ok(Arc::clone(t)),
            other => Err(other
                .unavailable_reason()
                .unwrap_or_else(|| "modelo no disponible".to_string())),
        };
        (engine, s.stt_config.language.clone())
    };
    tokio::spawn(async move {
        let text = match engine {
            Err(reason) => {
                error!("Recording discarded: {reason}");
                format!("ERROR: {reason}")
            }
            Ok(_) if audio.is_empty() => {
                warn!("Audio buffer empty, skipping transcription.");
                String::new()
            }
            Ok(t) => {
                let joined =
                    tokio::task::spawn_blocking(move || t.transcribe(&audio, Some(&language)))
                        .await;
                match joined {
                    Ok(Ok(text)) => text,
                    Ok(Err(e)) => {
                        error!("Transcription failed: {e}");
                        format!("ERROR: {e}")
                    }
                    Err(e) => {
                        error!("Transcription task panicked: {e}");
                        "ERROR: la transcripción falló inesperadamente".to_string()
                    }
                }
            }
        };
        let _ = result_tx.send((job_id, text)).await;
    });
}
