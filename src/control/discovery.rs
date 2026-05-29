//! Boot-time sub-agent discovery.
//!
//! On startup the voice loop scans the runtime directory for sockets owned by
//! other live gamechat processes, sends each a `List` request, and records
//! whatever active slugs come back. The local progress store is then seeded
//! with a stub entry per discovered slug so this instance can:
//!
//! - log who else is doing background work in this user account
//! - surface those slugs in `gamechat inspect` alongside its own jobs
//! - tell the voice model "there's already a refactor_docs agent running in
//!   pid 12345" when the user asks
//!
//! The seeded entries are clearly marked as `discovered:<pid>` providers so
//! they can't be confused with locally-owned jobs. They are read-only; we do
//! not try to drive remote work from this instance.

use super::protocol::{Request, Response};
use super::runtime_dir::{discover_sockets, socket_path_for_pid};
use crate::orchestrator::progress::{ProgressStore, SlugSummary};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::{Duration, timeout};

/// How long we'll wait on any one peer before giving up. Keeps a hung peer
/// from blocking startup forever.
const PEER_QUERY_TIMEOUT: Duration = Duration::from_millis(750);

/// A sub-agent learned about from another running gamechat instance.
#[derive(Debug, Clone)]
pub(crate) struct DiscoveredSubagent {
    pub pid: u32,
    // Held so callers can re-open the peer socket (e.g. to tail a slug).
    // Currently only used for logging by the discovery printer; suppress
    // the dead-code warning so we don't have to give the field a leading
    // underscore that would break the public-looking field name.
    #[allow(dead_code)]
    pub socket: PathBuf,
    pub summary: SlugSummary,
}

/// Scan the runtime dir and probe every other live gamechat socket. The
/// current process is excluded so we never recurse. All failures are logged
/// and dropped; the return value is a best-effort union of what came back.
pub(crate) async fn discover_existing_subagents() -> Vec<DiscoveredSubagent> {
    let own_pid = std::process::id();
    let sockets = match discover_sockets() {
        Ok(sockets) => sockets,
        Err(err) => {
            eprintln!("boot discovery skipped: failed to scan runtime dir: {err}");
            return Vec::new();
        }
    };

    let own_socket = socket_path_for_pid(own_pid).ok();
    let mut discovered: Vec<DiscoveredSubagent> = Vec::new();
    let mut peers_queried: usize = 0;
    let mut peers_with_slugs: usize = 0;

    for socket in sockets {
        if own_socket.as_ref().map(|p| p == &socket).unwrap_or(false) {
            continue;
        }
        let pid = match pid_for_socket(&socket) {
            Some(pid) => pid,
            None => continue,
        };
        if pid == own_pid {
            continue;
        }
        peers_queried += 1;
        match query_peer(&socket).await {
            Ok(slugs) if slugs.is_empty() => {
                eprintln!(
                    "boot discovery peer pid={pid} socket={} has no active sub-agents",
                    socket.display()
                );
            }
            Ok(slugs) => {
                peers_with_slugs += 1;
                eprintln!(
                    "boot discovery peer pid={pid} socket={} active_slugs={}",
                    socket.display(),
                    slugs.len()
                );
                for summary in slugs {
                    discovered.push(DiscoveredSubagent {
                        pid,
                        socket: socket.clone(),
                        summary,
                    });
                }
            }
            Err(err) => {
                eprintln!(
                    "boot discovery peer pid={pid} socket={} failed: {err}",
                    socket.display()
                );
            }
        }
    }
    eprintln!(
        "boot discovery done peers_queried={} peers_with_slugs={} discovered_total={}",
        peers_queried,
        peers_with_slugs,
        discovered.len()
    );
    discovered
}

/// Stamp discovered slugs into the local progress store so they show up
/// alongside locally-owned jobs. Each stub uses a `discovered:<pid>`
/// provider tag so it's obvious where it came from and so a future tool
/// call won't accidentally treat it as resumable.
pub(crate) fn seed_discovered_subagents(store: &ProgressStore, discovered: &[DiscoveredSubagent]) {
    if discovered.is_empty() {
        return;
    }
    for entry in discovered {
        let provider_tag = format!("discovered:{}", entry.pid);
        let local_slug = format!("peer_{}_{}", entry.pid, entry.summary.slug);
        store.register_job(&local_slug, leak_provider_tag(provider_tag));
        let elapsed = entry.summary.elapsed_seconds.round() as u64;
        let last = if entry.summary.last_message.trim().is_empty() {
            "(no recent message)".to_string()
        } else {
            entry.summary.last_message.clone()
        };
        store.push_progress(
            &local_slug,
            &format!(
                "Discovered at boot from pid {} (peer slug {}, status {}, elapsed {}s). Last message: {}",
                entry.pid, entry.summary.slug, entry.summary.status, elapsed, last
            ),
        );
        if let Some(session_id) = entry.summary.session_id.as_deref() {
            store.set_session_id(&local_slug, session_id);
        }
    }
}

