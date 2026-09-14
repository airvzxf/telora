use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{error, info};
use ringbuf::{HeapRb, Producer};
use std::sync::Arc;

pub struct AudioEngine {
    stream: Option<cpal::Stream>,
}

impl AudioEngine {
    pub fn new() -> Result<Self> {
        Ok(Self { stream: None })
    }

    pub fn start(&mut self, mut producer: Producer<f32, Arc<HeapRb<f32>>>) -> Result<u32> {
        let host = cpal::default_host();

        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow!("No input device found"))?;

        // If `default_input_device()` resolves to ALSA's `default` alias
        // (which on hosts running PipeWire routes through the PipeWire
        // ALSA plugin and then lies about sample-rate ranges — Fifine
        // capture hangs because the plugin advertises a junk 2 ch /
        // 44100 Hz / F32 default that Fifine rejects, and even 1-ch
        // mono configs through the plugin never start streaming),
        // pick a direct ALSA HW device instead. The plugin only
        // intercepts `default`; `sysdefault:CARD=*` and
        // `front:CARD=*,DEV=0` go straight to the kernel ALSA driver
        // and Fifine's HW path captures fine via them.
        // `host.input_devices()` enumerates those direct devices; we
        // pick the first one whose name looks like an ALSA HW hint
        // (contains "CARD=").
        let device = {
            let name = device.name().unwrap_or_default();
            if name == "default" {
                let replacement = host.input_devices().ok().and_then(|it| {
                    it.into_iter()
                        .find(|d| d.name().ok().map(|n| n.contains("CARD=")).unwrap_or(false))
                });
                if let Some(d) = replacement {
                    info!(
                        "Default input '{}' routes through PipeWire ALSA plugin; switching to direct ALSA device '{}'",
                        name,
                        d.name().unwrap_or_else(|_| "Unknown".to_string())
                    );
                    d
                } else {
                    device
                }
            } else {
                device
            }
        };

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

        let err_fn = |err| error!("an error occurred on stream: {}", err);

        let stream = match sample_format {
            cpal::SampleFormat::F32 => device.build_input_stream(
                &actual_config,
                move |data: &[f32], _: &_| {
                    // Downmix: si hay más de 1 canal, promediamos o solo tomamos el primero
                    for frame in data.chunks(channels as usize) {
                        let sum: f32 = frame.iter().sum();
                        let mono = sum / channels as f32;
                        let _ = producer.push(mono);
                    }
                },
                err_fn,
                None,
            )?,
            cpal::SampleFormat::I16 => device.build_input_stream(
                &actual_config,
                move |data: &[i16], _: &_| {
                    for frame in data.chunks(channels as usize) {
                        let sum: f32 = frame.iter().map(|&s| s as f32 / i16::MAX as f32).sum();
                        let mono = sum / channels as f32;
                        let _ = producer.push(mono);
                    }
                },
                err_fn,
                None,
            )?,
            cpal::SampleFormat::U16 => device.build_input_stream(
                &actual_config,
                move |data: &[u16], _: &_| {
                    for frame in data.chunks(channels as usize) {
                        let sum: f32 = frame
                            .iter()
                            .map(|&s| (s as f32 - u16::MAX as f32 / 2.0) / (u16::MAX as f32 / 2.0))
                            .sum();
                        let mono = sum / channels as f32;
                        let _ = producer.push(mono);
                    }
                },
                err_fn,
                None,
            )?,
            cpal::SampleFormat::U8 => device.build_input_stream(
                &actual_config,
                move |data: &[u8], _: &_| {
                    for frame in data.chunks(channels as usize) {
                        let sum: f32 = frame
                            .iter()
                            .map(|&s| (s as f32 - u8::MAX as f32 / 2.0) / (u8::MAX as f32 / 2.0))
                            .sum();
                        let mono = sum / channels as f32;
                        let _ = producer.push(mono);
                    }
                },
                err_fn,
                None,
            )?,
            cpal::SampleFormat::I32 => device.build_input_stream(
                &actual_config,
                move |data: &[i32], _: &_| {
                    // Fifine is s24le surfaced as I32 by cpal's ALSA
                    // backend (24-bit audio stored in 32-bit
                    // containers). Normalise against 2^31 to stay
                    // inside f32's [-1.0, 1.0] range.
                    for frame in data.chunks(channels as usize) {
                        let sum: f32 = frame.iter().map(|&s| s as f32 / i32::MAX as f32).sum();
                        let mono = sum / channels as f32;
                        let _ = producer.push(mono);
                    }
                },
                err_fn,
                None,
            )?,
            _ => return Err(anyhow!("Unsupported sample format")),
        };

        stream.play().context("Failed to start audio stream")?;

        self.stream = Some(stream);

        Ok(sample_rate)
    }
}
