//! `delegate_to_orchestrator` tool call: validate arguments, enqueue a
//! background job, and acknowledge the hand-off back to the voice model.

use super::slug::sanitize_slug;
use super::OrchestratorBridge;
use crate::orchestrator::jobs::{OrchestratorJob, OrchestratorJobManager};
use crate::orchestrator::shared::preview;
use crate::orchestrator::types::{DelegateToOrchestratorArgs, VoiceUpdate};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

impl OrchestratorBridge {
    pub(super) fn handle_delegate_call(
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
            eprintln!("delegate_to_orchestrator ignored empty arguments call_id={call_id}");
            return Ok(Vec::new());
        }
        let args: DelegateToOrchestratorArgs = match serde_json::from_str(args_raw) {
            Ok(args) => args,
            Err(e) => {
                eprintln!(
                    "delegate_to_orchestrator ignored invalid arguments call_id={} error={} raw={}",
                    call_id, e, args_raw
                );
                return Ok(Vec::new());
            }
        };
        let slug = match sanitize_slug(&args.slug) {
            Some(slug) => slug,
            None => {
                eprintln!(
                    "delegate_to_orchestrator ignored missing slug call_id={} intent_preview={}",
                    call_id,
                    preview(&args.user_intent)
                );
                return orchestrator_delegate_error_events(
                    call_id,
                    "delegate_to_orchestrator requires a non-empty snake_case slug. Choose a slug for the background conversation and call the tool again.",
                );
            }
        };

        let job_id = gen_job_id();
        eprintln!(
            "orchestrator bridge delegate call_id={} job={} slug={} urgency={} intent_preview={}",
            call_id,
            job_id,
            slug,
            args.urgency,
            preview(&args.user_intent)
        );
        jobs.enqueue(OrchestratorJob {
            id: job_id,
            slug: slug.clone(),
            args: DelegateToOrchestratorArgs {
                slug: slug.clone(),
                ..args
            },
        })?;

        let update = VoiceUpdate {
            message: format!("Queued {slug}. You will be delivered results once it's done."),
            should_interrupt: false,
            confidence: 1.0,
            done: false,
        };

        let output = serde_json::to_string(&update)
            .map_err(|e| format!("failed to serialize orchestrator output: {e}"))?;
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
}

fn orchestrator_delegate_error_events(
    call_id: &str,
    message: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let update = VoiceUpdate {
        message: message.to_string(),
        should_interrupt: false,
        confidence: 1.0,
        done: true,
    };
    let output = serde_json::to_string(&update)
        .map_err(|e| format!("failed to serialize orchestrator error output: {e}"))?;
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

fn gen_job_id() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("voice-job-{:x}-{:x}", d.as_secs(), d.subsec_nanos())
}
