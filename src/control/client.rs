//! Client side of the control protocol — used by the `inspect`, `tail`,
//! `open`, `reset`, and `discover` subcommands. Connects to a Unix socket
//! and prints results.

use super::discovery::discover_existing_subagents;
use super::protocol::{Request, Response};
use super::runtime_dir::{discover_sockets, socket_path_for_pid};
use super::{ControlSubcommand, ControlTarget};
use crate::orchestrator::progress::JobStatus;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::sleep;

pub(super) async fn run(
    subcommand: ControlSubcommand,
    target: ControlTarget,
) -> Result<(), String> {
    // `discover` doesn't talk to a single instance — it surveys the whole
    // runtime dir — so resolve the socket lazily for the other subcommands.
    if let ControlSubcommand::Discover = subcommand {
        return run_discover().await;
    }
    let socket = resolve_socket(&target)?;
    match subcommand {
        ControlSubcommand::Inspect => run_inspect(&socket).await,
        ControlSubcommand::Tail { slug } => run_tail(&socket, &slug).await,
        ControlSubcommand::Open { slug, launch } => run_open(&socket, &slug, launch).await,
        ControlSubcommand::Reset { reason } => run_reset(&socket, reason).await,
        ControlSubcommand::Discover => unreachable!(),
    }
}

fn resolve_socket(target: &ControlTarget) -> Result<PathBuf, String> {
    if let Some(path) = target.socket.as_ref() {
        return Ok(path.clone());
    }
    if let Some(pid) = target.pid {
        return socket_path_for_pid(pid);
    }
    let sockets = discover_sockets()?;
    match sockets.len() {
        0 => Err(
            "no running gamechat instance found. Start one with `gamechat --realtime`, or pass --pid <PID>.".to_string(),
        ),
        1 => Ok(sockets.into_iter().next().unwrap()),
        _ => {
            let names: Vec<String> = sockets
                .iter()
                .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(|s| s.to_string()))
                .collect();
            Err(format!(
                "multiple gamechat instances found (pids: {}). Pass --pid <PID> to choose one.",
                names.join(", ")
            ))
        }
    }
}

async fn send_request(socket: &Path, request: Request) -> Result<Response, String> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|e| format!("failed to connect to {}: {e}", socket.display()))?;
    let (read_half, mut write_half) = stream.into_split();
    let mut payload = serde_json::to_string(&request)
        .map_err(|e| format!("failed to serialize request: {e}"))?;
    payload.push('\n');
    write_half
        .write_all(payload.as_bytes())
        .await
        .map_err(|e| format!("failed to send request: {e}"))?;
    write_half
        .shutdown()
        .await
        .map_err(|e| format!("failed to half-close request: {e}"))?;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|e| format!("failed to read response: {e}"))?;
    serde_json::from_str::<Response>(line.trim())
        .map_err(|e| format!("invalid response: {e}: {line}"))
}

async fn run_inspect(socket: &Path) -> Result<(), String> {
    let hello = send_request(socket, Request::Hello).await?;
    if let Response::Hello { pid, version, provider } = &hello {
        println!("gamechat pid={pid} version={version} provider={provider}");
        println!("socket {}", socket.display());
        println!();
    }
    let list = send_request(socket, Request::List).await?;
    match list {
        Response::List { slugs } => {
            if slugs.is_empty() {
                println!("(no active sub-agents)");
                return Ok(());
            }
            println!(
                "{:<24} {:<10} {:<9} {:>9}  {}",
                "SLUG", "PROVIDER", "STATUS", "ELAPSED", "LAST"
            );
            for s in slugs {
                let last = truncate_for_table(&s.last_message, 60);
                println!(
                    "{:<24} {:<10} {:<9} {:>8}s  {}",
                    truncate_for_table(&s.slug, 24),
                    truncate_for_table(&s.provider, 10),
                    status_label(s.status),
                    s.elapsed_seconds.round() as u64,
                    last
                );
            }
        }
        Response::Error { message } => return Err(message),
        other => return Err(format!("unexpected response: {other:?}")),
    }
    Ok(())
}

async fn run_tail(socket: &Path, slug: &str) -> Result<(), String> {
    let mut cursor: Option<u64> = None;
    let mut first_pass = true;
    loop {
        let resp = send_request(
            socket,
            Request::Tail {
                slug: slug.to_string(),
                after_seq: cursor,
            },
        )
        .await?;
        match resp {
            Response::Tail {
                entries,
                next_cursor,
                status,
                done,
            } => {
                if first_pass && entries.is_empty() {
                    println!("(no buffered entries yet — waiting for activity, Ctrl-C to stop)");
                    first_pass = false;
                }
                for entry in &entries {
                    println!("{entry}");
                }
                if !entries.is_empty() {
                    first_pass = false;
                }
                cursor = next_cursor;
                if done {
                    eprintln!("-- {} {} --", slug, status_label(status));
                    return Ok(());
                }
            }
            Response::Error { message } => return Err(message),
            other => return Err(format!("unexpected response: {other:?}")),
        }
        sleep(Duration::from_millis(750)).await;
    }
}

