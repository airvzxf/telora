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
#[cfg(feature = "cuda-qwen3asr")]
use cudarc::driver::CudaContext;
use log::info;
use voxora_bridge::{
    AsrEngine, Device, EngineFamily, HuggingFaceSource, ModelSource, ResolveOptions,
    TranscribeOptions, WhisperEngine,
};
use voxora_registry::{ModelId, Registry, RegistryHfExt};

use crate::socket::SttConfig;

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
    /// On-disk model path: the `.bin` file for Whisper, the snapshot
    /// directory for Qwen3-ASR.
    resolved_path: String,
}

impl BridgeTranscriber {
    /// Construct from a Hugging Face model id and a [`EngineFamily`].
    ///
    /// `cache_dir` must be pinned explicitly so the operator's
    /// existing `~/.cache/voxora/models/huggingface` cache survives;
    /// voxora-hf would otherwise default to a voxora-config-derived
    /// root without the `models/huggingface` suffix.
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

        // Cap resolved files at 8 GiB (closes voxora EPIC #148,
        // adopted from voxora 0.5.3). 8 GiB clears the largest
        // legitimate artifact the daemon ever resolves
        // (Qwen/Qwen3-ASR-1.7B's BF16 `model.safetensors` is ~3.4
        // GB; ggml-large-v3 F32 is ~3.1 GB) with ~2.4x headroom
        // for future Qwen releases while staying well below any
        // "pathological 100 GB+" size.
        //
        // HONEST GAP (closes #148): voxora-hf 0.6.2 (and 0.5.1+)
        // does not yet honour `ResolveOptions::max_bytes` despite the
        // voxora-traits CHANGELOG claiming it does — zero references
        // to the field exist anywhere under voxora-hf/src/. The cap
        // therefore does NOT activate on the daemon's HF resolve
        // path today. It activates the moment voxora-hf plumbs
        // `opts.max_bytes` through `HfClient::get_to_file` (tracked
        // upstream). The cap is wired here so the daemon is
        // already prepared when the upstream fix lands and to
        // document the intended security posture to anyone reading
        // this code.
        let opts = ResolveOptions::with_max_bytes(8 * 1024 * 1024 * 1024);

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
                refuse_if_symlink(&dir.path)?;
                // Pick the device based on the local GPU's compute
                // capability. candle's WMMA BF16 kernels
                // (`candle-kernels/src/moe/moe_wmma*.cu`) target
                // sm_70+ (Volta); the CI-built telora-daemon
                // binary embeds sm_80 SASS for those kernels, and
                // `qwen3_asr::best_device()` does not know about
                // this floor — it picks CUDA if any NVIDIA driver
                // is present, then panics at first inference with
                // `CUDA_ERROR_INVALID_PTX` on a Pascal sm_61 host.
                // Whisper is unaffected because ggml-cuda ships
                // forward-compat PTX in addition to SASS.
                let device = pick_qwen3asr_device();
                let engine = if device.is_cpu() {
                    // CPU path: `from_hf` always loads on
                    // `best_device()`, so load with `load_with_device`.
                    // The official HF snapshot has no `tokenizer.json`,
                    // and the voxora 0.6 release we depend on only
                    // synthesises it inside `from_hf`. So on the first
                    // run (file missing) we still call `from_hf` for that
                    // side effect, which costs one throwaway load; once
                    // the file exists we skip it. Switch to
                    // `QwenAsrEngine::from_hf_with_device` once a voxora
                    // release ships it (airvzxf/voxora#249).
                    if !dir.path.join("tokenizer.json").is_file() {
                        info!(
                            "qwen3-asr: tokenizer.json missing; one-off preparation load \
                             before loading on CPU"
                        );
                        if let Err(e) = voxora_bridge::QwenAsrEngine::from_hf(
                            hf_source.as_ref(),
                            model_id,
                            &opts,
                        )
                        .await
                        {
                            log::warn!("qwen3-asr preparation load failed (continuing): {e}");
                        }
                    }
                    voxora_bridge::QwenAsrEngine::load_with_device(&dir.path, device).with_context(
                        || format!("failed to load Qwen3-ASR engine for {model_id:?}"),
                    )?
                } else {
                    voxora_bridge::QwenAsrEngine::from_hf(hf_source.as_ref(), model_id, &opts)
                        .await
                        .with_context(|| {
                            format!("failed to load Qwen3-ASR engine for {model_id:?}")
                        })?
                };
                (
                    Arc::new(engine) as Arc<dyn AsrEngine>,
                    dir.path.display().to_string(),
                )
            }
            // `EngineFamily` is `#[non_exhaustive]`; the registry
            // cross-check above already rejected any other family.
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

    /// On-disk model path, reported by STATUS.
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
}

