//! SwiftF0 ML-based pitch detector.
//!
//! Uses onnxruntime-web via JavaScript bridge for full ONNX operator support.

#[cfg(all(target_arch = "wasm32", feature = "web-ui"))]
use crate::web::onnx_bridge;

/// SwiftF0 model expects 16kHz audio
#[cfg(all(target_arch = "wasm32", feature = "web-ui"))]
const SWIFTF0_SAMPLE_RATE: u32 = 16000;

/// Minimum samples needed for inference
#[cfg(all(target_arch = "wasm32", feature = "web-ui"))]
const MIN_SAMPLES: usize = 1024;

/// SwiftF0 pitch detector using onnxruntime-web.
pub struct SwiftF0Detector {
    #[cfg(all(target_arch = "wasm32", feature = "web-ui"))]
    buffer_16k: Vec<f32>,
    #[allow(dead_code)] // Used in wasm builds
    input_sample_rate: u32,
    #[allow(dead_code)] // Used in wasm builds
    initialized: bool,
}

impl SwiftF0Detector {
    /// Create a new SwiftF0 detector.
    pub fn new(input_sample_rate: u32) -> Self {
        Self {
            #[cfg(all(target_arch = "wasm32", feature = "web-ui"))]
            buffer_16k: Vec::with_capacity(MIN_SAMPLES * 4),
            input_sample_rate,
            initialized: false,
        }
    }

    /// Initialize the ONNX model (async, call once).
    #[cfg(all(target_arch = "wasm32", feature = "web-ui"))]
    pub async fn init(&mut self) -> Result<(), String> {
        match onnx_bridge::init_swiftf0().await {
            Ok(_) => {
                self.initialized = true;
                Ok(())
            }
            Err(e) => {
                let msg = js_sys::JSON::stringify(&e)
                    .map(|s| s.as_string().unwrap_or_default())
                    .unwrap_or_else(|_| format!("{:?}", e));
                Err(msg)
            }
        }
    }

    #[cfg(not(all(target_arch = "wasm32", feature = "web-ui")))]
    pub async fn init(&mut self) -> Result<(), String> {
        Err("SwiftF0 requires the wasm32 web-ui feature".to_string())
    }

    /// Check if ready.
    #[cfg(all(target_arch = "wasm32", feature = "web-ui"))]
    pub fn is_ready(&self) -> bool {
        self.initialized && onnx_bridge::is_model_ready()
    }

    #[cfg(not(all(target_arch = "wasm32", feature = "web-ui")))]
    pub fn is_ready(&self) -> bool {
        false
    }

    /// Simple downsampling from input rate to 16kHz.
    #[cfg(all(target_arch = "wasm32", feature = "web-ui"))]
    fn downsample(&self, samples: &[f32]) -> Vec<f32> {
        let ratio = self.input_sample_rate / SWIFTF0_SAMPLE_RATE;
        if ratio <= 1 {
            return samples.to_vec();
        }
        samples.iter().step_by(ratio as usize).copied().collect()
    }

    /// Process audio samples and detect pitch (async).
    #[cfg(all(target_arch = "wasm32", feature = "web-ui"))]
    pub async fn detect(&mut self, samples: &[f32]) -> Option<(f64, f64)> {
        if !self.is_ready() {
            return None;
        }

        // Downsample to 16kHz
        let downsampled = self.downsample(samples);
        self.buffer_16k.extend_from_slice(&downsampled);

        // Need enough samples
        if self.buffer_16k.len() < MIN_SAMPLES {
            return None;
        }

        // Run inference via JS bridge
        let result = onnx_bridge::detect_pitch(&self.buffer_16k).await;
        let pitch_result = onnx_bridge::OnnxPitchResult::from_js(result);

        // Trim buffer
        let keep = SWIFTF0_SAMPLE_RATE as usize / 10;
        if self.buffer_16k.len() > keep * 2 {
            self.buffer_16k.drain(..self.buffer_16k.len() - keep);
        }

        pitch_result.map(|r| (r.hz, r.confidence))
    }

    #[cfg(not(all(target_arch = "wasm32", feature = "web-ui")))]
    pub async fn detect(&mut self, _samples: &[f32]) -> Option<(f64, f64)> {
        None
    }

    /// Reset the detector state.
    #[cfg(all(target_arch = "wasm32", feature = "web-ui"))]
    pub fn reset(&mut self) {
        self.buffer_16k.clear();
    }

    /// Reset the detector state (no-op on non-wasm).
    #[cfg(not(all(target_arch = "wasm32", feature = "web-ui")))]
    pub fn reset(&mut self) {}
}
