use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use telora_common::protocol;
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

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cmd = match Cli::parse().command {
        Commands::ToggleCopy => "toggle_copy",
        Commands::Cancel => "cancel",
        Commands::Last => "last",
    };

    let paths = telora_common::config::resolve_paths()?;
    let rt = Runtime::new().context("Failed to create Tokio runtime")?;
    let reply = rt
        .block_on(async {
            tokio::time::timeout(
                Duration::from_secs(3),
                protocol::call(&paths.control_sock, cmd),
            )
            .await
        })
        .context("telora-gui no respondió")?
        .context("no se pudo hablar con telora-gui (¿está corriendo?)")?;
    if let Err(e) = reply {
        bail!("{e}");
    }
    log::info!("Command '{cmd}' sent.");
    Ok(())
}
