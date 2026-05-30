//! The boundary between Realtime tool calls and background orchestrator jobs.
//! This module is the only place that kicks jobs off or turns completions into
//! Realtime conversation items.
//!
//! Layout:
//!
//! - `mod.rs` — the [`OrchestratorBridge`] façade: realtime event dispatch and
//!   the function-call routing that fans out to the per-tool handlers.
//! - `delegate` — the `delegate_to_orchestrator` tool call.
//! - `progress` — the `sub_agent_progress` tool call.
//! - `job_result` — finished-job completions ↔ realtime conversation items.
//! - `slug` — slug sanitization shared by the tool handlers.

mod delegate;
mod job_result;
mod progress;
mod slug;

use crate::orchestrator::jobs::{OrchestratorJobEvent, OrchestratorJobManager};
use crate::orchestrator::summarizer::SharedSummarizer;
use job_result::orchestrator_result_events;
use serde_json::json;
use std::collections::HashSet;

pub(crate) struct OrchestratorBridge {
    handled_function_calls: HashSet<String>,
    summarizer: SharedSummarizer,
}

impl OrchestratorBridge {
    pub(crate) fn new(summarizer: SharedSummarizer) -> Self {
        Self {
            handled_function_calls: HashSet::new(),
            summarizer,
        }
    }

    pub(crate) async fn handle_realtime_event(
        &mut self,
        value: &serde_json::Value,
        jobs: &OrchestratorJobManager,
    ) -> Result<Vec<serde_json::Value>, String> {
        match value.get("type").and_then(|v| v.as_str()) {
            Some("response.function_call_arguments.done") => {
                self.handle_function_call_done(value, jobs).await
            }
            Some("response.output_item.done") => {
                let Some(call) = function_call_from_output_item(value) else {
                    return Ok(Vec::new());
                };
                self.handle_function_call_done(&call, jobs).await
            }
            _ => Ok(Vec::new()),
        }
    }

    pub(crate) fn realtime_events_for_job_event(
        &self,
        event: OrchestratorJobEvent,
    ) -> Vec<serde_json::Value> {
        match event {
            OrchestratorJobEvent::Completed {
                job_id,
                slug,
                result,
            } => orchestrator_result_events(
                &job_id,
                &slug,
                result.as_ref().map(String::as_str).map_err(String::as_str),
            ),
        }
    }

    async fn handle_function_call_done(
        &mut self,
        value: &serde_json::Value,
        jobs: &OrchestratorJobManager,
    ) -> Result<Vec<serde_json::Value>, String> {
        let name = value.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let call_id = value
            .get("call_id")
            .and_then(|v| v.as_str())
            .ok_or("function call missing call_id")?;
        if !self.handled_function_calls.insert(call_id.to_string()) {
            eprintln!(
                "orchestrator bridge duplicate function ignored call_id={} name={}",
                call_id, name
            );
            return Ok(Vec::new());
        }

        match name {
            "delegate_to_orchestrator" => self.handle_delegate_call(call_id, value, jobs),
            "sub_agent_progress" => self.handle_progress_call(call_id, value, jobs).await,
            _ => Ok(Vec::new()),
        }
    }
}

