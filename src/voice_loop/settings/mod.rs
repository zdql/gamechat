use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

mod presets;

pub(crate) use presets::{builtin_presets, DEFAULT_VOICE};
use presets::BASE_INSTRUCTIONS;

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
