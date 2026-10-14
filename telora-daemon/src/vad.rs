//! Energy gate in front of the engines. Whisper invents subtitle-like
//! text ("Gracias por ver el video.") from silence or room noise, so a
//! recording without speech never reaches it, and the silence around
//! the speech is cut off.

use std::ops::Range;

use log::info;
use telora_common::protocol::Transcript;

use crate::transcriber::Transcriber;

const RATE_HZ: usize = 16_000;
/// 30 ms.
const FRAME: usize = 480;
/// Speech must stand this far (+8 dB) over the recording's own noise
/// floor: on the reference host the floor moves 10 dB with the
/// machine's load, so no fixed level separates noise from speech.
const OVER_FLOOR: f32 = 2.5;
/// Quieter frames are never speech, even over digital silence.
const MIN_SPEECH_RMS: f32 = 0.003;
/// A floor above this comes from a recording with no pause at all, and
/// must not lift the bar over its own words.
const MAX_FLOOR_RMS: f32 = 0.01;
/// A key click fills at most two frames; a vowel lasts longer.
const MIN_RUN_FRAMES: usize = 3;
/// Soft onsets and word endings stay under the threshold.
const PAD_BEFORE: usize = RATE_HZ * 300 / 1000;
const PAD_AFTER: usize = RATE_HZ * 500 / 1000;

/// The part of `audio` (16 kHz mono) worth transcribing, or `None`
/// when it holds no speech.
pub fn speech_span(audio: &[f32]) -> Option<Range<usize>> {
    let levels: Vec<f32> = audio.chunks(FRAME).map(rms).collect();
    let threshold = threshold(&levels);
    let mut first_frame = None;
    let mut end_frame = 0;
    let mut run = 0;
    for (i, &level) in levels.iter().enumerate() {
        if level < threshold {
            run = 0;
            continue;
        }
        run += 1;
        if run >= MIN_RUN_FRAMES {
            first_frame.get_or_insert(i + 1 - run);
            end_frame = i + 1;
        }
    }
    let start = (first_frame? * FRAME).saturating_sub(PAD_BEFORE);
    let end = (end_frame * FRAME + PAD_AFTER).min(audio.len());
    Some(start..end)
}

/// Transcribe only the speech in `audio`; the engine is not called when
/// there is none.
pub fn transcribe_speech(
    engine: &dyn Transcriber,
    audio: &[f32],
    language: &str,
) -> anyhow::Result<Transcript> {
    let Some(span) = speech_span(audio) else {
        let levels: Vec<f32> = audio.chunks(FRAME).map(rms).collect();
        let loudest = levels.iter().copied().fold(0.0, f32::max);
        info!(
            "No speech in {:.1} s (loudest frame RMS {loudest:.4}, threshold {:.4}); not transcribing",
            seconds(audio.len()),
            threshold(&levels)
        );
        return Ok(Transcript::NoSpeech);
    };
    info!(
        "Speech at {:.1}-{:.1} s of {:.1} s; transcribing that part",
        seconds(span.start),
        seconds(span.end),
        seconds(audio.len())
    );
    engine
        .transcribe(&audio[span], Some(language))
        .map(Transcript::Text)
}

/// Speech level for a recording whose frames have these RMS levels,
/// taking its 10th percentile as the noise floor.
fn threshold(levels: &[f32]) -> f32 {
    let mut sorted = levels.to_vec();
    sorted.sort_unstable_by(f32::total_cmp);
    let floor = sorted.get(sorted.len() / 10).copied().unwrap_or(0.0);
    (floor.min(MAX_FLOOR_RMS) * OVER_FLOOR).max(MIN_SPEECH_RMS)
}

