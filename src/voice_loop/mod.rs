mod audio;
mod reset;
mod session;
pub(crate) mod settings;

use crate::control::ResetSignal;
use crate::orchestrator::{
    OpenAiSummarizer, OrchestratorBridge, OrchestratorJobManager, OrchestratorProvider,
};
use audio::{
    AudioChunk, PlaybackBuffer, enqueue_audio_delta, i16_to_le_bytes, playback_depth_ms,
    resample_i16, start_input_stream, start_output_stream,
};
use reset::{ConversationItemTracker, ResetTrigger, build_reset_events, item_id_from_event};
use settings::ResolvedVoiceSettings;
use base64::Engine;
use futures_util::{Sink, SinkExt, StreamExt};
use serde_json::{Value, json};
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

    // Boot-time sub-agent discovery: probe every other live gamechat socket
    // for its active slugs and seed the local progress store so this
    // instance is aware of background work running elsewhere. Best-effort:
    // any failures are logged and ignored.
    if config.voice_settings.discover_existing_subagents {
        let discovered = crate::control::discover_existing_subagents().await;
        crate::control::seed_discovered_subagents(
            &orchestrator_jobs.progress_store(),
            &discovered,
        );
    } else {
        eprintln!("boot discovery disabled by settings.discover_existing_subagents=false");
    }

    let (reset_tx, reset_rx) = mpsc::unbounded_channel::<ResetSignal>();

    // Best-effort: keep the realtime loop running even if the control socket
    // fails to bind (read-only headless environments, etc).
    let _control_handle = match crate::control::spawn_server(
        orchestrator_jobs.progress_store(),
        provider_name,
        reset_tx,
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
        reset_rx,
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
    reset_rx: mpsc::UnboundedReceiver<ResetSignal>,
}

