use serde_json::json;

pub(crate) fn session_update_json() -> serde_json::Value {
    session_update_json_for_model("gpt-realtime-2")
}

pub(super) fn session_update_json_for_model(model: &str) -> serde_json::Value {
    json!({
        "type": "session.update",
        "session": {
            "type": "realtime",
            "model": model,
            "instructions": "You are a realtime voice frontend. Keep the spoken conversation moving. When the user asks for work that benefits from deeper reasoning, tools, files, research, or multi-step execution, call delegate_to_orchestrator. Always include a stable snake_case slug that names what the background agent will do, such as refactor_docs. Reuse the same slug to continue that background conversation; use a new slug for unrelated work. If the user asks how background work is going, call sub_agent_progress with that slug and summarize the returned last_activity. Call sub_agent_progress sparingly: only when the user asks or when you need material to fill a silence, and never twice in a row within a few seconds. If the response has rate_limited=true, wait retry_after_seconds before calling again — use the cached last_activity in the meantime. Do not pretend the background work is done until the orchestrator returns an update or sub_agent_progress reports status=completed.",
            "output_modalities": ["audio"],
            "audio": {
                "input": {
                    "format": {
                        "type": "audio/pcm",
                        "rate": 24000
                    },
                    "turn_detection": {
                        "type": "semantic_vad"
                    }
                },
                "output": {
                    "format": {
                        "type": "audio/pcm",
                        "rate": 24000
                    },
                    "voice": "marin"
                }
            },
            "tools": [
                {
                    "type": "function",
                    "name": "delegate_to_orchestrator",
                    "description": "Delegate deeper work to a background orchestrator agent.",
                    "parameters": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "slug": {
                                "type": "string",
                                "description": "Stable snake_case slug for this background task, e.g. refactor_docs. Reuse to continue the same agent conversation; use a new slug to start a separate agent."
                            },
                            "user_intent": {
                                "type": "string",
                                "description": "The user's request, rewritten as a clear task for the background orchestrator."
                            },
                            "recent_context": {
                                "type": "string",
                                "description": "Short recent transcript/context needed to understand the delegation."
                            },
                            "urgency": {
                                "type": "string",
                                "enum": ["now", "background"],
                                "description": "Use now for work the user is actively waiting on; background for longer-running side work."
                            },
                            "suggested_user_update": {
                                "type": "string",
                                "description": "Optional one-sentence phrase the voice model can say while the orchestrator starts."
                            }
                        },
                        "required": ["slug", "user_intent", "recent_context", "urgency"]
                    }
                },
                {
                    "type": "function",
                    "name": "sub_agent_progress",
                    "description": "Check the latest progress for a background orchestrator sub-agent by slug. Returns status, last_activity, recent_snippet (capped to ~1000 chars), elapsed_seconds, and a rate_limited flag. Call sparingly: at most once every few seconds for the same slug, and only when the user asks how the work is going or you need material for a spoken update.",
                    "parameters": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "slug": {
                                "type": "string",
                                "description": "Stable snake_case slug originally passed to delegate_to_orchestrator."
                            },
                            "window_size": {
                                "type": "integer",
                                "description": "Optional character budget for recent_snippet. Defaults to 1000 and is clamped between 200 and 1000."
                            }
                        },
                        "required": ["slug"]
                    }
                }
            ]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_exposes_sub_agent_progress_tool() {
        let config = session_update_json_for_model("test-model");
        let tools = config["session"]["tools"]
            .as_array()
            .expect("tools should be an array");

        let progress_tool = tools
            .iter()
            .find(|tool| tool["name"].as_str() == Some("sub_agent_progress"))
            .expect("sub_agent_progress tool should be present");

        assert_eq!(
            progress_tool["parameters"]["required"]
                .as_array()
                .and_then(|required| required.first())
                .and_then(|value| value.as_str()),
            Some("slug")
        );
        assert!(
            progress_tool["parameters"]["properties"]
                .get("window_size")
                .is_some()
        );
    }
}
