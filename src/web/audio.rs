//! Web Audio API integration for microphone input and pitch detection.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    AudioContext, AudioProcessingEvent, MediaStream, MediaStreamAudioSourceNode,
    MediaStreamConstraints, ScriptProcessorNode,
};

use crate::pitch::{DualPitchDetector, PitchEvent};

/// Buffer size for ScriptProcessorNode (power of 2, 256-16384).
/// 2048 samples at 48kHz = ~42ms of audio per callback.
const BUFFER_SIZE: u32 = 2048;

/// Web audio input handler that processes microphone input through pitch detection.
pub struct WebAudioInput {
    context: AudioContext,
    stream: Option<MediaStream>,
    source_node: Option<MediaStreamAudioSourceNode>,
    processor_node: Option<ScriptProcessorNode>,
    // Store the closure to prevent it from being dropped
    _callback: Option<Closure<dyn FnMut(AudioProcessingEvent)>>,
    running: bool,
}

impl WebAudioInput {
    /// Create a new web audio input handler.
    pub fn new() -> Result<Self, JsValue> {
        let context = AudioContext::new()?;
        Ok(Self {
            context,
            stream: None,
            source_node: None,
            processor_node: None,
            _callback: None,
            running: false,
        })
    }

    /// Request microphone access and start audio processing.
    /// The callback receives pitch events as they are detected.
    pub async fn start<F>(&mut self, detector: Rc<RefCell<DualPitchDetector>>, on_pitch: F) -> Result<(), JsValue>
    where
        F: Fn(PitchEvent) + 'static,
    {
        if self.running {
            return Ok(());
        }

        // Resume context if suspended (browser autoplay policy)
        if self.context.state() == web_sys::AudioContextState::Suspended {
            wasm_bindgen_futures::JsFuture::from(self.context.resume()?).await?;
        }

        // Request microphone access
        let window = web_sys::window().ok_or("No window")?;
        let navigator = window.navigator();
        let media_devices = navigator.media_devices().map_err(|_| "No media devices")?;

        let constraints = MediaStreamConstraints::new();
        constraints.set_audio(&JsValue::TRUE);
        constraints.set_video(&JsValue::FALSE);

        let stream_promise = media_devices.get_user_media_with_constraints(&constraints)?;
        let stream: MediaStream = wasm_bindgen_futures::JsFuture::from(stream_promise)
            .await?
            .dyn_into()?;

        // Create audio nodes
        let source = self.context.create_media_stream_source(&stream)?;

        // ScriptProcessorNode: buffer_size, num_input_channels, num_output_channels
        // Using 1 input channel (mono) for voice
        let processor = self.context.create_script_processor_with_buffer_size_and_number_of_input_channels_and_number_of_output_channels(
            BUFFER_SIZE,
            1,
            1,
        )?;

        // Set up the audio processing callback
        let callback = Closure::wrap(Box::new(move |event: AudioProcessingEvent| {
            if let Ok(input_buffer) = event.input_buffer() {
                // Get the first channel's data
                if let Ok(channel_data) = input_buffer.get_channel_data(0) {
                    // Process through pitch detector
                    let mut det = detector.borrow_mut();
                    let events = det.process_samples(&channel_data);

                    // Emit pitch events
                    for pitch_event in events {
                        on_pitch(pitch_event);
                    }
                }
            }
        }) as Box<dyn FnMut(AudioProcessingEvent)>);

        processor.set_onaudioprocess(Some(callback.as_ref().unchecked_ref()));

        // Connect: source -> processor -> destination
        // We need to connect to destination for the processor to receive data,
        // but we output silence (the processor doesn't modify the audio)
        source.connect_with_audio_node(&processor)?;
        processor.connect_with_audio_node(&self.context.destination())?;

        self.stream = Some(stream);
        self.source_node = Some(source);
        self.processor_node = Some(processor);
        self._callback = Some(callback);
        self.running = true;

        Ok(())
    }

    /// Stop audio input.
    pub fn stop(&mut self) {
        // Disconnect nodes
        if let Some(ref processor) = self.processor_node {
            processor.set_onaudioprocess(None);
            let _ = processor.disconnect();
        }
        if let Some(ref source) = self.source_node {
            let _ = source.disconnect();
        }

        // Stop media stream tracks
        if let Some(ref stream) = self.stream {
            let tracks = stream.get_tracks();
            for i in 0..tracks.length() {
                let track = tracks.get(i);
                if let Ok(track) = track.dyn_into::<web_sys::MediaStreamTrack>() {
                    track.stop();
                }
            }
        }

        self.stream = None;
        self.source_node = None;
        self.processor_node = None;
        self._callback = None;
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