async fn run_voice_loop(mut state: VoiceLoop<'_>) -> Result<(), String> {
    let mut response_active = false;
    let mut deferred_response_creates = VecDeque::<serde_json::Value>::new();
    let mut item_tracker = ConversationItemTracker::new();

    let session_update = session::session_update_json_for(&state.model, &state.voice_settings);

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
        .send(Message::Text(session_update.to_string().into()))
        .await
        .map_err(|e| format!("failed to send session.update: {e}"))?;
    eprintln!("connected. Speak into your microphone. Press Ctrl-C to stop.");

    loop {
        tokio::select! {
            Some(chunk) = state.mic_rx.recv() => {
                if response_active || playback_depth_ms(&state.playback, state.output_rate).unwrap_or(0) > 50 {
                    continue;
                }
                let pcm24 = resample_i16(&chunk.samples, chunk.sample_rate, 24_000);
                if pcm24.is_empty() {
                    continue;
                }
                let bytes = i16_to_le_bytes(&pcm24);
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
            Some(signal) = state.reset_rx.recv() => {
                eprintln!(
                    "voice loop received external reset signal reason={}",
                    signal.reason.as_deref().unwrap_or("unspecified")
                );
                perform_reset(
                    &mut write,
                    &mut item_tracker,
                    &session_update,
                    &mut response_active,
                    &mut deferred_response_creates,
                    ResetTrigger::Control,
                )
                .await?;
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
                let dispatch = handle_realtime_event(
                    value,
                    &mut state.orchestrator_bridge,
                    &state.orchestrator_jobs,
                    &mut response_active,
                    Arc::clone(&state.playback),
                    state.output_rate,
                    &mut item_tracker,
                )
                .await?;
                for event in dispatch.outbound {
                    send_or_defer_realtime_event(
                        &mut write,
                        event,
                        &mut response_active,
                        &mut deferred_response_creates,
                    )
                    .await?;
                }
                if dispatch.reset_requested {
                    perform_reset(
                        &mut write,
                        &mut item_tracker,
                        &session_update,
                        &mut response_active,
                        &mut deferred_response_creates,
                        ResetTrigger::Voice,
                    )
                    .await?;
                }
                if state.voice_settings.auto_reset_after_items > 0
                    && item_tracker.len() >= state.voice_settings.auto_reset_after_items
                {
                    eprintln!(
                        "voice context auto-reset triggered tracked_items={} threshold={}",
                        item_tracker.len(),
                        state.voice_settings.auto_reset_after_items
                    );
                    perform_reset(
                        &mut write,
                        &mut item_tracker,
                        &session_update,
                        &mut response_active,
                        &mut deferred_response_creates,
                        ResetTrigger::Auto,
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

async fn perform_reset<S>(
    write: &mut S,
    item_tracker: &mut ConversationItemTracker,
    session_update: &Value,
    response_active: &mut bool,
    deferred_response_creates: &mut VecDeque<Value>,
    trigger: ResetTrigger,
) -> Result<(), String>
where
    S: Sink<Message> + Unpin,
    S::Error: Display,
{
    let drained = item_tracker.drain();
    let plan = build_reset_events(&drained, session_update, *response_active);
    eprintln!(
        "voice context reset trigger={} cleared_items={} response_was_active={}",
        trigger.as_str(),
        plan.cleared_items,
        *response_active
    );
    // After a reset the deferred response queue is no longer meaningful — the
    // realtime conversation it would have continued has been wiped.
    if !deferred_response_creates.is_empty() {
        eprintln!(
            "voice context reset dropping deferred response.creates count={}",
            deferred_response_creates.len()
        );
        deferred_response_creates.clear();
    }
    *response_active = false;
    for event in plan.events {
        write
            .send(Message::Text(event.to_string().into()))
            .await
            .map_err(|e| format!("failed to send reset event: {e}"))?;
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

/// Outcome of processing one Realtime event from the server. The voice loop
/// uses this to decide both what to send back over the websocket and whether
/// a context-reset cycle should run before the next select tick.
struct EventDispatch {
    outbound: Vec<Value>,
    reset_requested: bool,
}

async fn handle_realtime_event(
    value: serde_json::Value,
    orchestrator_bridge: &mut OrchestratorBridge,
    orchestrator_jobs: &OrchestratorJobManager,
    response_active: &mut bool,
    playback: Arc<Mutex<PlaybackBuffer>>,
    output_rate: u32,
    item_tracker: &mut ConversationItemTracker,
) -> Result<EventDispatch, String> {
    let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match event_type {
        "session.created" | "session.updated" => {
            eprintln!("realtime event: {event_type}");
        }
        "response.created" => {
            *response_active = true;
            eprintln!("realtime event: response.created");
        }
        "response.done" => {
            *response_active = false;
            eprintln!(
                "realtime event: response.done playback_depth_ms={}",
                playback_depth_ms(&playback, output_rate).unwrap_or(0)
            );
        }
        "response.output_audio.delta" | "response.audio.delta" => {
            if let Some(delta) = value.get("delta").and_then(|v| v.as_str()) {
                enqueue_audio_delta(delta, playback, output_rate)?;
            }
        }
        "error" => {
            eprintln!("Realtime error: {}", value);
        }
        _ => {}
    }

    if let Some(id) = item_id_from_event(&value) {
        item_tracker.record(id);
    }

    let (reset_outbound, reset_requested) = handle_reset_voice_context_call(&value);

    let mut outbound = reset_outbound;
    let mut orchestrator_events = orchestrator_bridge
        .handle_realtime_event(&value, orchestrator_jobs)
        .await?;
    outbound.append(&mut orchestrator_events);

    Ok(EventDispatch {
        outbound,
        reset_requested,
    })
}

/// Detect a `reset_voice_context` function-call event and return the
/// function-output ack that should be sent back to the model, plus a flag
/// that tells the voice loop to actually run the reset sequence.
fn handle_reset_voice_context_call(value: &Value) -> (Vec<Value>, bool) {
    let call = match value.get("type").and_then(|v| v.as_str()) {
        Some("response.function_call_arguments.done") => Some(value.clone()),
        Some("response.output_item.done") => function_call_payload_from_output_item(value),
        _ => None,
    };
    let Some(call) = call else {
        return (Vec::new(), false);
    };
    if call.get("name").and_then(|v| v.as_str()) != Some("reset_voice_context") {
        return (Vec::new(), false);
    }
    let call_id = match call.get("call_id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => {
            eprintln!("reset_voice_context call ignored: missing call_id");
            return (Vec::new(), false);
        }
    };
    let reason = call
        .get("arguments")
        .and_then(|v| v.as_str())
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|args| {
            args.get("reason")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "unspecified".to_string());
    eprintln!(
        "realtime event: reset_voice_context call_id={call_id} reason={reason}"
    );
    let ack = json!({
        "type": "conversation.item.create",
        "item": {
            "type": "function_call_output",
            "call_id": call_id,
            "output": json!({
                "ok": true,
                "reset": true,
                "reason": reason,
                "instruction": "Conversation context has been reset. Continue speaking naturally without referring to prior turns."
            }).to_string(),
        }
    });
    (vec![ack], true)
}

fn function_call_payload_from_output_item(value: &Value) -> Option<Value> {
    let item = value.get("item")?;
    if item.get("type").and_then(|v| v.as_str()) != Some("function_call") {
        return None;
    }
    Some(json!({
        "type": "response.function_call_arguments.done",
        "name": item.get("name").cloned().unwrap_or_default(),
        "call_id": item.get("call_id").cloned().unwrap_or_default(),
        "arguments": item.get("arguments").cloned().unwrap_or_default(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_voice_context_call_returns_ack_and_flag() {
        let event = json!({
            "type": "response.function_call_arguments.done",
            "name": "reset_voice_context",
            "call_id": "call_reset_1",
            "arguments": "{\"reason\":\"context_overload\"}"
        });
        let (events, reset) = handle_reset_voice_context_call(&event);
        assert!(reset);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["item"]["call_id"], "call_reset_1");
        let output = events[0]["item"]["output"].as_str().unwrap();
        let payload: Value = serde_json::from_str(output).unwrap();
        assert_eq!(payload["reset"], true);
        assert_eq!(payload["reason"], "context_overload");
    }

    #[test]
    fn reset_voice_context_call_handles_missing_reason() {
        let event = json!({
            "type": "response.function_call_arguments.done",
            "name": "reset_voice_context",
            "call_id": "call_reset_2",
            "arguments": "{}"
        });
        let (events, reset) = handle_reset_voice_context_call(&event);
        assert!(reset);
        let output: Value =
            serde_json::from_str(events[0]["item"]["output"].as_str().unwrap()).unwrap();
        assert_eq!(output["reason"], "unspecified");
    }

    #[test]
    fn non_reset_function_calls_are_ignored() {
        let event = json!({
            "type": "response.function_call_arguments.done",
            "name": "delegate_to_orchestrator",
            "call_id": "call_delegate",
            "arguments": "{}"
        });
        let (events, reset) = handle_reset_voice_context_call(&event);
        assert!(!reset);
        assert!(events.is_empty());
    }

    #[test]
    fn reset_voice_context_via_output_item_done_is_handled() {
        let event = json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "name": "reset_voice_context",
                "call_id": "call_reset_3",
                "arguments": "{\"reason\":\"user_requested\"}"
            }
        });
        let (events, reset) = handle_reset_voice_context_call(&event);
        assert!(reset);
        assert_eq!(events.len(), 1);
        let output: Value =
            serde_json::from_str(events[0]["item"]["output"].as_str().unwrap()).unwrap();
        assert_eq!(output["reason"], "user_requested");
    }
}
