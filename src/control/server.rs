//! Unix socket server task that exposes a running gamechat's progress state.
//!
//! Wraps the same `Arc<ProgressStore>` the voice loop already owns. Each
//! incoming connection reads a single JSON line, computes a response, writes
//! it, and closes. No subscription state, no streaming — clients reconnect
//! for the next call (the polling tail loop relies on this).

use super::protocol::{Request, Response};
use super::runtime_dir::socket_path_for_pid;
use crate::orchestrator::progress::{JobStatus, ProgressStore, SlugSummary};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

/// Opaque signal carried from the control server to the voice loop when a
/// reset is requested. The voice loop maps it onto its own `ResetTrigger`.
#[derive(Debug, Clone)]
pub(crate) struct ResetSignal {
    pub reason: Option<String>,
}

/// Handle returned to the voice loop; dropping it removes the socket file.
pub(crate) struct ServerHandle {
    socket_path: PathBuf,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Start the control server. Best-effort: if the runtime dir can't be created
/// or the socket can't be bound, returns Err and the caller logs a warning.
pub(crate) fn spawn_server(
    store: Arc<ProgressStore>,
    provider_name: &'static str,
    reset_tx: mpsc::UnboundedSender<ResetSignal>,
) -> Result<ServerHandle, String> {
    let pid = std::process::id();
    let socket_path = socket_path_for_pid(pid)?;
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create runtime dir {}: {e}", parent.display()))?;
    }
    // Clean up any stale socket from a previous crash with the same pid (unlikely).
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)
        .map_err(|e| format!("failed to bind control socket {}: {e}", socket_path.display()))?;
    eprintln!(
        "control socket listening pid={pid} provider={provider_name} path={}",
        socket_path.display()
    );
    let store_for_task = Arc::clone(&store);
    tokio::spawn(async move {
        accept_loop(listener, store_for_task, provider_name, reset_tx).await;
    });
    Ok(ServerHandle { socket_path })
}

async fn accept_loop(
    listener: UnixListener,
    store: Arc<ProgressStore>,
    provider_name: &'static str,
    reset_tx: mpsc::UnboundedSender<ResetSignal>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let store = Arc::clone(&store);
                let reset_tx = reset_tx.clone();
                tokio::spawn(async move {
                    if let Err(err) =
                        handle_connection(stream, store, provider_name, reset_tx).await
                    {
                        eprintln!("control connection error: {err}");
                    }
                });
            }
            Err(err) => {
                eprintln!("control socket accept failed: {err}");
                break;
            }
        }
    }
}

async fn handle_connection(
    stream: UnixStream,
    store: Arc<ProgressStore>,
    provider_name: &'static str,
    reset_tx: mpsc::UnboundedSender<ResetSignal>,
) -> Result<(), String> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .await
        .map_err(|e| format!("failed to read request: {e}"))?;
    if n == 0 {
        return Ok(());
    }
    let response = match serde_json::from_str::<Request>(line.trim()) {
        Ok(request) => handle_request(request, &store, provider_name, &reset_tx),
        Err(err) => Response::Error {
            message: format!("invalid request: {err}"),
        },
    };
    let mut payload = serde_json::to_string(&response)
        .map_err(|e| format!("failed to serialize response: {e}"))?;
    payload.push('\n');
    write_half
        .write_all(payload.as_bytes())
        .await
        .map_err(|e| format!("failed to write response: {e}"))?;
    write_half
        .flush()
        .await
        .map_err(|e| format!("failed to flush response: {e}"))?;
    Ok(())
}

