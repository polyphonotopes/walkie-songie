//! Web Audio API integration for microphone input.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{
    AudioContext, AudioProcessingEvent, MediaStream, MediaStreamConstraints, ScriptProcessorNode,
};

/// Buffer size for ScriptProcessorNode (power of 2, 256-16384).
/// 2048 samples at 48kHz = ~42ms of audio per callback.
const BUFFER_SIZE: u32 = 2048;

/// Web audio input handler that captures microphone input.
pub struct WebAudioInput {
    context: AudioContext,
    stream: Option<MediaStream>,
    processor_node: Option<ScriptProcessorNode>,
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
            processor_node: None,
            _callback: None,
            running: false,
        })
    }

    /// Request microphone access and start audio capture.
    /// Raw samples are pushed to the provided buffer for ML processing.
    pub async fn start(&mut self, sample_buffer: Rc<RefCell<Vec<f32>>>) -> Result<(), JsValue> {
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
        let processor = self.context.create_script_processor_with_buffer_size_and_number_of_input_channels_and_number_of_output_channels(
            BUFFER_SIZE,
            1,
            1,
        )?;

        // Set up the audio processing callback
        let callback = Closure::wrap(Box::new(move |event: AudioProcessingEvent| {
            if let Ok(input_buffer) = event.input_buffer() {
                if let Ok(channel_data) = input_buffer.get_channel_data(0) {
                    // Push raw samples to buffer for SwiftF0 ML processing
                    sample_buffer.borrow_mut().extend_from_slice(&channel_data);
                }
            }
        }) as Box<dyn FnMut(AudioProcessingEvent)>);

        processor.set_onaudioprocess(Some(callback.as_ref().unchecked_ref()));

        // Connect: source -> processor -> destination
        source.connect_with_audio_node(&processor)?;
        processor.connect_with_audio_node(&self.context.destination())?;

        self.stream = Some(stream);
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
        self.processor_node = None;
        self._callback = None;
        self.running = false;
    }
}

impl Drop for WebAudioInput {
    fn drop(&mut self) {
        self.stop();
    }
}
