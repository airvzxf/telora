use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::{Config, File};
use log::{error, info, warn};
use ringbuf::HeapRb;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, sleep};

mod audio;
mod socket;
mod transcriber;
mod vad;

use audio::AudioEngine;
use socket::{Command, SocketServer, StatusResponse, SttConfig};
use transcriber::{Transcriber, WhisperTranscriber};

// Config references
const SOCKET_PATH: &str = "/tmp/telora-sock";
const CONTROL_SOCKET: &str = "/tmp/telora-control.sock";

async fn notify_client_auto_stop() {
    if let Ok(mut stream) = UnixStream::connect(CONTROL_SOCKET).await {
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

    /// Path or name of the model file (overrides config).
    /// If a name is provided (e.g., 'ggml-base.bin'), it searches in order:
    /// 1. ~/.local/share/telora/models/
    /// 2. /usr/share/telora/models/
    /// 3. ./models/
    #[arg(short, long)]
    model: Option<String>,

    /// Language (overrides config)
    #[arg(short, long)]
    language: Option<String>,

    /// Maximum recording time in seconds (overrides config)
    #[arg(long)]
    max_recording_seconds: Option<u32>,
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

fn load_config(args: &Args) -> SttConfig {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());

    // Load configuration from multiple sources in order of precedence (last one wins)
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

    // 4. Environment variables - Highest priority
    builder = builder.add_source(config::Environment::with_prefix("TELORA"));

    let config_res = builder.build();
    let mut stt_config: SttConfig = match config_res {
        Ok(c) => c.try_deserialize().unwrap_or(SttConfig {
            model_path: "ggml-base.bin".to_string(),
            language: "es".to_string(),
            max_recording_seconds: 600,
        }),
        Err(e) => {
            warn!("Configuration warning: {}. Using defaults.", e);
            SttConfig {
                model_path: "ggml-base.bin".to_string(),
                language: "es".to_string(),
                max_recording_seconds: 600,
            }
        }
    };

    // CLI args override
    if let Some(m) = &args.model {
        stt_config.model_path = m.clone();
    }
    if let Some(l) = &args.language {
        stt_config.language = l.clone();
    }
    if let Some(s) = args.max_recording_seconds {
        stt_config.max_recording_seconds = s;
    }

    // Attempt to resolve model path if it's just a filename
    if !std::path::Path::new(&stt_config.model_path).exists() {
        let filename = if stt_config.model_path.contains('/') {
            // If it contains a slash but doesn't exist, we'll still try to see if it's just the end part
            std::path::Path::new(&stt_config.model_path)
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or(&stt_config.model_path)
        } else {
            &stt_config.model_path
        };

        let candidates = vec![
            format!("{}/.local/share/telora/models/{}", home, filename),
            format!("/usr/share/telora/models/{}", filename),
            format!("models/{}", filename),
            filename.to_string(),
        ];

        for path in candidates {
            if std::path::Path::new(&path).exists() {
                stt_config.model_path = path;
                break;
            }
        }
    }

    stt_config
}

async fn run_refresh_client(config: SttConfig) -> Result<()> {
    let mut stream = match UnixStream::connect(SOCKET_PATH).await {
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

async fn run_status_client() -> Result<()> {
    let mut stream = match UnixStream::connect(SOCKET_PATH).await {
        Ok(s) => s,
        Err(_) => {
            println!("Telora Daemon Status");
            println!(
                "{:<10} {:<10} {:<30} {:<10} {:<10} {:<15}",
                "ACTIVE", "PID", "MODEL", "LANG", "MAX_SEC", "STATE"
            );
            println!(
                "{:-<10} {:-<10} {:-<30} {:-<10} {:-<10} {:-<15}",
                "", "", "", "", "", ""
            );
            println!(
                "{:<10} {:<10} {:<30} {:<10} {:<10} {:<15}",
                "NO", "-", "-", "-", "-", "STOPPED"
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
        "{:<10} {:<10} {:<30} {:<10} {:<10} {:<15}",
        "ACTIVE", "PID", "MODEL", "LANG", "MAX_SEC", "STATE"
    );
    println!(
        "{:-<10} {:-<10} {:-<30} {:-<10} {:-<10} {:-<15}",
        "", "", "", "", "", ""
    );

    let model_display = if status.model_path.len() > 28 {
        format!(
            "...{}",
            &status.model_path[status.model_path.len().saturating_sub(25)..]
        )
    } else {
        status.model_path.clone()
    };

    println!(
        "{:<10} {:<10} {:<30} {:<10} {:<10} {:<15}",
        if status.active { "YES" } else { "NO" },
        status.pid,
        model_display,
        status.language,
        status.max_recording_seconds,
        status.state
    );

    if status.active {
        println!("\nFull Model Path: {}", status.model_path);
    }

    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();

    if let Some(Commands::Status) = args.command {
        if let Err(e) = run_status_client().await {
            eprintln!("Error querying status: {}", e);
        }
        return Ok(());
    }

    if let Some(Commands::Refresh) = args.command {
        let stt_config = load_config(&args);
        if let Err(e) = run_refresh_client(stt_config).await {
            eprintln!("Error refreshing daemon: {}", e);
        }
        return Ok(());
    }

    let mut stt_config = load_config(&args);

    info!("Starting Telora Daemon...");
    info!("Using model: {}", stt_config.model_path);
    info!("Language: {}", stt_config.language);

    // 1. Initialize Components
    let mut transcriber: Box<dyn Transcriber> = Box::new(
        WhisperTranscriber::new(&stt_config.model_path).context("Failed to load Whisper model")?,
    );

    // Audio Engine initialization
    let rb = HeapRb::<f32>::new(16000 * 30); // 30 seconds buffer
    let (producer, mut consumer) = rb.split();

    let mut audio_engine = AudioEngine::new().context("Failed to init audio engine")?;
    audio_engine
        .start(producer)
        .context("Failed to start audio engine")?;

    // Socket
    let (cmd_tx, mut cmd_rx) = mpsc::channel(32);
    let socket_server = SocketServer::bind(SOCKET_PATH, cmd_tx).context("Failed to bind socket")?;

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

    info!("System Ready. Waiting for commands on {}", SOCKET_PATH);

    loop {
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
                    info!("Command: REFRESH");
                    let mut reload_transcriber = false;
                    if new_config.model_path != stt_config.model_path {
                        reload_transcriber = true;
                    }

                    stt_config = new_config;

                    if reload_transcriber {
                        info!("Model path changed, reloading transcriber...");
                        match WhisperTranscriber::new(&stt_config.model_path) {
                            Ok(new_transcriber) => {
                                transcriber = Box::new(new_transcriber);
                                info!("Transcriber reloaded successfully.");
                                let _ = response_tx.send(Ok(()));
                            }
                            Err(e) => {
                                error!("Failed to reload transcriber: {}", e);
                                let _ = response_tx
                                    .send(Err(anyhow::anyhow!("Failed to load model: {}", e)));
                            }
                        }
                    } else {
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
                    tokio::spawn(async move {
                        notify_client_auto_stop().await;
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
}
