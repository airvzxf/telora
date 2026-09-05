//! End-to-end regression for `airvzxf/telora#79` against the
//! registry introduced in voxora 0.2.0.
//!
//! The registry semantics it pins have been stable since the
//! voxora 0.2.0 fix; the test currently runs against the voxora
//! 0.4 line.
//!
//! Before the fix, `telora-daemon/src/transcriber.rs::from_id` would
//! take the on-disk directory returned by `voxora_hf::resolve_single_file`,
//! lex-sort every `*.bin` file inside it, and pick the first one.
//! For `model_id = "ggerganov/whisper.cpp/ggml-large-v3.bin"` against
//! a cache that already contained both `ggml-base.bin` and
//! `ggml-large-v3.bin`, the lex-sort picked `ggml-base.bin` and the
//! daemon mmap'd the wrong weights (147 MB, 6 audio layers) instead
//! of the requested 3 GB model.
//!
//! The voxora 0.2.0 resolver shipped two independent fixes:
//!
//! 1. `voxora-hf::resolve_single_file` no longer trusts the
//!    `.complete` marker blindly — if the marker is set but the
//!    requested file is missing, it returns `AsrError::ModelNotFound`
//!    instead of silently returning the directory.
//! 2. `ModelDir::entry: Option<PathBuf>` names the exact file the
//!    caller asked for in 3-segment `org/repo/file` resolves, so the
//!    daemon no longer has to scan the directory at all.
//!
//! These tests still pin both behaviours against a hand-rolled cache
//! layout under the current voxora 0.4 line. The registry semantics
//! introduced in voxora 0.2.0 are stable across the 0.2 → 0.3 → 0.4
//! bumps (voxora 0.4 removed the `voxora-core` deprecation shim and
//! moved those traits into `voxora-traits`, but `voxora-bridge`
//! re-exports `voxora_traits::*` so the imports below still resolve
//! unchanged). They run by default (no `#[ignore]`) and use only a
//! few bytes per stub file — the file contents do not matter, only
//! the names and the `.complete` marker shape.
//!
//! Run with:
//!
//! ```text
//! cargo test -p telora-daemon --test voxora_020_resolution -- --nocapture
//! ```

use std::sync::Arc;

use telora_daemon::BridgeTranscriber;
use tempfile::TempDir;
use voxora_bridge::{AsrError, EngineFamily, HuggingFaceSource, ModelSource, ResolveOptions};
use voxora_registry::{ModelId, Registry, RegistryError, RegistryHfExt};

/// Build a hand-rolled HF cache layout that mirrors
/// `voxora-hf`'s `<cache_root>/<org>/<repo>/<revision>/` convention
/// and populate it with the exact files the cache must contain for
/// the test. Returns the tempdir handle so the caller can `await?`
/// on the source's resolve and drop the dir on teardown.
fn build_cache_with_two_ggml_bins() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp
        .path()
        .join("ggerganov")
        .join("whisper.cpp")
        .join("main");
    std::fs::create_dir_all(&dir).expect("mkdir cache layout");
    // The names are what matters, not the bytes. A single byte is
    // enough for `is_file()` to return true and for voxora's
    // `cache::is_complete` to take the fast path.
    std::fs::write(dir.join("ggml-base.bin"), b"b").expect("write base");
    std::fs::write(dir.join("ggml-large-v3.bin"), b"L").expect("write large-v3");
    std::fs::write(dir.join(".complete"), b"").expect("write complete marker");
    tmp
}

/// Build a hand-rolled cache where `.complete` is set but the
/// requested `ggml-tiny.en.bin` is missing. This is the exact
/// shape voxora-hf 0.2 was failing silently on under 0.1.x and is
/// the dual half of the #79 fix.
fn build_cache_with_marker_but_missing_file() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp
        .path()
        .join("ggerganov")
        .join("whisper.cpp")
        .join("main");
    std::fs::create_dir_all(&dir).expect("mkdir cache layout");
    std::fs::write(dir.join("ggml-base.bin"), b"b").expect("write base");
    // Deliberately no ggml-tiny.en.bin.
    std::fs::write(dir.join(".complete"), b"").expect("write complete marker");
    tmp
}

