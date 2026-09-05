use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::{Config, File};
use log::{error, info, warn};
use ringbuf::HeapRb;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use telora_common::cache::resolve_voxora_cache;
use telora_daemon::{
    AudioEngine, BridgeTranscriber, Command, DaemonConfig, SocketServer, StatusResponse, SttConfig,
    Transcriber, paths, telora_env_source,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, sleep};

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

#[derive(PartialEq)]
enum State {
    Idle,
    Recording,
    Processing,
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
    // construction is centralised in [`telora_env_source`] (the daemon's
    // `TELORA_*` environment-source helper) because `config` 0.13's
    // defaults silently drop `TELORA_PATHS__SOCKET_DIR`; see that
    // helper's rustdoc for the why. The integration test
    // `telora-daemon/tests/config_env_cascade.rs` calls the same
    // helper to pin the behaviour.
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
        Err(_) => {
            eprintln!("Error: Daemon is not running.");
            return Ok(());
        }
    };

    let config_json = serde_json::to_string(&config)?;
    let command = format!("REFRESH {}", config_json);

    if let Err(e) = stream.write_all(command.as_bytes()).await {
        eprintln!("Failed to send refresh command to daemon: {}", e);
        return Ok(());
    }

    let mut buf = Vec::new();
    if let Err(e) = stream.read_to_end(&mut buf).await {
        eprintln!("Failed to read response from daemon: {}", e);
        return Ok(());
    }

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
            "\nFull Model Id:   {}\nResolved Path:   {}\nEngine Kind:     {}",
            status.model_id, status.model_path, status.model_kind
        );
    }

    Ok(())
}

