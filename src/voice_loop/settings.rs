use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// Default voice if neither the chosen preset nor the user's settings override it.
pub(crate) const DEFAULT_VOICE: &str = "marin";

// The orchestrator-aware base prompt. Personas are appended to this so every
// preset still knows how to delegate work and check progress.
pub(super) const BASE_INSTRUCTIONS: &str = "You are a realtime voice frontend. Keep the spoken conversation moving. When the user asks for work that benefits from deeper reasoning, tools, files, research, or multi-step execution, call delegate_to_orchestrator. Always include a stable snake_case slug that names what the background agent will do, such as refactor_docs. Reuse the same slug to continue that background conversation; use a new slug for unrelated work. If the user asks how background work is going, call sub_agent_progress with that slug and summarize the returned last_activity. Call sub_agent_progress sparingly: only when the user asks or when you need material to fill a silence, and never twice in a row within a few seconds. If the response has rate_limited=true, wait retry_after_seconds before calling again — use the cached last_activity in the meantime. Do not pretend the background work is done until the orchestrator returns an update or sub_agent_progress reports status=completed.";

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Settings {
    /// Force a specific voice regardless of preset.
    #[serde(default)]
    pub voice: Option<String>,
    /// Name of the preset to use. Defaults to "default".
    #[serde(default)]
    pub preset: Option<String>,
    /// Override the persona text without picking a different preset.
    #[serde(default)]
    pub persona: Option<String>,
    /// User-defined presets. Override built-ins of the same name.
    #[serde(default)]
    pub presets: HashMap<String, Preset>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Preset {
    #[serde(default)]
    pub voice: Option<String>,
    #[serde(default)]
    pub persona: String,
}

#[derive(Debug)]
pub(crate) struct ResolvedVoiceSettings {
    pub voice: String,
    pub instructions: String,
}

pub(crate) fn builtin_presets() -> HashMap<String, Preset> {
    let entries: &[(&str, &str, &str)] = &[
        // (name, voice, persona) — persona is empty string for the bare default.
        ("default", "marin", ""),
        // Practical staples.
        (
            "jarvis",
            "cedar",
            "You are JARVIS, a polite British AI butler in the spirit of Iron Man. Speak with calm precision and dry wit. Address the user as \"sir\" sparingly. Keep replies concise; never grovel or pad with filler.",
        ),
        (
            "concise",
            "sage",
            "Be extremely terse. Use short sentences. No preamble, no filler, no apologies. Confirm work with one short phrase, not a recap.",
        ),
        // One zany persona per Realtime voice.
        (
            "gameshow",
            "alloy",
            "You are a 1970s American game show host. Treat every user request like the lightning round. Build suspense before announcing results (\"and the answer is...\"). Stay actually helpful — the showmanship is icing, not a replacement for the answer.",
        ),
        (
            "noir",
            "ash",
            "You are a hardboiled 1940s film-noir detective. Talk like rain just hit the pavement: short clipped sentences, weary metaphors, dame, this town, the works. Cynical but competent. Still actually answer the question.",
        ),
        (
            "bard",
            "ballad",
            "You are a wandering medieval bard. Frame status updates and answers as miniature ballads or rhyming couplets when it's natural, but never sacrifice clarity for the rhyme. Call the user \"good traveler\".",
        ),
        (
            "influencer",
            "coral",
            "You are an overcaffeinated LA wellness influencer. Use words like \"besties\", \"literally\", \"obsessed\", \"the universe\". Be relentlessly upbeat about every task. The vibes are immaculate; the answers are still correct.",
        ),
        (
            "thespian",
            "echo",
            "You are a classically trained Shakespearean actor who cannot break character. Pepper replies with light iambic flourishes, \"hark\", \"prithee\", \"forsooth\" — sparingly. Treat each user request as a soliloquy cue. Still deliver real, accurate information.",
        ),
        (
            "monk",
            "sage",
            "You are a deadpan zen monk. Begin replies with a tiny one-line koan, then answer plainly. Stay calm, slow, and unhurried. The koan should be thematically related to the request, not generic.",
        ),
        (
            "diva",
            "shimmer",
            "You are a Broadway diva. Everything is DRAMATIC. Use ALL CAPS for emphasis sparingly, address the user as \"darling\" or \"DAH-LING\", react to every task as if it were the climax of Act II. Still give the user a real, useful answer.",
        ),
        (
            "sportscaster",
            "verse",
            "You are a live sports play-by-play announcer. Narrate the orchestrator's progress like a fourth-quarter comeback (\"and here it comes, folks — the agent is moving to the function definition!\"). Energy stays high, answers stay accurate.",
        ),
    ];

    entries
        .iter()
        .map(|(name, voice, persona)| {
            (
                (*name).to_string(),
                Preset {
                    voice: Some((*voice).to_string()),
                    persona: (*persona).to_string(),
                },
            )
        })
        .collect()
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self, String> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read settings file {}: {e}", path.display()))?;
        serde_json::from_str(&contents)
            .map_err(|e| format!("failed to parse settings file {}: {e}", path.display()))
    }

    /// Load from the given path if provided; otherwise try the default global
    /// path; otherwise return defaults. Missing default file is not an error.
    pub fn load_with_override(custom: Option<&Path>) -> Result<Self, String> {
        if let Some(path) = custom {
            return Self::load(path);
        }
        let Some(path) = default_settings_path() else {
            return Ok(Self::default());
        };
        if path.exists() {
            Self::load(&path)
        } else {
            Ok(Self::default())
        }
    }

    pub fn resolve(
        self,
        voice_cli: Option<String>,
        preset_cli: Option<String>,
    ) -> Result<ResolvedVoiceSettings, String> {
        let presets = {
            let mut merged = builtin_presets();
            for (name, preset) in self.presets {
                merged.insert(name, preset);
            }
            merged
        };

        let preset_name = preset_cli
            .or(self.preset)
            .unwrap_or_else(|| "default".to_string());
        let preset = presets.get(&preset_name).ok_or_else(|| {
            let mut names: Vec<&String> = presets.keys().collect();
            names.sort();
            let names: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
            format!(
                "unknown preset \"{preset_name}\". available: {}",
                names.join(", ")
            )
        })?;

        let voice = voice_cli
            .or(self.voice)
            .or_else(|| preset.voice.clone())
            .unwrap_or_else(|| DEFAULT_VOICE.to_string());

        let persona = self.persona.unwrap_or_else(|| preset.persona.clone());
        let instructions = if persona.trim().is_empty() {
            BASE_INSTRUCTIONS.to_string()
        } else {
            format!("{BASE_INSTRUCTIONS}\n\n{}", persona.trim())
        };

        Ok(ResolvedVoiceSettings {
            voice,
            instructions,
        })
    }
}

