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

        info!(
            "Using input device: {}",
            device.name().unwrap_or("Unknown".to_string())
        );

        let supported_config = device
            .default_input_config()
            .context("Failed to get default input config")?;
        let sample_format = supported_config.sample_format();

        // Whisper prefiere 16000Hz Mono. Intentamos configurar eso.
        let config = cpal::StreamConfig {
            channels: 1, // Intentamos Mono
            sample_rate: cpal::SampleRate(16000),
            buffer_size: cpal::BufferSize::Default,
        };

        // Only force 16 kHz mono when the device's reported sample-rate range
        // is *exactly* 16 kHz (i.e. a fixed-rate native USB headset). The
        // previous `<= 16k && >= 16k` check passed whenever the range
        // *included* 16 kHz, but cpal/PipeWire often reports implausibly
        // wide ranges (e.g. 1 Hz-384000 Hz for Fifine USB mics) where the
        // actual node only accepts 44100/48000/192000 Hz. Forcing 16 kHz
        // against a lying wide range made `build_input_stream` +
        // `stream.play()` wait forever for PipeWire to honour an
        // unsupported rate. Falling through to `default_input_config()`
        // here gives the device's real native rate (typically 48000 Hz).
        let actual_config = if device.supported_input_configs()?.any(|c| {
            c.channels() == 1 && c.min_sample_rate().0 == 16000 && c.max_sample_rate().0 == 16000
        }) {
            info!("Forcing 16000Hz Mono...");
            config
        } else {
            info!("Using default device config...");
            supported_config.into()
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
            _ => return Err(anyhow!("Unsupported sample format")),
        };

        stream.play().context("Failed to start audio stream")?;

        self.stream = Some(stream);

        Ok(sample_rate)
    }
}
