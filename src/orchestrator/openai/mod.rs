//! OpenAI backend — drives the Codex CLI (`codex exec`).
//!
//! Codex runs as a fire-and-forget subprocess per send. There is no
//! persistent conversation; we mint a synthetic conversation id from the
//! slug so the rest of the app has something stable to log.
//!
//! Public surface (for `interface.rs`): [`OpenAiProvider`].

mod client;

use crate::orchestrator::interface::{Provider, Session, SendResult};
use crate::orchestrator::progress::ProgressReporter;
use async_trait::async_trait;
use client::CodexClient;

pub(crate) struct OpenAiProvider {
    codex_bin: Option<String>,
    model: Option<String>,
}

impl OpenAiProvider {
    pub(crate) fn new(codex_bin: Option<String>, model: Option<String>) -> Self {
        Self { codex_bin, model }
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn name(&self) -> &'static str {
        "codex"
    }

    async fn open_session(&self, slug: &str) -> Result<Box<dyn Session>, String> {
        let conversation_id = format!("codex-{slug}");
        let client =
            CodexClient::spawn(self.codex_bin.clone(), self.model.clone(), conversation_id).await?;
        Ok(Box::new(OpenAiSession { client }))
    }
}

struct OpenAiSession {
    client: CodexClient,
}

#[async_trait]
impl Session for OpenAiSession {
    fn conversation_id(&self) -> &str {
        self.client.conversation_id()
    }

    fn provider_name(&self) -> &'static str {
        "codex"
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