/// `ProgressStore::register_job` wants `&'static str` for the provider name
/// so it can be cheaply re-stored. Discovered providers are only known at
/// runtime, so we intentionally leak the string. The set of distinct
/// `discovered:<pid>` tags is bounded by the number of peer gamechat
/// processes ever seen during this binary's lifetime — at most a handful.
fn leak_provider_tag(tag: String) -> &'static str {
    Box::leak(tag.into_boxed_str())
}

fn pid_for_socket(path: &Path) -> Option<u32> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse::<u32>().ok())
}

async fn query_peer(socket: &Path) -> Result<Vec<SlugSummary>, String> {
    let task = async {
        let stream = UnixStream::connect(socket)
            .await
            .map_err(|e| format!("connect failed: {e}"))?;
        let (read_half, mut write_half) = stream.into_split();
        let mut payload = serde_json::to_string(&Request::List)
            .map_err(|e| format!("serialize list request failed: {e}"))?;
        payload.push('\n');
        write_half
            .write_all(payload.as_bytes())
            .await
            .map_err(|e| format!("write list request failed: {e}"))?;
        write_half
            .shutdown()
            .await
            .map_err(|e| format!("half-close failed: {e}"))?;

        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("read response failed: {e}"))?;
        let response: Response = serde_json::from_str(line.trim())
            .map_err(|e| format!("parse response failed: {e}: {line}"))?;
        match response {
            Response::List { slugs } => Ok(slugs),
            Response::Error { message } => Err(format!("peer reported error: {message}")),
            other => Err(format!("unexpected response: {other:?}")),
        }
    };
    match timeout(PEER_QUERY_TIMEOUT, task).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "peer query exceeded {}ms",
            PEER_QUERY_TIMEOUT.as_millis()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::progress::JobStatus;

    fn fake_summary(slug: &str) -> SlugSummary {
        SlugSummary {
            slug: slug.to_string(),
            provider: "claude".to_string(),
            status: JobStatus::Running,
            last_message: "doing the thing".to_string(),
            elapsed_seconds: 12.0,
            session_id: Some("sess-99".to_string()),
            last_seq: Some(2),
        }
    }

    #[test]
    fn seed_writes_each_discovered_slug_into_store() {
        let store = ProgressStore::new();
        let entries = vec![
            DiscoveredSubagent {
                pid: 42,
                socket: PathBuf::from("/tmp/42.sock"),
                summary: fake_summary("refactor_docs"),
            },
            DiscoveredSubagent {
                pid: 99,
                socket: PathBuf::from("/tmp/99.sock"),
                summary: fake_summary("scan_repo"),
            },
        ];
        seed_discovered_subagents(&store, &entries);
        let summaries = store.snapshot_all();
        assert_eq!(summaries.len(), 2);
        // Sorted by slug.
        assert_eq!(summaries[0].slug, "peer_42_refactor_docs");
        assert!(summaries[0].provider.starts_with("discovered:42"));
        assert_eq!(summaries[0].session_id.as_deref(), Some("sess-99"));
        assert_eq!(summaries[1].slug, "peer_99_scan_repo");
    }

    #[test]
    fn seed_noop_for_empty_input() {
        let store = ProgressStore::new();
        seed_discovered_subagents(&store, &[]);
        assert!(store.snapshot_all().is_empty());
    }

    #[test]
    fn seed_falls_back_to_placeholder_when_no_last_message() {
        let store = ProgressStore::new();
        let mut summary = fake_summary("blank");
        summary.last_message = "   ".to_string();
        let entries = vec![DiscoveredSubagent {
            pid: 7,
            socket: PathBuf::from("/tmp/7.sock"),
            summary,
        }];
        seed_discovered_subagents(&store, &entries);
        let summaries = store.snapshot_all();
        assert_eq!(summaries.len(), 1);
        assert!(summaries[0].last_message.contains("(no recent message)"));
    }

    #[tokio::test]
    async fn query_peer_timeouts_when_no_one_listens() {
        // Build a path that does not have a listener bound; the connect
        // attempt will fail fast (which is fine — we just need to assert
        // we return an Err rather than hanging).
        let path = std::env::temp_dir().join(format!(
            "gamechat-test-nopeer-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let err = query_peer(&path).await.expect_err("should fail");
        assert!(
            err.contains("connect failed") || err.contains("exceeded"),
            "unexpected error: {err}"
        );
    }
}
