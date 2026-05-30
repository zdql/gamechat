//! Tool-call argument and result types exchanged between the realtime voice
//! model and the background orchestrator.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DelegateToOrchestratorArgs {
    #[serde(alias = "task_slug")]
    pub(crate) slug: String,
    pub(crate) user_intent: String,
    #[serde(default)]
    pub(crate) recent_context: String,
    #[serde(default = "default_urgency")]
    pub(crate) urgency: String,
    #[serde(default)]
    pub(crate) suggested_user_update: Option<String>,
}

impl DelegateToOrchestratorArgs {
    pub(crate) fn to_agent_message(&self) -> String {
        let mut message = String::from(
            "VOICE_DELEGATION\n\
             You are the background orchestrator behind a realtime voice model.\n\
             The voice model is handling live speech and delegated this intent to you.\n\
             Work normally, use tools/subagents if useful, and keep the final answer concise enough for speech.\n\n",
        );
        message.push_str("# User intent\n");
        message.push_str(self.user_intent.trim());
        message.push_str("\n\n# Slug\n");
        message.push_str(self.slug.trim());
        message.push_str("\n\n# Urgency\n");
        message.push_str(self.urgency.trim());

        if !self.recent_context.trim().is_empty() {
            message.push_str("\n\n# Recent voice context\n");
            message.push_str(self.recent_context.trim());
        }

        if let Some(update) = self.suggested_user_update.as_deref() {
            if !update.trim().is_empty() {
                message.push_str("\n\n# Voice model suggested user update\n");
                message.push_str(update.trim());
            }
        }

        message
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct VoiceUpdate {
    pub(crate) message: String,
    pub(crate) should_interrupt: bool,
    pub(crate) confidence: f32,
    pub(crate) done: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CheckSubagentProgressArgs {
    #[serde(alias = "subagent_slug", alias = "sub_agent_slug", alias = "job_id")]
    pub slug: String,
    #[serde(default)]
    pub window_size: Option<usize>,
    /// Optional natural-language question that the summarizer should bias
    /// the summary toward answering (e.g. "is it done yet?", "did it find
    /// the bug?"). When absent the summarizer produces a generic update.
    #[serde(default)]
    pub question: Option<String>,
}

fn default_urgency() -> String {
    "background".to_string()
}
