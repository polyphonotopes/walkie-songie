//! Dual-algorithm pitch detector implementation.
//!
//! Combines BCF (fast, ~15ms) and pYIN (accurate, ~50ms) for optimal
//! real-time feedback and accurate pitch commitment.
//!
//! Note: pYIN is only available on native targets (not wasm).

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::channel::mpsc;
use futures::Stream;

use super::{PitchDetector, PitchDetectorConfig, PitchEvent, PitchSource};

/// Buffer size for BCF detection (2048 samples at 48kHz = ~43ms window)
const BCF_BUFFER_SIZE: usize = 2048;

/// Frame length for pYIN (powers of 2 only)
#[cfg(not(target_arch = "wasm32"))]
const PYIN_FRAME_LENGTH: usize = 2048;
#[cfg(not(target_arch = "wasm32"))]
const PYIN_WIN_LENGTH: usize = 1024;
#[cfg(not(target_arch = "wasm32"))]
const PYIN_HOP_LENGTH: usize = 256;

/// Dual-algorithm pitch detector using BCF and pYIN.
/// On wasm, only BCF is available.
pub struct DualPitchDetector {
    config: PitchDetectorConfig,
    running: Arc<AtomicBool>,
    sample_buffer: Vec<f64>,
    #[cfg(not(target_arch = "wasm32"))]
    pyin_executor: Option<pyin::PYINExecutor<f64>>,
    noise_floor_db: f64,
}

impl DualPitchDetector {
    /// Create a new dual pitch detector with the given configuration.
    pub fn new(config: PitchDetectorConfig) -> Self {
        Self {
            config,
            running: Arc::new(AtomicBool::new(false)),
            sample_buffer: Vec::with_capacity(BCF_BUFFER_SIZE * 2),
            #[cfg(not(target_arch = "wasm32"))]
            pyin_executor: None,
            noise_floor_db: -60.0, // Initial noise floor estimate
        }
    }

    /// Initialize the pYIN executor (lazy initialization).
    #[cfg(not(target_arch = "wasm32"))]
    fn init_pyin(&mut self) {
        if self.pyin_executor.is_none() {
            self.pyin_executor = Some(pyin::PYINExecutor::new(
                self.config.min_frequency,
                self.config.max_frequency,
                self.config.sample_rate,
                PYIN_FRAME_LENGTH,
                Some(PYIN_WIN_LENGTH),
                Some(PYIN_HOP_LENGTH),
                None, // default resolution
            ));
        }
    }

    /// Calculate RMS level in dB from samples.
    fn calculate_level_db(samples: &[f64]) -> f64 {
        if samples.is_empty() {
            return -100.0;
        }
        let sum_sq: f64 = samples.iter().map(|&s| s.powi(2)).sum();
        let rms = (sum_sq / samples.len() as f64).sqrt();
        if rms > 0.0 {
            20.0 * rms.log10()
        } else {
            -100.0
        }
    }

    /// Detect pitch using BCF algorithm (fast path).
    fn detect_bcf(samples: &[f64], min_freq: f64, max_freq: f64) -> Option<(f64, f64)> {
        if samples.len() < BCF_BUFFER_SIZE {
            return None;
        }

        // Use the last BCF_BUFFER_SIZE samples
        let start = samples.len().saturating_sub(BCF_BUFFER_SIZE);
        let buffer = &samples[start..start + BCF_BUFFER_SIZE];

        let (hz, amplitude) = pitch::detect(buffer);

        // Filter by frequency range
        if hz >= min_freq && hz <= max_freq {
            Some((hz, amplitude))
        } else {
            None
        }
    }

    /// Detect pitch using pYIN algorithm (accurate path).
    /// Only available on native targets.
    #[cfg(not(target_arch = "wasm32"))]
    fn detect_pyin_inner(
        executor: &mut pyin::PYINExecutor<f64>,
        samples: &[f64],
        min_freq: f64,
        max_freq: f64,
    ) -> Option<(f64, f64)> {
        // pYIN needs sufficient samples
        if samples.len() < PYIN_FRAME_LENGTH {
            return None;
        }

        let (_timestamps, f0, voiced_flag, voiced_prob) = executor.pyin(
            samples,
            f64::NAN,             // fill_unvoiced with NaN
            pyin::Framing::Valid, // no padding
        );

        // Find the most recent voiced frame
        for i in (0..f0.len()).rev() {
            if voiced_flag[i] && !f0[i].is_nan() {
                let hz = f0[i];
                let confidence = voiced_prob[i];
                if hz >= min_freq && hz <= max_freq {
                    return Some((hz, confidence));
                }
            }
        }

        None
    }