fn rms(frame: &[f32]) -> f32 {
    (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt()
}

fn seconds(samples: usize) -> f32 {
    samples as f32 / RATE_HZ as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const RATE: usize = 16_000;

    /// Deterministic white noise of the given RMS.
    fn noise(samples: usize, rms: f32) -> Vec<f32> {
        let amplitude = rms * 3f32.sqrt();
        let mut state: u32 = 0x1234_5678;
        (0..samples)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state as f32 / u32::MAX as f32 * 2.0 - 1.0) * amplitude
            })
            .collect()
    }

    /// The reference room with the machine idle (Fifine, 30 ms frames:
    /// p50 0.0015, max 0.0021).
    fn room_noise(samples: usize) -> Vec<f32> {
        noise(samples, 0.002)
    }

    /// The same room with the machine under load (p50 0.005, max 0.0067).
    fn busy_machine_noise(samples: usize) -> Vec<f32> {
        noise(samples, 0.0055)
    }

    /// Overwrite `range` with a 200 Hz tone of the given RMS, as a
    /// stand-in for a voiced sound.
    fn voice(audio: &mut [f32], range: Range<usize>, rms: f32) {
        for (i, s) in audio[range].iter_mut().enumerate() {
            *s = rms
                * 2f32.sqrt()
                * (i as f32 * 2.0 * std::f32::consts::PI * 200.0 / RATE as f32).sin();
        }
    }

    fn ms(ms: usize) -> usize {
        RATE * ms / 1000
    }

    #[test]
    fn room_noise_is_not_speech() {
        assert_eq!(speech_span(&room_noise(ms(4000))), None);
    }

    // The noise floor rose 10 dB in an hour on the reference host; a
    // fixed threshold let Whisper turn it into "¡Hola!".
    #[test]
    fn the_noise_of_a_busy_machine_is_not_speech() {
        assert_eq!(speech_span(&busy_machine_noise(ms(4000))), None);
    }

    #[test]
    fn speech_over_the_noise_of_a_busy_machine_is_found() {
        let mut audio = busy_machine_noise(ms(4000));
        voice(&mut audio, 40 * 480..80 * 480, 0.03);
        assert_eq!(
            speech_span(&audio),
            Some(40 * 480 - ms(300)..80 * 480 + ms(500))
        );
    }

    #[test]
    fn digital_silence_and_empty_audio_are_not_speech() {
        assert_eq!(speech_span(&vec![0.0; ms(3000)]), None);
        assert_eq!(speech_span(&[]), None);
    }

    // The START and STOP hotkeys click into the microphone; each click
    // can straddle two frames.
    #[test]
    fn key_clicks_are_not_speech() {
        let mut audio = room_noise(ms(3000));
        voice(&mut audio, 470..470 + ms(20), 0.2);
        voice(&mut audio, 40_310..40_310 + ms(20), 0.2);
        assert_eq!(speech_span(&audio), None);
    }

    // A one-word answer ("sí", "no") is shorter than voxora-vad's
    // 250 ms debounce, which drops it.
    #[test]
    fn a_short_word_is_speech() {
        let mut audio = room_noise(ms(3000));
        voice(&mut audio, ms(1000)..ms(1150), 0.05);
        assert!(speech_span(&audio).is_some());
    }

    #[test]
    fn speech_20_db_below_normal_is_still_speech() {
        let mut audio = room_noise(ms(3000));
        voice(&mut audio, ms(1000)..ms(2000), 0.01);
        assert!(speech_span(&audio).is_some());
    }

    #[test]
    fn the_span_keeps_300_ms_before_and_500_ms_after_the_speech() {
        let mut audio = room_noise(ms(4000));
        voice(&mut audio, 40 * 480..80 * 480, 0.05);
        assert_eq!(
            speech_span(&audio),
            Some(40 * 480 - ms(300)..80 * 480 + ms(500))
        );
    }

    #[test]
    fn the_span_is_clamped_to_the_recording() {
        let mut audio = room_noise(ms(2000));
        let len = audio.len();
        voice(&mut audio, 0..len, 0.05);
        assert_eq!(speech_span(&audio), Some(0..len));
    }

    // The STOP click comes after the user went quiet; keeping the
    // silence before it is what makes Whisper invent a closing line.
    #[test]
    fn a_click_after_the_speech_does_not_extend_the_span() {
        let mut audio = room_noise(ms(5000));
        voice(&mut audio, 40 * 480..80 * 480, 0.05);
        voice(&mut audio, ms(4500)..ms(4520), 0.2);
        assert_eq!(speech_span(&audio).map(|s| s.end), Some(80 * 480 + ms(500)));
    }

    /// Records what it was asked to transcribe.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<usize>>);

    impl Transcriber for Recorder {
        fn transcribe(&self, audio: &[f32], _language: Option<&str>) -> anyhow::Result<String> {
            self.0.lock().unwrap().push(audio.len());
            Ok("hola".to_string())
        }
    }

    #[test]
    fn audio_without_speech_never_reaches_the_engine() {
        let engine = Recorder::default();
        let result = transcribe_speech(&engine, &room_noise(ms(4000)), "es").unwrap();
        assert_eq!(result, Transcript::NoSpeech);
        assert!(engine.0.lock().unwrap().is_empty());
    }

    #[test]
    fn the_engine_hears_only_the_speech_span() {
        let engine = Recorder::default();
        let mut audio = room_noise(ms(6000));
        voice(&mut audio, 40 * 480..80 * 480, 0.05);
        let result = transcribe_speech(&engine, &audio, "es").unwrap();
        assert_eq!(result, Transcript::Text("hola".to_string()));
        assert_eq!(
            *engine.0.lock().unwrap(),
            vec![80 * 480 + ms(500) - (40 * 480 - ms(300))]
        );
    }
}
