use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// User settings persisted to `~/.config/yespanda/config.toml`
/// (or `$XDG_CONFIG_HOME/yespanda/config.toml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub window: WindowConfig,
    #[serde(default)]
    pub sidebar: SidebarConfig,
    /// PDF opened last session; reopened on the next launch.
    #[serde(default)]
    pub last_file: Option<String>,
    /// Whether fit-to-width is the default viewing mode.
    #[serde(default)]
    pub fit_width: bool,
    #[serde(default)]
    pub theme: ThemePreference,
    #[serde(default)]
    pub keymap: KeymapConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowConfig {
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            width: 1100,
            height: 760,
            maximized: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidebarConfig {
    pub width_fraction: f64,
    pub collapsed: bool,
}

impl Default for SidebarConfig {
    fn default() -> Self {
        Self {
            width_fraction: 0.24,
            collapsed: false,
        }
    }
}

/// Vim-style key bindings. Single keys are given as a plain key name;
/// `sidebar_toggle` is a Space-prefixed chord (`space e`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeymapConfig {
    pub scroll_down: String,
    pub scroll_up: String,
    pub scroll_left: String,
    pub scroll_right: String,
    pub sidebar_toggle: String,
}

impl Default for KeymapConfig {
    fn default() -> Self {
        Self {
            scroll_down: "j".into(),
            scroll_up: "k".into(),
            scroll_left: "h".into(),
            scroll_right: "l".into(),
            sidebar_toggle: "space e".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            window: WindowConfig::default(),
            sidebar: SidebarConfig::default(),
            last_file: None,
            fit_width: true,
            theme: ThemePreference::System,
            keymap: KeymapConfig::default(),
        }
    }
}

impl Config {
    /// Path of the config file, honouring the XDG base directory spec.
    pub fn path() -> PathBuf {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME").unwrap_or_default();
                PathBuf::from(home).join(".config")
            })
            .join("yespanda")
            .join("config.toml")
    }

    /// Load the config file; a missing or malformed file yields defaults.
    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|raw| toml::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// Write the config file, creating the directory if needed.
    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(self)?;
        std::fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))
    }
}

impl From<ThemePreference> for adw::ColorScheme {
    fn from(value: ThemePreference) -> Self {
        match value {
            ThemePreference::System => adw::ColorScheme::Default,
            ThemePreference::Light => adw::ColorScheme::ForceLight,
            ThemePreference::Dark => adw::ColorScheme::ForceDark,
        }
    }
}
