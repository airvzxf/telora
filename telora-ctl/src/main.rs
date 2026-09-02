use anyhow::Context;
use clap::{Parser, Subcommand};
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::runtime::Runtime;

use log::info;

/// Resolve the GUI's control socket path from the same XDG cascade
/// the GUI and daemon use (see `telora-gui/src/paths.rs` and
/// `telora-daemon/src/paths.rs`). Falls back through:
///   1. `$XDG_RUNTIME_DIR/telora/control.sock`
///   2. `/run/user/<uid>/telora/control.sock`
///   3. `/tmp/telora-<uid>/control.sock`
///
/// Mirrors the GUI's resolver to keep behavior consistent across
/// the three crates (sub-issue #34); intentionally inline because
/// `telora-ctl` has no other socket-path plumbing.
fn control_socket_path() -> std::path::PathBuf {
    let uid = current_uid();
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR")
        && !xdg.is_empty()
        && is_writable(Path::new(&xdg))
    {
        return Path::new(&xdg).join("telora").join("control.sock");
    }
    let run_user = format!("/run/user/{uid}");
    if is_writable(Path::new(&run_user)) {
        return Path::new(&run_user).join("telora").join("control.sock");
    }
    Path::new(&format!("/tmp/telora-{uid}")).join("control.sock")
}

/// Read the real UID of the current process from
/// `/proc/self/status`. Matches the helper in `telora-gui`'s
/// `paths` module; we re-implement it here to avoid pulling
/// `nix` solely for `getuid` in this minimal CLI.
fn current_uid() -> u32 {
    let Ok(contents) = std::fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("Uid:")
            && let Some(first) = rest.split_whitespace().next()
        {
            return first.parse().unwrap_or(0);
        }
    }
    0
}

fn is_writable(p: &Path) -> bool {
    std::fs::metadata(p)
        .map(|m| !m.permissions().readonly())
        .unwrap_or(false)
}

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
    let mut stream = UnixStream::connect(control_socket_path())
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
