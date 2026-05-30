//! Claude Code backend — fire-and-forget `claude -p` per send.
//!
//! Conversations are persisted by Claude via `--name` / `--resume`, so a
//! session is just a process-launching helper that remembers the session id
//! the CLI returns.
//!
//! Public surface (for `interface.rs`): [`ClaudeProvider`].

mod cleaner;
mod client;

use crate::orchestrator::interface::{CleanLogLine, Provider, SendResult, Session};
use crate::orchestrator::progress::ProgressReporter;
use async_trait::async_trait;
use client::ClaudeClient;

pub(crate) struct ClaudeProvider {
    claude_bin: Option<String>,
    model: Option<String>,
}

impl ClaudeProvider {
    pub(crate) fn new(claude_bin: Option<String>, model: Option<String>) -> Self {
        Self { claude_bin, model }
    }
}

#[async_trait]
impl Provider for ClaudeProvider {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn clean_log_line(&self) -> CleanLogLine {
        cleaner::clean_log_line
    }

    async fn open_session(&self, slug: &str) -> Result<Box<dyn Session>, String> {
        let conversation_id = format!("claude-{slug}");
        let client = ClaudeClient::spawn(
            self.claude_bin.clone(),
            self.model.clone(),
            conversation_id,
            self.clean_log_line(),
        )
        .await?;
        Ok(Box::new(ClaudeSession { client }))
    }
}

struct ClaudeSession {
    client: ClaudeClient,
}

#[async_trait]
impl Session for ClaudeSession {
    fn conversation_id(&self) -> &str {
        self.client.conversation_id()
    }

    fn provider_name(&self) -> &'static str {
        "claude"
    }

    async fn send_message_until_done_for_job(
        &mut self,
        job_id: &str,
        message: &str,
        progress: Option<ProgressReporter>,
    ) -> Result<SendResult, String> {
        self.client
            .send_message_until_done_for_job(job_id, message, progress)
            .await
    }
}
