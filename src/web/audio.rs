//! Web Audio API integration for microphone input and pitch detection.

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{AudioContext, MediaStream, MediaStreamConstraints};

use crate::pitch::{DualPitchDetector, PitchEvent};

/// Web audio input handler.
pub struct WebAudioInput {
    context: AudioContext,
    stream: Option<MediaStream>,
    running: bool,
}

impl WebAudioInput {
    /// Create a new web audio input handler.
    pub fn new() -> Result<Self, JsValue> {
        let context = AudioContext::new()?;
        Ok(Self {
            context,
            stream: None,
            running: false,
        })
    }

    /// Request microphone access and start audio input.
    pub async fn start(&mut self) -> Result<(), JsValue> {
        if self.running {
            return Ok(());
        }

        // Request microphone access
        let window = web_sys::window().ok_or("No window")?;
        let navigator = window.navigator();
        let media_devices = navigator
            .media_devices()
            .map_err(|_| "No media devices")?;

        let mut constraints = MediaStreamConstraints::new();
        constraints.set_audio(&JsValue::TRUE);
        constraints.set_video(&JsValue::FALSE);

        let stream_promise = media_devices.get_user_media_with_constraints(&constraints)?;
        let stream: MediaStream = wasm_bindgen_futures::JsFuture::from(stream_promise)
            .await?
            .dyn_into()?;

        self.stream = Some(stream);
        self.running = true;

        Ok(())
    }

    /// Stop audio input.
    pub fn stop(&mut self) {
        if let Some(stream) = self.stream.take() {
            // Stop all tracks
            let tracks = stream.get_tracks();
            for i in 0..tracks.length() {
                let track = tracks.get(i);
                if let Ok(track) = track.dyn_into::<web_sys::MediaStreamTrack>() {
                    track.stop();
                }
            }
        }
        self.running = false;
    }

    /// Check if audio input is running.
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Get the audio context sample rate.
    pub fn sample_rate(&self) -> f32 {
        self.context.sample_rate()
    }
}

impl Drop for WebAudioInput {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Process audio samples through the pitch detector.
/// This would typically be called from an AudioWorklet.
pub fn process_audio_chunk(
    detector: &mut DualPitchDetector,
    samples: &[f32],
) -> Vec<PitchEvent> {
    detector.process_samples(samples)
}
