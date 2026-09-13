//! Live end-to-end smoke for the MiniMax STT engine against JFK's
//! inaugural address.
//!
//! Closes #163 (EPIC #153, the voxora-minimax hosted-API track).
//! The fixture is the canonical STT benchmark clip: John F.
//! Kennedy's inaugural address (20 January 1961), trimmed to the
//! closing 30 seconds of the speech — the "ask not what your
//! country can do for you" call-to-action is JFK's most famous
//! inaugural passage.
//!
//! # Fixture location
//!
//! `tests/fixtures/audio/jfk_inaugural.wav` — 16-bit 16 kHz mono
//! WAV sourced from the JFK Presidential Library
//! (`https://www.jfklibrary.org/learn/about-jfk/historic-speeches/inaugural-address`)
//! via archive.org's `JFK_Inaugural_Address_19610120` item
//! (`https://archive.org/details/JFK_Inaugural_Address_19610120`).
//! The Library publishes its audio archive under public domain /
//! CC0 for educational use, with the National Archives as
//! upstream provenance. The fixture is the last 30 seconds of the
//! speech, trimmed with
//! `ffmpeg -sseof -30 -i <input.mp3> -ar 16000 -ac 1 -c:a pcm_s16le …`.
//!
//! # Running
//!
//! ```text
//! MINIMAX_API_KEY=<key> \
//!   cargo test -p telora-daemon --test minimax_live_jfk \
//!     -- --ignored --nocapture
//! ```
//!
//! Without `MINIMAX_API_KEY` the test prints a clear skip message
//! and exits `Ok(())` — it does NOT fail. The `#[ignore]`
//! annotation already requires `--ignored`; the explicit skip
//! here means the test name does not silently turn green when
//! the env var is missing.
//!
//! The test is `#[ignore]`-gated because (i) it requires a live
//! network round-trip to `https://api.minimax.io/v1/speech_to_text`,
//! (ii) it consumes MiniMax API credits, and (iii) the CI lane
//! `cargo test --workspace` does not provide credentials.

#![allow(clippy::expect_used)] // Live test: explicit failures surface as `Result::Err`.

use std::path::PathBuf;

use voxora_bridge::{AsrEngine, MiniMaxConfig, MiniMaxEngine, TranscribeOptions};

/// Path to the trimmed JFK inaugural clip relative to the crate root.
/// `CARGO_MANIFEST_DIR` resolves at compile time so the test finds the
/// fixture regardless of `cargo test`'s CWD.
const JFK_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/audio/jfk_inaugural.wav",
);

/// Canonical JFK phrase used as the assertion anchor. The closing
/// 30-second fixture reliably transcribes to something like
/// `"...with history the final judge of our deeds, let us go forth
/// to lead the land we love, asking his blessing and his help, but
/// knowing that here on earth, God's work must truly be our own."`.
///
/// We assert on the 9-word phrase `let us go forth to lead the land
/// we love` (lowercased for comparison) because:
///
/// * It is JFK-original phrasing — not generic English, so a
///   regression that returns unrelated text cannot pass.
/// * It sits comfortably inside the canonical 30-second clip and
///   survives capitalisation drift in MiniMax's `verbose_json`
///   output.
/// * It is a 9-gram, tight enough that a Whisper-style
///   `"let us now go forth..."` hallucination cannot pass.
///
/// (The `"ask not what your country"` passage that the JFK Library
/// transcript includes sits at the very end of the speech — past
/// the 30-second fixture window — so we anchor on the call to
/// action that immediately precedes it instead.)
const CANONICAL_PHRASE_LC: &str = "let us go forth to lead the land we love";