impl Transcriber for BridgeTranscriber {
    fn transcribe(&self, audio_data: &[f32], language: Option<&str>) -> Result<String> {
        let lang =
            engine_language(self.model_kind, language.unwrap_or("auto")).map_err(|e| anyhow!(e))?;

        let opts = TranscribeOptions::new(lang.clone(), false, true);
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

/// The engine family named by `model_kind`, if telora can run it.
///
/// # Errors
///
/// A message naming the rejected value.
pub fn engine_family(model_kind: &str) -> Result<EngineFamily, String> {
    EngineFamily::from_config(model_kind)
        .filter(|k| matches!(k, EngineFamily::Whisper | EngineFamily::Qwen3Asr))
        .ok_or_else(|| format!("motor no soportado: {model_kind:?}; usa `whisper` o `qwen3-asr`"))
}

/// The engine's spelling of the configured language; `None` (from
/// `"auto"`) makes the engine detect it.
fn engine_language(kind: EngineFamily, language: &str) -> Result<Option<String>, String> {
    if language.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    let code = language.to_ascii_lowercase();
    let mapped = match kind {
        EngineFamily::Whisper => voxora_whisper::validate_lang(&code).ok().map(|_| code),
        EngineFamily::Qwen3Asr => iso_to_qwen_name(&code),
        _ => None,
    };
    mapped.map(Some).ok_or_else(|| {
        format!(
            "idioma no soportado por {kind}: {language:?}; usa un código ISO 639-1 \
             (por ejemplo \"es\") o \"auto\""
        )
    })
}

/// Reject a config the daemon could not run, before anything loads it.
///
/// # Errors
///
/// A message naming the offending field.
pub fn validate_stt_config(config: &SttConfig) -> Result<(), String> {
    let kind = engine_family(&config.model_kind)?;
    if config.model_id.trim().is_empty() {
        return Err("model_id está vacío".to_string());
    }
    engine_language(kind, &config.language)?;
    if !(1..=MAX_RECORDING_SECONDS).contains(&config.max_recording_seconds) {
        return Err(format!(
            "max_recording_seconds debe estar entre 1 y {MAX_RECORDING_SECONDS} (es {})",
            config.max_recording_seconds
        ));
    }
    Ok(())
}

/// One hour; the whole recording is held in RAM (~230 MB at this cap).
const MAX_RECORDING_SECONDS: u32 = 3600;

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

/// Pick the best candle device for the qwen3-asr engine based on the
/// local GPU compute capability.
///
/// candle-kernels' WMMA BF16 kernels in
/// `candle-kernels/src/moe/moe_wmma*.cu` target sm_70+ (Volta). The
/// CI-built telora-daemon binary embeds sm_80 SASS for those kernels;
/// on a Pascal sm_61 host the driver rejects the load at first
/// inference with `CUDA_ERROR_INVALID_PTX` because the SASS uses
/// instructions the local GPU does not implement. `best_device()`
/// does not know about this floor — it only checks whether the CUDA
/// driver is reachable, not whether the bundled SASS will execute —
/// so without this probe a Pascal laptop would load the engine
/// cleanly and only blow up at first `transcribe()`.
///
/// Whisper's CUDA path is unaffected: ggml-cuda ships
/// forward-compat PTX alongside its SASS, so JIT falls back to the
/// local ISA without the operator touching anything.
///
/// Returns `Device::Cpu` on any of:
/// - no NVIDIA driver (CUDA context creation fails),
/// - the probe fails for any reason (treated as "can't tell, fall back"),
/// - the GPU's compute capability is below sm_70,
/// - the device creation succeeds but `Device::new_cuda(0)` later
///   fails (out of VRAM, exclusivity conflict, etc.).
#[cfg(feature = "cuda-qwen3asr")]
fn pick_qwen3asr_device() -> Device {
    let ctx = match CudaContext::new(0) {
        Ok(c) => c,
        Err(e) => {
            info!("no local CUDA context ({e}); qwen3-asr will use CPU");
            return Device::Cpu;
        }
    };
    let (major, minor) = match ctx.compute_capability() {
        Ok(cc) => cc,
        Err(e) => {
            log::warn!(
                "could not query local CUDA compute capability ({e}); \
                 qwen3-asr will use CPU"
            );
            return Device::Cpu;
        }
    };
    if major < 7 {
        log::warn!(
            "local GPU compute capability is sm_{major}.{minor}, below candle's WMMA \
             BF16 floor (sm_70 / Volta); forcing CPU for qwen3-asr. The CI-built \
             telora-daemon binary embeds sm_80 SASS for qwen3-asr's CUDA path that \
             cannot execute on this hardware. Whisper keeps its GPU path because \
             ggml-cuda ships forward-compat PTX. To re-enable GPU Qwen3-ASR on this \
             host you would need a Volta-or-newer GPU; rebuilds with \
             CUDA_COMPUTE_CAP=61 cannot compile the WMMA kernels and produce an \
             unusable binary."
        );
        return Device::Cpu;
    }
    // GPU is new enough. Let candle actually create the device —
    // it can still fail for VRAM / exclusivity reasons unrelated to
    // compute_capability, in which case we fall back to CPU rather
    // than propagate the error.
    Device::new_cuda(0).unwrap_or_else(|e| {
        log::warn!(
            "local GPU is sm_{major}.{minor} but Device::new_cuda(0) failed ({e}); \
             qwen3-asr will use CPU"
        );
        Device::Cpu
    })
}

/// CPU-only build: qwen3-asr has no GPU backend compiled in.
#[cfg(not(feature = "cuda-qwen3asr"))]
fn pick_qwen3asr_device() -> Device {
    Device::Cpu
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

    fn stt(kind: &str, language: &str, seconds: u32) -> SttConfig {
        SttConfig {
            model_id: "org/modelo".to_string(),
            model_kind: kind.to_string(),
            model_path: String::new(),
            language: language.to_string(),
            max_recording_seconds: seconds,
        }
    }

    #[test]
    fn auto_language_asks_both_engines_to_detect() {
        assert_eq!(engine_language(EngineFamily::Whisper, "auto"), Ok(None));
        assert_eq!(engine_language(EngineFamily::Qwen3Asr, "AUTO"), Ok(None));
    }

    #[test]
    fn languages_are_spelled_the_way_each_engine_expects() {
        assert_eq!(
            engine_language(EngineFamily::Whisper, "ES"),
            Ok(Some("es".to_string()))
        );
        assert_eq!(
            engine_language(EngineFamily::Qwen3Asr, "es"),
            Ok(Some("spanish".to_string()))
        );
    }

    #[test]
    fn valid_configs_pass() {
        for (kind, language) in [("whisper", "es"), ("whisper", "auto"), ("qwen3-asr", "en")] {
            assert_eq!(validate_stt_config(&stt(kind, language, 1800)), Ok(()));
        }
    }

    #[test]
    fn invalid_configs_name_the_problem() {
        let cases = [
            (stt("whisper", "xx", 60), "xx"),
            (stt("qwen3-asr", "bn", 60), "bn"),
            (stt("whisper", "", 60), "idioma"),
            (stt("minimax", "es", 60), "minimax"),
            (stt("whisper", "es", 0), "max_recording_seconds"),
            (stt("whisper", "es", 3601), "max_recording_seconds"),
            (
                SttConfig {
                    model_id: " ".to_string(),
                    ..stt("whisper", "es", 60)
                },
                "model_id",
            ),
        ];
        for (config, needle) in cases {
            let err = validate_stt_config(&config).unwrap_err();
            assert!(err.contains(needle), "{config:?}: {err}");
        }
    }
}
