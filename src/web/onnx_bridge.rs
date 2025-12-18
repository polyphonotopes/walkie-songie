//! Bridge to onnxruntime-web for ML inference.
//!
//! Uses JavaScript interop to call onnxruntime-web which has full ONNX operator support.

use wasm_bindgen::prelude::*;

#[wasm_bindgen(module = "/assets/onnx-bridge.js")]
extern "C" {
    /// Initialize the ONNX model (call once at startup).
    /// Returns a Promise that resolves when ready.
    #[wasm_bindgen(js_name = "initSwiftF0", catch)]
    pub async fn init_swiftf0() -> Result<JsValue, JsValue>;

    /// Run pitch detection on audio samples.
    /// Returns [pitch_hz, confidence] or null if no pitch detected.
    #[wasm_bindgen(js_name = "detectPitch")]
    pub async fn detect_pitch(samples: &[f32]) -> JsValue;

    /// Check if the model is ready.
    #[wasm_bindgen(js_name = "isModelReady")]
    pub fn is_model_ready() -> bool;
}

/// Result from pitch detection.
#[derive(Debug, Clone)]
pub struct OnnxPitchResult {
    pub hz: f64,
    pub confidence: f64,
}

impl OnnxPitchResult {
    /// Parse result from JS.
    pub fn from_js(value: JsValue) -> Option<Self> {
        if value.is_null() || value.is_undefined() {
            return None;
        }

        let array = js_sys::Array::from(&value);
        if array.length() < 2 {
            return None;
        }

        let hz = array.get(0).as_f64()?;
        let confidence = array.get(1).as_f64()?;

        // Filter invalid results
        if hz > 46.0 && hz < 2100.0 && confidence > 0.3 {
            Some(Self { hz, confidence })
        } else {
            None
        }
    }
}