fn function_call_from_output_item(value: &serde_json::Value) -> Option<serde_json::Value> {
    let item = value.get("item")?;
    if item.get("type").and_then(|v| v.as_str()) != Some("function_call") {
        return None;
    }
    let arguments = item.get("arguments").cloned().unwrap_or_default();
    if arguments.as_str().map(str::trim).unwrap_or("").is_empty() {
        eprintln!("realtime function_call output_item.done ignored empty arguments");
        return None;
    }
    Some(json!({
        "type": "response.function_call_arguments.done",
        "name": item.get("name").cloned().unwrap_or_default(),
        "call_id": item.get("call_id").cloned().unwrap_or_default(),
        "arguments": arguments,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::summarizer::test_support::{ErroringSummarizer, FakeSummarizer};
    use crate::orchestrator::OrchestratorProvider;
    use std::sync::Arc;

    fn bridge_with_fake_summary(reply: &str) -> (OrchestratorBridge, Arc<FakeSummarizer>) {
        let fake = Arc::new(FakeSummarizer::new(reply));
        let bridge = OrchestratorBridge::new(fake.clone());
        (bridge, fake)
    }

    #[tokio::test]
    async fn sub_agent_progress_unknown_slug_returns_error_payload() {
        let (mut bridge, _) = bridge_with_fake_summary("unused");
        let jobs = OrchestratorJobManager::spawn(OrchestratorProvider::claude(None, None));
        let call = json!({
            "type": "response.function_call_arguments.done",
            "name": "sub_agent_progress",
            "call_id": "call_progress_unknown",
            "arguments": "{\"slug\":\"missing_slug\"}"
        });

        let events = bridge
            .handle_realtime_event(&call, &jobs)
            .await
            .expect("progress call should produce a tool result");
        let output = events[0]["item"]["output"]
            .as_str()
            .expect("function output should be serialized JSON");
        let payload: serde_json::Value =
            serde_json::from_str(output).expect("payload should be valid JSON");

        assert_eq!(payload["ok"], false);
        assert_eq!(payload["status"], "unknown");
        assert_eq!(payload["slug"], "missing_slug");
        assert_eq!(events[1]["type"], "response.create");
    }

    #[tokio::test]
    async fn sub_agent_progress_returns_summary_then_rate_limits() {
        let (mut bridge, fake) =
            bridge_with_fake_summary("Reading files and drafting changes.");
        let jobs = OrchestratorJobManager::spawn(OrchestratorProvider::claude(None, None));
        // Register a slug directly via enqueue so we don't depend on the
        // provider binary actually being available — enqueue calls into
        // the progress store immediately.
        jobs.enqueue(crate::orchestrator::jobs::OrchestratorJob {
            id: "job-test".to_string(),
            slug: "refactor_docs".to_string(),
            args: crate::orchestrator::types::DelegateToOrchestratorArgs {
                slug: "refactor_docs".to_string(),
                user_intent: "noop".to_string(),
                recent_context: String::new(),
                urgency: "background".to_string(),
                suggested_user_update: None,
            },
        })
        .expect("enqueue should succeed");

        let mk_call = |call_id: &str, args: &str| json!({
            "type": "response.function_call_arguments.done",
            "name": "sub_agent_progress",
            "call_id": call_id,
            "arguments": args,
        });

        let first = bridge
            .handle_realtime_event(
                &mk_call(
                    "call_progress_first",
                    "{\"slug\":\"refactor_docs\",\"question\":\"is it done?\"}",
                ),
                &jobs,
            )
            .await
            .expect("first progress call should succeed");
        let first_payload: serde_json::Value =
            serde_json::from_str(first[0]["item"]["output"].as_str().unwrap()).unwrap();
        assert_eq!(first_payload["ok"], true);
        assert_eq!(first_payload["slug"], "refactor_docs");
        assert!(first_payload.get("elapsed_seconds").is_some());
        assert_eq!(first_payload["rate_limited"], false);
        assert_eq!(
            first_payload["summary"].as_str(),
            Some("Reading files and drafting changes.")
        );
        assert!(first_payload.get("last_activity").is_none());

        let recorded = fake
            .last_request
            .lock()
            .unwrap()
            .clone()
            .expect("summarizer should have been called");
        assert_eq!(recorded.slug, "refactor_docs");
        assert_eq!(recorded.question.as_deref(), Some("is it done?"));

        // Immediate second call should be flagged rate_limited and skip the
        // summarizer entirely.
        let second = bridge
            .handle_realtime_event(
                &mk_call("call_progress_second", "{\"slug\":\"refactor_docs\"}"),
                &jobs,
            )
            .await
            .expect("second progress call should succeed");
        let second_payload: serde_json::Value =
            serde_json::from_str(second[0]["item"]["output"].as_str().unwrap()).unwrap();
        assert_eq!(second_payload["ok"], true);
        assert_eq!(second_payload["rate_limited"], true);
        assert!(second_payload["retry_after_seconds"].as_u64().unwrap_or(0) > 0);
        assert!(second_payload.get("summary").is_none());
    }

    #[tokio::test]
    async fn sub_agent_progress_summarizer_error_returns_error_payload() {
        let summarizer = Arc::new(ErroringSummarizer {
            error: "boom".to_string(),
        });
        let mut bridge = OrchestratorBridge::new(summarizer);
        let jobs = OrchestratorJobManager::spawn(OrchestratorProvider::claude(None, None));
        jobs.enqueue(crate::orchestrator::jobs::OrchestratorJob {
            id: "job-err".to_string(),
            slug: "failing_slug".to_string(),
            args: crate::orchestrator::types::DelegateToOrchestratorArgs {
                slug: "failing_slug".to_string(),
                user_intent: "noop".to_string(),
                recent_context: String::new(),
                urgency: "background".to_string(),
                suggested_user_update: None,
            },
        })
        .expect("enqueue should succeed");

        let call = json!({
            "type": "response.function_call_arguments.done",
            "name": "sub_agent_progress",
            "call_id": "call_progress_err",
            "arguments": "{\"slug\":\"failing_slug\"}"
        });
        let events = bridge
            .handle_realtime_event(&call, &jobs)
            .await
            .expect("progress call should still emit tool events");
        let payload: serde_json::Value =
            serde_json::from_str(events[0]["item"]["output"].as_str().unwrap()).unwrap();
        assert_eq!(payload["ok"], false);
        assert_eq!(payload["status"], "summarizer_error");
        assert!(
            payload["error"]
                .as_str()
                .unwrap_or("")
                .contains("boom")
        );
    }
}
