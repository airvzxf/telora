use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{error, info};
use ringbuf::{HeapRb, Producer};
use std::sync::Arc;

use crate::socket::AudioConfig;

/// Sample rate the downstream ASR (voxora-bridge `AsrEngine::transcribe`)
/// and the daemon's recording buffer are sized for. The capture stream
/// may run at a different native rate (e.g. Fifine's 48 kHz via direct
/// ALSA); the audio thread downsamples to this rate so the ring buffer
/// always holds audio at the rate the rest of the pipeline expects.
///
/// Per `voxora-traits/src/engine.rs`: "Transcribe a buffer of mono PCM
/// samples at the engine's expected sample rate (typically 16 kHz, f32
/// in [-1.0, 1.0])". The daemon's recording cap (`16000 *
/// max_recording_seconds`) and MiniMax's WAV header also assume this
/// rate, so the buffer MUST hold samples at this rate regardless of
/// capture rate.
const WHISPER_RATE_HZ: u32 = 16000;

/// Push one mono frame into the ring buffer, averaging every
/// `decimation_factor` consecutive samples first when the capture rate
/// exceeds the target rate (i.e. `decimation_factor > 1`). The scratch
/// buffer carries any unaligned leftovers across callbacks so a partial
/// window at the end of one callback is completed by the next, instead
/// of being silently dropped.
///
/// `decimation_factor <= 1` is a passthrough (capture already at 16 kHz
/// or below).
fn push_mono_decimated(
    mono_frame: f32,
    producer: &mut Producer<f32, Arc<HeapRb<f32>>>,
    scratch: &mut Vec<f32>,
    decimation_factor: usize,
) {
    if decimation_factor <= 1 {
        let _ = producer.push(mono_frame);
        return;
    }
    scratch.push(mono_frame);
    if scratch.len() >= decimation_factor {
        // Average exactly `decimation_factor` samples and reset the
        // scratch. `drain(..)` consumes the elements; we divide by the
        // configured factor (not `scratch.len()` post-drain, which is
        // already 0).
        let avg: f32 = scratch.drain(..).sum::<f32>() / decimation_factor as f32;
        let _ = producer.push(avg);
    }
}

pub struct AudioEngine {
    stream: Option<cpal::Stream>,
}

impl AudioEngine {
    pub fn new() -> Result<Self> {
        Ok(Self { stream: None })
    }

