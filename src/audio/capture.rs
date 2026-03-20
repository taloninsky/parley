use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    AudioContext, AudioContextOptions, MediaStream, MediaStreamAudioSourceNode,
    ScriptProcessorNode,
};

/// Handle to an active browser microphone capture session.
/// Dropping this will NOT automatically stop capture — call stop() explicitly.
pub struct BrowserCapture {
    audio_context: AudioContext,
    _source_node: MediaStreamAudioSourceNode,
    processor_node: ScriptProcessorNode,
    media_stream: MediaStream,
}

impl BrowserCapture {
    /// Request microphone access and begin capturing PCM samples.
    /// `on_audio` is called with chunks of f32 PCM data (mono, 16kHz).
    pub async fn start(
        on_audio: impl Fn(Vec<f32>) + 'static,
    ) -> Result<Self, JsValue> {
        let window = web_sys::window().ok_or("no window")?;
        let navigator = window.navigator();
        let media_devices = navigator.media_devices()?;

        // Request audio-only stream
        let constraints = web_sys::MediaStreamConstraints::new();
        constraints.set_audio(&JsValue::TRUE);
        constraints.set_video(&JsValue::FALSE);

        let stream_promise = media_devices.get_user_media_with_constraints(&constraints)?;
        let stream_js = wasm_bindgen_futures::JsFuture::from(stream_promise).await?;
        let media_stream: MediaStream = stream_js.unchecked_into();

        // Create AudioContext at 16kHz (AssemblyAI's expected sample rate)
        let ctx_options = AudioContextOptions::new();
        ctx_options.set_sample_rate(16_000.0);
        let audio_context = AudioContext::new_with_context_options(&ctx_options)?;

        // Connect stream to a ScriptProcessorNode for raw PCM access
        let source = audio_context.create_media_stream_source(&media_stream)?;
        // Buffer size 4096, mono input, mono output
        let processor = audio_context.create_script_processor_with_buffer_size_and_number_of_input_channels_and_number_of_output_channels(
            4096, 1, 1,
        )?;

        let callback = Closure::<dyn FnMut(web_sys::AudioProcessingEvent)>::new(
            move |event: web_sys::AudioProcessingEvent| {
                let input_buffer = event.input_buffer().unwrap();
                let channel_data = input_buffer.get_channel_data(0).unwrap();
                on_audio(channel_data);
            },
        );

        processor.set_onaudioprocess(Some(callback.as_ref().unchecked_ref()));
        callback.forget(); // leak — lives for the duration of the capture

        source.connect_with_audio_node(&processor)?;
        processor.connect_with_audio_node(&audio_context.destination().unchecked_into())?;

        Ok(Self {
            audio_context,
            _source_node: source,
            processor_node: processor,
            media_stream,
        })
    }

    /// Stop capture and release microphone.
    pub fn stop(self) {
        let _ = self.processor_node.disconnect();
        let _ = self.audio_context.close();
        // Stop all tracks to release the mic
        let tracks = self.media_stream.get_tracks();
        for i in 0..tracks.length() {
            let track = tracks.get(i);
            if let Ok(track) = track.dyn_into::<web_sys::MediaStreamTrack>() {
                track.stop();
            }
        }
    }
}
