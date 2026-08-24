use std::path::PathBuf;

use anyhow::{Context, Result};
use relm4::gtk::gdk;
use serde::{Deserialize, Serialize};

/// Parse a binding spec like `"ctrl h"`, `"j"` or `"<Primary>plus"` into a
/// modifier mask and key. Modifiers (`ctrl`/`control`/`primary`, `shift`,
/// `alt`, `super`) may be written with or without angle brackets. Returns
/// `None` if no key name is recognized.
pub fn parse_binding(spec: &str) -> Option<(gdk::ModifierType, gdk::Key)> {
    let mut modifiers = gdk::ModifierType::empty();
    let mut key = None;
    for raw in spec.split_whitespace() {
        let token = raw.trim_matches(['<', '>']);
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "primary" => modifiers |= gdk::ModifierType::CONTROL_MASK,
            "shift" => modifiers |= gdk::ModifierType::SHIFT_MASK,
            "alt" | "option" => modifiers |= gdk::ModifierType::ALT_MASK,
            "super" | "meta" | "cmd" => modifiers |= gdk::ModifierType::SUPER_MASK,
            "space" => key = Some(gdk::Key::space),
            other => {
                if let Some(k) = gdk::Key::from_name(other) {
                    key = Some(k);
                } else {
                    // Accept the printed symbol for common punctuation keys.
                    let symbol = match other {
                        "plus" => "+",
                        "minus" => "-",
                        "equal" => "=",
                        _ => other,
                    };
                    if let Some(k) = gdk::Key::from_name(symbol) {
                        key = Some(k);
                    }
                }
            }
        }
    }
    key.map(|key| (modifiers, key))
}

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

/// Key bindings. Each value is a space-separated list of modifiers (e.g.
/// `ctrl`, `shift`, `alt`, `super`) followed by a key name (`j`, `space`,
/// `plus`...); a missing/unknown key disables the binding. `sidebar_toggle`
/// is a two-key chord (`space e`) instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeymapConfig {
    #[serde(default = "key_default_scroll_down")]
    pub scroll_down: String,
    #[serde(default = "key_default_scroll_up")]
    pub scroll_up: String,
    #[serde(default = "key_default_scroll_left")]
    pub scroll_left: String,
    #[serde(default = "key_default_scroll_right")]
    pub scroll_right: String,
    /// Two-key chord: `<leader> <key>` toggles the sidebar.
    #[serde(default = "key_default_sidebar_toggle")]
    pub sidebar_toggle: String,
    /// Sidebar cursor keys (when the sidebar has keyboard focus).
    #[serde(default = "key_default_sidebar_down")]
    pub sidebar_down: String,
    #[serde(default = "key_default_sidebar_up")]
    pub sidebar_up: String,
    /// Open a leaf entry, or expand/collapse an entry with children.
    #[serde(default = "key_default_sidebar_activate")]
    pub sidebar_activate: String,
    /// Collapse the entry with children under the sidebar cursor.
    #[serde(default = "key_default_sidebar_collapse")]
    pub sidebar_collapse: String,
    /// Move focus to the TOC sidebar.
    #[serde(default = "key_default_focus_sidebar")]
    pub focus_sidebar: String,
    /// Move focus to the PDF content.
    #[serde(default = "key_default_focus_pdf")]
    pub focus_pdf: String,
    #[serde(default = "key_default_zoom_in")]
    pub zoom_in: String,
    #[serde(default = "key_default_zoom_out")]
    pub zoom_out: String,
}

macro_rules! key_default {
    ($name:ident, $value:literal) => {
        fn $name() -> String {
            $value.into()
        }
    };
}

key_default!(key_default_scroll_down, "j");
key_default!(key_default_scroll_up, "k");
key_default!(key_default_scroll_left, "h");
key_default!(key_default_scroll_right, "l");
key_default!(key_default_sidebar_toggle, "space e");
key_default!(key_default_sidebar_down, "j");
key_default!(key_default_sidebar_up, "k");
key_default!(key_default_sidebar_activate, "l");
key_default!(key_default_sidebar_collapse, "h");
key_default!(key_default_focus_sidebar, "ctrl h");
key_default!(key_default_focus_pdf, "ctrl l");
key_default!(key_default_zoom_in, "ctrl plus");
key_default!(key_default_zoom_out, "ctrl minus");

impl Default for KeymapConfig {
    fn default() -> Self {
        Self {
            scroll_down: "j".into(),
            scroll_up: "k".into(),
            scroll_left: "h".into(),
            scroll_right: "l".into(),
            sidebar_toggle: "space e".into(),
            sidebar_down: "j".into(),
            sidebar_up: "k".into(),
            sidebar_activate: "l".into(),
            sidebar_collapse: "h".into(),
            focus_sidebar: "ctrl h".into(),
            focus_pdf: "ctrl l".into(),
            zoom_in: "ctrl plus".into(),
            zoom_out: "ctrl minus".into(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_binding_defaults() {
        let keymap = KeymapConfig::default();
        assert_eq!(
            parse_binding(&keymap.scroll_down),
            Some((gdk::ModifierType::empty(), gdk::Key::j))
        );
        assert_eq!(
            parse_binding(&keymap.scroll_up),
            Some((gdk::ModifierType::empty(), gdk::Key::k))
        );
        assert_eq!(
            parse_binding(&keymap.focus_sidebar),
            Some((gdk::ModifierType::CONTROL_MASK, gdk::Key::h))
        );
        assert_eq!(
            parse_binding(&keymap.focus_pdf),
            Some((gdk::ModifierType::CONTROL_MASK, gdk::Key::l))
        );
        assert_eq!(
            parse_binding(&keymap.zoom_in),
            Some((gdk::ModifierType::CONTROL_MASK, gdk::Key::plus))
        );
        assert_eq!(
            parse_binding(&keymap.zoom_out),
            Some((gdk::ModifierType::CONTROL_MASK, gdk::Key::minus))
        );
        assert_eq!(
            parse_binding(&keymap.sidebar_activate),
            Some((gdk::ModifierType::empty(), gdk::Key::l))
        );
        assert_eq!(parse_binding("bogus_key"), None);
        assert_eq!(parse_binding(""), None);
    }
}