/// Async constructor for a fresh [`BridgeTranscriber`] from an
/// [`SttConfig`]. Centralised so the daemon's startup and
/// `ReloadConfig` handler both go through the same path.
async fn build_transcriber(
    config: &SttConfig,
    voxora_cache: std::path::PathBuf,
) -> Result<(Box<dyn Transcriber>, String)> {
    let kind = voxora_bridge::EngineFamily::from_config(&config.model_kind).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown model_kind {:?}; expected one of `whisper` or `qwen3-asr`",
            config.model_kind
        )
    })?;
    let token = std::env::var("HF_TOKEN")
        .ok()
        .or_else(|| std::env::var("HUGGING_FACE_HUB_TOKEN").ok());

    let bridge =
        BridgeTranscriber::from_id(&config.model_id, kind, Some(voxora_cache), token).await?;
    let resolved_path = bridge.resolved_path().to_string();
    Ok((Box::new(bridge), resolved_path))
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
                runtime_dir: c.paths.runtime_dir.clone(),
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
            runtime_dir: cfg.paths.runtime_dir.clone(),
            socket_dir: cfg.paths.socket_dir.clone(),
            daemon_socket: cfg.paths.daemon_socket.clone(),
            control_socket: cfg.paths.control_socket.clone(),
        };
        let resolved = paths::resolve(&paths_cfg).context("resolving daemon socket path")?;
        let daemon_sock = resolved.daemon_sock.to_string_lossy().into_owned();
        if let Err(e) = run_refresh_client(cfg.stt, &daemon_sock).await {
            eprintln!("Error refreshing daemon: {}", e);
        }
        return Ok(());
    }

    let daemon_cfg = match load_config(&args) {
        Ok(c) => c,
        Err(e) => {
            return Err(e.context("loading telora-daemon configuration"));
        }
    };
    let paths_config = daemon_cfg.paths.clone();
    let mut stt_config = daemon_cfg.stt;

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

    // 1. Initialize Components — voxora engine via BridgeTranscriber
    let (mut transcriber, resolved_path) = build_transcriber(&stt_config, voxora_cache.clone())
        .await
        .context("Failed to load voxora engine")?;
    stt_config.model_path = resolved_path;

    // Audio Engine initialization
    let rb = HeapRb::<f32>::new(16000 * 30); // 30 seconds buffer
    let (producer, mut consumer) = rb.split();

    let mut audio_engine = AudioEngine::new().context("Failed to init audio engine")?;
    audio_engine
        .start(producer)
        .context("Failed to start audio engine")?;

    // Socket
    let (cmd_tx, mut cmd_rx) = mpsc::channel(32);
    // Resolve the socket location through the [paths] cascade
    // introduced in EPIC #27. EPIC #28 lifted the resolver into
    // `telora_common::paths` so the daemon, GUI, and CLI all share
    // the same cascade.
    let paths_cfg = paths::PathsConfig {
        runtime_dir: paths_config.runtime_dir.clone(),
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

    // 2. Event Loop
    let mut state = State::Idle;
    let mut audio_buffer: Vec<f32> = Vec::with_capacity(16000 * 30); // Linear buffer for recording
    let chunk_size = 512;
    let mut chunk_buf: Vec<f32> = Vec::with_capacity(chunk_size);
    let mut response_tx_opt: Option<oneshot::Sender<String>> = None;
    let mut pending_result: Option<String> = None;

    info!(
        "System Ready. Waiting for commands on {}",
        resolved_paths.daemon_sock.display()
    );

    // Graceful shutdown: systemd sends SIGTERM on
    // `systemctl --user stop telora-daemon.socket`; without a handler the
    // loop dies abruptly with no log and no chance to drop the audio
    // engine or socket server cleanly. SIGINT lets Ctrl-C in a dev shell
    // do the same.
    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown_signal = Arc::clone(&shutdown);
        let mut sigterm = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
        let mut sigint = signal(SignalKind::interrupt()).context("installing SIGINT handler")?;
        tokio::spawn(async move {
            tokio::select! {
                _ = sigterm.recv() => info!("Received SIGTERM; initiating graceful shutdown"),
                _ = sigint.recv()  => info!("Received SIGINT; initiating graceful shutdown"),
            }
            shutdown_signal.store(true, Ordering::SeqCst);
        });
    }

    loop {
        // Check shutdown flag at every tick (idle tick is 5 ms) so SIGTERM
        // and SIGINT from systemd / Ctrl-C drain the loop promptly.
        if shutdown.load(Ordering::SeqCst) {
            info!("Shutdown flag set; exiting event loop");
            break;
        }

        // Non-blocking check for commands
        if let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                Command::Start => {
                    info!("Command: START");
                    state = State::Recording;
                    audio_buffer.clear();
                    pending_result = None;
                }
                Command::Stop { response_tx } => {
                    info!("Command: STOP");
                    match state {
                        State::Recording => {
                            state = State::Processing;
                            response_tx_opt = Some(response_tx);
                        }
                        State::Processing => {
                            response_tx_opt = Some(response_tx);
                        }
                        State::Idle => {
                            if let Some(res) = pending_result.take() {
                                let _ = response_tx.send(res);
                            } else {
                                let _ = response_tx.send("".to_string());
                            }
                        }
                    }
                }
                Command::Cancel => {
                    info!("Command: CANCEL");
                    state = State::Idle;
                    audio_buffer.clear();
                    response_tx_opt = None;
                    pending_result = None;
                }
                Command::GetStatus { response_tx } => {
                    let status_resp = StatusResponse {
                        active: true,
                        pid: std::process::id(),
                        model_id: stt_config.model_id.clone(),
                        model_kind: stt_config.model_kind.clone(),
                        model_path: stt_config.model_path.clone(),
                        language: stt_config.language.clone(),
                        max_recording_seconds: stt_config.max_recording_seconds,
                        state: match state {
                            State::Idle => "Idle".to_string(),
                            State::Recording => "Recording".to_string(),
                            State::Processing => "Processing".to_string(),
                        },
                    };
                    let _ = response_tx.send(status_resp);
                }
                Command::ReloadConfig {
                    new_config,
                    response_tx,
                } => {
                    info!(
                        "Command: REFRESH (model_kind={} model_id={})",
                        new_config.model_kind, new_config.model_id
                    );
                    // Atomicity: the engine swap and the config
                    // mutation MUST happen together. Build the new
                    // transcriber FIRST and only commit the new
                    // `stt_config` on success — otherwise a failed
                    // model load (wrong id, mmap error, …) would
                    // leave the daemon reporting the new model_id /
                    // model_kind in `STATUS` while transcription
                    // still runs through the old engine. Silent
                    // corruption: a closed ticket of #79's shape.
                    let needs_reload = new_config.model_id != stt_config.model_id
                        || new_config.model_kind != stt_config.model_kind;
                    if needs_reload {
                        match build_transcriber(&new_config, voxora_cache.clone()).await {
                            Ok((new_transcriber, resolved_path)) => {
                                transcriber = new_transcriber;
                                stt_config = new_config;
                                stt_config.model_path = resolved_path;
                                info!("Transcriber reloaded successfully.");
                                let _ = response_tx.send(Ok(()));
                            }
                            Err(e) => {
                                error!("Failed to reload transcriber: {}", e);
                                let _ = response_tx
                                    .send(Err(anyhow::anyhow!("Failed to load model: {}", e)));
                                // stt_config and transcriber are
                                // intentionally left untouched on
                                // failure — atomicity guarantee.
                            }
                        }
                    } else {
                        // No engine swap needed, but other fields
                        // (language, max_recording_seconds) still
                        // need to take effect. The new config is
                        // safe to commit because no engine load
                        // happened.
                        stt_config = new_config;
                        info!("Configuration updated (no model change).");
                        let _ = response_tx.send(Ok(()));
                    }
                }
            }
        }

        // Process Audio from RingBuffer
        let available = consumer.len();
        if available >= chunk_size {
            for _ in 0..chunk_size {
                if let Some(sample) = consumer.pop() {
                    chunk_buf.push(sample);
                }
            }

            // If Recording, save to buffer
            if state == State::Recording {
                // Safety limit: User-defined or default maximum time
                if audio_buffer.len() < 16000 * stt_config.max_recording_seconds as usize {
                    audio_buffer.extend_from_slice(&chunk_buf);
                } else {
                    warn!(
                        "Audio buffer limit reached ({}s). Stopping recording automatically.",
                        stt_config.max_recording_seconds
                    );
                    state = State::Processing;
                    // Notify client to stop UI and request result
                    let control_sock = resolved_paths.control_sock.to_string_lossy().into_owned();
                    tokio::spawn(async move {
                        notify_client_auto_stop(&control_sock).await;
                    });
                }
            }

            chunk_buf.clear();
        } else {
            // Sleep briefly to avoid busy loop
            sleep(Duration::from_millis(5)).await;
        }

        // Processing State
        if state == State::Processing {
            info!("Processing {} samples...", audio_buffer.len());

            let text = if audio_buffer.is_empty() {
                warn!("Audio buffer empty, skipping transcription.");
                "".to_string()
            } else {
                match transcriber.transcribe(&audio_buffer, Some(&stt_config.language)) {
                    Ok(text) => text,
                    Err(e) => {
                        error!("Transcription failed: {}", e);
                        format!("ERROR: {}", e)
                    }
                }
            };

            if let Some(tx) = response_tx_opt.take() {
                let _ = tx.send(text);
                pending_result = None;
            } else {
                pending_result = Some(text);
            }

            state = State::Idle;
            audio_buffer.clear();
        }
    }

    info!("Telora daemon stopped cleanly");
    Ok(())
}
