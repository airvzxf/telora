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
//! exposes the same view as `voxora list`.
//!
//! # Migration
//!
//! New flows should call `voxora` directly. `telora-models` is
//! kept here so old documentation and packaging recipes keep
//! working. Going forward the plan is to retire it; see
//! `TODO.md`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use telora_common::cache::resolve_voxora_cache;
use voxora_traits::{ModelSource, ResolveOptions};

#[derive(Parser)]
#[command(
    author,
    version,
    about = "Telora Model Manager - delegates to voxora-hf",
    long_about = "telora-models is a thin wrapper around `voxora-hf`. \
                  Same UX as before, but all downloads go through \
                  voxora's cache at $XDG_CACHE_HOME/voxora/models/huggingface. \
                  New flows should call `voxora` directly."
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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();

    // Single source of truth for the cache root — `list` and
    // `download` scan the same directory `download` writes to. Both
    // CLI and environment overrides go through the shared sanitizer.
    let cache_root = resolve_cache_root(cli.voxora_cache.as_deref())?;
    let source = build_source(Some(&cache_root))?;

    match cli.command {
        Commands::List => list_cmd(&cache_root),
        Commands::Download { model_id } => download_cmd(&source, &model_id).await,
        Commands::Path => path_cmd(&cache_root),
    }
}

fn resolve_cache_root(cli_override: Option<&Path>) -> Result<PathBuf> {
    let env_override = std::env::var_os("VOXORA_CACHE_DIR").map(PathBuf::from);
    resolve_voxora_cache(cli_override, env_override.as_deref())
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

/// Char-boundary-aware truncation. The previous implementation
/// byte-sliced the string and could panic on multi-byte UTF-8 paths
/// (the operator's home directory was the easy reproducer). Budget
/// here is in Unicode scalar values, not bytes — the right unit
/// for "we want this to fit in a column".
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut = s
        .char_indices()
        .nth_back(max.saturating_sub(3))
        .map(|(i, _)| i)
        .unwrap_or(0);
    format!("...{}", &s[cut..])
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

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvRestore {
        xdg_cache_home: Option<OsString>,
        home: Option<OsString>,
        voxora_cache_dir: Option<OsString>,
    }

    impl EnvRestore {
        fn setup(root: &Path) -> Self {
            let previous = Self {
                xdg_cache_home: std::env::var_os("XDG_CACHE_HOME"),
                home: std::env::var_os("HOME"),
                voxora_cache_dir: std::env::var_os("VOXORA_CACHE_DIR"),
            };
            unsafe {
                std::env::set_var("XDG_CACHE_HOME", root);
                std::env::set_var("HOME", root);
                std::env::remove_var("VOXORA_CACHE_DIR");
            }
            previous
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            restore("XDG_CACHE_HOME", self.xdg_cache_home.take());
            restore("HOME", self.home.take());
            restore("VOXORA_CACHE_DIR", self.voxora_cache_dir.take());
        }
    }

    fn restore(name: &str, value: Option<OsString>) {
        unsafe {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }

    #[test]
    fn cache_override_traversal_is_rejected() {
        let _lock = ENV_LOCK.lock().unwrap();
        let root = tempfile::tempdir().expect("create test cache root");
        let _env = EnvRestore::setup(root.path());
        let bad = PathBuf::from("/tmp/foo/../bar");
        unsafe {
            std::env::set_var("VOXORA_CACHE_DIR", &bad);
        }

        let resolved = resolve_cache_root(None).expect("default cache should resolve");
        assert!(resolved.ends_with(Path::new("voxora/models/huggingface")));
        assert_ne!(resolved, PathBuf::from("/tmp/bar"));
    }

    #[test]
    fn cli_cache_override_is_sanitized_too() {
        let _lock = ENV_LOCK.lock().unwrap();
        let root = tempfile::tempdir().expect("create test cache root");
        let _env = EnvRestore::setup(root.path());
        let bad = Path::new("/tmp/foo/../bar");

        let resolved = resolve_cache_root(Some(bad)).expect("default cache should resolve");
        assert!(resolved.ends_with(Path::new("voxora/models/huggingface")));
        assert_ne!(resolved, PathBuf::from("/tmp/bar"));
    }
}
