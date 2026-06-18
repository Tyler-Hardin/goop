//! Speech-to-text via whisper.cpp, loaded locally.
//!
//! ## Design
//!
//! [`SpeechToText`] wraps a whisper.cpp model loaded from disk.  Models are
//! auto-downloaded from HuggingFace on first use and cached in
//! `~/.config/goop/models/whisper/`.  The engine is a server-level singleton
//! shared across all sessions — loaded lazily and wrapped in a tokio Mutex
//! since whisper.cpp contexts are not `Sync`.
//!
//! Transcription is batch-only (push-to-talk): the client sends a single
//! WAV-encoded audio frame, and the server returns the transcribed text.
//! Streaming / partial results are future work.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ── model selection ───────────────────────────────────────────────────

/// Whisper model variant. Determines accuracy vs speed vs memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WhisperModel {
    /// ~75 MB — fastest, least accurate.
    Tiny,
    /// ~142 MB — good balance for general use.
    #[default]
    Base,
    /// ~466 MB — better accuracy.
    Small,
    /// ~1.5 GB — even better accuracy.
    Medium,
    /// ~2.9 GB — best accuracy, slowest.
    Large,
}

impl WhisperModel {
    /// Human-readable label for logging.
    pub fn label(self) -> &'static str {
        match self {
            Self::Tiny => "tiny",
            Self::Base => "base",
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }
}

// ── error ─────────────────────────────────────────────────────────────

/// Errors that can occur during speech-to-text operations.
#[derive(thiserror::Error, Debug)]
pub enum SttError {
    /// The model file was not found at the expected path.
    #[error("model not found at {0}")]
    ModelNotFound(PathBuf),

    /// Model download failed (network, HTTP error, disk write, etc.).
    #[error("download failed: {0}")]
    DownloadFailed(String),

    /// An error from whisper.cpp.
    #[error("whisper error: {0}")]
    Whisper(#[from] whisper_rs::WhisperError),

    /// Audio data is malformed, unsupported format, or silent.
    #[error("invalid audio: {0}")]
    InvalidAudio(String),
}

// ── engine ────────────────────────────────────────────────────────────

/// Owns a loaded Whisper model.  Thread-safe via an internal tokio
/// [`Mutex`](tokio::sync::Mutex) — whisper.cpp contexts are not `Sync`,
/// but contention is negligible since prompts are processed serially.
pub struct SpeechToText {
    ctx: tokio::sync::Mutex<whisper_rs::WhisperContext>,
}

impl SpeechToText {
    /// Load a Whisper model from the given path.
    ///
    /// The load is CPU-bound (model parsing), so it runs on a blocking
    /// thread via [`tokio::task::spawn_blocking`].
    pub async fn load(model_path: &Path) -> Result<Self, SttError> {
        if !model_path.exists() {
            return Err(SttError::ModelNotFound(model_path.to_path_buf()));
        }

        let path = model_path.to_path_buf();

        let ctx = tokio::task::spawn_blocking(move || {
            whisper_rs::WhisperContext::new_with_params(
                &path,
                whisper_rs::WhisperContextParameters::default(),
            )
        })
        .await
        .map_err(|e| SttError::InvalidAudio(format!("spawn_blocking: {e}")))?
        .map_err(SttError::Whisper)?;

        tracing::info!("whisper model loaded from {}", model_path.display());

        Ok(Self {
            ctx: tokio::sync::Mutex::new(ctx),
        })
    }

