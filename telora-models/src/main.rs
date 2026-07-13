//! `telora-models` — thin wrapper around `voxora-hf`.
//!
//! The legacy `telora-models list / download / path` CLI surface is
//! preserved for backwards compatibility, but the implementation
//! now delegates every Hugging Face operation to [`voxora_hf`].
//! The previous hardcoded `MODELS` table and direct `reqwest`
//! downloader are gone.
//!
//! All models live in voxora's cache:
//! `$XDG_CACHE_HOME/voxora/models/huggingface`. `telora-models`
//! exposes the same view as `voxora-cli list`.
//!
//! # Migration
//!
//! New flows should call `voxora-cli` directly. `telora-models` is
//! kept here so old documentation and packaging recipes keep
//! working. Going forward the plan is to retire it; see
//! `TODO.md`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use voxora_core::{ModelSource, ResolveOptions};

#[derive(Parser)]
#[command(
    author,
    version,
    about = "Telora Model Manager - delegates to voxora-hf",
    long_about = "telora-models is a thin wrapper around `voxora-hf`. \
                  Same UX as before, but all downloads go through \
                  voxora's cache at $XDG_CACHE_HOME/voxora/models/huggingface. \
                  New flows should call `voxora-cli` directly."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Override the voxora cache directory (otherwise
    /// `$XDG_CACHE_HOME/voxora/models/huggingface`).
    #[arg(long, global = true, value_name = "DIR")]
    voxora_cache: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
    /// List models currently cached by voxora.
    List,
    /// Download (or re-resolve) a Hugging Face model id through
    /// voxora-hf. Accepts any HF id (e.g.
    /// `Qwen/Qwen3-ASR-0.6B` or
    /// `ggerganov/whisper.cpp/ggml-base.bin`).
    Download {
        /// Hugging Face model identifier.
        #[arg(value_name = "HF_MODEL_ID")]
        model_id: String,
    },
    /// Show the voxora cache directory used by telora.
    Path,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    let cache_root = cli.voxora_cache.clone().unwrap_or_else(default_cache_dir);
    let source = build_source(cli.voxora_cache.as_deref())?;

    match cli.command {
        Commands::List => list_cmd(&cache_root),
        Commands::Download { model_id } => download_cmd(&source, &model_id).await,
        Commands::Path => path_cmd(&cache_root),
    }
}

fn build_source(cache: Option<&Path>) -> Result<voxora_hf::HuggingFaceSource> {
    let mut builder = voxora_hf::HuggingFaceSource::builder();
    if let Some(dir) = cache {
        builder = builder.cache_dir(dir.to_path_buf());
    }
    // Token is auto-resolved by voxora-hf from HF_TOKEN /
    // HUGGING_FACE_HUB_TOKEN when builder.token() is not called.
    builder.build().context("failed to build HuggingFaceSource")
}

fn list_cmd(cache_root: &Path) -> Result<()> {
    let entries = voxora_hf::cache::list_cached(cache_root).map_err(|e| anyhow::anyhow!("{e}"))?;

    if entries.is_empty() {
        println!("No models cached yet.");
        println!("Run `telora-models download <hf-model-id>` to fetch one.");
        println!("Run `voxora list` for the same view (recommended).");
        return Ok(());
    }

    println!("Models cached by voxora (source-of-truth cache):");
    println!("{:<70} {:<10} COMPLETE", "PATH", "BYTES");
    println!("{:-<70} {:-<10} {:-<8}", "", "", "");
    for entry in entries {
        println!(
            "{:<70} {:<10} {}",
            truncate(&entry.path.display().to_string(), 70),
            human_bytes(entry.bytes_total),
            if entry.complete_marker_present {
                "yes"
            } else {
                "no"
            }
        );
    }

    println!("\nNote: telora-models now uses voxora's cache. Run `voxora list` for the same view.");
    Ok(())
}

async fn download_cmd(source: &voxora_hf::HuggingFaceSource, model_id: &str) -> Result<()> {
    let opts = ResolveOptions::default();
    let dir = source
        .resolve(model_id, &opts)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("failed to resolve {model_id:?}"))?;

    println!("Model {model_id:?} is ready at {}.", dir.path.display());
    println!(
        "Quantization: {:?}\nSource:       {}\n\nNext: set `model_id = {model_id:?}` in telora.toml and start the daemon.",
        dir.quantization,
        dir.kind.tag()
    );

    Ok(())
}

fn path_cmd(cache_root: &Path) -> Result<()> {
    println!("Telora (voxora) model cache: {}", cache_root.display());
    Ok(())
}

/// Mirror of `voxora_hf::cache::default_cache_root`. Kept private
/// there to keep the voxora-hf API surface minimal; duplicated here
/// because we only need it for the `path` subcommand.
fn default_cache_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("VOXORA_CACHE_DIR") {
        return PathBuf::from(custom);
    }
    let base = dirs::cache_dir().unwrap_or_else(|| PathBuf::from(".cache"));
    base.join("voxora").join("models").join("huggingface")
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("...{}", &s[s.len().saturating_sub(max - 3)..])
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", n, UNITS[0])
    } else {
        format!("{:.1} {}", v, UNITS[i])
    }
}
