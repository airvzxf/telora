//! Transcriber trait and the [`BridgeTranscriber`] implementation.
//!
//! The [`Transcriber`] trait is the internal contract that
//! `telora-daemon/src/main.rs` consumes; it has not changed shape
//! since the original Whisper-only implementation. What changed is
//! the concrete [`BridgeTranscriber`], which holds a voxora engine
//! behind `Arc<dyn voxora_bridge::AsrEngine>` instead of a
//! `WhisperContext` directly.
//!
//! # Resolution path (airvzxf/telora#79)
//!
//! `from_id` goes through `voxora-registry` (`ModelId::parse` +
//! `Registry::resolve`) so the on-disk file we load is exactly the
//! one the user asked for — the registry's [`ResolvedModel`] carries
//! [`voxora_bridge::ModelDir::entry`], which names the specific file
//! for 3-segment HF ids (`org/repo/file`). That replaces the
//! 0.1.x-era lex-sort of `*.bin` files inside the cache directory
//! (the original #79 bug). The registry is built with an explicit
//! `HuggingFaceSource` (NOT `hf_registry()`) so the operator's
//! `$XDG_CACHE_HOME/voxora/models/huggingface` cache survives the
//! 0.2 bump.
//!
//! # Engine families
//!
//! [`EngineFamily`] is the canonical spelling used in config files
//! and CLI flags (re-exported through voxora-bridge from voxora-
//! engine; the older `voxora-bridge::ModelKind` was deprecated in
//! voxora 0.2.0 and removed in 0.3.0). Whisper speaks ISO 639-1
//! directly; Qwen3-ASR wants full English names ("english",
//! "chinese", …) and the bridge keeps a closed 20-entry table.
//!
//! # Symlink refusal (security)
//!
//! voxora-hf and voxora-whisper both follow symlinks when probing a
//! resolved path (`is_file()` and `std::fs::metadata` are
//! symlink-following). If the operator's cache directory is shared
//! with another local user — or an attacker can plant a single
//! symlink inside the cache root — whisper.cpp's mmap call would
//! happily map the symlink target instead of the requested model.
//! We refuse to load any model path whose final component (or the
//! directory itself, for Qwen) is a symlink. See
//! [`refuse_if_symlink`].

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use log::info;
use voxora_bridge::{
    AsrEngine, EngineFamily, HuggingFaceSource, ModelSource, ResolveOptions, TranscribeOptions,
    WhisperEngine,
};
use voxora_registry::{ModelId, Registry, RegistryHfExt};

/// Internal transcription contract used by the daemon's main loop.
///
/// `transcribe` takes `&self` (not `&mut self`) because the
/// underlying voxora engine is held behind `Arc<dyn AsrEngine>`
/// and is itself `Send + Sync`. Sharing a read lock across the
/// call lets the daemon's event loop keep STATUS / START / STOP
/// responsive while a REFRESH in `tokio::spawn` commits a new
/// engine under the write lock — see issue #93.
pub trait Transcriber: Send + Sync {
    fn transcribe(&self, audio_data: &[f32], language: Option<&str>) -> Result<String>;
}

/// No-op transcriber used as a sentinel during REFRESH. While the
/// daemon drops the old engine and waits for the new one, the
/// `Processing` branch of the event loop can still fire (e.g. a
/// STOP that arrived in the swap window); this stub returns an
/// empty string so the daemon stays processable instead of
/// panicking on a `None` engine.
///
/// Also used as the install-target when a REFRESH starts: the
/// main loop takes a write lock, replaces the live engine with
/// `NoopTranscriber`, drops the lock, then awaits `build_transcriber`
/// in the spawned task before committing the real engine. That
/// keeps the swap window bounded by `max(old, new) + build_scratch`
/// instead of `old + new + build_scratch` (#94).
#[derive(Debug, Default)]
pub struct NoopTranscriber;

impl Transcriber for NoopTranscriber {
    fn transcribe(&self, _audio_data: &[f32], _language: Option<&str>) -> Result<String> {
        Ok(String::new())
    }
}

/// voxora-backed transcriber.
///
/// Holds `Arc<dyn AsrEngine>` so the same instance can be shared
/// across reloads without rebuilding the underlying context every
/// time. The trait method is `&self` because the engine itself is
/// `Send + Sync`; that is what allows the daemon to take only a
/// read lock for `transcribe` and reserve the write lock for
/// engine swaps (issue #93).
pub struct BridgeTranscriber {
    engine: Arc<dyn AsrEngine>,
    model_id: String,
    model_kind: EngineFamily,
    /// Resolved local path of the model on disk (filled in after
    /// `from_id` succeeds). Surfaced through the status response so
    /// the GUI can show the user where the model actually lives.
    resolved_path: String,
}

