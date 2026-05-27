use super::settings::ResolvedVoiceSettings;
use serde_json::json;

pub(crate) fn session_update_json_for(
    model: &str,
    settings: &ResolvedVoiceSettings,
) -> serde_json::Value {
    json!({
        "type": "session.update",
        "session": {
            "type": "realtime",
            "model": model,
            "instructions": settings.instructions,
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
                    "voice": settings.voice
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
                    "description": "Check the latest progress for a background orchestrator sub-agent by slug. Returns a one- or two-sentence natural-language `summary` of what the sub-agent is doing right now, plus status, elapsed_seconds, and a rate_limited flag. The summary is produced by a small model from the raw log buffer, so reading it aloud is safe. Call sparingly: at most once every few seconds for the same slug.",
                    "parameters": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "slug": {
                                "type": "string",
                                "description": "Stable snake_case slug originally passed to delegate_to_orchestrator."
                            },
                            "question": {
                                "type": "string",
                                "description": "Optional natural-language question the summary should bias toward answering (e.g. \"is it done?\", \"did it find the bug?\"). Pass through whatever the user just asked."
                            },
                            "window_size": {
                                "type": "integer",
                                "description": "Optional character budget for the log buffer the summarizer sees. Defaults to 1000 and is clamped between 200 and 1000."
                            }
                        },
                        "required": ["slug"]
                    }
                },
                {
                    "type": "function",
                    "name": "reset_voice_context",
                    "description": "Reset the realtime conversation context to a safe baseline. Use when the conversation has grown long enough that context overload is a risk, or when the user explicitly asks for a fresh start. The reset preserves your persona, tools, and any currently-playing audio; it only clears the prior conversation items the server is reasoning over. Do not announce the reset; keep speaking naturally afterward.",
                    "parameters": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "reason": {
                                "type": "string",
                                "description": "Short rationale for the reset, e.g. \"context_overload\" or \"user_requested\". Used only for logging."
                            }
                        }
                    }
                }
            ]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_for_test(voice: &str, instructions: &str) -> ResolvedVoiceSettings {
        ResolvedVoiceSettings {
            voice: voice.to_string(),
            instructions: instructions.to_string(),
            auto_reset_after_items: 0,
            discover_existing_subagents: true,
        }
    }

    #[test]
    fn session_exposes_sub_agent_progress_tool() {
        let settings = settings_for_test("marin", "base");
        let config = session_update_json_for("test-model", &settings);
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

    #[test]
    fn session_exposes_reset_voice_context_tool() {
        let settings = settings_for_test("marin", "base");
        let config = session_update_json_for("test-model", &settings);
        let tools = config["session"]["tools"]
            .as_array()
            .expect("tools should be an array");
        let reset_tool = tools
            .iter()
            .find(|tool| tool["name"].as_str() == Some("reset_voice_context"))
            .expect("reset_voice_context tool should be present");
        // Reason is optional; no required fields at all.
        let required = reset_tool["parameters"]
            .get("required")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        assert_eq!(required, 0);
        assert!(
            reset_tool["parameters"]["properties"]
                .get("reason")
                .is_some()
        );
    }

    #[test]
    fn session_uses_resolved_voice_and_instructions() {
        let settings = settings_for_test("cedar", "custom instructions");
        let config = session_update_json_for("test-model", &settings);
        assert_eq!(
            config["session"]["audio"]["output"]["voice"].as_str(),
            Some("cedar")
        );
        assert_eq!(
            config["session"]["instructions"].as_str(),
            Some("custom instructions")
        );
    }
}
