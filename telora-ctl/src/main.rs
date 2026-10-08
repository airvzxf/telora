use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use telora_common::paths::control_socket_path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::runtime::Runtime;

#[derive(Parser)]
#[command(author, version, about = "Telora CLI - Control client", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start recording, or stop it and copy the transcription to the clipboard
    #[command(alias = "toggle")]
    ToggleCopy,
    /// Cancel the current recording
    Cancel,
    /// Copy the last transcription to the clipboard again
    Last,
}

async fn send_control_command(cmd: &str) -> anyhow::Result<String> {
    let mut stream = UnixStream::connect(control_socket_path())
        .await
        .context("Failed to connect to control socket (is telora-gui running?)")?;
    stream
        .write_all(cmd.as_bytes())
        .await
        .context("Failed to send control command")?;
    stream.shutdown().await.ok();
    let mut reply = String::new();
    // Older GUIs close without replying; treat a silent close as success.
    let _ = tokio::time::timeout(Duration::from_secs(3), stream.read_to_string(&mut reply)).await;
    Ok(reply)
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cmd = match Cli::parse().command {
        Commands::ToggleCopy => "TOGGLE_COPY",
        Commands::Cancel => "CANCEL",
        Commands::Last => "LAST",
    };

    let rt = Runtime::new().context("Failed to create Tokio runtime")?;
    let reply = rt.block_on(send_control_command(cmd))?;
    if let Some(reason) = reply.strip_prefix("ERROR:") {
        bail!("{}", reason.trim());
    }
    log::info!("Command '{cmd}' sent.");
    Ok(())
}