fn handle_request(
    request: Request,
    store: &ProgressStore,
    provider_name: &'static str,
    reset_tx: &mpsc::UnboundedSender<ResetSignal>,
) -> Response {
    match request {
        Request::Hello => Response::Hello {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            provider: provider_name.to_string(),
        },
        Request::List => Response::List {
            slugs: store.snapshot_all(),
        },
        Request::Tail { slug, after_seq } => match store.entries_after(&slug, after_seq) {
            Some(batch) => Response::Tail {
                entries: batch.entries,
                next_cursor: batch.next_cursor,
                status: batch.status,
                done: batch.done,
            },
            None => Response::Error {
                message: format!("unknown slug: {slug}"),
            },
        },
        Request::Resume { slug } => {
            let summary = store
                .snapshot_all()
                .into_iter()
                .find(|s: &SlugSummary| s.slug == slug);
            match summary {
                Some(s) => Response::Resume {
                    slug: s.slug,
                    provider: s.provider,
                    session_id: s.session_id,
                },
                None => Response::Error {
                    message: format!("unknown slug: {slug}"),
                },
            }
        }
        Request::Reset { reason } => {
            let display_reason = reason.clone().unwrap_or_else(|| "unspecified".to_string());
            eprintln!(
                "control socket reset requested reason={display_reason}"
            );
            let dispatched = reset_tx.send(ResetSignal { reason }).is_ok();
            if !dispatched {
                eprintln!(
                    "control socket reset dropped: voice loop reset channel closed"
                );
            }
            Response::Reset { dispatched }
        }
    }
}

