//! Transcriber trait and the [`BridgeTranscriber`] implementation.
//!
//! The [`Transcriber`] trait is the internal contract that
//! `telora-daemon/src/main.rs` consumes; it has not changed shape
//! since the original Whisper-only implementation. What changed is
//! the concrete [`BridgeTranscriber`], which holds a voxora engine
//! behind `Arc<dyn voxora_bridge::AsrEngine>` instead of a
//! `WhisperContext` directly.
//!
//! # Model kinds
//!
//! The bridge accepts a `model_kind` (one of
//! [`voxora_bridge::ModelKind`]) at construction time and uses it
//! both to pick the engine adapter (`WhisperEngine` vs
//! `QwenAsrEngine`) and to translate the user-facing ISO 639-1
//! language code into the engine-specific vocabulary. Whisper speaks
//! ISO 639-1 directly; Qwen3-ASR wants full English names
//! ("english", "chinese", …) and the bridge keeps a closed 20-entry
//! table.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use log::info;
use voxora_bridge::{
    AsrEngine, AsrError, HuggingFaceSource, ModelKind, ModelSource, ResolveOptions,
    TranscribeOptions, WhisperEngine,
};

/// Internal transcription contract used by the daemon's main loop.
pub trait Transcriber: Send {
    fn transcribe(&mut self, audio_data: &[f32], language: Option<&str>) -> Result<String>;
}

/// voxora-backed transcriber.
///
/// Holds `Arc<dyn AsrEngine>` so the same instance can be shared
/// across reloads without rebuilding the underlying context every
/// time. The trait requires `&mut self` for symmetry with the
/// legacy `WhisperTranscriber`, but the implementation is `&self`
/// internally — voxora engines are `Send + Sync`.
pub struct BridgeTranscriber {
    engine: Arc<dyn AsrEngine>,
    model_id: String,
    model_kind: ModelKind,
    /// Resolved local path of the model on disk (filled in after
    /// `from_id` succeeds). Surfaced through the status response so
    /// the GUI can show the user where the model actually lives.
    resolved_path: String,
}

impl BridgeTranscriber {
    /// Construct from a Hugging Face model id and a `model_kind`.
    ///
    /// This calls into the chosen voxora engine adapter's `from_hf`
    /// constructor, which downloads (if necessary), caches, and loads
    /// the model.
    pub async fn from_id(
        model_id: &str,
        model_kind: ModelKind,
        cache_dir: Option<std::path::PathBuf>,
        hf_token: Option<String>,
    ) -> Result<Self> {
        let mut builder = HuggingFaceSource::builder();
        if let Some(dir) = cache_dir {
            builder = builder.cache_dir(dir);
        }
        if let Some(token) = hf_token {
            builder = builder.token(Some(token));
        }
        let source = builder
            .build()
            .context("failed to build HuggingFaceSource")?;

        let resolve_opts = ResolveOptions::default();

        let (engine, resolved_path) = match model_kind {
            ModelKind::Whisper => {
                let engine = WhisperEngine::from_hf(&source, model_id, &resolve_opts)
                    .await
                    .with_context(|| format!("failed to load Whisper engine for {model_id:?}"))?;
                // WhisperEngine::from_hf resolves to a directory; the
                // .bin file inside it is what `WhisperEngine::load`
                // would have used directly. We surface the .bin path
                // so the status response stays close to the old
                // model_path field.
                let bin = find_whisper_bin(&source, model_id).await?;
                (Arc::new(engine) as Arc<dyn AsrEngine>, bin)
            }
            ModelKind::Qwen3Asr => {
                let engine =
                    voxora_bridge::QwenAsrEngine::from_hf(&source, model_id, &resolve_opts)
                        .await
                        .with_context(|| {
                            format!("failed to load Qwen3-ASR engine for {model_id:?}")
                        })?;
                let dir = source
                    .resolve(model_id, &resolve_opts)
                    .await
                    .map_err(asr_to_anyhow)?;
                (
                    Arc::new(engine) as Arc<dyn AsrEngine>,
                    dir.path.display().to_string(),
                )
            }
        };

        info!(
            "loaded {} model from {model_id:?} (resolved to {resolved_path})",
            model_kind
        );

        Ok(Self {
            engine,
            model_id: model_id.to_string(),
            model_kind,
            resolved_path,
        })
    }

    /// Resolved local path of the loaded model. Used by the status
    /// response so the GUI can show the on-disk location.
    pub fn resolved_path(&self) -> &str {
        &self.resolved_path
    }

