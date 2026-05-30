use crate::orchestrator::{OrchestratorBridge, OrchestratorJobManager};
use crate::voice_loop::audio::{PlaybackBuffer, enqueue_audio_delta, playback_depth_ms};
use futures_util::{Sink, SinkExt};
use std::collections::VecDeque;
use std::fmt::Display;
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::Message;

pub(super) async fn send_or_defer_realtime_event<S>(
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

pub(super) async fn flush_deferred_response_create<S>(
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

pub(super) async fn handle_realtime_event(
    value: serde_json::Value,
    orchestrator_bridge: &mut OrchestratorBridge,
    orchestrator_jobs: &OrchestratorJobManager,
    response_active: &mut bool,
    playback: Arc<Mutex<PlaybackBuffer>>,
    output_rate: u32,
) -> Result<Vec<serde_json::Value>, String> {
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
    orchestrator_bridge
        .handle_realtime_event(&value, orchestrator_jobs)
        .await
}
