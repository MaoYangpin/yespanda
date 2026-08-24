use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// User settings persisted to `~/.config/yespanda/config.toml`
/// (or `$XDG_CONFIG_HOME/yespanda/config.toml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub window: WindowConfig,
    pub sidebar: SidebarConfig,
    /// PDF opened last session; reopened on the next launch.
    pub last_file: Option<String>,
    /// Whether fit-to-width is the default viewing mode.
    pub fit_width: bool,
    pub theme: ThemePreference,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowConfig {
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidebarConfig {
    pub width_fraction: f64,
    pub collapsed: bool,
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
            window: WindowConfig {
                width: 1100,
                height: 760,
                maximized: false,
            },
            sidebar: SidebarConfig {
                width_fraction: 0.24,
                collapsed: false,
            },
            last_file: None,
            fit_width: true,
            theme: ThemePreference::System,
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