    /// The HF model id the engine was loaded from.
    #[allow(dead_code)]
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// Which engine adapter this transcriber wraps.
    #[allow(dead_code)]
    pub fn model_kind(&self) -> ModelKind {
        self.model_kind
    }

    /// Translate the user-facing ISO 639-1 code into the engine-
    /// specific spelling. Returns `None` if the code is not
    /// recognised; callers should treat that as a user error.
    fn map_language(&self, iso: &str) -> Option<String> {
        match self.model_kind {
            ModelKind::Whisper => Some(iso.to_ascii_lowercase()),
            ModelKind::Qwen3Asr => iso_to_qwen_name(iso),
        }
    }
}

impl Transcriber for BridgeTranscriber {
    fn transcribe(&mut self, audio_data: &[f32], language: Option<&str>) -> Result<String> {
        let lang = match language {
            Some(s) => self.map_language(s).ok_or_else(|| {
                anyhow!(
                    "language code {s:?} is not supported by {self_model_kind}; \
                     see `voxora_bridge::known_languages` for the accepted set",
                    self_model_kind = self.model_kind
                )
            })?,
            None => match self.model_kind {
                ModelKind::Whisper => "auto".to_string(),
                ModelKind::Qwen3Asr => "auto".to_string(),
            },
        };

        let opts = TranscribeOptions::new(Some(lang.clone()), false, true);
        let result = self
            .engine
            .transcribe(audio_data, &opts)
            .map_err(asr_to_anyhow)?;

        info!(
            "transcribed {} samples with {}, language={lang:?}, len={}",
            audio_data.len(),
            self.model_kind,
            result.text.len()
        );
        Ok(result.text.trim().to_string())
    }
}

/// Find the .bin file inside the Whisper model directory returned
/// by voxora-hf. WhisperEngine::from_hf resolves to a directory that
/// contains exactly one `ggml-*.bin` file (or one selected by the
/// HF id).
async fn find_whisper_bin(source: &HuggingFaceSource, model_id: &str) -> Result<String> {
    let dir = source
        .resolve(model_id, &ResolveOptions::default())
        .await
        .map_err(asr_to_anyhow)?;
    let mut bins: Vec<_> = std::fs::read_dir(&dir.path)
        .with_context(|| format!("listing {}", dir.path.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("bin"))
        })
        .collect();
    bins.sort();
    let bin = bins.into_iter().next().ok_or_else(|| {
        anyhow!(
            "no ggml-*.bin file found in resolved Whisper model directory {}",
            dir.path.display()
        )
    })?;
    Ok(bin.display().to_string())
}

/// Convert an [`AsrError`] into an `anyhow::Error` so the existing
/// `main.rs` error chain stays consistent.
fn asr_to_anyhow(e: AsrError) -> anyhow::Error {
    anyhow!("voxora: {e}")
}

/// Map an ISO 639-1 code (e.g. "en") to a Qwen3-ASR full English
/// name (e.g. "english"). Returns `None` if the code is not in
/// the closed 20-entry list.
fn iso_to_qwen_name(iso: &str) -> Option<String> {
    let iso = iso.to_ascii_lowercase();
    let name = match iso.as_str() {
        "en" => "english",
        "zh" | "zh-cn" | "zh-hans" => "chinese",
        "yue" | "zh-yue" => "cantonese",
        "ar" => "arabic",
        "de" => "german",
        "fr" => "french",
        "es" => "spanish",
        "pt" => "portuguese",
        "id" => "indonesian",
        "it" => "italian",
        "ko" => "korean",
        "ru" => "russian",
        "th" => "thai",
        "vi" => "vietnamese",
        "ja" => "japanese",
        "hi" => "hindi",
        "bn" => "bengali",
        "ms" => "malay",
        "tr" => "turkish",
        "nl" => "dutch",
        "sv" => "swedish",
        _ => return None,
    };
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_to_qwen_known_codes_round_trip() {
        for iso in [
            "en", "zh", "yue", "ar", "de", "fr", "es", "pt", "id", "it", "ko", "ru", "th", "vi",
            "ja", "hi",
        ] {
            assert!(
                iso_to_qwen_name(iso).is_some(),
                "iso {iso:?} should map to a Qwen language name"
            );
        }
    }

    #[test]
    fn iso_to_qwen_rejects_unknown() {
        assert!(iso_to_qwen_name("xx").is_none());
        assert!(iso_to_qwen_name("").is_none());
    }

    #[test]
    fn iso_to_qwen_is_case_insensitive() {
        assert_eq!(iso_to_qwen_name("EN").unwrap(), "english");
        assert_eq!(iso_to_qwen_name("ZH").unwrap(), "chinese");
    }
}
