use crate::orchestrator::jobs::{OrchestratorJob, OrchestratorJobEvent, OrchestratorJobManager};
use crate::orchestrator::progress::DEFAULT_WINDOW_SIZE;
use crate::orchestrator::shared::preview;
use crate::types::{CheckSubagentProgressArgs, DelegateToOrchestratorArgs, VoiceUpdate};
use serde_json::json;
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

// The boundary between Realtime tool calls and background orchestrator jobs.
// This module is the only place that kicks jobs off or turns completions into
// Realtime conversation items.
pub(crate) struct OrchestratorBridge {
    handled_function_calls: HashSet<String>,
}

impl OrchestratorBridge {
    pub(crate) fn new() -> Self {
        Self {
            handled_function_calls: HashSet::new(),
        }
    }

    pub(crate) fn handle_realtime_event(
        &mut self,
        value: &serde_json::Value,
        jobs: &OrchestratorJobManager,
    ) -> Result<Vec<serde_json::Value>, String> {
        match value.get("type").and_then(|v| v.as_str()) {
            Some("response.function_call_arguments.done") => {
                self.handle_function_call_done(value, jobs)
            }
            Some("response.output_item.done") => {
                let Some(call) = function_call_from_output_item(value) else {
                    return Ok(Vec::new());
                };
                self.handle_function_call_done(&call, jobs)
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

    fn handle_function_call_done(
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
            "sub_agent_progress" => self.handle_progress_call(call_id, value, jobs),
            _ => Ok(Vec::new()),
        }
    }

    fn handle_delegate_call(
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

    fn handle_progress_call(
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

        progress_tool_events(
            call_id,
            json!({
                "ok": true,
                "slug": slug,
                "status": snapshot.status,
                "provider": snapshot.provider,
                "last_activity": snapshot.last_message,
                "recent_snippet": snapshot.recent_snippet,
                "elapsed_seconds": snapshot.elapsed_seconds,
                "rate_limited": snapshot.rate_limited,
                "retry_after_seconds": snapshot.retry_after_seconds,
                "guidance": "Use this to give a concise spoken update. Do not claim the work is complete unless status is completed. If rate_limited is true, do not call this tool again until retry_after_seconds has elapsed."
            }),
        )
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

fn orchestrator_result_events(
    job_id: &str,
    slug: &str,
    result: Result<&str, &str>,
) -> Vec<serde_json::Value> {
    let call_id = synthetic_result_call_id(job_id);
    let output = match result {
        Ok(reply) => json!({
            "ok": true,
            "job_id": job_id,
            "slug": slug,
            "instruction": "THIS IS THE RESULT OF YOUR TOOL CALL - YOU SHOULD INFORM THE USER ABOUT THIS.",
            "result": reply,
        }),
        Err(error) => json!({
            "ok": false,
            "job_id": job_id,
            "slug": slug,
            "instruction": "THIS IS THE RESULT OF YOUR TOOL CALL - YOU SHOULD INFORM THE USER THAT THE BACKGROUND WORK FAILED.",
            "error": error,
        }),
    };

    vec![
        json!({
            "type": "conversation.item.create",
            "event_id": format!("event_{call_id}_call"),
            "item": {
                "type": "function_call",
                "call_id": call_id,
                "name": "background_orchestrator_result",
                "arguments": serde_json::to_string(&json!({
                    "job_id": job_id,
                    "slug": slug
                }))
                    .unwrap_or_else(|_| "{}".to_string()),
            }
        }),
        json!({
            "type": "conversation.item.create",
            "event_id": format!("event_{call_id}_output"),
            "item": {
                "type": "function_call_output",
                "call_id": call_id,
                "output": serde_json::to_string(&output).unwrap_or_else(|_| {
                    "{\"ok\":false,\"error\":\"failed to serialize orchestrator result\"}"
                        .to_string()
                }),
            }
        }),
        json!({ "type": "response.create" }),
    ]
}

fn synthetic_result_call_id(job_id: &str) -> String {
    let mut id = String::from("call_");
    for ch in job_id.chars() {
        if ch.is_ascii_alphanumeric() {
            id.push(ch);
        } else {
            id.push('_');
        }
    }
    id
}

fn gen_job_id() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("voice-job-{:x}-{:x}", d.as_secs(), d.subsec_nanos())
}

fn sanitize_slug(value: &str) -> Option<String> {
    let mut slug = String::new();
    let mut last_was_separator = false;
    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_separator = false;
        } else if !last_was_separator && !slug.is_empty() {
            slug.push('_');
            last_was_separator = true;
        }
    }
    while slug.ends_with('_') {
        slug.pop();
    }
    (!slug.is_empty()).then_some(slug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::OrchestratorProvider;

    #[tokio::test]
    async fn sub_agent_progress_unknown_slug_returns_error_payload() {
        let mut bridge = OrchestratorBridge::new();
        let jobs = OrchestratorJobManager::spawn(OrchestratorProvider::claude(None, None));
        let call = json!({
            "type": "response.function_call_arguments.done",
            "name": "sub_agent_progress",
            "call_id": "call_progress_unknown",
            "arguments": "{\"slug\":\"missing_slug\"}"
        });

        let events = bridge
            .handle_realtime_event(&call, &jobs)
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
    async fn sub_agent_progress_known_slug_includes_elapsed_and_rate_limited() {
        let mut bridge = OrchestratorBridge::new();
        let jobs = OrchestratorJobManager::spawn(OrchestratorProvider::claude(None, None));
        // Register a slug directly via enqueue so we don't depend on the
        // provider binary actually being available — enqueue calls into
        // the progress store immediately.
        jobs.enqueue(crate::orchestrator::jobs::OrchestratorJob {
            id: "job-test".to_string(),
            slug: "refactor_docs".to_string(),
            args: crate::types::DelegateToOrchestratorArgs {
                slug: "refactor_docs".to_string(),
                user_intent: "noop".to_string(),
                recent_context: String::new(),
                urgency: "background".to_string(),
                suggested_user_update: None,
            },
        })
        .expect("enqueue should succeed");

        let mk_call = |call_id: &str| json!({
            "type": "response.function_call_arguments.done",
            "name": "sub_agent_progress",
            "call_id": call_id,
            "arguments": "{\"slug\":\"refactor_docs\"}"
        });

        let first = bridge
            .handle_realtime_event(&mk_call("call_progress_first"), &jobs)
            .expect("first progress call should succeed");
        let first_payload: serde_json::Value =
            serde_json::from_str(first[0]["item"]["output"].as_str().unwrap()).unwrap();
        assert_eq!(first_payload["ok"], true);
        assert_eq!(first_payload["slug"], "refactor_docs");
        assert!(first_payload.get("elapsed_seconds").is_some());
        assert_eq!(first_payload["rate_limited"], false);

        // Immediate second call should be flagged rate_limited but still
        // ok=true and carry the cached last_activity.
        let second = bridge
            .handle_realtime_event(&mk_call("call_progress_second"), &jobs)
            .expect("second progress call should succeed");
        let second_payload: serde_json::Value =
            serde_json::from_str(second[0]["item"]["output"].as_str().unwrap()).unwrap();
        assert_eq!(second_payload["ok"], true);
        assert_eq!(second_payload["rate_limited"], true);
        assert!(second_payload["retry_after_seconds"].as_u64().unwrap_or(0) > 0);
    }
}