/// Build a `HuggingFaceSource` pinned at the test's cache root,
/// plus a `Registry` that uses it. Returns both because the
/// cache-marker test (`resolve_rejects_cache_with_marker_but_missing_file`)
/// needs `source.resolve(...)` directly to assert on the underlying
/// `AsrError::ModelNotFound` without going through `RegistryError::Parse`,
/// and both tests need the registry for the canonical resolve path.
fn build_source_and_registry(cache_root: &std::path::Path) -> (Arc<HuggingFaceSource>, Registry) {
    let source = Arc::new(
        HuggingFaceSource::builder()
            .cache_dir(cache_root.to_path_buf())
            // Unreachable so the slow path (network re-download)
            // can never succeed; the tests rely on the fast path
            // (cache hit) exclusively.
            .base_url("http://127.0.0.1:1")
            .build()
            .expect("build HuggingFaceSource"),
    );
    let dyn_source: Arc<dyn ModelSource> = source.clone();
    let registry = Registry::new(dyn_source).with_builtin_descriptors();
    (source, registry)
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_picks_the_requested_ggml_large_v3_bin_not_lex_sort_base() {
    let tmp = build_cache_with_two_ggml_bins();
    let (_source, registry) = build_source_and_registry(tmp.path());
    let opts = ResolveOptions::default();

    let parsed = ModelId::parse("ggerganov/whisper.cpp/ggml-large-v3.bin").expect("ModelId::parse");
    let resolved = registry
        .resolve(&parsed, &opts)
        .await
        .expect("registry.resolve must succeed");

    assert_eq!(
        resolved.descriptor.family,
        EngineFamily::Whisper,
        "3-segment id under ggerganov/whisper.cpp must resolve to the Whisper descriptor"
    );

    let entry = resolved
        .model_dir
        .entry
        .clone()
        .expect("3-segment HF id must populate ModelDir.entry");

    assert_eq!(
        entry.file_name().and_then(|s| s.to_str()),
        Some("ggml-large-v3.bin"),
        "ModelDir.entry must name the exact file the user asked for (NOT ggml-base.bin from the lex-sort)"
    );

    // And the file itself must be the requested one, not whatever
    // happened to lex-sort first.
    assert!(
        entry.ends_with("ggerganov/whisper.cpp/main/ggml-large-v3.bin"),
        "entry path must end with the exact large-v3 file, got {}",
        entry.display()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_rejects_cache_with_marker_but_missing_file() {
    let tmp = build_cache_with_marker_but_missing_file();
    let (source, registry) = build_source_and_registry(tmp.path());
    let opts = ResolveOptions::default();

    let parsed = ModelId::parse("ggerganov/whisper.cpp/ggml-tiny.en.bin").expect("ModelId::parse");
    let registry_err = registry
        .resolve(&parsed, &opts)
        .await
        .expect_err("registry must reject 3-segment id when the cache marker lies about the file");

    // The registry wraps the source error in RegistryError::Parse;
    // the underlying cause is `AsrError::ModelNotFound("...is marked
    // complete but does not contain...")`. We surface that to
    // `voxora_traits::AsrError::ModelNotFound` (re-exported through
    // `voxora_bridge::AsrError` since voxora 0.4 dropped the
    // `voxora-core` shim) via the source's own resolve so the
    // assertion can be precise.
    let direct = source
        .resolve("ggerganov/whisper.cpp/ggml-tiny.en.bin", &opts)
        .await
        .expect_err("source.resolve must surface AsrError::ModelNotFound");
    match direct {
        AsrError::ModelNotFound(msg) => {
            assert!(
                msg.contains("ggml-tiny.en.bin"),
                "error must name the missing file, got: {msg}",
            );
        }
        other => panic!("expected AsrError::ModelNotFound, got {other:?}"),
    }

    // The registry error must itself reference the missing file
    // (the source error is wrapped via `RegistryError::Parse`), so
    // operators debugging a failed REFRESH see the offending file
    // name without having to follow the chain by hand.
    let display = format!("{registry_err:?}");
    assert!(
        display.contains("ggml-tiny.en.bin") || display.contains("ModelNotFound"),
        "registry error must name the missing file, got: {display}"
    );
    // Belt and braces: confirm the variant shape we expect.
    assert!(
        matches!(registry_err, RegistryError::Parse(_)),
        "expected RegistryError::Parse, got {registry_err:?}"
    );
}

// ---- Daemon-API regression tests for F2 follow-up (S1, B2, etc.) ----
//
// These tests drive `BridgeTranscriber::from_id` directly (the
// daemon's public API) instead of going through the registry in
// isolation. They pin the lex-sort fix at the daemon's surface (Test4)
// and the new security / config-mismatch guards (Test2, Test3, Test6).

/// Build a 2-bins cache (no `.complete` marker) for the
/// 2-segment-Whisper rejection test. The marker is intentionally
/// absent so the lex-sort path is the only one that could possibly
/// succeed — but the daemon must reject this BEFORE any engine
/// load because `ModelDir.entry` is `None`.
fn build_cache_with_two_ggml_bins_no_marker() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp
        .path()
        .join("ggerganov")
        .join("whisper.cpp")
        .join("main");
    std::fs::create_dir_all(&dir).expect("mkdir cache layout");
    std::fs::write(dir.join("ggml-base.bin"), b"b").expect("write base");
    std::fs::write(dir.join("ggml-large-v3.bin"), b"L").expect("write large-v3");
    // Deliberately no `.complete` marker — we want to verify the
    // daemon rejects this BEFORE voxora-hf tries to download.
    tmp
}

/// Build a Qwen3-ASR cache layout so the Qwen descriptor accepts
/// the resolve, then run `from_id` with `EngineFamily::Whisper` to
/// trigger the cross-check.
fn build_cache_with_qwen_config() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path().join("Qwen").join("Qwen3-ASR-0.6B").join("main");
    std::fs::create_dir_all(&dir).expect("mkdir cache layout");
    // The cross-check fires before any engine load — the stub file
    // is enough to make `voxora-hf::resolve` think the model is
    // cached.
    std::fs::write(dir.join("config.json"), b"{}").expect("write config");
    std::fs::write(dir.join(".complete"), b"").expect("write complete marker");
    tmp
}

/// Build a cache where `ggml-large-v3.bin` is a symlink pointing at
/// an arbitrary readable file (`/etc/hostname`). voxora-hf's
/// `is_file()` probe follows the symlink and reports the cache as
/// complete; `BridgeTranscriber::from_id` must refuse on its own
/// before whisper.cpp's mmap has a chance to follow the symlink.
fn build_cache_with_symlinked_bin() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp
        .path()
        .join("ggerganov")
        .join("whisper.cpp")
        .join("main");
    std::fs::create_dir_all(&dir).expect("mkdir cache layout");
    // Pick any file we know exists on the test host. /etc/hostname
    // is the conventional choice — short, present on every Linux
    // host we run CI on, and harmless to read.
    let target = std::path::PathBuf::from("/etc/hostname");
    assert!(
        target.is_file(),
        "/etc/hostname must exist for the symlink-refusal test to be meaningful"
    );
    std::os::unix::fs::symlink(&target, dir.join("ggml-large-v3.bin"))
        .expect("create symlink in cache");
    std::fs::write(dir.join(".complete"), b"").expect("write complete marker");
    tmp
}