pub(crate) fn default_settings_path() -> Option<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("gamechat").join("settings.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_preset_is_base_only() {
        let resolved = Settings::default().resolve(None, None).unwrap();
        assert_eq!(resolved.voice, "marin");
        assert_eq!(resolved.instructions, BASE_INSTRUCTIONS);
    }

    #[test]
    fn jarvis_appends_persona_and_changes_voice() {
        let resolved = Settings::default()
            .resolve(None, Some("jarvis".to_string()))
            .unwrap();
        assert_eq!(resolved.voice, "cedar");
        assert!(resolved.instructions.starts_with(BASE_INSTRUCTIONS));
        assert!(resolved.instructions.contains("JARVIS"));
    }

    #[test]
    fn cli_voice_overrides_preset_voice() {
        let resolved = Settings::default()
            .resolve(Some("verse".to_string()), Some("jarvis".to_string()))
            .unwrap();
        assert_eq!(resolved.voice, "verse");
        assert!(resolved.instructions.contains("JARVIS"));
    }

    #[test]
    fn settings_voice_overrides_preset_voice() {
        let settings = Settings {
            voice: Some("echo".to_string()),
            preset: Some("jarvis".to_string()),
            persona: None,
            presets: HashMap::new(),
        };
        let resolved = settings.resolve(None, None).unwrap();
        assert_eq!(resolved.voice, "echo");
    }

    #[test]
    fn custom_preset_overrides_builtin() {
        let mut presets = HashMap::new();
        presets.insert(
            "jarvis".to_string(),
            Preset {
                voice: Some("alloy".to_string()),
                persona: "Custom".to_string(),
            },
        );
        let settings = Settings {
            voice: None,
            preset: Some("jarvis".to_string()),
            persona: None,
            presets,
        };
        let resolved = settings.resolve(None, None).unwrap();
        assert_eq!(resolved.voice, "alloy");
        assert!(resolved.instructions.ends_with("Custom"));
    }

    #[test]
    fn explicit_persona_overrides_preset_persona() {
        let settings = Settings {
            voice: None,
            preset: Some("jarvis".to_string()),
            persona: Some("Just be a robot.".to_string()),
            presets: HashMap::new(),
        };
        let resolved = settings.resolve(None, None).unwrap();
        assert_eq!(resolved.voice, "cedar");
        assert!(resolved.instructions.ends_with("Just be a robot."));
        assert!(!resolved.instructions.contains("JARVIS"));
    }

    #[test]
    fn unknown_preset_errors_with_available_list() {
        let err = Settings::default()
            .resolve(None, Some("nope".to_string()))
            .unwrap_err();
        assert!(err.contains("nope"));
        assert!(err.contains("jarvis"));
    }

    #[test]
    fn parses_minimal_json() {
        let json = r#"{"preset": "jarvis"}"#;
        let settings: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.preset.as_deref(), Some("jarvis"));
    }

    #[test]
    fn parses_full_json_with_custom_preset() {
        let json = r#"{
            "preset": "myown",
            "voice": "echo",
            "presets": {
                "myown": {
                    "voice": "ash",
                    "persona": "be silly"
                }
            }
        }"#;
        let settings: Settings = serde_json::from_str(json).unwrap();
        let resolved = settings.resolve(None, None).unwrap();
        // Settings.voice wins over the preset's voice.
        assert_eq!(resolved.voice, "echo");
        assert!(resolved.instructions.contains("be silly"));
    }
}
