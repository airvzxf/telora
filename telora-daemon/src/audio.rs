use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{error, info};
use ringbuf::{Consumer, HeapRb, Producer};
use std::sync::Arc;

use crate::daemon::AudioSource;
use crate::resample::MonoResampler;
use crate::socket::AudioConfig;

/// Rate the engines expect and the recording buffer is sized for; the
/// capture runs at whatever the device offers and is resampled to this.
const TARGET_RATE_HZ: u32 = 16000;

/// Samples the capture callback can queue before the daemon drains them
/// (it drains every 20 ms).
const QUEUE_SAMPLES: usize = TARGET_RATE_HZ as usize * 30;

/// The capture device, open only while recording so the desktop's
/// microphone indicator is off the rest of the time.
pub struct Microphone {
    config: AudioConfig,
    capture: Option<Capture>,
}

struct Capture {
    // Dropping the stream closes the device.
    _stream: cpal::Stream,
    queue: Consumer<f32, Arc<HeapRb<f32>>>,
}

impl Microphone {
    pub fn new(config: AudioConfig) -> Self {
        Self {
            config,
            capture: None,
        }
    }
}

impl AudioSource for Microphone {
    fn start(&mut self) -> Result<(), String> {
        self.capture = None;
        let capture = open_capture(&self.config).map_err(|e| format!("{e:#}"))?;
        self.capture = Some(capture);
        Ok(())
    }

    fn stop(&mut self) {
        self.capture = None;
    }

    fn drain_into(&mut self, buffer: &mut Vec<f32>, limit: usize) {
        let Some(capture) = &mut self.capture else {
            return;
        };
        while buffer.len() < limit {
            match capture.queue.pop() {
                Some(sample) => buffer.push(sample),
                None => break,
            }
        }
    }
}

fn open_capture(config: &AudioConfig) -> Result<Capture> {
    let host = cpal::default_host();
    let device = select_input_device(&host, config).context("no usable input device")?;
    info!(
        "Using input device: {}",
        device.name().unwrap_or("Unknown".to_string())
    );
    let (stream_config, sample_format) = choose_stream_config(&device)?;
    info!("Input config: {stream_config:?}, format {sample_format:?}");

    let (producer, queue) = HeapRb::<f32>::new(QUEUE_SAMPLES).split();
    let stream = match sample_format {
        cpal::SampleFormat::F32 => build_stream(&device, &stream_config, producer, |s: f32| s),
        cpal::SampleFormat::I16 => build_stream(&device, &stream_config, producer, |s: i16| {
            f32::from(s) / f32::from(i16::MAX)
        }),
        // 24-bit devices (e.g. Fifine, s24le) arrive in 32-bit containers.
        cpal::SampleFormat::I32 => build_stream(&device, &stream_config, producer, |s: i32| {
            s as f32 / i32::MAX as f32
        }),
        cpal::SampleFormat::U16 => build_stream(&device, &stream_config, producer, |s: u16| {
            (f32::from(s) - 32767.5) / 32767.5
        }),
        cpal::SampleFormat::U8 => build_stream(&device, &stream_config, producer, |s: u8| {
            (f32::from(s) - 127.5) / 127.5
        }),
        other => Err(anyhow!("unsupported sample format {other:?}")),
    }?;
    stream.play().context("failed to start the audio stream")?;
    Ok(Capture {
        _stream: stream,
        queue,
    })
}

/// Pick a mono config from `supported_input_configs()`.
///
/// `default_input_config()` is not trusted: through the PipeWire ALSA
/// plugin it reports 2 ch / 44.1 kHz even for a mono USB mic, and some
/// mics (Fifine) then hang in `play()` on the 2-channel negotiation.
fn choose_stream_config(device: &cpal::Device) -> Result<(cpal::StreamConfig, cpal::SampleFormat)> {
    const RATE_PREFERENCE_HZ: &[u32] = &[48000, 44100, 32000, 22050, 16000];
    const FORMAT_PREFERENCE: &[cpal::SampleFormat] = &[
        cpal::SampleFormat::I32,
        cpal::SampleFormat::I16,
        cpal::SampleFormat::F32,
        cpal::SampleFormat::U8,
    ];

    let supported: Vec<cpal::SupportedStreamConfigRange> = device
        .supported_input_configs()
        .context("failed to enumerate supported input configs")?
        .collect();
    let mono = FORMAT_PREFERENCE.iter().find_map(|&fmt| {
        supported
            .iter()
            .find(|c| c.channels() == 1 && c.sample_format() == fmt)
    });

    if let Some(range) = mono {
        let rate = RATE_PREFERENCE_HZ
            .iter()
            .copied()
            .find(|&r| range.min_sample_rate().0 <= r && range.max_sample_rate().0 >= r)
            .unwrap_or(range.max_sample_rate().0);
        let config = range.with_sample_rate(cpal::SampleRate(rate));
        return Ok((config.config(), config.sample_format()));
    }

    info!("No mono input config; using the device default and downmixing");
    let default = device
        .default_input_config()
        .context("failed to get the default input config")?;
    Ok((default.config(), default.sample_format()))
}

/// Capture callback: downmix each frame to mono, resample to 16 kHz and
/// queue it for the daemon.
fn build_stream<T: cpal::SizedSample + Send + 'static>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut producer: Producer<f32, Arc<HeapRb<f32>>>,
    to_f32: fn(T) -> f32,
) -> Result<cpal::Stream> {
    let channels = usize::from(config.channels);
    let mut resampler = MonoResampler::new(config.sample_rate.0, TARGET_RATE_HZ)?;
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _: &_| {
            for frame in data.chunks(channels) {
                let mono = frame.iter().map(|&s| to_f32(s)).sum::<f32>() / channels as f32;
                resampler.push(mono, |s| {
                    let _ = producer.push(s);
                });
            }
        },
        |err| error!("audio stream error: {err}"),
        None,
    )?;
    Ok(stream)
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
