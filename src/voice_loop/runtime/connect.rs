use crate::voice_loop::settings::ResolvedVoiceSettings;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use std::collections::VecDeque;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type RealtimeSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The live Realtime socket split into its send/receive halves, plus the
/// response-pacing state the select loop threads through every event.
pub(super) struct RealtimeConnection {
    pub write: SplitSink<RealtimeSocket, Message>,
    pub read: SplitStream<RealtimeSocket>,
    pub response_active: bool,
    pub deferred_response_creates: VecDeque<serde_json::Value>,
}

/// Opens the Realtime websocket, pushes the initial `session.update`, and hands
/// back a ready-to-drive connection with fresh response-pacing state.
pub(super) async fn handle_config(
    model: &str,
    api_key: &str,
    voice_settings: &ResolvedVoiceSettings,
) -> Result<RealtimeConnection, String> {
    let url = format!("wss://api.openai.com/v1/realtime?model={model}");
    let mut request = url
        .into_client_request()
        .map_err(|e| format!("failed to build websocket request: {e}"))?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {api_key}")
            .parse()
            .map_err(|e| format!("failed to build authorization header: {e}"))?,
    );

    eprintln!("connecting to OpenAI Realtime model {model}...");
    let (ws, _) = connect_async(request)
        .await
        .map_err(|e| format!("failed to connect to OpenAI Realtime: {e}"))?;
    let (mut write, read) = ws.split();

    write
        .send(Message::Text(
            super::tools::session_update_json_for(model, voice_settings)
                .to_string()
                .into(),
        ))
        .await
        .map_err(|e| format!("failed to send session.update: {e}"))?;

    Ok(RealtimeConnection {
        write,
        read,
        response_active: false,
        deferred_response_creates: VecDeque::new(),
    })
}
