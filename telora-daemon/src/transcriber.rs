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
use cudarc::driver::CudaContext;
use log::{info, warn};
use voxora_bridge::{
    AsrEngine, Device, EngineFamily, HuggingFaceSource, MiniMaxConfig, MiniMaxEngine, ModelSource,
    ResolveOptions, TranscribeOptions, WhisperEngine,
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
    /// `from_id` succeeds). For Whisper / Qwen3-ASR this is the
    /// path inside `$XDG_CACHE_HOME/voxora/models/huggingface/...`
    /// that voxora-hf returned from `Registry::resolve`. For the
    /// MiniMax hosted engine this is the empty string — there
    /// is no on-disk model; the API endpoint lives in
    /// [`Self::endpoint`] instead. The two-field split (closes
    /// #167) lets the status display pick `Resolved Path:` for
    /// local engines and `Endpoint:` for hosted engines without
    /// overloading a single field with a URL string.
    resolved_path: String,
    /// Resolved API endpoint URL for hosted engines (closes
    /// #167). Empty for the on-disk engines. For MiniMax this is
    /// `voxora_minimax::MiniMaxConfig::endpoint()` — defaults to
    /// `https://api.minimax.io`, overridable per engine via
    /// `VOXORA_MINIMAX_ENDPOINT`. Always paired with
    /// `resolved_path = ""` so the display logic can branch on
    /// "is this URL-shaped? → show under Endpoint:".
    endpoint: String,
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
    ///
    /// `minimax_env_file` (closes #166) is the optional path to a
    /// `.env` file containing the MiniMax bearer token (e.g.
    /// `--minimax-env-file /run/secrets/telora.env`). When `None`,
    /// the resolver falls back to the default discovery cascade
    /// (`/etc/telora/.env` → `$XDG_CONFIG_HOME/telora/.env` →
    /// `VOXORA_MINIMAX_API_KEY` → `MINIMAX_API_KEY`) inside
    /// `telora_common::resolve_minimax_api_key`. Only consulted
    /// when `model_kind == EngineFamily::MiniMax`; ignored
    /// otherwise.
    pub async fn from_id(
        model_id: &str,
        model_kind: EngineFamily,
        cache_dir: Option<std::path::PathBuf>,
        hf_token: Option<String>,
        minimax_env_file: Option<&std::path::Path>,
    ) -> Result<Self> {
        // ── Hosted-API shortcut (closes #163, EPIC #153, #165) ─
        // MiniMax does NOT use the voxora HF registry: there is no
        // on-disk model, no cache_dir, no hf_token, no model_path,
        // no symlink check, no GPU device picker. Just an API key
        // (resolved from the `.env` cascade) and a `voxora-minimax`
        // engine that POSTs the WAV over HTTPS. We short-circuit
        // before the HuggingFaceSource builder so the cache_dir /
        // hf_token arguments stay on the function signature
        // unchanged for every other engine family.
        if model_kind == EngineFamily::MiniMax {
            let (_, key) = telora_common::resolve_minimax_api_key(minimax_env_file);
            let api_key = key?;
            let mut config =
                MiniMaxConfig::new(api_key).map_err(|e| anyhow!("voxora-minimax: {e}"))?;

            // The operator's `model_id` field is now honoured for
            // MiniMax (closes #165). Empty / whitespace / equals
            // the legacy Whisper default that `default_stt_config()`
            // fills in when the operator omits the field → fall
            // back to voxora's `DEFAULT_MODEL` and log a one-line
            // notice so the operator sees what was used in
            // `journalctl -u telora-daemon`. Any other non-empty
            // value flows into `MiniMaxConfig::with_model(...)`;
            // MiniMax ships exactly one model today (`asr-1.0`,
            // voxora-minimax/src/client.rs:31-34), so anything else
            // yields a 400 `bad_request_error` server-side and
            // surfaces as `AsrError::InvalidInput` on the first
            // `transcribe()`.
            let trimmed = model_id.trim();
            const WHISPER_LEGACY_DEFAULT: &str = "ggerganov/whisper.cpp/ggml-base.bin";
            if !trimmed.is_empty() && trimmed != WHISPER_LEGACY_DEFAULT {
                info!("MiniMax model overridden via telora.toml: {trimmed}");
                config = config.with_model(trimmed);
            } else {
                info!(
                    "using voxora default MiniMax model `{}` because telora.toml is silent \
                     (set `model_id = \"…\"` in telora.toml to override)",
                    config.model(),
                );
            }

            let endpoint = config.endpoint().to_string();
            let model = config.model().to_string();
            let engine = MiniMaxEngine::new(config).map_err(|e| anyhow!("voxora: {e}"))?;
            // Closes #167: `resolved_path` is the empty string for
            // the hosted engine (no on-disk file), and the URL
            // lives in the new `endpoint` field. The status display
            // branches on "is `endpoint` non-empty?" to pick
            // `Endpoint:` over `Resolved Path:`.
            let resolved_path = String::new();
            info!("loaded MiniMax hosted ASR engine (endpoint={endpoint}, model={model})");
            return Ok(Self {
                engine: Arc::new(engine) as Arc<dyn AsrEngine>,
                // Store the *resolved* model (not the operator's
                // raw input) so `bridge.model_id()` matches what
                // voxora actually sends to the wire. Without this
                // the status display shows the legacy Whisper
                // default while the request goes out with
                // `model=asr-1.0`.
                model_id: model,
                model_kind,
                resolved_path,
                endpoint,
            });
        }
        // ── End hosted-API shortcut ─────────────────────────────

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
                    // CPU path: bypass voxora's `from_hf` (which
                    // always calls `best_device()` and would re-pick
                    // CUDA) and use `load_with_device` directly.
                    // voxora's tokenizer-synthesis step is private
                    // to voxora-qwen3asr; calling `from_hf` once
                    // writes `tokenizer.json` to disk before
                    // attempting the engine load, so even if the
                    // CUDA load fails on this host the cache is
                    // shaped correctly for the CPU retry.
                    let _ =
                        voxora_bridge::QwenAsrEngine::from_hf(hf_source.as_ref(), model_id, &opts)
                            .await;
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
            // `EngineFamily` is `#[non_exhaustive]` so future engine
            // families (parakeet, voxtral, …) land as a new variant
            // without breaking this match. The registry cross-check
            // above guarantees we only see families we have a real
            // loader for; anything else is a config-mismatch bug we
            // want to hear about loudly. The MiniMax arm is handled
            // by the pre-match shortcut above (closes #163) — the
            // hosted engine never enters the registry path.
            other => {
                return Err(anyhow!(
                    "model_kind {other:?} has no voxora engine adapter wired up in telora; \
                     current set: Whisper, Qwen3Asr, MiniMax"
                ));
            }
        };
        // Closes #167: on-disk engines have `endpoint = ""`. The
        // status display prints `Resolved Path:` (driven by
        // `resolved_path`) and skips the `Endpoint:` line entirely.
        let endpoint = String::new();

        info!(
            "loaded {} model from {model_id:?} (resolved to {resolved_path})",
            model_kind
        );

        Ok(Self {
            engine,
            model_id: model_id.to_string(),
            model_kind,
            resolved_path,
            endpoint,
        })
    }

    /// Resolved local path of the loaded model. Used by the status
    /// response so the GUI can show the on-disk location. For
    /// hosted engines (MiniMax) this is the empty string — the
    /// API endpoint lives in [`Self::endpoint`] instead.
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

    /// Resolved API endpoint URL for hosted engines (closes
    /// #167). Empty for the on-disk engines (Whisper / Qwen3-ASR).
    /// The status display branches on `endpoint().is_empty()` to
    /// pick the `Endpoint:` heading for hosted engines and the
    /// `Resolved Path:` heading for on-disk ones — keeping the
    /// two concepts in separate fields avoids the previous
    /// overloading where a URL was printed under a
    /// filesystem-path-shaped heading.
    #[allow(dead_code)]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Translate the user-facing ISO 639-1 code into the engine-
    /// specific spelling. Returns `None` if the code is not
    /// recognised; callers should treat that as a user error.
    fn map_language(&self, iso: &str) -> Option<String> {
        match self.model_kind {
            EngineFamily::Whisper => Some(iso.to_ascii_lowercase()),
            EngineFamily::Qwen3Asr => iso_to_qwen_name(iso),
            // MiniMax (closes #163) accepts the same bare 2-letter
            // BCP-47 tags as its whitelist (`voxora-minimax/src/
            // language.rs:19-40`); the validator runs server-side.
            // We reuse the Whisper passthrough rather than the
            // Qwen full-name mapping because the latter would
            // rewrite "en" → "english" and MiniMax's whitelist
            // rejects the full English spelling with a 400.
            EngineFamily::MiniMax => Some(iso.to_ascii_lowercase()),
            // `EngineFamily` is `#[non_exhaustive]`. We promise only
            // the three variants above are wired up; anything else
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
                // MiniMax (closes #163) treats `None` / `""` /
                // whitespace as "auto-detect / mixed-language" and
                // validates against the whitelist
                // (`voxora-minimax/src/language.rs:60-76`); the
                // literal `"auto"` would be rejected by the
                // validator. We send an empty string and let the
                // engine omit the `language` form field, which is
                // exactly the upstream documented auto-detect mode.
                EngineFamily::MiniMax => String::new(),
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

// Note (closes #166): the MiniMax bearer-token resolver moved to
// `telora_common::resolve_minimax_api_key`. That helper reads
// `~/.config/telora/.env` and `/etc/telora/.env` via `dotenvy`
// before falling back to the env-var cascade, so the secret never
// lives in `telora.toml`.

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
            warn!(
                "could not query local CUDA compute capability ({e}); \
                 qwen3-asr will use CPU"
            );
            return Device::Cpu;
        }
    };
    if major < 7 {
        warn!(
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
        warn!(
            "local GPU is sm_{major}.{minor} but Device::new_cuda(0) failed ({e}); \
             qwen3-asr will use CPU"
        );
        Device::Cpu
    })
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

    // ── Closes #165 — model_id propagation for MiniMax ────────
    //
    // `BridgeTranscriber::from_id` for `EngineFamily::MiniMax`
    // stores the *resolved* model label in `bridge.model_id()`,
    // not the operator's raw TOML input. The construction is
    // pure (`MiniMaxEngine::new` does no I/O, `MiniMaxConfig::new`
    // only rejects empty keys), so a dummy key is enough — the
    // tests never call `transcribe()` and never reach the network.
    //
    // The process-global env lock serialises the cases that touch
    // `MINIMAX_API_KEY` so a parallel test cannot race.

    static MINIMAX_BUILD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvRestore {
        prev_minimax: Option<String>,
        prev_voxora_minimax: Option<String>,
    }

    impl EnvRestore {
        fn new() -> Self {
            Self {
                prev_minimax: std::env::var("MINIMAX_API_KEY").ok(),
                prev_voxora_minimax: std::env::var("VOXORA_MINIMAX_API_KEY").ok(),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            // SAFETY: tests hold MINIMAX_BUILD_LOCK for their entire
            // lifetime, so no other thread observes a
            // half-modified environment.
            match &self.prev_minimax {
                Some(v) => unsafe { std::env::set_var("MINIMAX_API_KEY", v) },
                None => unsafe { std::env::remove_var("MINIMAX_API_KEY") },
            }
            match &self.prev_voxora_minimax {
                Some(v) => unsafe { std::env::set_var("VOXORA_MINIMAX_API_KEY", v) },
                None => unsafe { std::env::remove_var("VOXORA_MINIMAX_API_KEY") },
            }
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime for MiniMax unit tests")
    }

    #[test]
    fn minimax_empty_model_id_falls_back_to_voxora_default() {
        let _lock = MINIMAX_BUILD_LOCK.lock().unwrap();
        let _restore = EnvRestore::new();
        // SAFETY: lock held.
        unsafe {
            std::env::set_var("MINIMAX_API_KEY", "sk-test-fixture");
        }
        runtime().block_on(async {
            let bridge = BridgeTranscriber::from_id("", EngineFamily::MiniMax, None, None, None)
                .await
                .expect("MiniMax engine builds");
            assert_eq!(bridge.model_id(), "asr-1.0");
            assert_eq!(
                bridge.endpoint(),
                "https://api.minimax.io",
                "endpoint must default to voxora-minimax's documented host"
            );
            assert!(
                bridge.resolved_path().is_empty(),
                "resolved_path must be empty for hosted engines (closes #167); got {:?}",
                bridge.resolved_path()
            );
        });
    }

    #[test]
    fn minimax_legacy_whisper_default_model_id_falls_back_to_voxora_default() {
        let _lock = MINIMAX_BUILD_LOCK.lock().unwrap();
        let _restore = EnvRestore::new();
        // SAFETY: lock held.
        unsafe {
            std::env::set_var("MINIMAX_API_KEY", "sk-test-fixture");
        }
        runtime().block_on(async {
            let bridge = BridgeTranscriber::from_id(
                "ggerganov/whisper.cpp/ggml-base.bin",
                EngineFamily::MiniMax,
                None,
                None,
                None,
            )
            .await
            .expect("MiniMax engine builds");
            // The legacy Whisper default is treated as "unset" so
            // the daemon does not forward the stale Whisper id
            // upstream and get a 400.
            assert_eq!(bridge.model_id(), "asr-1.0");
        });
    }

    #[test]
    fn minimax_explicit_model_id_is_honoured() {
        let _lock = MINIMAX_BUILD_LOCK.lock().unwrap();
        let _restore = EnvRestore::new();
        // SAFETY: lock held.
        unsafe {
            std::env::set_var("MINIMAX_API_KEY", "sk-test-fixture");
        }
        runtime().block_on(async {
            let bridge = BridgeTranscriber::from_id(
                "asr-2.0-preview",
                EngineFamily::MiniMax,
                None,
                None,
                None,
            )
            .await
            .expect("MiniMax engine builds");
            assert_eq!(
                bridge.model_id(),
                "asr-2.0-preview",
                "operator's model_id must flow through .with_model() (closes #165)"
            );
        });
    }
}
