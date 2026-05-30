// Owns the live microphone/websocket/playback select loop. Orchestrator jobs are
// visible here only as bridge events that produce Realtime API messages.
mod connect;
mod dispatch;
mod event_loop;
mod tools;

pub(crate) use tools::session_update_json_for;

use crate::orchestrator::{
    OpenAiSummarizer, OrchestratorBridge, OrchestratorJobManager, OrchestratorProvider,
};
use crate::voice_loop::audio::{
    AudioChunk, PlaybackBuffer, start_input_stream, start_output_stream,
};
use crate::voice_loop::settings::ResolvedVoiceSettings;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub(crate) struct RealtimeRunConfig {
    pub openai_api_key: String,
    pub model: String,
    pub orchestrator_provider: OrchestratorProvider,
    pub voice_settings: ResolvedVoiceSettings,
}

pub(crate) async fn run_realtime_voice(config: RealtimeRunConfig) -> Result<(), String> {
    let provider_name = config.orchestrator_provider.name();
    let orchestrator_jobs = OrchestratorJobManager::spawn(config.orchestrator_provider);
    let summarizer = Arc::new(OpenAiSummarizer::new(config.openai_api_key.clone(), None)?);
    let orchestrator_bridge = OrchestratorBridge::new(summarizer);

    // Best-effort: keep the realtime loop running even if the control socket
    // fails to bind (read-only headless environments, etc).
    let _control_handle =
        match crate::control::spawn_server(orchestrator_jobs.progress_store(), provider_name) {
            Ok(handle) => Some(handle),
            Err(err) => {
                eprintln!("control socket disabled: {err}");
                None
            }
        };

    let playback = Arc::new(Mutex::new(PlaybackBuffer::new()));
    let (_output_stream, output_rate) = start_output_stream(Arc::clone(&playback))?;
    let (mic_tx, mut mic_rx) = mpsc::unbounded_channel::<AudioChunk>();
    let _input_stream = start_input_stream(mic_tx)?;

    event_loop::run_voice_loop(event_loop::VoiceLoop {
        openai_api_key: config.openai_api_key,
        model: config.model,
        voice_settings: config.voice_settings,
        mic_rx: &mut mic_rx,
        playback,
        output_rate,
        orchestrator_jobs,
        orchestrator_bridge,
    })
    .await
}