#[tokio::test(flavor = "multi_thread")]
async fn from_id_cross_check_rejects_family_id_mismatch() {
    let tmp = build_cache_with_qwen_config();
    let result = BridgeTranscriber::from_id(
        "Qwen/Qwen3-ASR-0.6B",
        EngineFamily::Whisper,
        Some(tmp.path().to_path_buf()),
        None,
    )
    .await;
    let err = match result {
        Ok(_) => panic!("cross-check must fail when the user pins Whisper but id resolves to Qwen"),
        Err(e) => e,
    };
    let display = format!("{err:#}");
    assert!(
        display.contains("does not match"),
        "cross-check error must mention the mismatch, got: {display}"
    );
    // The Display rendering of `EngineFamily` must be the canonical
    // spelling ("whisper" / "qwen3-asr"), NOT the Debug spelling
    // ("EngineFamily::Whisper"). Operators see this string in the
    // daemon's log.
    assert!(
        !display.contains("EngineFamily::"),
        "cross-check error must use Display, not Debug, got: {display}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn from_id_two_segment_whisper_rejects_with_actionable_error() {
    let tmp = build_cache_with_two_ggml_bins_no_marker();
    let result = BridgeTranscriber::from_id(
        "ggerganov/whisper.cpp",
        EngineFamily::Whisper,
        Some(tmp.path().to_path_buf()),
        None,
    )
    .await;
    let err = match result {
        Ok(_) => panic!("2-segment whisper id without entry must be rejected"),
        Err(e) => e,
    };
    let display = format!("{err:#}");
    assert!(
        display.contains("3-segment form") || display.contains("ggml-<variant>"),
        "2-segment whisper error must point the user at the 3-segment form, got: {display}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn from_id_surfaces_resolved_path_matching_dir_entry_end_to_end() {
    let tmp = build_cache_with_two_ggml_bins();
    let result = BridgeTranscriber::from_id(
        "ggerganov/whisper.cpp/ggml-large-v3.bin",
        EngineFamily::Whisper,
        Some(tmp.path().to_path_buf()),
        None,
    )
    .await;
    match result {
        Ok(bridge) => {
            let p = bridge.resolved_path();
            assert!(
                p.ends_with("ggml-large-v3.bin"),
                "resolved_path must name the requested file, got: {p}"
            );
            assert!(
                !p.ends_with("ggml-base.bin"),
                "resolved_path must NOT lex-sort to base, got: {p}"
            );
        }
        Err(e) => {
            // The stub bytes won't pass whisper.cpp's header check
            // — that's fine. The structural guarantee we care about
            // is that the error context names the RIGHT file (the
            // requested `ggml-large-v3.bin`), not the lex-sort
            // winner `ggml-base.bin`.
            let display = format!("{e:#}");
            assert!(
                display.contains("ggml-large-v3.bin"),
                "error context must name the requested file, got: {display}"
            );
            assert!(
                !display.contains("ggml-base.bin"),
                "error must not mention the lex-sorted base file, got: {display}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn from_id_rejects_symlinked_model_file() {
    let tmp = build_cache_with_symlinked_bin();
    let result = BridgeTranscriber::from_id(
        "ggerganov/whisper.cpp/ggml-large-v3.bin",
        EngineFamily::Whisper,
        Some(tmp.path().to_path_buf()),
        None,
    )
    .await;
    let err = match result {
        Ok(_) => {
            panic!("BridgeTranscriber must refuse a symlinked model file before any engine load")
        }
        Err(e) => e,
    };
    let display = format!("{err:#}");
    assert!(
        display.contains("symlink"),
        "symlink-refusal error must say `symlink`, got: {display}"
    );
    // And it must point at the offending path so the operator can
    // investigate.
    assert!(
        display.contains("ggml-large-v3.bin"),
        "symlink-refusal error must name the offending file, got: {display}"
    );
}
