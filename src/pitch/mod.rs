//! Pitch detection abstractions.
//!
//! Provides the `PitchDetector` trait for platform-agnostic pitch detection,
//! with implementations for web (AudioWorklet) and native (cpal) targets.

mod detector;

use std::pin::Pin;

use futures::Stream;

pub use detector::DualPitchDetector;

/// A detected pitch event from the pitch detector.
#[derive(Debug, Clone)]
pub struct PitchEvent {
    /// Detected frequency in Hz, or None if no pitch detected (gate closed)
    pub hz: Option<f64>,
    /// Confidence level (0.0 to 1.0)
    pub confidence: f64,
    /// Whether this is from the fast (BCF) or accurate (pYIN) detector
    pub source: PitchSource,
    /// Current noise gate threshold in dB
    pub gate_threshold_db: f64,
    /// Current signal level in dB
    pub signal_level_db: f64,
}

/// Source of a pitch detection result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PitchSource {
    /// Fast BCF algorithm (~15ms latency)
    Fast,
    /// Accurate pYIN algorithm (~50ms latency)
    Accurate,
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
    /// Noise gate margin above noise floor in dB
    pub gate_margin_db: f64,
    /// Minimum confidence threshold for pitch detection
    pub min_confidence: f64,
}

impl Default for PitchDetectorConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            min_frequency: 80.0,   // ~E2, low male voice
            max_frequency: 1000.0, // ~B5, high female voice
            gate_margin_db: 6.0,
            min_confidence: 0.8,
        }
    }
}

/// Trait for pitch detection implementations.
///
/// Implementations provide a stream of pitch events from audio input.
/// The stream should emit both fast (BCF) and accurate (pYIN) results.
pub trait PitchDetector: Send {
    /// Start pitch detection and return a stream of pitch events.
    fn start(&mut self) -> Pin<Box<dyn Stream<Item = PitchEvent> + Send + '_>>;

    /// Stop pitch detection.
    fn stop(&mut self);

    /// Check if the detector is currently running.
    fn is_running(&self) -> bool;

    /// Get the current configuration.
    fn config(&self) -> &PitchDetectorConfig;
}

/// A mock pitch detector for testing.
#[cfg(test)]
pub struct MockPitchDetector {
    config: PitchDetectorConfig,
    running: bool,
}

#[cfg(test)]
impl MockPitchDetector {
    pub fn new(config: PitchDetectorConfig) -> Self {
        Self {
            config,
            running: false,
        }
    }
}

#[cfg(test)]
impl PitchDetector for MockPitchDetector {
    fn start(&mut self) -> Pin<Box<dyn Stream<Item = PitchEvent> + Send + '_>> {
        self.running = true;
        Box::pin(futures::stream::empty())
    }

    fn stop(&mut self) {
        self.running = false;
    }

    fn is_running(&self) -> bool {
        self.running
    }

    fn config(&self) -> &PitchDetectorConfig {
        &self.config
    }
}
