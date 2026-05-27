//! Wrapper around the `claude` CLI in print mode.

use crate::orchestrator::interface::{CleanLogLine, SendResult};
use crate::orchestrator::progress::ProgressReporter;
use crate::orchestrator::shared::{format_job, preview, read_logged_stream};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Client that drives Claude Code as a background coding agent.
///
/// Claude Code exposes a print mode (`claude -p`) for non-interactive
/// execution. We use stream-json output so stdout can be logged while the
/// process is running and the final `result` can be extracted cleanly.
pub(crate) struct ClaudeClient {
    path: PathBuf,
    model: Option<String>,
    conversation_id: String,
    session_id: Option<String>,
    clean_log_line: CleanLogLine,
}

impl ClaudeClient {
    pub(crate) async fn spawn(
        claude_bin: Option<String>,
        model: Option<String>,
        conversation_id: String,
        clean_log_line: CleanLogLine,
    ) -> Result<Self, String> {
        let path = resolve_claude_bin(claude_bin)?;

        if path.components().count() > 1 && !path.exists() {
            return Err(format!(
                "claude binary not found at {} -- install Claude Code or pass --claude-bin",
                path.display()
            ));
        }

        let output = Command::new(&path)
            .arg("--version")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("failed to probe claude at {}: {e}", path.display()))?;
        if !output.status.success() {
            return Err(format!(
                "claude probe failed at {}: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        eprintln!("claude client spawned bin={}", path.display());
        Ok(Self {
            path,
            model,
            conversation_id,
            session_id: None,
            clean_log_line,
        })
    }

    pub(crate) fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    pub(crate) async fn send_message_until_done_for_job(
        &mut self,
        job_id: &str,
        message: &str,
        progress: Option<ProgressReporter>,
    ) -> Result<SendResult, String> {
        eprintln!(
            "claude stdin{} message_bytes={} preview={}",
            format_job(Some(job_id)),
            message.len(),
            preview(message)
        );
        if let Some(reporter) = progress.as_ref() {
            reporter.push(&format!("Claude stdin: {}", preview(message)));
        }

        let mut command = Command::new(&self.path);
        command
            .arg("-p")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--dangerously-skip-permissions")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(model) = self.model.as_deref() {
            command.arg("--model").arg(model);
        }
        if let Some(session_id) = self.session_id.as_deref() {
            command.arg("--resume").arg(session_id);
        } else {
            command.arg("--name").arg(&self.conversation_id);
        }

        let mut child = command
            .spawn()
            .map_err(|e| format!("failed to spawn claude: {e}"))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(message.as_bytes())
                .await
                .map_err(|e| format!("failed to write to claude stdin: {e}"))?;
            drop(stdin);
        }

        let stdout = child
            .stdout
            .take()
            .ok_or("claude stdout unavailable")?;
        let stderr = child
            .stderr
            .take()
            .ok_or("claude stderr unavailable")?;
        let stdout_task = tokio::spawn(read_logged_stream(
            "claude",
            "stdout",
            job_id.to_string(),
            stdout,
            progress.clone(),
            self.clean_log_line,
        ));
        let stderr_task = tokio::spawn(read_logged_stream(
            "claude",
            "stderr",
            job_id.to_string(),
            stderr,
            progress.clone(),
            self.clean_log_line,
        ));

        let status = child
            .wait()
            .await
            .map_err(|e| format!("failed to wait for claude: {e}"))?;

        let stdout_buf = stdout_task
            .await
            .map_err(|e| format!("failed to join claude stdout reader: {e}"))??;
        let stderr_buf = stderr_task
            .await
            .map_err(|e| format!("failed to join claude stderr reader: {e}"))??;
        let stderr_text = String::from_utf8_lossy(&stderr_buf);

        if !status.success() {
            return Err(format!(
                "claude exited with code {:?}: {}",
                status.code(),
                preview(&stderr_text)
            ));
        }

        let stdout_text = String::from_utf8(stdout_buf)
            .map_err(|e| format!("claude output was not valid utf-8: {e}"))?;
        let (reply, session_id) = parse_claude_stream_output(&stdout_text);
        if let Some(session_id) = session_id {
            if let Some(reporter) = progress.as_ref() {
                reporter.set_session_id(&session_id);
            }
            self.session_id = Some(session_id);
        }
        let reply = reply.unwrap_or(stdout_text);

        eprintln!(
            "claude recv{} reply_bytes={} reply_preview={}",
            format_job(Some(job_id)),
            reply.len(),
            preview(&reply)
        );
        if let Some(reporter) = progress.as_ref() {
            reporter.push(&format!("Claude finished: {}", preview(&reply)));
        }

        Ok(SendResult {
            reply,
            tool_calls: Vec::new(),
            suspended: false,
        })
    }
}

fn parse_claude_stream_output(output: &str) -> (Option<String>, Option<String>) {
    let mut result = None;
    let mut session_id = None;

    for line in output.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(id) = value.get("session_id").and_then(Value::as_str) {
            session_id = Some(id.to_string());
        }
        if value.get("type").and_then(Value::as_str) == Some("result") {
            if let Some(text) = value.get("result").and_then(Value::as_str) {
                result = Some(text.to_string());
            }
            if let Some(id) = value.get("session_id").and_then(Value::as_str) {
                session_id = Some(id.to_string());
            }
        }
    }

    (result, session_id)
}

fn resolve_claude_bin(claude_bin: Option<String>) -> Result<PathBuf, String> {
    match claude_bin {
        Some(path) => Ok(PathBuf::from(path)),
        None => which_claude(),
    }
}

fn which_claude() -> Result<PathBuf, String> {
    let candidates = [
        PathBuf::from("/usr/local/bin/claude"),
        PathBuf::from("/opt/homebrew/bin/claude"),
        PathBuf::from(format!(
            "{}/.local/bin/claude",
            std::env::var("HOME").unwrap_or_default()
        )),
        PathBuf::from(format!(
            "{}/.npm-global/bin/claude",
            std::env::var("HOME").unwrap_or_default()
        )),
    ];
    for candidate in &candidates {
        if candidate.exists() {
            return Ok(candidate.clone());
        }
    }

    Ok(PathBuf::from("claude"))
}