// Suppress an "unused" warning for JobStatus until the client uses it via
// the deserialized Tail response.
#[allow(dead_code)]
fn _force_use_job_status(_: JobStatus) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::progress::ProgressStore;
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn unused_reset_sender() -> mpsc::UnboundedSender<ResetSignal> {
        let (tx, _rx) = mpsc::unbounded_channel();
        tx
    }

    #[tokio::test]
    async fn end_to_end_list_and_tail_over_socket() {
        let store = Arc::new(ProgressStore::new());
        store.register_job("e2e", "claude");
        store.set_running("e2e");
        store.push_progress("e2e", "step one");
        store.set_session_id("e2e", "abc123");

        // Bind a temporary socket inside the test's tmpdir to avoid colliding
        // with the runtime dir layout.
        let dir = tempdir_test();
        let socket_path = dir.join("test.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let store_for_task = Arc::clone(&store);
        let reset_tx = unused_reset_sender();
        tokio::spawn(async move {
            accept_loop(listener, store_for_task, "claude", reset_tx).await
        });

        // List → should see our slug + session_id.
        let resp = roundtrip(&socket_path, Request::List).await;
        match resp {
            Response::List { slugs } => {
                assert_eq!(slugs.len(), 1);
                assert_eq!(slugs[0].slug, "e2e");
                assert_eq!(slugs[0].provider, "claude");
                assert_eq!(slugs[0].session_id.as_deref(), Some("abc123"));
            }
            other => panic!("expected List, got {other:?}"),
        }

        // Tail starting from None returns everything; then cursor-based call
        // returns only newly-pushed entries.
        let resp = roundtrip(
            &socket_path,
            Request::Tail {
                slug: "e2e".into(),
                after_seq: None,
            },
        )
        .await;
        let cursor = match resp {
            Response::Tail {
                entries,
                next_cursor,
                ..
            } => {
                assert_eq!(entries, vec!["step one"]);
                next_cursor
            }
            other => panic!("expected Tail, got {other:?}"),
        };

        store.push_progress("e2e", "step two");
        let resp = roundtrip(
            &socket_path,
            Request::Tail {
                slug: "e2e".into(),
                after_seq: cursor,
            },
        )
        .await;
        match resp {
            Response::Tail { entries, .. } => {
                assert_eq!(entries, vec!["step two"]);
            }
            other => panic!("expected Tail, got {other:?}"),
        }

        // Resume returns the session id we attached.
        let resp = roundtrip(
            &socket_path,
            Request::Resume {
                slug: "e2e".into(),
            },
        )
        .await;
        match resp {
            Response::Resume { session_id, .. } => {
                assert_eq!(session_id.as_deref(), Some("abc123"));
            }
            other => panic!("expected Resume, got {other:?}"),
        }
    }

    async fn roundtrip(socket: &PathBuf, request: Request) -> Response {
        let stream = UnixStream::connect(socket).await.expect("connect");
        let (read_half, mut write_half) = stream.into_split();
        let mut line = serde_json::to_string(&request).unwrap();
        line.push('\n');
        write_half.write_all(line.as_bytes()).await.unwrap();
        write_half.shutdown().await.unwrap();
        let mut reader = BufReader::new(read_half);
        let mut response_line = String::new();
        reader.read_line(&mut response_line).await.unwrap();
        serde_json::from_str(response_line.trim()).expect("valid response")
    }

    fn tempdir_test() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "gamechat-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn list_handler_returns_summary() {
        let store = Arc::new(ProgressStore::new());
        store.register_job("a", "claude");
        store.set_running("a");
        store.push_progress("a", "step");
        store.set_session_id("a", "abc");

        let reset_tx = unused_reset_sender();
        let resp = handle_request(Request::List, &store, "claude", &reset_tx);
        match resp {
            Response::List { slugs } => {
                assert_eq!(slugs.len(), 1);
                assert_eq!(slugs[0].slug, "a");
                assert_eq!(slugs[0].session_id.as_deref(), Some("abc"));
            }
            other => panic!("expected List response, got {other:?}"),
        }
    }

    #[test]
    fn resume_handler_returns_session_id() {
        let store = Arc::new(ProgressStore::new());
        store.register_job("solo", "claude");
        store.set_session_id("solo", "sess-xyz");
        let reset_tx = unused_reset_sender();
        let resp = handle_request(
            Request::Resume {
                slug: "solo".into(),
            },
            &store,
            "claude",
            &reset_tx,
        );
        match resp {
            Response::Resume {
                slug,
                provider,
                session_id,
            } => {
                assert_eq!(slug, "solo");
                assert_eq!(provider, "claude");
                assert_eq!(session_id.as_deref(), Some("sess-xyz"));
            }
            other => panic!("expected Resume, got {other:?}"),
        }
    }

    #[test]
    fn tail_handler_streams_entries() {
        let store = Arc::new(ProgressStore::new());
        store.register_job("t", "claude");
        store.push_progress("t", "one");
        store.push_progress("t", "two");

        let reset_tx = unused_reset_sender();
        let resp = handle_request(
            Request::Tail {
                slug: "t".into(),
                after_seq: None,
            },
            &store,
            "claude",
            &reset_tx,
        );
        match resp {
            Response::Tail {
                entries,
                next_cursor,
                done,
                ..
            } => {
                assert_eq!(entries, vec!["one", "two"]);
                assert!(next_cursor.is_some());
                assert!(!done);
            }
            other => panic!("expected Tail, got {other:?}"),
        }
    }

    #[test]
    fn unknown_slug_in_tail_is_error() {
        let store = Arc::new(ProgressStore::new());
        let reset_tx = unused_reset_sender();
        let resp = handle_request(
            Request::Tail {
                slug: "ghost".into(),
                after_seq: None,
            },
            &store,
            "claude",
            &reset_tx,
        );
        match resp {
            Response::Error { message } => assert!(message.contains("ghost")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn reset_handler_dispatches_to_voice_loop_channel() {
        let store = Arc::new(ProgressStore::new());
        let (reset_tx, mut reset_rx) = mpsc::unbounded_channel::<ResetSignal>();
        let resp = handle_request(
            Request::Reset {
                reason: Some("context_overload".into()),
            },
            &store,
            "claude",
            &reset_tx,
        );
        match resp {
            Response::Reset { dispatched } => assert!(dispatched),
            other => panic!("expected Reset, got {other:?}"),
        }
        let signal = reset_rx
            .try_recv()
            .expect("reset signal should have been forwarded to voice loop");
        assert_eq!(signal.reason.as_deref(), Some("context_overload"));
    }

    #[test]
    fn reset_handler_reports_undispatched_when_channel_closed() {
        let store = Arc::new(ProgressStore::new());
        let (reset_tx, reset_rx) = mpsc::unbounded_channel::<ResetSignal>();
        drop(reset_rx);
        let resp = handle_request(
            Request::Reset { reason: None },
            &store,
            "claude",
            &reset_tx,
        );
        match resp {
            Response::Reset { dispatched } => assert!(!dispatched),
            other => panic!("expected Reset, got {other:?}"),
        }
    }
}