    pub fn start(
        &mut self,
        mut producer: Producer<f32, Arc<HeapRb<f32>>>,
        config: &AudioConfig,
    ) -> Result<u32> {
        let host = cpal::default_host();

        let device = select_input_device(&host, config).context("no usable input device")?;

        info!(
            "Using input device: {}",
            device.name().unwrap_or("Unknown".to_string())
        );

        // Pick a sane mono config from `supported_input_configs()`.
        // The naive `default_input_config()` lies on hosts where ALSA
        // "default" routes through the PipeWire ALSA plugin (returns
        // 2 ch / 44100 Hz / F32 even for a mono USB mic, and Fifine
        // then rejects the 2-channel negotiation — `stream.play()`
        // hangs forever). Walk a deterministic preference list:
        // signed / wider sample formats first (Fifine is s24le,
        // surfaced as I32 by cpal's ALSA backend because 24-bit audio
        // is stored in 32-bit containers), 48000 Hz first (Fifine's
        // HW-native rate via ALSA), then common alternates. Only fall
        // back to `default_input_config()` when no mono config exists
        // at all.
        const RATE_PREFERENCE_HZ: &[u32] = &[48000, 44100, 32000, 22050, 16000];
        const FORMAT_PREFERENCE: &[cpal::SampleFormat] = &[
            cpal::SampleFormat::I32,
            cpal::SampleFormat::I16,
            cpal::SampleFormat::F32,
            cpal::SampleFormat::U8,
        ];

        let supported_configs: Vec<cpal::SupportedStreamConfigRange> = device
            .supported_input_configs()
            .context("Failed to enumerate supported input configs")?
            .collect();

        let pick_mono = |fmt: cpal::SampleFormat| -> Option<&cpal::SupportedStreamConfigRange> {
            supported_configs
                .iter()
                .find(|c| c.channels() == 1 && c.sample_format() == fmt)
        };

        let chosen = FORMAT_PREFERENCE
            .iter()
            .find_map(|&fmt| pick_mono(fmt).map(|cfg| (cfg, fmt)));

        let default_config = device
            .default_input_config()
            .context("Failed to get default input config")?;

        let (actual_config, sample_format) = if let Some((cfg, _fmt)) = chosen {
            let chosen_rate = RATE_PREFERENCE_HZ
                .iter()
                .copied()
                .find(|&r| cfg.min_sample_rate().0 <= r && cfg.max_sample_rate().0 >= r)
                .unwrap_or(cfg.max_sample_rate().0);
            info!(
                "Using mono device config (1 ch, {} Hz, format {:?}, range {}-{} Hz)...",
                chosen_rate,
                cfg.sample_format(),
                cfg.min_sample_rate().0,
                cfg.max_sample_rate().0,
            );
            let stream_cfg = cfg
                .with_sample_rate(cpal::SampleRate(chosen_rate))
                .config()
                .clone();
            (stream_cfg, cfg.sample_format())
        } else {
            info!("Using default device config (no mono config found, last resort)...");
            (
                default_config.config().clone(),
                default_config.sample_format(),
            )
        };

        let sample_rate = actual_config.sample_rate.0;
        let channels = actual_config.channels;

        info!("Input config: {:?}", actual_config);

        // How many capture frames collapse into one ring-buffer sample.
        // 48 kHz capture → factor 3 → 16 kHz buffer. 16 kHz capture →
        // factor 1 (passthrough). Below 16 kHz capture is degenerate
        // (would force factor 0); the config preference list keeps that
        // from happening, but defend against it explicitly.
        let decimation_factor: usize = if sample_rate >= WHISPER_RATE_HZ {
            (sample_rate / WHISPER_RATE_HZ) as usize
        } else {
            1
        };
        info!(
            "Audio thread: capture {} Hz, downsampling to {} Hz (decimation factor {})",
            sample_rate, WHISPER_RATE_HZ, decimation_factor
        );

        let err_fn = |err| error!("an error occurred on stream: {}", err);

        let stream = match sample_format {
            cpal::SampleFormat::F32 => {
                let mut scratch: Vec<f32> = Vec::new();
                device.build_input_stream(
                    &actual_config,
                    move |data: &[f32], _: &_| {
                        // Downmix: si hay más de 1 canal, promediamos.
                        for frame in data.chunks(channels as usize) {
                            let sum: f32 = frame.iter().sum();
                            let mono = sum / channels as f32;
                            push_mono_decimated(
                                mono,
                                &mut producer,
                                &mut scratch,
                                decimation_factor,
                            );
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            cpal::SampleFormat::I16 => {
                let mut scratch: Vec<f32> = Vec::new();
                device.build_input_stream(
                    &actual_config,
                    move |data: &[i16], _: &_| {
                        for frame in data.chunks(channels as usize) {
                            let sum: f32 = frame.iter().map(|&s| s as f32 / i16::MAX as f32).sum();
                            let mono = sum / channels as f32;
                            push_mono_decimated(
                                mono,
                                &mut producer,
                                &mut scratch,
                                decimation_factor,
                            );
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            cpal::SampleFormat::U16 => {
                let mut scratch: Vec<f32> = Vec::new();
                device.build_input_stream(
                    &actual_config,
                    move |data: &[u16], _: &_| {
                        for frame in data.chunks(channels as usize) {
                            let sum: f32 = frame
                                .iter()
                                .map(|&s| {
                                    (s as f32 - u16::MAX as f32 / 2.0) / (u16::MAX as f32 / 2.0)
                                })
                                .sum();
                            let mono = sum / channels as f32;
                            push_mono_decimated(
                                mono,
                                &mut producer,
                                &mut scratch,
                                decimation_factor,
                            );
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            cpal::SampleFormat::U8 => {
                let mut scratch: Vec<f32> = Vec::new();
                device.build_input_stream(
                    &actual_config,
                    move |data: &[u8], _: &_| {
                        for frame in data.chunks(channels as usize) {
                            let sum: f32 = frame
                                .iter()
                                .map(|&s| {
                                    (s as f32 - u8::MAX as f32 / 2.0) / (u8::MAX as f32 / 2.0)
                                })
                                .sum();
                            let mono = sum / channels as f32;
                            push_mono_decimated(
                                mono,
                                &mut producer,
                                &mut scratch,
                                decimation_factor,
                            );
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            cpal::SampleFormat::I32 => {
                let mut scratch: Vec<f32> = Vec::new();
                device.build_input_stream(
                    &actual_config,
                    move |data: &[i32], _: &_| {
                        // Fifine is s24le surfaced as I32 by cpal's ALSA
                        // backend (24-bit audio stored in 32-bit
                        // containers). Normalise against 2^31 to stay
                        // inside f32's [-1.0, 1.0] range.
                        for frame in data.chunks(channels as usize) {
                            let sum: f32 = frame.iter().map(|&s| s as f32 / i32::MAX as f32).sum();
                            let mono = sum / channels as f32;
                            push_mono_decimated(
                                mono,
                                &mut producer,
                                &mut scratch,
                                decimation_factor,
                            );
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            _ => return Err(anyhow!("Unsupported sample format")),
        };

        stream.play().context("Failed to start audio stream")?;

        self.stream = Some(stream);

        Ok(sample_rate)
    }
}

/// Pick the capture device according to the operator's `[audio]`
/// settings.
///
/// Precedence (highest wins):
///
/// 1. `config.input_device` — case-insensitive substring match
///    against `Device::name()`. Empty skips this branch.
/// 2. `cpal::default_input_device()` — the desktop default
///    (PipeWire / PulseAudio on KDE Plasma 6, GNOME, etc.).
///    `prefer_direct_alsa = true` overrides only this step: when
///    the default device is the ALSA plugin (name == `"default"`),
///    the helper falls back to the first direct ALSA HW device with
///    `"CARD="` in its name.
///
/// This used to swap the system default unconditionally whenever the
/// ALSA plugin was in play. That heuristic predated modern PipeWire
/// and mis-selected laptops with both an integrated Intel HDA mic
/// and a USB capture card (cpal enumerated the built-in card first,
/// so telora captured from the laptop's internal mic while the
/// operator spoke into the USB one). Honours the desktop default
/// now; the legacy fallback is opt-in via `prefer_direct_alsa` and
/// the pin via `input_device`.
pub fn select_input_device(host: &cpal::Host, config: &AudioConfig) -> Result<cpal::Device> {
    let pin = config.input_device.trim();
    if !pin.is_empty() {
        let needle = pin.to_lowercase();
        let picked = host.input_devices().ok().and_then(|it| {
            it.into_iter().find(|d| {
                d.name()
                    .ok()
                    .map(|n| n.to_lowercase().contains(&needle))
                    .unwrap_or(false)
            })
        });
        return match picked {
            Some(d) => {
                info!(
                    "audio.input_device '{}' matched capture device '{}'",
                    pin,
                    d.name().unwrap_or_default()
                );
                Ok(d)
            }
            None => Err(anyhow!(
                "audio.input_device '{}' did not match any capture device",
                pin
            )),
        };
    }

    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("No input device found"))?;

    let name = device.name().unwrap_or_default();
    if config.prefer_direct_alsa && name == "default" {
        let replacement = host.input_devices().ok().and_then(|it| {
            it.into_iter()
                .find(|d| d.name().ok().map(|n| n.contains("CARD=")).unwrap_or(false))
        });
        if let Some(d) = replacement {
            info!(
                "audio.prefer_direct_alsa=true: bypassing ALSA plugin 'default' for direct capture on '{}'",
                d.name().unwrap_or_default()
            );
            return Ok(d);
        }
        info!(
            "audio.prefer_direct_alsa=true but no direct ALSA 'CARD=' device found; falling back to '{}'",
            name
        );
    }
    Ok(device)
}

#[cfg(test)]
mod tests {
    //! Exercise the audio device-selection helper against the live
    //! `cpal::default_host()`. The tests do not need an actual
    //! capture stream — only the device-name surface cpal exposes —
    //! which makes them cheap and deterministic on whatever ALSA /
    //! PipeWire setup the CI runner happens to use.

    use super::*;
    use crate::socket::AudioConfig;

    #[test]
    fn empty_config_respects_system_default() {
        let host = cpal::default_host();
        let cfg = AudioConfig::default();
        let picked = select_input_device(&host, &cfg).expect("default device available");
        let picked_name = picked.name().unwrap_or_default();
        // With `prefer_direct_alsa=false` and an empty `input_device`,
        // the helper MUST return exactly what `cpal::default_input_device()`
        // returns — the desktop default, even when a `CARD=` device
        // exists alongside it. The historical bug was overriding the
        // desktop default with the first `CARD=` device; this test
        // pins the new "respect the desktop default" contract.
        let system_default_name = host
            .default_input_device()
            .and_then(|d| d.name().ok())
            .unwrap_or_default();
        assert_eq!(
            picked_name, system_default_name,
            "select_input_device silently overrode the desktop default \
             '{system_default_name}' with '{picked_name}' even though \
             prefer_direct_alsa=false; check the precedence rules."
        );
    }

    #[test]
    fn input_device_pin_matches_case_insensitively() {
        let host = cpal::default_host();
        let names: Vec<String> = host
            .input_devices()
            .ok()
            .map(|it| it.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default();
        let some_candidate = names
            .iter()
            .find(|n| n.contains("CARD="))
            .cloned()
            .or_else(|| names.first().cloned());
        let Some(target) = some_candidate else {
            // No input devices on this host at all — `cpal` can't
            // enumerate anything. Skip: nothing to match against.
            return;
        };

        // Take the first `CARD=` chunk from the device name and
        // search for it; the match is case-insensitive.
        let needle: String = target
            .split("CARD=")
            .nth(1)
            .and_then(|rest| rest.split([',', ' ']).next())
            .map(|s| s.to_lowercase())
            .unwrap_or_else(|| target.to_lowercase());
        let cfg = AudioConfig {
            input_device: needle.clone(),
            prefer_direct_alsa: false,
        };
        let picked = select_input_device(&host, &cfg).expect("device matching the pin");
        assert!(
            picked
                .name()
                .ok()
                .map(|n| n.to_lowercase().contains(&needle))
                .unwrap_or(false),
            "picked device '{}' did not contain pin '{}'",
            picked.name().unwrap_or_default(),
            needle
        );
    }

    #[test]
    fn input_device_pin_with_no_match_returns_error() {
        let host = cpal::default_host();
        let cfg = AudioConfig {
            input_device: "definitely-not-a-real-device-xyz".to_string(),
            prefer_direct_alsa: false,
        };
        assert!(
            select_input_device(&host, &cfg).is_err(),
            "expected an error when the pin matches no capture device"
        );
    }
}