impl BridgeTranscriber {
    /// Construct from a Hugging Face model id and a [`EngineFamily`].
    ///
    /// Goes through `voxora-registry` (`ModelId::parse` +
    /// `Registry::resolve`) so the loaded file is exactly the one
    /// the user asked for: the resulting `ResolvedModel.model_dir`
    /// has its `entry` field populated for 3-segment ids, and the
    /// [`WhisperEngine::load`] call below uses that explicit path
    /// instead of a lex-sort of `*.bin` files (which is what #79 was
    /// about).
    ///
    /// `cache_dir` must be pinned explicitly so the operator's
    /// existing `~/.cache/voxora/models/huggingface` cache survives
    /// the bump; voxora-hf 0.4 would otherwise default to a
    /// voxora-config-derived root that drops the `models/huggingface`
    /// suffix and orphans every cached model.
    pub async fn from_id(
        model_id: &str,
        model_kind: EngineFamily,
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
        let hf_source: Arc<HuggingFaceSource> = Arc::new(
            builder
                .build()
                .context("failed to build HuggingFaceSource")?,
        );

        let opts = ResolveOptions::default();

        // Build the registry around the source we already configured.
        // `hf_registry()` would construct its own `HuggingFaceSource`
        // internally and bypass our `cache_dir` override — that is
        // exactly what we must avoid to keep the operator's existing
        // cache alive.
        let dyn_source: Arc<dyn ModelSource> = hf_source.clone();
        let registry = Registry::new(dyn_source).with_builtin_descriptors();

        let parsed = ModelId::parse(model_id)
            .map_err(|e| anyhow!("voxora: invalid model id {model_id:?}: {e}"))?;
        let resolved = registry
            .resolve(&parsed, &opts)
            .await
            .map_err(|e| anyhow!("voxora: {e}"))?;

        // Cross-check: the family the registry derived from the id
        // must match the family the user configured. Without this a
        // user who wrote `model_kind = "whisper"` but
        // `model_id = "Qwen/Qwen3-ASR-0.6B"` would silently route to
        // the wrong engine instead of failing loudly.
        if resolved.descriptor.family != model_kind {
            return Err(anyhow!(
                "model_kind {model_kind} does not match model_id {model_id:?} \
                 (registry resolved to {family}); fix your telora.toml",
                family = resolved.descriptor.family
            ));
        }

        let dir = resolved.model_dir;

        let (engine, resolved_path) = match model_kind {
            EngineFamily::Whisper => {
                // 3-segment HF ids (e.g.
                // `ggerganov/whisper.cpp/ggml-large-v3.bin`) always
                // come back with `dir.entry` populated — that is the
                // structural fix for #79. A 2-segment `org/repo`
                // request would leave `entry` as `None`; for whisper
                // that is a misconfiguration (the resolved directory
                // is a snapshot of `ggml-*.bin` files, not a single
                // model), so we surface that as a clear error rather
                // than fall back to the old lex-sort.
                let bin = dir.entry.clone().ok_or_else(|| {
                    anyhow!(
                        "whisper model_id {model_id:?} resolved to a directory but no \
                         specific .bin file; use the 3-segment form \
                         ggerganov/whisper.cpp/ggml-<variant>.bin"
                    )
                })?;
                refuse_if_symlink(&bin)?;
                let engine = WhisperEngine::load(&bin).with_context(|| {
                    format!("failed to load Whisper model from {}", bin.display())
                })?;
                (
                    Arc::new(engine) as Arc<dyn AsrEngine>,
                    bin.display().to_string(),
                )
            }
            EngineFamily::Qwen3Asr => {
                // `QwenAsrEngine::from_hf` owns the `tokenizer.json`
                // synthesis that no other path exposes, so we keep
                // using it for the engine load. The registry-
                // resolved dir is the source of truth for the
                // surfaced path (it is what the status response
                // reports to the GUI).
                refuse_if_symlink(&dir.path)?;
                let engine =
                    voxora_bridge::QwenAsrEngine::from_hf(hf_source.as_ref(), model_id, &opts)
                        .await
                        .with_context(|| {
                            format!("failed to load Qwen3-ASR engine for {model_id:?}")
                        })?;
                (
                    Arc::new(engine) as Arc<dyn AsrEngine>,
                    dir.path.display().to_string(),
                )
            }
            // `EngineFamily` is `#[non_exhaustive]` so future engine
            // families (parakeet, voxtral, …) land as a new variant
            // without breaking this match. The registry cross-check
            // above guarantees we only see families we have a real
            // loader for; anything else is a config-mismatch bug we
            // want to hear about loudly.
            other => {
                return Err(anyhow!(
                    "model_kind {other:?} has no voxora engine adapter wired up in telora; \
                     current set: Whisper, Qwen3Asr"
                ));
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
    pub fn model_kind(&self) -> EngineFamily {
        self.model_kind
    }

    /// Translate the user-facing ISO 639-1 code into the engine-
    /// specific spelling. Returns `None` if the code is not
    /// recognised; callers should treat that as a user error.
    fn map_language(&self, iso: &str) -> Option<String> {
        match self.model_kind {
            EngineFamily::Whisper => Some(iso.to_ascii_lowercase()),
            EngineFamily::Qwen3Asr => iso_to_qwen_name(iso),
            // `EngineFamily` is `#[non_exhaustive]`. We promise only
            // the two variants above are wired up; anything else
            // lands here as a user-visible error.
            _ => None,
        }
    }
}

impl Transcriber for BridgeTranscriber {
    fn transcribe(&self, audio_data: &[f32], language: Option<&str>) -> Result<String> {
        let lang = match language {
            Some(s) => self.map_language(s).ok_or_else(|| {
                anyhow!(
                    "language code {s:?} is not supported by {self_model_kind}; \
                     see `voxora_bridge::known_languages` for the accepted set",
                    self_model_kind = self.model_kind
                )
            })?,
            None => match self.model_kind {
                EngineFamily::Whisper => "auto".to_string(),
                EngineFamily::Qwen3Asr => "auto".to_string(),
                // `EngineFamily` is `#[non_exhaustive]`; anything
                // else is a misconfigured engine and is unreachable
                // because `from_id` already rejected it above.
                _ => "auto".to_string(),
            },
        };

        let opts = TranscribeOptions::new(Some(lang.clone()), false, true);
        let result = self
            .engine
            .transcribe(audio_data, &opts)
            .map_err(|e| anyhow!("voxora: {e}"))?;

        info!(
            "transcribed {} samples with {}, language={lang:?}, len={}",
            audio_data.len(),
            self.model_kind,
            result.text.len()
        );
        Ok(result.text.trim().to_string())
    }
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
        "ms" => "malay",
        "tr" => "turkish",
        "nl" => "dutch",
        "sv" => "swedish",
        _ => return None,
    };
    Some(name.to_string())
}

/// Refuse to load a model from a path that is (or whose final
/// component is) a symbolic link.
///
/// voxora-hf's `is_file()` probe and voxora-whisper's
/// `std::fs::metadata` both follow symlinks — so a planted symlink
/// at the resolved path would otherwise be handed to whisper.cpp's
/// mmap and the daemon would happily map whatever file the symlink
/// points to. The voxora cache directory is the operator's machine
/// root and is not a hardened location, so we treat any symlink
/// along the model's resolved path as a hostile tamper.
///
/// `path` may not exist yet (the resolved path can point at a file
/// we are about to download). In that case we walk the existing
/// ancestors and refuse if any of them is a symlink — same threat
/// model, just one level up.
fn refuse_if_symlink(p: &Path) -> Result<()> {
    let md = match std::fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Path does not exist (cache miss; voxora-hf will
            // download). Walk the existing ancestors and refuse if
            // any of them is itself a symlink — the download would
            // land inside a directory the attacker controls.
            let mut cur = p.parent();
            while let Some(ancestor) = cur {
                if ancestor.as_os_str().is_empty() {
                    break;
                }
                if let Ok(am) = std::fs::symlink_metadata(ancestor)
                    && am.file_type().is_symlink()
                {
                    return Err(anyhow!(
                        "refusing to load model: parent {ancestor:?} of {p:?} is a symlink; \
                         the voxora cache must contain a regular directory"
                    ));
                }
                cur = ancestor.parent();
            }
            return Ok(());
        }
        Err(e) => {
            return Err(anyhow!(
                "refusing to load model from {p:?}: cannot stat ({e}); \
                 the voxora cache must be readable"
            ));
        }
    };
    if md.file_type().is_symlink() {
        return Err(anyhow!(
            "refusing to load model from symlink {p:?}; \
             the voxora cache must contain a regular file"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_to_qwen_known_codes_round_trip() {
        for iso in [
            "en", "zh", "zh-cn", "zh-hans", "yue", "zh-yue", "ar", "de", "fr", "es", "pt", "id",
            "it", "ko", "ru", "th", "vi", "ja", "hi", "ms", "tr", "nl", "sv",
        ] {
            assert!(
                iso_to_qwen_name(iso).is_some(),
                "iso {iso:?} should map to a Qwen language name"
            );
        }
        // `bn` is intentionally NOT mapped — voxora-qwen3asr's closed
        // 20-entry list does not include `bengali`. A user who writes
        // `language = "bn"` in `telora.toml` now hits the daemon's
        // own "not supported" error path (which already names
        // `voxora_bridge::known_languages` as the canonical list)
        // instead of a misleading "looks OK" pass-through that
        // voxora then rejects.
        assert!(
            iso_to_qwen_name("bn").is_none(),
            "bn must not be mapped: voxora-qwen3asr does not accept 'bengali'"
        );
    }

    #[test]
    fn iso_to_qwen_rejects_unknown() {
        assert!(iso_to_qwen_name("xx").is_none());
        assert!(iso_to_qwen_name("").is_none());
        // `bn` is the canonical "looks-plausible-but-unmapped" code;
        // pin it explicitly so a future re-addition of `bengali` to
        // voxora-qwen3asr is a deliberate code change, not a silent
        // regression.
        assert!(
            iso_to_qwen_name("bn").is_none(),
            "bn must not be mapped to a Qwen language name"
        );
    }

    #[test]
    fn iso_to_qwen_is_case_insensitive() {
        assert_eq!(iso_to_qwen_name("EN").unwrap(), "english");
        assert_eq!(iso_to_qwen_name("ZH").unwrap(), "chinese");
    }
}
