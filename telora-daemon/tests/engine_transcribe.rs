//! Each engine must transcribe with the options the daemon sends.
//!
//! Needs the model in the voxora cache (it is downloaded otherwise),
//! so it is ignored by default:
//!
//! ```text
//! cargo test -p telora-daemon --no-default-features --test engine_transcribe -- --ignored
//! ```

use telora_daemon::{BridgeTranscriber, Transcriber, default_voxora_cache_dir};
use voxora_bridge::EngineFamily;

/// Two seconds of a 440 Hz tone at 16 kHz: enough to run the full
/// inference path; the text itself is irrelevant.
fn tone() -> Vec<f32> {
    #[allow(clippy::cast_precision_loss)]
    (0..32_000)
        .map(|i| 0.1 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 16_000.0).sin())
        .collect()
}

// voxora-qwen3asr rejects `timestamps: true` with
// `Unsupported("timestamps")` since 0.6.3.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Qwen/Qwen3-ASR-0.6B in the voxora cache"]
async fn qwen3asr_transcribes_with_daemon_options() {
    let cache = default_voxora_cache_dir().expect("voxora cache dir");
    let transcriber = BridgeTranscriber::from_id(
        "Qwen/Qwen3-ASR-0.6B",
        EngineFamily::Qwen3Asr,
        Some(cache),
        None,
    )
    .await
    .expect("load Qwen3-ASR");
    let result = tokio::task::spawn_blocking(move || transcriber.transcribe(&tone(), Some("es")))
        .await
        .expect("join");
    assert!(
        result.is_ok(),
        "transcribe failed: {:#}",
        result.unwrap_err()
    );
}
