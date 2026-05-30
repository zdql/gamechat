// Converts our internal voice settings into the OpenAI Realtime `session.update`
// payload, including the function tools we expose to the model.
use crate::voice_loop::settings::ResolvedVoiceSettings;
use serde_json::json;

mod definitions;

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
                definitions::delegate_to_orchestrator_tool(),
                definitions::sub_agent_progress_tool()
            ]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_exposes_sub_agent_progress_tool() {
        let settings = ResolvedVoiceSettings {
            voice: "marin".to_string(),
            instructions: "base".to_string(),
        };
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
    fn session_uses_resolved_voice_and_instructions() {
        let settings = ResolvedVoiceSettings {
            voice: "cedar".to_string(),
            instructions: "custom instructions".to_string(),
        };
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
