//! SwiftF0 ML-based pitch detector.
//!
//! Uses a small CNN model (~400KB) for accurate pitch detection.
//! Much better at avoiding harmonic confusion than traditional algorithms.

use tract_onnx::prelude::*;

/// SwiftF0 model expects 16kHz audio
const SWIFTF0_SAMPLE_RATE: u32 = 16000;

/// Minimum samples needed for inference (about 64ms at 16kHz)
const MIN_SAMPLES: usize = 1024;

/// Embedded ONNX model
static SWIFTF0_MODEL: &[u8] = include_bytes!("../../assets/swiftf0.onnx");

/// SwiftF0 pitch detector using tract ONNX runtime.
pub struct SwiftF0Detector {
    model: SimplePlan<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>,
    buffer_16k: Vec<f32>,
    input_sample_rate: u32,
}

impl SwiftF0Detector {
    /// Create a new SwiftF0 detector.
    /// input_sample_rate: the sample rate of audio you'll feed in (e.g., 48000)
    pub fn new(input_sample_rate: u32) -> Result<Self, anyhow::Error> {
        // Load and optimize the model
        let model = tract_onnx::onnx()
            .model_for_read(&mut std::io::Cursor::new(SWIFTF0_MODEL))?
            .into_optimized()?
            .into_runnable()?;

        Ok(Self {
            model,
            buffer_16k: Vec::with_capacity(MIN_SAMPLES * 4),
            input_sample_rate,
        })
    }

    /// Simple downsampling from input rate to 16kHz.
    /// For 48kHz -> 16kHz, takes every 3rd sample.
    fn downsample(&self, samples: &[f32]) -> Vec<f32> {
        let ratio = self.input_sample_rate / SWIFTF0_SAMPLE_RATE;
        if ratio <= 1 {
            return samples.to_vec();
        }
        samples.iter().step_by(ratio as usize).copied().collect()
    }

    /// Process audio samples and detect pitch.
    /// Returns (hz, confidence) if pitch detected, None otherwise.
    pub fn detect(&mut self, samples: &[f32]) -> Option<(f64, f64)> {
        // Downsample to 16kHz
        let downsampled = self.downsample(samples);
        self.buffer_16k.extend_from_slice(&downsampled);

        // Need enough samples for inference
        if self.buffer_16k.len() < MIN_SAMPLES {
            return None;
        }

        // Prepare input tensor [1, audio_length]
        let input: Tensor = tract_ndarray::Array2::from_shape_vec(
            (1, self.buffer_16k.len()),
            self.buffer_16k.clone(),
        )
        .ok()?
        .into();

        // Run inference
        let outputs = self.model.run(tvec!(input.into())).ok()?;

        // Output 0: pitch_hz, Output 1: confidence
        let pitch_hz = outputs[0].to_array_view::<f32>().ok()?;
        let confidence = outputs[1].to_array_view::<f32>().ok()?;

        // Get the last frame's pitch and confidence
        let len = pitch_hz.len();
        if len == 0 {
            return None;
        }

        let hz = pitch_hz.as_slice()?[len - 1] as f64;
        let conf = confidence.as_slice()?[len - 1] as f64;

        // Trim buffer to prevent unbounded growth (keep last ~100ms)
        let keep = SWIFTF0_SAMPLE_RATE as usize / 10; // ~1600 samples
        if self.buffer_16k.len() > keep * 2 {
            self.buffer_16k.drain(..self.buffer_16k.len() - keep);
        }

        // Filter out invalid pitches
        if hz > 46.0 && hz < 2100.0 && conf > 0.3 {
            Some((hz, conf))
        } else {
            None
        }
    }

    /// Reset the detector state.
    pub fn reset(&mut self) {
        self.buffer_16k.clear();
    }
}
