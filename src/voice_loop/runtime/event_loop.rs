use super::connect::{self, RealtimeConnection};
use super::dispatch;
use crate::orchestrator::jobs::OrchestratorJobEvent;
use crate::orchestrator::{OrchestratorBridge, OrchestratorJobManager};
use crate::voice_loop::audio::{
    AudioChunk, PlaybackBuffer, i16_to_le_bytes, playback_depth_ms, resample_i16,
};
use crate::voice_loop::settings::ResolvedVoiceSettings;
use base64::Engine;
use futures_util::{Sink, SinkExt, StreamExt};
use serde_json::json;
use std::collections::VecDeque;
use std::fmt::Display;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Error, Message};

pub(super) struct VoiceLoop<'a> {
    pub(super) openai_api_key: String,
    pub(super) model: String,
    pub(super) voice_settings: ResolvedVoiceSettings,
    pub(super) mic_rx: &'a mut mpsc::UnboundedReceiver<AudioChunk>,
    pub(super) playback: Arc<Mutex<PlaybackBuffer>>,
    pub(super) output_rate: u32,
    pub(super) orchestrator_jobs: OrchestratorJobManager,
    pub(super) orchestrator_bridge: OrchestratorBridge,
}

pub(super) async fn run_voice_loop(mut state: VoiceLoop<'_>) -> Result<(), String> {
    let mut conn =
        connect::handle_config(&state.model, &state.openai_api_key, &state.voice_settings).await?;
    eprintln!("connected. Speak into your microphone. Press Ctrl-C to stop.");

    loop {
        tokio::select! {
            Some(chunk) = state.mic_rx.recv() => {
                forward_microphone_chunk(
                    &mut conn.write,
                    chunk,
                    conn.response_active,
                    &state.playback,
                    state.output_rate,
                )
                .await?;
            }
            Some(event) = state.orchestrator_jobs.next_event() => {
                iterate_next_events(
                    &mut conn.write,
                    event,
                    &state.orchestrator_bridge,
                    &mut conn.response_active,
                    &mut conn.deferred_response_creates,
                )
                .await?;
            }
            msg = conn.read.next() => {
                handle_next_action(&mut conn, &mut state, msg).await?;
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

/// Resample a fresh microphone chunk to 24 kHz and stream it to OpenAI, unless a
/// response is in flight or playback is still draining (we'd talk over the model).
async fn forward_microphone_chunk<S>(
    write: &mut S,
    chunk: AudioChunk,
    response_active: bool,
    playback: &Arc<Mutex<PlaybackBuffer>>,
    output_rate: u32,
) -> Result<(), String>
where
    S: Sink<Message> + Unpin,
    S::Error: Display,
{
    if response_active || playback_depth_ms(playback, output_rate).unwrap_or(0) > 50 {
        return Ok(());
    }
    let pcm24 = resample_i16(&chunk.samples, chunk.sample_rate, 24_000);
    if pcm24.is_empty() {
        return Ok(());
    }
    let bytes = i16_to_le_bytes(&pcm24);
    let audio = base64::engine::general_purpose::STANDARD.encode(bytes);
    let event = json!({
        "type": "input_audio_buffer.append",
        "audio": audio,
    });
    write
        .send(Message::Text(event.to_string().into()))
        .await
        .map_err(|e| format!("failed to stream microphone audio: {e}"))
}

/// Translate a single orchestrator job event into Realtime messages and send (or
/// defer) each one.
async fn iterate_next_events<S>(
    write: &mut S,
    event: OrchestratorJobEvent,
    orchestrator_bridge: &OrchestratorBridge,
    response_active: &mut bool,
    deferred_response_creates: &mut VecDeque<serde_json::Value>,
) -> Result<(), String>
where
    S: Sink<Message> + Unpin,
    S::Error: Display,
{
    for event in orchestrator_bridge.realtime_events_for_job_event(event) {
        dispatch::send_or_defer_realtime_event(
            write,
            event,
            response_active,
            deferred_response_creates,
        )
        .await?;
    }
    Ok(())
}

/// Handle one inbound websocket frame: parse it, run it through the dispatcher,
/// then send any resulting events and flush a deferred `response.create`.
async fn handle_next_action(
    conn: &mut RealtimeConnection,
    state: &mut VoiceLoop<'_>,
    msg: Option<Result<Message, Error>>,
) -> Result<(), String> {
    let Some(msg) = msg else {
        return Err("Realtime websocket closed".to_string());
    };
    let msg = msg.map_err(|e| format!("Realtime websocket error: {e}"))?;
    let Message::Text(text) = msg else {
        return Ok(());
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("invalid Realtime event JSON: {e}: {text}"))?;
    let events = dispatch::handle_realtime_event(
        value,
        &mut state.orchestrator_bridge,
        &state.orchestrator_jobs,
        &mut conn.response_active,
        Arc::clone(&state.playback),
        state.output_rate,
    )
    .await?;
    for event in events {
        dispatch::send_or_defer_realtime_event(
            &mut conn.write,
            event,
            &mut conn.response_active,
            &mut conn.deferred_response_creates,
        )
        .await?;
    }
    dispatch::flush_deferred_response_create(
        &mut conn.write,
        &mut conn.response_active,
        &mut conn.deferred_response_creates,
    )
    .await?;
    Ok(())
}
