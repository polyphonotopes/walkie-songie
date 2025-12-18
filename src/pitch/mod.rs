//! Pitch detection using SwiftF0 ML model.
//!
//! SwiftF0 is a fast and accurate pitch detector using a small CNN model,
//! running via onnxruntime-web in the browser.

mod swiftf0;

pub use swiftf0::SwiftF0Detector;

/// A detected pitch event from the pitch detector.
#[derive(Debug, Clone)]
pub struct PitchEvent {
    /// Detected frequency in Hz, or None if no pitch detected
    pub hz: Option<f64>,
    /// Confidence level (0.0 to 1.0)
    pub confidence: f64,
}

/// Configuration for pitch detection.
#[derive(Debug, Clone)]
pub struct PitchDetectorConfig {
    /// Sample rate in Hz (typically 44100 or 48000)
    pub sample_rate: u32,
    /// Minimum detectable frequency in Hz
    pub min_frequency: f64,
    /// Maximum detectable frequency in Hz
    pub max_frequency: f64,
}

impl Default for PitchDetectorConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            min_frequency: 80.0,   // ~E2, low male voice
            max_frequency: 1000.0, // ~B5, high female voice
        }
    }
}