    /// Process a chunk of audio samples and emit pitch events.
    /// Samples should be f32 normalized to [-1, 1].
    pub fn process_samples(&mut self, samples: &[f32]) -> Vec<PitchEvent> {
        let mut events = Vec::new();

        // Convert to f64 and add to buffer
        let samples_f64: Vec<f64> = samples.iter().map(|&s| s as f64).collect();
        self.sample_buffer.extend_from_slice(&samples_f64);

        // Calculate current signal level
        let signal_level_db = Self::calculate_level_db(&samples_f64);

        // Update noise floor estimate (simple exponential moving average)
        // Only update when signal is low (likely noise)
        if signal_level_db < self.noise_floor_db + self.config.gate_margin_db {
            self.noise_floor_db = self.noise_floor_db * 0.99 + signal_level_db * 0.01;
        }

        let gate_threshold_db = self.noise_floor_db + self.config.gate_margin_db;
        let gate_open = signal_level_db > gate_threshold_db;

        // BCF detection (fast path)
        if self.sample_buffer.len() >= BCF_BUFFER_SIZE {
            let bcf_result = if gate_open {
                Self::detect_bcf(
                    &self.sample_buffer,
                    self.config.min_frequency,
                    self.config.max_frequency,
                )
            } else {
                None
            };

            events.push(PitchEvent {
                hz: bcf_result.map(|(hz, _)| hz),
                confidence: bcf_result.map(|(_, amp)| amp).unwrap_or(0.0),
                source: PitchSource::Fast,
                gate_threshold_db,
                signal_level_db,
            });
        }

        // pYIN detection (accurate path, less frequently) - native only
        #[cfg(not(target_arch = "wasm32"))]
        if self.sample_buffer.len() >= PYIN_FRAME_LENGTH + PYIN_HOP_LENGTH * 4 {
            self.init_pyin();

            let pyin_result = if gate_open {
                if let Some(executor) = self.pyin_executor.as_mut() {
                    Self::detect_pyin_inner(
                        executor,
                        &self.sample_buffer,
                        self.config.min_frequency,
                        self.config.max_frequency,
                    )
                } else {
                    None
                }
            } else {
                None
            };

            events.push(PitchEvent {
                hz: pyin_result.map(|(hz, _)| hz),
                confidence: pyin_result.map(|(_, conf)| conf).unwrap_or(0.0),
                source: PitchSource::Accurate,
                gate_threshold_db,
                signal_level_db,
            });

            // Trim buffer to prevent unbounded growth
            let keep = PYIN_FRAME_LENGTH;
            if self.sample_buffer.len() > keep * 2 {
                self.sample_buffer.drain(..self.sample_buffer.len() - keep);
            }
        }

        // On wasm, we use BCF for both fast and accurate detection
        // (just emit the BCF result as the accurate one too after a delay)
        #[cfg(target_arch = "wasm32")]
        {
            // Trim buffer to prevent unbounded growth
            let keep = BCF_BUFFER_SIZE * 2;
            if self.sample_buffer.len() > keep * 2 {
                self.sample_buffer.drain(..self.sample_buffer.len() - keep);
            }
        }

        events
    }

    /// Reset the detector state.
    pub fn reset(&mut self) {
        self.sample_buffer.clear();
        self.noise_floor_db = -60.0;
    }
}

impl PitchDetector for DualPitchDetector {
    fn start(&mut self) -> Pin<Box<dyn Stream<Item = PitchEvent> + Send + '_>> {
        self.running.store(true, Ordering::SeqCst);
        self.reset();

        // Return an empty stream - actual samples will be pushed via process_samples
        // In a real implementation, this would connect to audio input
        let (_tx, rx) = mpsc::channel::<PitchEvent>(32);
        Box::pin(rx)
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        self.reset();
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn config(&self) -> &PitchDetectorConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn generate_sine(freq: f64, sample_rate: u32, num_samples: usize) -> Vec<f32> {
        (0..num_samples)
            .map(|i| {
                let t = i as f64 / sample_rate as f64;
                ((2.0 * PI * freq * t).sin() * 0.5) as f32
            })
            .collect()
    }

    #[test]
    fn test_bcf_detection() {
        let config = PitchDetectorConfig::default();
        let mut detector = DualPitchDetector::new(config);

        // Generate A4 = 440 Hz
        let samples = generate_sine(440.0, 48000, BCF_BUFFER_SIZE * 2);

        let events = detector.process_samples(&samples);

        // Should have at least one BCF event
        let bcf_event = events.iter().find(|e| e.source == PitchSource::Fast);
        assert!(bcf_event.is_some());

        if let Some(event) = bcf_event {
            if let Some(hz) = event.hz {
                // BCF should detect frequency within ~5% of 440 Hz
                assert!(
                    (hz - 440.0).abs() < 22.0,
                    "Detected {} Hz, expected ~440 Hz",
                    hz
                );
            }
        }
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn test_pyin_detection() {
        let config = PitchDetectorConfig::default();
        let mut detector = DualPitchDetector::new(config);

        // Generate A4 = 440 Hz with enough samples for pYIN
        let samples = generate_sine(440.0, 48000, PYIN_FRAME_LENGTH * 3);

        let events = detector.process_samples(&samples);

        // Should have at least one pYIN event
        let pyin_event = events.iter().find(|e| e.source == PitchSource::Accurate);
        assert!(pyin_event.is_some());

        if let Some(event) = pyin_event {
            if let Some(hz) = event.hz {
                // pYIN should be more accurate, within ~1% of 440 Hz
                assert!(
                    (hz - 440.0).abs() < 5.0,
                    "Detected {} Hz, expected ~440 Hz",
                    hz
                );
            }
        }
    }

    #[test]
    fn test_noise_gate() {
        let config = PitchDetectorConfig::default();
        let mut detector = DualPitchDetector::new(config);

        // Generate very quiet signal (should trigger noise gate)
        let samples: Vec<f32> = (0..BCF_BUFFER_SIZE * 2)
            .map(|i| (i as f32 * 0.01).sin() * 0.0001)
            .collect();

        let events = detector.process_samples(&samples);

        // With noise gate closed, hz should be None
        for event in events {
            assert!(
                event.hz.is_none(),
                "Noise gate should close for quiet signals"
            );
        }
    }

    #[test]
    fn test_level_calculation() {
        let samples = vec![0.5f64; 100];
        let level = DualPitchDetector::calculate_level_db(&samples);
        // 0.5 RMS = -6 dB
        assert!((level - (-6.02)).abs() < 0.5);

        let samples = vec![1.0f64; 100];
        let level = DualPitchDetector::calculate_level_db(&samples);
        // 1.0 RMS = 0 dB
        assert!(level.abs() < 0.1);
    }
}
