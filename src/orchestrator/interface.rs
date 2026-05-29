//! Provider-facing boundary for background coding agents.
//!
//! Code outside the `orchestrator` module — the voice loop, the realtime
//! bridge, and the job manager — only ever sees the types in this file:
//!
//! - [`OrchestratorProvider`] — a clonable factory configured per backend.
//! - [`OrchestratorSession`] — one in-flight conversation with a backend.
//! - [`SendResult`] / [`ToolCallInfo`] — values flowing back to callers.
//! - [`Provider`] / [`Session`] — the traits each backend implements.
//!
//! Provider crates (`claude/`, `openai/`) implement the traits; the rest of
//! the app depends only on this interface.

use crate::orchestrator::claude::ClaudeProvider;
use crate::orchestrator::openai::OpenAiProvider;
use crate::orchestrator::progress::ProgressReporter;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ── Interface types ─────────────────────────────────────────────────────

/// Provider-specific transformation applied to each raw line emitted by a
/// backend's child process (`stdout` or `stderr`) before it lands in the
/// progress buffer.
///
/// Returning `Some(cleaned)` pushes the cleaned form; returning `None` drops
/// the line entirely. The cleaned buffer is what the summarizer LLM sees, so
/// implementations should strip wire noise (e.g. raw JSON envelopes) and keep
/// human-readable signal (assistant text, tool calls, final results).
///
/// Implemented as a `fn` pointer rather than a closure so call sites can
/// hand the cleaner across `tokio::spawn` boundaries without an `Arc`.
pub(crate) type CleanLogLine = fn(line: &str, stream: &str) -> Option<String>;

/// Reply produced by a single round-trip with the background agent.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct SendResult {
    pub(crate) reply: String,
    #[serde(default)]
    pub(crate) tool_calls: Vec<ToolCallInfo>,
    #[serde(default)]
    pub(crate) suspended: bool,
}

/// Summary of a tool call observed during a send. Empty for providers that
/// do not expose tool-call details.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ToolCallInfo {
    pub(crate) name: String,
    pub(crate) arguments: String,
    pub(crate) result: String,
}

// ── Provider trait ──────────────────────────────────────────────────────

/// Backend-agnostic factory for orchestrator sessions.
///
/// Implementations live next to each backend (e.g. `claude::ClaudeProvider`)
/// and are intentionally cheap to clone and reuse: heavy work happens inside
/// [`Provider::open_session`].
#[async_trait]
pub(crate) trait Provider: Send + Sync {
    /// Stable name used in logs and progress events.
    fn name(&self) -> &'static str;

    /// Backend-specific log-line cleaner used by the shared stream reader.
    /// See [`CleanLogLine`].
    fn clean_log_line(&self) -> CleanLogLine;

    /// Spawn a new session keyed to `slug`. Callers that reuse a slug expect
    /// the provider to route them back into the same logical conversation if
    /// the backend supports continuity.
    async fn open_session(&self, slug: &str) -> Result<Box<dyn Session>, String>;
}

/// A single live orchestrator conversation.
///
/// Sessions are not `Sync`: each session is driven by exactly one slug worker
/// inside the job manager.
#[async_trait]
pub(crate) trait Session: Send {
    /// Stable identifier for this conversation as reported by the backend.
    fn conversation_id(&self) -> &str;

    /// Stable backend name; mirrors [`Provider::name`].
    fn provider_name(&self) -> &'static str;

    /// Send a user message and wait for the final assistant reply.
    async fn send_message_until_done_for_job(
        &mut self,
        job_id: &str,
        message: &str,
        progress: Option<ProgressReporter>,
    ) -> Result<SendResult, String>;
}

// ── Public wrappers used outside `orchestrator/` ────────────────────────

/// Configured orchestrator backend.
///
/// A thin wrapper over `Arc<dyn Provider>` so the rest of the voice app can
/// pass providers around as plain values without naming concrete backend
/// types.
#[derive(Clone)]
pub(crate) struct OrchestratorProvider {
    inner: Arc<dyn Provider>,
}

impl OrchestratorProvider {
    pub(crate) fn openai(codex_bin: Option<String>, model: Option<String>) -> Self {
        Self {
            inner: Arc::new(OpenAiProvider::new(codex_bin, model)),
        }
    }

    pub(crate) fn claude(claude_bin: Option<String>, model: Option<String>) -> Self {
        Self {
            inner: Arc::new(ClaudeProvider::new(claude_bin, model)),
        }
    }

    pub(crate) fn name(&self) -> &'static str {
        self.inner.name()
    }

    pub(crate) async fn open_session(&self, slug: &str) -> Result<OrchestratorSession, String> {
        let session = self.inner.open_session(slug).await?;
        eprintln!(
            "orchestrator session opened provider={} slug={} conversation={}",
            session.provider_name(),
            slug,
            session.conversation_id()
        );
        Ok(OrchestratorSession {
            slug: slug.to_string(),
            inner: session,
        })
    }
}

impl std::fmt::Debug for OrchestratorProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrchestratorProvider")
            .field("name", &self.inner.name())
            .finish()
    }
}

/// Live orchestrator session as seen by the rest of the voice app.
pub(crate) struct OrchestratorSession {
    slug: String,
    inner: Box<dyn Session>,
}

impl OrchestratorSession {
    pub(crate) fn conversation_id(&self) -> &str {
        self.inner.conversation_id()
    }

    pub(crate) async fn send_message_until_done_for_job(
        &mut self,
        job_id: &str,
        message: &str,
        progress: Option<ProgressReporter>,
    ) -> Result<SendResult, String> {
        eprintln!(
            "orchestrator session send provider={} slug={} job={} conversation={} message_bytes={}",
            self.inner.provider_name(),
            self.slug,
            job_id,
            self.inner.conversation_id(),
            message.len()
        );
        self.inner
            .send_message_until_done_for_job(job_id, message, progress)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each provider's cleaner is exposed via the trait so live stream
    /// readers consume the configured implementation rather than reaching
    /// past the abstraction.
    #[test]
    fn claude_provider_returns_stream_json_cleaner() {
        let provider = OrchestratorProvider::claude(None, None);
        let cleaner = provider.inner.clean_log_line();
        let init = r#"{"type":"system","subtype":"init","session_id":"abc"}"#;
        assert_eq!(cleaner(init, "stdout"), None);
        let turn = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}"#;
        assert_eq!(cleaner(turn, "stdout"), Some("assistant: hello".to_string()));
    }

    #[test]
    fn codex_provider_returns_passthrough_cleaner() {
        let provider = OrchestratorProvider::openai(None, None);
        let cleaner = provider.inner.clean_log_line();
        assert_eq!(
            cleaner("running tests", "stdout"),
            Some("running tests".to_string())
        );
        assert_eq!(cleaner("   ", "stdout"), None);
    }
}