    /// Transcribe WAV-encoded audio bytes to text.
    ///
    /// Expects mono PCM audio (16-bit or float).  Resamples to 16 kHz if
    /// needed.  Blocks the caller for the duration of transcription
    /// (typically 200–800 ms for a short utterance on the `base` model).
    pub async fn transcribe_wav(&self, wav_bytes: &[u8]) -> Result<String, SttError> {
        // Parse WAV header and extract samples.
        let mut reader = hound::WavReader::new(std::io::Cursor::new(wav_bytes))
            .map_err(|e| SttError::InvalidAudio(format!("WAV parse: {e}")))?;

        let spec = reader.spec();
        let sample_count = reader.duration();

        // Log WAV diagnostics so we can spot sample-rate mismatches.
        let duration_secs = sample_count as f64 / spec.sample_rate as f64;
        let format_label = match spec.sample_format {
            hound::SampleFormat::Int => "int16",
            hound::SampleFormat::Float => "float32",
        };
        tracing::info!(
            "STT WAV: {} Hz, {}, {} channels, {} samples ({:.2} s)",
            spec.sample_rate,
            format_label,
            spec.channels,
            sample_count,
            duration_secs,
        );

        if spec.channels != 1 {
            return Err(SttError::InvalidAudio("audio must be mono".into()));
        }

        // Convert to 32-bit float samples.
        let samples: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Int => {
                let int_samples: Vec<i16> = reader
                    .samples::<i16>()
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| SttError::InvalidAudio(format!("read samples: {e}")))?;
                let mut float_samples = vec![0.0_f32; int_samples.len()];
                whisper_rs::convert_integer_to_float_audio(&int_samples, &mut float_samples)
                    .map_err(|e| SttError::InvalidAudio(format!("convert audio: {e}")))?;
                float_samples
            }
            hound::SampleFormat::Float => reader
                .samples::<f32>()
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| SttError::InvalidAudio(format!("read samples: {e}")))?,
        };

        // Resample to 16 kHz if needed (linear interpolation — fine for speech).
        let samples = if spec.sample_rate != 16_000 {
            resample_linear(&samples, spec.sample_rate, 16_000)
        } else {
            samples
        };

        // Log peak amplitude so we can tell actual silence from quiet speech.
        let peak = samples.iter().fold(0.0_f32, |acc, &s| acc.max(s.abs()));
        tracing::info!(
            "STT: {} samples after resample, peak amplitude = {:.4}",
            samples.len(),
            peak
        );

        if samples.is_empty() {
            return Err(SttError::InvalidAudio("no audio samples".into()));
        }

        // Run transcription.  whisper-rs `full()` is blocking, but the
        // lock-protected context prevents concurrent access from multiple
        // sessions (which wouldn't happen anyway — the prompt queue is serial).
        let ctx = self.ctx.lock().await;
        let mut state = ctx.create_state().map_err(SttError::Whisper)?;

        let mut params =
            whisper_rs::FullParams::new(whisper_rs::SamplingStrategy::Greedy { best_of: 1 });

        // Optimise for English command-length utterances.
        params.set_language(Some("en"));
        params.set_translate(false);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        // Single segment is usually enough for push-to-talk.
        params.set_single_segment(true);

        state.full(params, &samples).map_err(SttError::Whisper)?;

        let num_segments = state.full_n_segments();

        let mut text = String::new();
        for i in 0..num_segments {
            if let Some(segment) = state.get_segment(i) {
                text.push_str(&segment.to_string());
            }
        }

        let trimmed = text.trim().to_string();

        if trimmed.is_empty() {
            return Err(SttError::InvalidAudio("no speech detected".into()));
        }

        Ok(trimmed)
    }
}

// ── resampling ────────────────────────────────────────────────────────

/// Simple linear resampling.  Adequate for speech audio — the quality
/// difference vs sinc is imperceptible for STT at 8–48 kHz source rates.
fn resample_linear(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || samples.is_empty() {
        return samples.to_vec();
    }

    let ratio = from_rate as f64 / to_rate as f64;
    let out_len = (samples.len() as f64 / ratio).ceil() as usize;

    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_idx = i as f64 * ratio;
        let src_idx_floor = (src_idx.floor() as usize).min(samples.len() - 1);
        let src_idx_ceil = (src_idx_floor + 1).min(samples.len() - 1);
        let frac = src_idx - src_idx_floor as f64;
        let sample =
            samples[src_idx_floor] as f64 * (1.0 - frac) + samples[src_idx_ceil] as f64 * frac;
        out.push(sample as f32);
    }
    out
}

// ── model download ────────────────────────────────────────────────────

/// Download URL for a Whisper GGML model on HuggingFace.
pub fn model_download_url(model: WhisperModel) -> &'static str {
    match model {
        WhisperModel::Tiny => {
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.bin"
        }
        WhisperModel::Base => {
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin"
        }
        WhisperModel::Small => {
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin"
        }
        WhisperModel::Medium => {
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.bin"
        }
        WhisperModel::Large => {
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3.bin"
        }
    }
}

/// Local filename for a model variant.
pub fn model_filename(model: WhisperModel) -> &'static str {
    match model {
        WhisperModel::Tiny => "ggml-tiny.bin",
        WhisperModel::Base => "ggml-base.bin",
        WhisperModel::Small => "ggml-small.bin",
        WhisperModel::Medium => "ggml-medium.bin",
        WhisperModel::Large => "ggml-large-v3.bin",
    }
}

/// Ensure the Whisper model file is present on disk, downloading it if
/// needed.  Returns the path to the model file.
pub async fn ensure_model(model: WhisperModel, models_dir: &Path) -> Result<PathBuf, SttError> {
    let model_path = models_dir.join(model_filename(model));

    if model_path.exists() {
        tracing::info!(
            "whisper model {} found at {}",
            model.label(),
            model_path.display(),
        );
        return Ok(model_path);
    }

    tokio::fs::create_dir_all(models_dir)
        .await
        .map_err(|e| SttError::DownloadFailed(format!("create dir: {e}")))?;

    let url = model_download_url(model);
    tracing::info!(
        "downloading whisper {} model (this is a one-time download)…",
        model.label(),
    );
    tracing::info!("  {url}");

    let response = reqwest::get(url)
        .await
        .map_err(|e| SttError::DownloadFailed(format!("GET: {e}")))?;

    if !response.status().is_success() {
        return Err(SttError::DownloadFailed(format!(
            "HTTP {}",
            response.status()
        )));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| SttError::DownloadFailed(format!("read: {e}")))?;

    let size_mb = bytes.len() as f64 / 1_048_576.0;
    tracing::info!("downloaded {size_mb:.1} MB, writing to disk…");

    tokio::fs::write(&model_path, &bytes)
        .await
        .map_err(|e| SttError::DownloadFailed(format!("write: {e}")))?;

    tracing::info!("whisper model ready at {}", model_path.display());

    Ok(model_path)
}
