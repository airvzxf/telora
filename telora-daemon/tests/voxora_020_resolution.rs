//! End-to-end regression for `airvzxf/telora#79` against the
//! voxora 0.2.0 resolver.
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
//! voxora 0.2.0 ships two independent fixes:
//!
//! 1. `voxora-hf::resolve_single_file` no longer trusts the
//!    `.complete` marker blindly — if the marker is set but the
//!    requested file is missing, it returns `AsrError::ModelNotFound`
//!    instead of silently returning the directory.
//! 2. `ModelDir::entry: Option<PathBuf>` names the exact file the
//!    caller asked for in 3-segment `org/repo/file` resolves, so the
//!    daemon no longer has to scan the directory at all.
//!
//! These tests pin both behaviours against a hand-rolled cache
//! layout. They run by default (no `#[ignore]`) and use only a few
//! bytes per stub file — the file contents do not matter, only the
//! names and the `.complete` marker shape.
//!
//! Run with:
//!
//! ```text
//! cargo test -p telora-daemon --test voxora_020_resolution -- --nocapture
//! ```

use std::sync::Arc;

use tempfile::TempDir;
use voxora_bridge::{AsrError, EngineFamily, HuggingFaceSource, ModelSource, ResolveOptions};
use voxora_registry::{ModelId, Registry, RegistryHfExt};

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
/// plus a `Registry` that uses it. Returns both because every test
/// here needs the source for `capabilities_for` sanity checks and
/// the registry for the canonical resolve path.
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
        .expect_err("registry must propagate the ModelNotFound from voxora-hf");

    // The registry wraps the source error in RegistryError::Parse;
    // the underlying cause is `AsrError::ModelNotFound("...is marked
    // complete but does not contain...")`. We surface that to
    // `voxora_core::AsrError::ModelNotFound` via the source's own
    // resolve so the assertion can be precise.
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

    // Just smoke-test the registry error shape — it must be a
    // RegistryError (not ModelNotFound directly) since the
    // registry today wraps source failures.
    let _ = registry_err;
}
