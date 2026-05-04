use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::runtime::Runtime;

use log::info;

const CONTROL_SOCKET: &str = "/tmp/telora-control.sock";

#[derive(Parser)]
#[command(author, version, about = "Telora CLI - Control client", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Toggle recording and type the result
    ToggleType,
    /// Toggle recording and copy the result to clipboard
    ToggleCopy,
    /// Cancel current recording
    Cancel,
}

async fn send_control_command(cmd: &str) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(CONTROL_SOCKET)
        .await
        .context("Failed to connect to control socket (is the GUI running?)")?;
    stream
        .write_all(cmd.as_bytes())
        .await
        .context("Failed to send control command")?;
    Ok(())
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    let cmd_str = match cli.command {
        Commands::ToggleType => "TOGGLE_TYPE",
        Commands::ToggleCopy => "TOGGLE_COPY",
        Commands::Cancel => "CANCEL",
    };

    let rt = Runtime::new().expect("Failed to create Tokio runtime");
    rt.block_on(async {
        match send_control_command(cmd_str).await {
            Ok(_) => info!("Command '{}' sent successfully.", cmd_str),
            Err(e) => log::error!("Failed to send command: {}", e),
        }
    });
}
