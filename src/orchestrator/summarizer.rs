//! Tiny LLM that turns a buffer of raw subagent log lines into a one- or
//! two-sentence spoken update for the realtime voice model.
//!
//! Exposed as a trait so the bridge can be tested without hitting the
//! network. The production impl ([`OpenAiSummarizer`]) calls OpenAI's
//! chat completions API on `gpt-5-nano` by default and reuses the same
//! `OPENAI_API_KEY` the realtime websocket already requires.

use crate::orchestrator::progress::JobStatus;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_MODEL: &str = "gpt-5-nano";
const REQUEST_TIMEOUT_SECS: u64 = 8;

/// Context handed to the summarizer alongside the raw log buffer.
pub(crate) struct SummarizeRequest<'a> {
    pub(crate) slug: &'a str,
    pub(crate) provider: &'a str,
    pub(crate) status: JobStatus,
    pub(crate) elapsed_seconds: f64,
    pub(crate) recent_logs: &'a str,
    /// What the user (or voice model) actually wants to know. When present,
    /// the summarizer is told to bias the summary toward answering it.
    pub(crate) question: Option<&'a str>,
}

#[async_trait]
pub(crate) trait Summarizer: Send + Sync {
    async fn summarize(&self, request: SummarizeRequest<'_>) -> Result<String, String>;
}

pub(crate) type SharedSummarizer = Arc<dyn Summarizer>;

pub(crate) struct OpenAiSummarizer {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl OpenAiSummarizer {
    pub(crate) fn new(api_key: String, model: Option<String>) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| format!("failed to build summarizer http client: {e}"))?;
        Ok(Self {
            client,
            api_key,
            model: model.unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        })
    }
}

#[async_trait]
impl Summarizer for OpenAiSummarizer {
    async fn summarize(&self, request: SummarizeRequest<'_>) -> Result<String, String> {
        let user_prompt = build_user_prompt(&request);
        let body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": user_prompt}
            ]
        });

        let response = self
            .client
            .post("https://api.openai.com/v1/chat/completions")
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("summarizer request failed: {e}"))?;

        let status = response.status();
        let payload: serde_json::Value = response
            .json()
            .await
            .map_err(|e| format!("summarizer response was not valid JSON: {e}"))?;

        if !status.is_success() {
            let detail = payload
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            return Err(format!("summarizer http {status}: {detail}"));
        }

        let content = payload
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .ok_or_else(|| "summarizer response missing choices[0].message.content".to_string())?
            .trim()
            .to_string();

        if content.is_empty() {
            return Err("summarizer returned empty content".to_string());
        }
        Ok(content)
    }
}

const SYSTEM_PROMPT: &str = "You summarize raw subagent log output for a realtime voice assistant. \
Reply with at most two short sentences in plain spoken English. \
Describe what the agent appears to be doing right now, grounded only in the logs provided. \
Do not invent progress, do not quote raw log lines, and do not claim completion unless the status field says completed. \
If a question is provided, bias the summary toward answering it.";

fn build_user_prompt(request: &SummarizeRequest<'_>) -> String {
    let mut prompt = String::new();
    prompt.push_str("Subagent slug: ");
    prompt.push_str(request.slug);
    prompt.push_str("\nProvider: ");
    prompt.push_str(request.provider);
    prompt.push_str("\nStatus: ");
    prompt.push_str(&request.status.to_string());
    prompt.push_str(&format!(
        "\nElapsed seconds: {:.1}",
        request.elapsed_seconds
    ));
    if let Some(question) = request.question {
        let trimmed = question.trim();
        if !trimmed.is_empty() {
            prompt.push_str("\n\nQuestion the summary should help answer:\n");
            prompt.push_str(trimmed);
        }
    }
    prompt.push_str("\n\nRecent log lines (oldest first):\n");
    if request.recent_logs.trim().is_empty() {
        prompt.push_str("(no log output yet)");
    } else {
        prompt.push_str(request.recent_logs);
    }
    prompt
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::Mutex;

    /// Records the last request it was handed and returns a fixed reply.
    pub(crate) struct FakeSummarizer {
        reply: String,
        pub(crate) last_request: Mutex<Option<RecordedRequest>>,
    }

    #[derive(Debug, Clone)]
    #[allow(dead_code)]
    pub(crate) struct RecordedRequest {
        pub(crate) slug: String,
        pub(crate) status: JobStatus,
        pub(crate) recent_logs: String,
        pub(crate) question: Option<String>,
    }

    impl FakeSummarizer {
        pub(crate) fn new(reply: impl Into<String>) -> Self {
            Self {
                reply: reply.into(),
                last_request: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl Summarizer for FakeSummarizer {
        async fn summarize(&self, request: SummarizeRequest<'_>) -> Result<String, String> {
            *self.last_request.lock().unwrap() = Some(RecordedRequest {
                slug: request.slug.to_string(),
                status: request.status,
                recent_logs: request.recent_logs.to_string(),
                question: request.question.map(str::to_string),
            });
            Ok(self.reply.clone())
        }
    }

    /// Returns the given error regardless of input.
    pub(crate) struct ErroringSummarizer {
        pub(crate) error: String,
    }

    #[async_trait]
    impl Summarizer for ErroringSummarizer {
        async fn summarize(&self, _request: SummarizeRequest<'_>) -> Result<String, String> {
            Err(self.error.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_prompt_includes_question_when_present() {
        let prompt = build_user_prompt(&SummarizeRequest {
            slug: "refactor_docs",
            provider: "orchestrator",
            status: JobStatus::Running,
            elapsed_seconds: 12.4,
            recent_logs: "claude stdout: reading files\nclaude stdout: editing\n",
            question: Some("is it done yet?"),
        });
        assert!(prompt.contains("refactor_docs"));
        assert!(prompt.contains("running"));
        assert!(prompt.contains("is it done yet?"));
        assert!(prompt.contains("reading files"));
    }

    #[test]
    fn user_prompt_handles_empty_logs() {
        let prompt = build_user_prompt(&SummarizeRequest {
            slug: "x",
            provider: "orchestrator",
            status: JobStatus::Queued,
            elapsed_seconds: 0.0,
            recent_logs: "",
            question: None,
        });
        assert!(prompt.contains("(no log output yet)"));
    }
}
