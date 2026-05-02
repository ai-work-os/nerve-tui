use crate::theme::Theme;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiConfig {
    #[serde(default = "default_theme")]
    pub theme: String,
}

fn default_theme() -> String {
    "warm-light".to_string()
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self { theme: default_theme() }
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("nerve-tui")
        .join("config.toml")
}

pub fn load_config() -> TuiConfig {
    let path = config_path();
    if let Ok(content) = std::fs::read_to_string(&path) {
        toml::from_str(&content).unwrap_or_default()
    } else {
        TuiConfig::default()
    }
}

pub fn save_config(cfg: &TuiConfig) {
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(content) = toml::to_string_pretty(cfg) {
        let _ = std::fs::write(&path, content);
    }
}

pub fn resolve_theme(name: &str) -> Option<Theme> {
    match name {
        "warm-light" => Some(Theme::warm_light()),
        "opencode-dark" => Some(Theme::opencode_dark()),
        _ => None,
    }
}

pub fn available_themes() -> Vec<&'static str> {
    vec!["warm-light", "opencode-dark"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_warm_light() {
        let cfg = TuiConfig::default();
        assert_eq!(cfg.theme, "warm-light");
    }

    #[test]
    fn resolve_builtin_theme() {
        assert!(resolve_theme("warm-light").is_some());
        assert!(resolve_theme("opencode-dark").is_some());
        assert!(resolve_theme("nonexistent").is_none());
    }

    #[test]
    fn config_roundtrip() {
        let cfg = TuiConfig { theme: "opencode-dark".to_string() };
        let toml_str = toml::to_string_pretty(&cfg).unwrap();
        let parsed: TuiConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.theme, "opencode-dark");
    }
}
