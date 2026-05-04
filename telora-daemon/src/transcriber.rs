use anyhow::{Context, Result, anyhow};
use log::info;
use std::path::Path;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

pub trait Transcriber: Send {
    fn transcribe(&mut self, audio_data: &[f32], language: Option<&str>) -> Result<String>;
}

pub struct WhisperTranscriber {
    ctx: WhisperContext,
}

impl WhisperTranscriber {
    pub fn new(model_path: &str) -> Result<Self> {
        if !Path::new(model_path).exists() {
            return Err(anyhow!("Model file not found: {}", model_path));
        }

        info!("Loading Whisper model from {}...", model_path);

        // Load the model
        let ctx = WhisperContext::new_with_params(model_path, WhisperContextParameters::default())
            .context("Failed to load Whisper model")?;

        info!("Whisper model loaded successfully.");
        Ok(Self { ctx })
    }
}

impl Transcriber for WhisperTranscriber {
    fn transcribe(&mut self, audio_data: &[f32], language: Option<&str>) -> Result<String> {
        let mut state = self
            .ctx
            .create_state()
            .context("Failed to create Whisper state")?;

        // Configure parameters
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some(language.unwrap_or("es")));
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        // Run the transcription
        state
            .full(params, audio_data)
            .context("Failed to run full transcription")?;

        // Fetch the results
        let num_segments = state
            .full_n_segments()
            .context("Failed to get number of segments")?;
        let mut text = String::new();

        for i in 0..num_segments {
            let segment = state
                .full_get_segment_text(i)
                .context("Failed to get segment text")?;
            text.push_str(&segment);
            text.push(' ');
        }

        Ok(text.trim().to_string())
    }
}
