//! Turning a finished background job into the synthetic function-call +
//! function-call-output pair that delivers the result back into the realtime
//! conversation.

use serde_json::json;

pub(super) fn orchestrator_result_events(
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