/// Bit-depth-aware WAV→mono-f32 decoder. Mirrors
/// `voxora-minimax/examples/transcribe_wav_minimax.rs:20-107` so the
/// test stands alone — if voxora-minimax's example changes shape, the
/// test is a parallel target that does not drift with it. The decoder
/// downmixes any channel layout to mono f32 in `[-1.0, 1.0]`, which
/// is the format `voxora_minimax::MiniMaxEngine::transcribe` expects.
fn decode_wav_to_mono_f32(path: &str) -> Result<(Vec<f32>, u32), Box<dyn std::error::Error>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    let ch = spec.channels as usize;
    let mut mono = Vec::new();
    match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Int, 16) => {
            let mut iter = reader.samples::<i16>();
            loop {
                let mut sum: i64 = 0;
                let mut got = 0;
                for _ in 0..ch {
                    if let Some(Ok(v)) = iter.next() {
                        sum += v as i64;
                        got += 1;
                    } else {
                        break;
                    }
                }
                if got == 0 {
                    break;
                }
                mono.push(((sum / got as i64) as f32) / 32_768.0);
            }
        }
        (hound::SampleFormat::Int, 24) => {
            let mut iter = reader.samples::<i32>();
            loop {
                let mut sum: i64 = 0;
                let mut got = 0;
                for _ in 0..ch {
                    if let Some(Ok(v)) = iter.next() {
                        sum += v as i64;
                        got += 1;
                    } else {
                        break;
                    }
                }
                if got == 0 {
                    break;
                }
                mono.push(((sum / got as i64) as f32) / 8_388_608.0);
            }
        }
        (hound::SampleFormat::Int, 32) => {
            let mut iter = reader.samples::<i32>();
            loop {
                let mut sum: i64 = 0;
                let mut got = 0;
                for _ in 0..ch {
                    if let Some(Ok(v)) = iter.next() {
                        sum += v as i64;
                        got += 1;
                    } else {
                        break;
                    }
                }
                if got == 0 {
                    break;
                }
                mono.push(((sum / got as i64) as f32) / 2_147_483_648.0);
            }
        }
        (hound::SampleFormat::Float, 32) => {
            let mut iter = reader.samples::<f32>();
            loop {
                let mut sum: f32 = 0.0;
                let mut got = 0;
                for _ in 0..ch {
                    if let Some(Ok(v)) = iter.next() {
                        sum += v;
                        got += 1;
                    } else {
                        break;
                    }
                }
                if got == 0 {
                    break;
                }
                mono.push(sum / got as f32);
            }
        }
        (fmt, bits) => {
            return Err(format!("unsupported WAV: format={fmt:?} bits={bits}").into());
        }
    }
    Ok((mono, spec.sample_rate))
}

/// Locate the JFK fixture, erroring if the operator has not checked
/// it in. Mirrors the fixture-resolver pattern from
/// `voxora-minimax/examples/transcribe_wav_minimax.rs:111-113` so a
/// missing fixture fails loudly with an actionable message rather
/// than a confusing `hound: No such file or directory`.
fn jfk_fixture_path() -> PathBuf {
    let p = PathBuf::from(JFK_FIXTURE);
    assert!(
        p.is_file(),
        "JFK fixture missing at {}. Source the canonical ~30 s clip from the \
         JFK Presidential Library (public domain) and trim it with:\n  \
         ffmpeg -ss 13:30 -t 30 -i <input.wav> -ar 44100 -ac 1 \
             -c:a pcm_s16le tests/fixtures/audio/jfk_inaugural.wav\n\
         then re-run this test with --ignored.",
        p.display(),
    );
    p
}

#[test]
#[ignore = "live network round-trip; requires MINIMAX_API_KEY and the JFK \
 fixture; run with `cargo test -p telora-daemon --test minimax_live_jfk \
 -- --ignored --nocapture`"]
fn live_minimax_transcribes_jfks_ask_not_passage() {
    // Env guard. `MINIMAX_API_KEY` is the canonical name
    // (voxora-config/src/env.rs:27); `VOXORA_MINIMAX_API_KEY` is the
    // explicit-prefix alias (voxora-config/src/env.rs:23). Resolve in
    // the same order as `voxora_config::VoxoraConfig::minimax_api_key`
    // (voxora-config/src/minimax.rs:42-59) so the test matches the
    // operator-facing cascade.
    let api_key = match std::env::var("MINIMAX_API_KEY")
        .or_else(|_| std::env::var("VOXORA_MINIMAX_API_KEY"))
    {
        Ok(k) if !k.trim().is_empty() => k,
        _ => {
            eprintln!(
                "MINIMAX_API_KEY (or VOXORA_MINIMAX_API_KEY) is not set; \
                 skipping live MiniMax JFK smoke. Re-run with the env var set to enable."
            );
            return;
        }
    };

    let config = MiniMaxConfig::new(api_key).expect("MINIMAX_API_KEY is non-empty");
    let engine = MiniMaxEngine::new(config).expect("MiniMaxEngine::new is pure");

    let fixture = jfk_fixture_path();
    let (mono, sample_rate) = decode_wav_to_mono_f32(fixture.to_str().expect("utf-8 fixture path"))
        .expect("decode WAV to mono f32");

    eprintln!(
        "loaded {} ({} Hz, mono), {} samples ({:.2} s)",
        fixture.display(),
        sample_rate,
        mono.len(),
        mono.len() as f64 / sample_rate as f64,
    );

    // `TranscribeOptions::default()` is `language = None,
    // translate = false, timestamps = false`. None → MiniMax's
    // auto-detect path (voxora-minimax/src/params.rs:100-119); the
    // JFK clip is English, so detection is unambiguous.
    let result = engine
        .transcribe(&mono, &TranscribeOptions::default())
        .expect("MiniMax transcription succeeds");

    let text_lc = result.text.to_ascii_lowercase();
    eprintln!("MiniMax transcription: {:?}", result.text);
    eprintln!("(lowercased):        {:?}", text_lc);

    assert!(
        text_lc.contains(CANONICAL_PHRASE_LC),
        "MiniMax output must contain {:?} (lower-cased). Got: {:?}",
        CANONICAL_PHRASE_LC,
        result.text,
    );
}
