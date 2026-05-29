mod audio;
mod session;
pub(crate) mod settings;

use crate::orchestrator::{
    OpenAiSummarizer, OrchestratorBridge, OrchestratorJobManager, OrchestratorProvider,
};
use audio::{
    AudioChunk, PlaybackBuffer, clear_playback, duck_samples, enqueue_audio_delta, i16_to_le_bytes,
    playback_depth_ms, resample_i16, start_input_stream, start_output_stream,
};
use settings::ResolvedVoiceSettings;
use base64::Engine;
use futures_util::{Sink, SinkExt, StreamExt};
use serde_json::json;
use std::collections::VecDeque;
use std::fmt::Display;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

// Owns the live microphone/websocket/playback select loop. Orchestrator jobs are
// visible here only as bridge events that produce Realtime API messages.
pub(crate) use session::session_update_json_for;

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
    let _control_handle = match crate::control::spawn_server(
        orchestrator_jobs.progress_store(),
        provider_name,
    ) {
        Ok(handle) => Some(handle),
        Err(err) => {
            eprintln!("control socket disabled: {err}");
            None
        }
    };

    // Best-effort: keep the realtime loop running even if the control socket
    // fails to bind (read-only headless environments, etc).
    let _control_handle = match crate::control::spawn_server(
        orchestrator_jobs.progress_store(),
        provider_name,
    ) {
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

    run_voice_loop(VoiceLoop {
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

struct VoiceLoop<'a> {
    openai_api_key: String,
    model: String,
    voice_settings: ResolvedVoiceSettings,
    mic_rx: &'a mut mpsc::UnboundedReceiver<AudioChunk>,
    playback: Arc<Mutex<PlaybackBuffer>>,
    output_rate: u32,
    orchestrator_jobs: OrchestratorJobManager,
    orchestrator_bridge: OrchestratorBridge,
}

async fn run_voice_loop(mut state: VoiceLoop<'_>) -> Result<(), String> {
    let mut response_active = false;
    let mut deferred_response_creates = VecDeque::<serde_json::Value>::new();
    // Barge-in bookkeeping. `current_item_id` is the id of the assistant
    // message item that's currently being voiced; `enqueued_assistant_ms`
    // is the total assistant audio (in wall-clock ms) we've handed to the
    // local playback buffer since the response started. Both reset on
    // response.done. On input_audio_buffer.speech_started while a response
    // is active we use them to emit conversation.item.truncate with an
    // accurate audio_end_ms.
    let mut current_item_id: Option<String> = None;
    let mut enqueued_assistant_ms: u64 = 0;
    let mic_ducking_gain = state.voice_settings.mic_ducking_gain;

    let url = format!("wss://api.openai.com/v1/realtime?model={}", state.model);
    let mut request = url
        .into_client_request()
        .map_err(|e| format!("failed to build websocket request: {e}"))?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", state.openai_api_key)
            .parse()
            .map_err(|e| format!("failed to build authorization header: {e}"))?,
    );

    eprintln!("connecting to OpenAI Realtime model {}...", state.model);
    let (ws, _) = connect_async(request)
        .await
        .map_err(|e| format!("failed to connect to OpenAI Realtime: {e}"))?;
    let (mut write, mut read) = ws.split();

    write
        .send(Message::Text(
            session::session_update_json_for(&state.model, &state.voice_settings)
                .to_string()
                .into(),
        ))
        .await
        .map_err(|e| format!("failed to send session.update: {e}"))?;
    eprintln!("connected. Speak into your microphone. Press Ctrl-C to stop.");

    loop {
        tokio::select! {
            Some(chunk) = state.mic_rx.recv() => {
                let pcm24 = resample_i16(&chunk.samples, chunk.sample_rate, 24_000);
                if pcm24.is_empty() {
                    continue;
                }
                // While the assistant is speaking (or its audio is still
                // draining out of the local playback buffer), duck the mic
                // so speaker echo stays below the server VAD threshold, but
                // keep streaming so a deliberate user utterance can still
                // trigger input_audio_buffer.speech_started for barge-in.
                let speaking = response_active
                    || playback_depth_ms(&state.playback, state.output_rate).unwrap_or(0) > 50;
                let pcm_to_send = if speaking {
                    if mic_ducking_gain <= 0.0 {
                        // Legacy behavior: fully gate the mic during
                        // assistant speech (opt-in, disables barge-in).
                        continue;
                    }
                    if mic_ducking_gain >= 1.0 {
                        pcm24
                    } else {
                        duck_samples(&pcm24, mic_ducking_gain)
                    }
                } else {
                    pcm24
                };
                let bytes = i16_to_le_bytes(&pcm_to_send);
                let audio = base64::engine::general_purpose::STANDARD.encode(bytes);
                let event = json!({
                    "type": "input_audio_buffer.append",
                    "audio": audio,
                });
                if let Err(e) = write.send(Message::Text(event.to_string().into())).await {
                    return Err(format!("failed to stream microphone audio: {e}"));
                }
            }
            Some(event) = state.orchestrator_jobs.next_event() => {
                for event in state.orchestrator_bridge.realtime_events_for_job_event(event) {
                    send_or_defer_realtime_event(
                        &mut write,
                        event,
                        &mut response_active,
                        &mut deferred_response_creates,
                    )
                    .await?;
                }
            }
            msg = read.next() => {
                let Some(msg) = msg else {
                    return Err("Realtime websocket closed".to_string());
                };
                let msg = msg.map_err(|e| format!("Realtime websocket error: {e}"))?;
                let Message::Text(text) = msg else {
                    continue;
                };
                let value: serde_json::Value = serde_json::from_str(&text)
                    .map_err(|e| format!("invalid Realtime event JSON: {e}: {text}"))?;
                let events = handle_realtime_event(
                    value,
                    &mut state.orchestrator_bridge,
                    &state.orchestrator_jobs,
                    &mut response_active,
                    &mut current_item_id,
                    &mut enqueued_assistant_ms,
                    Arc::clone(&state.playback),
                    state.output_rate,
                )
                .await?;
                for event in events {
                    send_or_defer_realtime_event(
                        &mut write,
                        event,
                        &mut response_active,
                        &mut deferred_response_creates,
                    )
                    .await?;
                }
                flush_deferred_response_create(
                    &mut write,
                    &mut response_active,
                    &mut deferred_response_creates,
                )
                .await?;
            }
            result = tokio::signal::ctrl_c() => {
                result.map_err(|e| format!("failed to listen for Ctrl-C: {e}"))?;
                eprintln!("stopping realtime voice service");
                break;
            }
        }
    }

    Ok(())
}

async fn send_or_defer_realtime_event<S>(
    write: &mut S,
    event: serde_json::Value,
    response_active: &mut bool,
    deferred_response_creates: &mut VecDeque<serde_json::Value>,
) -> Result<(), String>
where
    S: Sink<Message> + Unpin,
    S::Error: Display,
{
    if event.get("type").and_then(|v| v.as_str()) == Some("response.create") && *response_active {
        eprintln!("realtime response.create deferred because a response is active");
        deferred_response_creates.push_back(event);
        return Ok(());
    }
    if event.get("type").and_then(|v| v.as_str()) == Some("response.create") {
        eprintln!("realtime response.create send");
        *response_active = true;
    }
    write
        .send(Message::Text(event.to_string().into()))
        .await
        .map_err(|e| format!("failed to send realtime event: {e}"))
}

async fn flush_deferred_response_create<S>(
    write: &mut S,
    response_active: &mut bool,
    deferred_response_creates: &mut VecDeque<serde_json::Value>,
) -> Result<(), String>
where
    S: Sink<Message> + Unpin,
    S::Error: Display,
{
    if *response_active {
        return Ok(());
    }
    let Some(event) = deferred_response_creates.pop_front() else {
        return Ok(());
    };
    eprintln!(
        "realtime response.create send deferred remaining={}",
        deferred_response_creates.len()
    );
    *response_active = true;
    write
        .send(Message::Text(event.to_string().into()))
        .await
        .map_err(|e| format!("failed to send deferred response.create: {e}"))
}

async fn handle_realtime_event(
    value: serde_json::Value,
    orchestrator_bridge: &mut OrchestratorBridge,
    orchestrator_jobs: &OrchestratorJobManager,
    response_active: &mut bool,
    current_item_id: &mut Option<String>,
    enqueued_assistant_ms: &mut u64,
    playback: Arc<Mutex<PlaybackBuffer>>,
    output_rate: u32,
) -> Result<Vec<serde_json::Value>, String> {
    let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let mut emitted: Vec<serde_json::Value> = Vec::new();
    match event_type {
        "session.created" | "session.updated" => {
            eprintln!("realtime event: {event_type}");
        }
        "response.created" => {
            *response_active = true;
            *current_item_id = None;
            *enqueued_assistant_ms = 0;
            eprintln!("realtime event: response.created");
        }
        "response.output_item.added" => {
            // Capture the assistant message item id so we can truncate it
            // accurately on barge-in. Only the first audio-bearing item per
            // response is tracked; subsequent items in the same response
            // overwrite (the most recently active item is what would be
            // mid-flight when the user speaks).
            if let Some(id) = value
                .get("item")
                .and_then(|item| item.get("id"))
                .and_then(|id| id.as_str())
            {
                *current_item_id = Some(id.to_string());
            }
        }
        "response.done" => {
            *response_active = false;
            *current_item_id = None;
            *enqueued_assistant_ms = 0;
            eprintln!(
                "realtime event: response.done playback_depth_ms={}",
                playback_depth_ms(&playback, output_rate).unwrap_or(0)
            );
        }
        "response.output_audio.delta" | "response.audio.delta" => {
            if let Some(delta) = value.get("delta").and_then(|v| v.as_str()) {
                let samples_24k = enqueue_audio_delta(delta, Arc::clone(&playback), output_rate)?;
                // 24 kHz mono: 24 samples == 1 ms.
                let delta_ms = (samples_24k / 24) as u64;
                *enqueued_assistant_ms = enqueued_assistant_ms.saturating_add(delta_ms);
            }
        }
        "input_audio_buffer.speech_started" => {
            // Server-side VAD detected user speech. If the assistant is
            // mid-response, cancel it, mark the played duration on the
            // assistant item, and drop any audio still in the local buffer
            // so the user hears their barge-in immediately.
            if *response_active {
                let remaining_ms = playback_depth_ms(&playback, output_rate).unwrap_or(0) as u64;
                let played_ms = enqueued_assistant_ms.saturating_sub(remaining_ms);
                eprintln!(
                    "realtime barge-in: speech_started enqueued_ms={} remaining_ms={} played_ms={}",
                    enqueued_assistant_ms, remaining_ms, played_ms
                );
                emitted.push(json!({ "type": "response.cancel" }));
                if let Some(item_id) = current_item_id.take() {
                    emitted.push(json!({
                        "type": "conversation.item.truncate",
                        "item_id": item_id,
                        "content_index": 0,
                        "audio_end_ms": played_ms,
                    }));
                }
                clear_playback(&playback);
                // Keep response_active=true until the server confirms the
                // cancellation with response.done; that's our source of
                // truth for "the assistant turn is over".
            }
        }
        "error" => {
            eprintln!("Realtime error: {}", value);
        }
        _ => {}
    }
    let mut bridge_events = orchestrator_bridge
        .handle_realtime_event(&value, orchestrator_jobs)
        .await?;
    emitted.append(&mut bridge_events);
    Ok(emitted)
}
