//! Wrapper around the OpenAI Codex CLI (`codex exec`).

use crate::orchestrator::interface::{CleanLogLine, SendResult};
use crate::orchestrator::progress::ProgressReporter;
use crate::orchestrator::shared::{format_job, preview, read_logged_stream};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Client that drives the OpenAI Codex CLI as a background coding agent.
///
/// Unlike the Harness client (JSON-RPC over stdio), Codex runs as a
/// fire-and-forget subprocess: we send a prompt and collect stdout as the
/// reply. Tool-call details are not exposed by the CLI, so `tool_calls` is
/// always empty and `suspended` is always false.
pub(crate) struct CodexClient {
    path: PathBuf,
    model: Option<String>,
    conversation_id: String,
    clean_log_line: CleanLogLine,
}

impl CodexClient {
    pub(crate) async fn spawn(
        codex_bin: Option<String>,
        model: Option<String>,
        conversation_id: String,
        clean_log_line: CleanLogLine,
    ) -> Result<Self, String> {
        let path = resolve_codex_bin(codex_bin)?;

        if path.components().count() > 1 && !path.exists() {
            return Err(format!(
                "codex binary not found at {} — install codex-cli or pass --codex-bin",
                path.display()
            ));
        }

        let output = Command::new(&path)
            .arg("exec")
            .arg("--version")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("failed to probe codex at {}: {e}", path.display()))?;
        if !output.status.success() {
            return Err(format!(
                "codex probe failed at {}: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        eprintln!("codex client spawned bin={}", path.display());
        Ok(Self {
            path,
            model,
            conversation_id,
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
            "codex stdin{} message_bytes={} preview={}",
            format_job(Some(job_id)),
            message.len(),
            preview(message)
        );
        if let Some(reporter) = progress.as_ref() {
            reporter.push(&format!("Codex stdin: {}", preview(message)));
        }

        let output_path = output_path_for_job(job_id);
        let mut command = Command::new(&self.path);
        command
            .arg("exec")
            .arg("--full-auto")
            .arg("--output-last-message")
            .arg(&output_path)
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(model) = self.model.as_deref() {
            command.arg("--model").arg(model);
        }

        let mut child = command
            .spawn()
            .map_err(|e| format!("failed to spawn codex: {e}"))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(message.as_bytes())
                .await
                .map_err(|e| format!("failed to write to codex stdin: {e}"))?;
            drop(stdin);
        }

        let stdout = child.stdout.take().ok_or("codex stdout unavailable")?;
        let stderr = child.stderr.take().ok_or("codex stderr unavailable")?;
        let stdout_task = tokio::spawn(read_logged_stream(
            "codex",
            "stdout",
            job_id.to_string(),
            stdout,
            progress.clone(),
            self.clean_log_line,
        ));
        let stderr_task = tokio::spawn(read_logged_stream(
            "codex",
            "stderr",
            job_id.to_string(),
            stderr,
            progress.clone(),
            self.clean_log_line,
        ));

        let status = child
            .wait()
            .await
            .map_err(|e| format!("failed to wait for codex: {e}"))?;

        let stdout_buf = stdout_task
            .await
            .map_err(|e| format!("failed to join codex stdout reader: {e}"))??;
        let stderr_buf = stderr_task
            .await
            .map_err(|e| format!("failed to join codex stderr reader: {e}"))??;
        let stderr_text = String::from_utf8_lossy(&stderr_buf);

        if !status.success() {
            return Err(format!(
                "codex exited with code {:?}: {}",
                status.code(),
                preview(&stderr_text)
            ));
        }

        let reply = match tokio::fs::read_to_string(&output_path).await {
            Ok(reply) => reply,
            Err(_) => String::from_utf8(stdout_buf)
                .map_err(|e| format!("codex output was not valid utf-8: {e}"))?,
        };
        let _ = tokio::fs::remove_file(&output_path).await;

        eprintln!(
            "codex recv{} reply_bytes={} reply_preview={}",
            format_job(Some(job_id)),
            reply.len(),
            preview(&reply)
        );
        if let Some(reporter) = progress.as_ref() {
            reporter.push(&format!("Codex finished: {}", preview(&reply)));
        }

        Ok(SendResult {
            reply,
            tool_calls: Vec::new(),
            suspended: false,
        })
    }
}

fn resolve_codex_bin(codex_bin: Option<String>) -> Result<PathBuf, String> {
    match codex_bin {
        Some(path) => Ok(PathBuf::from(path)),
        None => which_codex(),
    }
}

fn output_path_for_job(job_id: &str) -> PathBuf {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let safe_job_id = job_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    std::env::temp_dir().join(format!(
        "gamechat-codex-{safe_job_id}-{:x}-{:x}.txt",
        d.as_secs(),
        d.subsec_nanos()
    ))
}

fn which_codex() -> Result<PathBuf, String> {
    let candidates = [
        PathBuf::from("/usr/local/bin/codex"),
        PathBuf::from(std::env::var("HOME").unwrap_or_default() + "/.local/bin/codex"),
        PathBuf::from(std::env::var("HOME").unwrap_or_default() + "/.npm-global/bin/codex"),
    ];
    for candidate in &candidates {
        if candidate.exists() {
            return Ok(candidate.clone());
        }
    }

    Ok(PathBuf::from("codex"))
}
