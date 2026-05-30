//! `sub_agent_progress` tool call: look up a job's progress snapshot, rate-limit
//! repeat polls, and turn recent logs into an LLM-summarized status for the
//! voice model to speak.

use super::slug::sanitize_slug;
use super::OrchestratorBridge;
use crate::orchestrator::jobs::OrchestratorJobManager;
use crate::orchestrator::progress::DEFAULT_WINDOW_SIZE;
use crate::orchestrator::summarizer::SummarizeRequest;
use crate::orchestrator::types::CheckSubagentProgressArgs;
use serde_json::json;

impl OrchestratorBridge {
    pub(super) async fn handle_progress_call(
        &mut self,
        call_id: &str,
        value: &serde_json::Value,
        jobs: &OrchestratorJobManager,
    ) -> Result<Vec<serde_json::Value>, String> {
        let args_raw = value
            .get("arguments")
            .and_then(|v| v.as_str())
            .unwrap_or("{}");
        if args_raw.trim().is_empty() {
            return progress_tool_events(
                call_id,
                json!({
                    "ok": false,
                    "status": "error",
                    "error": "sub_agent_progress requires a slug argument."
                }),
            );
        }
        let args: CheckSubagentProgressArgs = match serde_json::from_str(args_raw) {
            Ok(args) => args,
            Err(e) => {
                return progress_tool_events(
                    call_id,
                    json!({
                        "ok": false,
                        "status": "error",
                        "error": format!("invalid sub_agent_progress arguments: {e}")
                    }),
                );
            }
        };
        let slug = match sanitize_slug(&args.slug) {
            Some(slug) => slug,
            None => {
                return progress_tool_events(
                    call_id,
                    json!({
                        "ok": false,
                        "status": "error",
                        "error": "sub_agent_progress requires a non-empty snake_case slug."
                    }),
                );
            }
        };
        let window_size = args
            .window_size
            .map(|size| size.clamp(200, DEFAULT_WINDOW_SIZE))
            .or(Some(DEFAULT_WINDOW_SIZE));
        let Some(snapshot) = jobs.get_progress(&slug, window_size) else {
            return progress_tool_events(
                call_id,
                json!({
                    "ok": false,
                    "status": "unknown",
                    "slug": slug,
                    "error": "No background orchestrator agent is known for that slug. It may not have been queued in this voice session, or it may be unavailable."
                }),
            );
        };

        if snapshot.rate_limited {
            return progress_tool_events(
                call_id,
                json!({
                    "ok": true,
                    "slug": slug,
                    "status": snapshot.status,
                    "provider": snapshot.provider,
                    "elapsed_seconds": snapshot.elapsed_seconds,
                    "rate_limited": true,
                    "retry_after_seconds": snapshot.retry_after_seconds,
                    "guidance": "Rate limited. Wait retry_after_seconds before calling again; reuse the prior summary verbatim or stall briefly."
                }),
            );
        }

        let question = args.question.as_deref().map(str::trim).filter(|q| !q.is_empty());
        let summary = match self
            .summarizer
            .summarize(SummarizeRequest {
                slug: &slug,
                provider: &snapshot.provider,
                status: snapshot.status,
                elapsed_seconds: snapshot.elapsed_seconds,
                recent_logs: &snapshot.recent_snippet,
                question,
            })
            .await
        {
            Ok(summary) => summary,
            Err(err) => {
                eprintln!(
                    "sub_agent_progress summarizer failed slug={} error={}",
                    slug, err
                );
                return progress_tool_events(
                    call_id,
                    json!({
                        "ok": false,
                        "status": "summarizer_error",
                        "slug": slug,
                        "error": format!("summarizer unavailable: {err}")
                    }),
                );
            }
        };

        progress_tool_events(
            call_id,
            json!({
                "ok": true,
                "slug": slug,
                "status": snapshot.status,
                "provider": snapshot.provider,
                "summary": summary,
                "elapsed_seconds": snapshot.elapsed_seconds,
                "rate_limited": false,
                "retry_after_seconds": 0,
                "guidance": "Speak the summary as-is or paraphrase lightly. Do not claim the work is complete unless status is completed."
            }),
        )
    }
}

fn progress_tool_events(
    call_id: &str,
    payload: serde_json::Value,
) -> Result<Vec<serde_json::Value>, String> {
    let output = serde_json::to_string(&payload)
        .map_err(|e| format!("failed to serialize progress output: {e}"))?;
    Ok(vec![
        json!({
            "type": "conversation.item.create",
            "item": {
                "type": "function_call_output",
                "call_id": call_id,
                "output": output,
            }
        }),
        json!({"type": "response.create"}),
    ])
}