async fn run_reset(socket: &Path, reason: Option<String>) -> Result<(), String> {
    let resp = send_request(socket, Request::Reset { reason: reason.clone() }).await?;
    match resp {
        Response::Reset { dispatched } => {
            if dispatched {
                println!(
                    "reset signal dispatched to {} (reason={})",
                    socket.display(),
                    reason.as_deref().unwrap_or("unspecified")
                );
            } else {
                return Err(format!(
                    "voice loop reset channel is closed; reset NOT applied (socket {})",
                    socket.display()
                ));
            }
            Ok(())
        }
        Response::Error { message } => Err(message),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

async fn run_discover() -> Result<(), String> {
    let discovered = discover_existing_subagents().await;
    if discovered.is_empty() {
        println!("(no other live gamechat instances expose any sub-agents)");
        return Ok(());
    }
    println!(
        "{:<8} {:<24} {:<10} {:<9} {:>9}  {}",
        "PEER", "SLUG", "PROVIDER", "STATUS", "ELAPSED", "LAST"
    );
    for entry in discovered {
        println!(
            "{:<8} {:<24} {:<10} {:<9} {:>8}s  {}",
            entry.pid,
            truncate_for_table(&entry.summary.slug, 24),
            truncate_for_table(&entry.summary.provider, 10),
            status_label(entry.summary.status),
            entry.summary.elapsed_seconds.round() as u64,
            truncate_for_table(&entry.summary.last_message, 60),
        );
    }
    Ok(())
}

async fn run_open(socket: &Path, slug: &str, launch: bool) -> Result<(), String> {
    let resp = send_request(
        socket,
        Request::Resume {
            slug: slug.to_string(),
        },
    )
    .await?;
    let (provider, session_id) = match resp {
        Response::Resume {
            provider,
            session_id,
            ..
        } => (provider, session_id),
        Response::Error { message } => return Err(message),
        other => return Err(format!("unexpected response: {other:?}")),
    };
    let cmd = resume_command(&provider, session_id.as_deref(), slug)?;
    if launch {
        launch_in_terminal(&cmd)?;
        println!("launched: {cmd}");
    } else {
        println!("{cmd}");
        println!("(run this in a terminal, or re-run with --launch on macOS)");
    }
    Ok(())
}

fn resume_command(
    provider: &str,
    session_id: Option<&str>,
    slug: &str,
) -> Result<String, String> {
    match provider {
        "claude" => {
            let session_id = session_id.ok_or_else(|| {
                format!(
                    "no resume id for slug {slug:?} yet — the claude agent hasn't replied once. Wait for it to make progress, then retry."
                )
            })?;
            Ok(format!("claude --resume {session_id}"))
        }
        "codex" => Err(format!(
            "`open` is not supported for codex slugs: the codex CLI has no documented resume flag, so there's no UI to reopen. Use `gamechat tail {slug}` to follow its progress instead."
        )),
        other => Err(format!(
            "`open` is not supported for provider {other:?}. Only claude exposes a resume command."
        )),
    }
}

#[cfg(target_os = "macos")]
fn launch_in_terminal(cmd: &str) -> Result<(), String> {
    // `osascript` is the most reliable way to open a new Terminal window with
    // a pre-typed command on macOS.
    let script = format!(
        "tell application \"Terminal\" to do script \"{}\"",
        cmd.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let status = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .status()
        .map_err(|e| format!("failed to invoke osascript: {e}"))?;
    if !status.success() {
        return Err(format!("osascript exited with {status}"));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn launch_in_terminal(_cmd: &str) -> Result<(), String> {
    Err("--launch is only implemented on macOS; copy the printed command instead".to_string())
}

fn status_label(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Queued => "queued",
        JobStatus::Running => "running",
        JobStatus::Completed => "done",
        JobStatus::Failed => "failed",
    }
}

fn truncate_for_table(s: &str, max: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{}…", &head[..head.len().saturating_sub(1)])
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use super::resume_command;

    #[test]
    fn claude_with_session_id_returns_resume_command() {
        let cmd = resume_command("claude", Some("sess-xyz"), "refactor_docs").unwrap();
        assert_eq!(cmd, "claude --resume sess-xyz");
    }

    #[test]
    fn claude_without_session_id_errors_clearly() {
        let err = resume_command("claude", None, "refactor_docs").unwrap_err();
        assert!(err.contains("refactor_docs"));
        assert!(err.contains("hasn't replied"));
    }

    #[test]
    fn codex_returns_unsupported_error() {
        let err = resume_command("codex", Some("conv-abc"), "scan_repo").unwrap_err();
        assert!(err.contains("not supported for codex"));
        assert!(err.contains("gamechat tail scan_repo"));
    }

    #[test]
    fn unknown_provider_returns_unsupported_error() {
        let err = resume_command("mystery", Some("id"), "slug").unwrap_err();
        assert!(err.contains("mystery"));
        assert!(err.contains("Only claude"));
    }
}
