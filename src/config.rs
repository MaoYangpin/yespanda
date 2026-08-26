use std::path::{Path, PathBuf};

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
                // Preserve the original case for the key itself: Shift+g
                // produces the uppercase keyval (`G`), so a binding spelled
                // `shift G` must look up the uppercase key name.
                let token = raw.trim_matches(['<', '>']);
                let parsed = gdk::Key::from_name(token).or_else(|| {
                    // Accept the printed symbol for common punctuation keys.
                    gdk::Key::from_name(match other {
                        "plus" => "+",
                        "minus" => "-",
                        "equal" => "=",
                        "slash" => "/",
                        _ => return None,
                    })
                });
                if let Some(k) = parsed {
                    key = Some(k);
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
    /// Whether fit-to-width is the default viewing mode.
    #[serde(default)]
    pub fit_width: bool,
    #[serde(default)]
    pub theme: ThemePreference,
    #[serde(default)]
    pub keymap: KeymapConfig,
    #[serde(default)]
    pub picker: PickerConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickerConfig {
    /// Directory `fd` searches for PDFs. A leading `~/` expands to the home
    /// directory; empty means `$HOME`.
    #[serde(default)]
    pub root: String,
    /// Width of the picker dialog in pixels.
    #[serde(default = "picker_default_width")]
    pub width: i32,
    /// Height of the picker dialog in pixels.
    #[serde(default = "picker_default_height")]
    pub height: i32,
}

fn picker_default_width() -> i32 {
    720
}

fn picker_default_height() -> i32 {
    520
}

impl Default for PickerConfig {
    fn default() -> Self {
        Self {
            root: "~".into(),
            width: picker_default_width(),
            height: picker_default_height(),
        }
    }
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
/// and `scroll_top` are two-key chords (`space e`, `g g`) instead, and the
/// `g` leader doubles as a page-jump prefix (`g 4 2 <Enter>`).
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
    /// Two-key chord (`g g`) jumping to the start of the document.
    #[serde(default = "key_default_scroll_top")]
    pub scroll_top: String,
    /// Jump to the end of the document (Shift+G).
    #[serde(default = "key_default_scroll_bottom")]
    pub scroll_bottom: String,
    /// Open the in-document search bar (`/`).
    #[serde(default = "key_default_search")]
    pub search: String,
    /// Jump to the next match while a search is active (`n`).
    #[serde(default = "key_default_search_next")]
    pub search_next: String,
    /// Jump to the previous match while a search is active (`N`).
    #[serde(default = "key_default_search_prev")]
    pub search_prev: String,
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
    /// Open the fd+fzf PDF picker dialog.
    #[serde(default = "key_default_pick_file")]
    pub pick_file: String,
    /// Alternative picker binding as a two-key chord (`space space`, LazyVim
    /// style). Both it and `pick_file` open the picker.
    #[serde(default = "key_default_pick_file_chord")]
    pub pick_file_chord: String,
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
key_default!(key_default_scroll_top, "g g");
key_default!(key_default_scroll_bottom, "shift G");
key_default!(key_default_search, "slash");
key_default!(key_default_search_next, "n");
key_default!(key_default_search_prev, "shift N");
key_default!(key_default_sidebar_toggle, "space e");
key_default!(key_default_sidebar_down, "j");
key_default!(key_default_sidebar_up, "k");
key_default!(key_default_sidebar_activate, "l");
key_default!(key_default_sidebar_collapse, "h");
key_default!(key_default_focus_sidebar, "ctrl h");
key_default!(key_default_focus_pdf, "ctrl l");
key_default!(key_default_zoom_in, "ctrl plus");
key_default!(key_default_zoom_out, "ctrl minus");
key_default!(key_default_pick_file, "ctrl o");
key_default!(key_default_pick_file_chord, "space space");

impl Default for KeymapConfig {
    fn default() -> Self {
        Self {
            scroll_down: "j".into(),
            scroll_up: "k".into(),
            scroll_left: "h".into(),
            scroll_right: "l".into(),
            scroll_top: "g g".into(),
            scroll_bottom: "shift G".into(),
            search: "slash".into(),
            search_next: "n".into(),
            search_prev: "shift N".into(),
            sidebar_toggle: "space e".into(),
            sidebar_down: "j".into(),
            sidebar_up: "k".into(),
            sidebar_activate: "l".into(),
            sidebar_collapse: "h".into(),
            focus_sidebar: "ctrl h".into(),
            focus_pdf: "ctrl l".into(),
            zoom_in: "ctrl plus".into(),
            zoom_out: "ctrl minus".into(),
            pick_file: "ctrl o".into(),
            pick_file_chord: "space space".into(),
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
            fit_width: true,
            theme: ThemePreference::System,
            keymap: KeymapConfig::default(),
            picker: PickerConfig::default(),
        }
    }
}

impl Config {
    /// Base directory for yespanda state (`$XDG_CONFIG_HOME/yespanda`),
    /// honouring the XDG base directory spec.
    pub fn config_dir() -> PathBuf {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME").unwrap_or_default();
                PathBuf::from(home).join(".config")
            })
            .join("yespanda")
    }

    /// Path of the config file, honouring the XDG base directory spec.
    pub fn path() -> PathBuf {
        Self::config_dir().join("config.toml")
    }

    /// Load the config file; a missing or malformed file yields defaults.
    /// Also makes sure the config directory exists so first-run saves work.
    pub fn load() -> Self {
        if let Err(error) = std::fs::create_dir_all(Self::config_dir()) {
            eprintln!("failed to create config dir: {error:#}");
        }
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

/// Recently opened documents with their last-viewed page, persisted to
/// `~/.config/yespanda/history`. Most recent entry first.
#[derive(Debug, Clone, Default)]
pub struct History {
    entries: Vec<(PathBuf, usize)>,
}

const HISTORY_MAX: usize = 50;

impl History {
    /// Path of the history file inside the config directory.
    pub fn path() -> PathBuf {
        Config::config_dir().join("history")
    }

    pub fn load() -> Self {
        if let Err(error) = std::fs::create_dir_all(Config::config_dir()) {
            eprintln!("failed to create config dir: {error:#}");
        }
        let mut entries = Vec::new();
        if let Ok(raw) = std::fs::read_to_string(Self::path()) {
            for line in raw.lines() {
                // Split at the last tab so paths may contain tabs themselves.
                if let Some((path, page)) = line.rsplit_once('\t')
                    && let Ok(page) = page.trim().parse::<usize>() {
                        entries.push((PathBuf::from(path), page));
                    }
            }
        }
        Self { entries }
    }

    /// The last-viewed page for `path`, if any.
    pub fn page_for(&self, path: &Path) -> Option<usize> {
        self.entries
            .iter()
            .find(|(entry, _)| entry == path)
            .map(|(_, page)| *page)
    }

    /// Most recently opened document (the file is written newest-first,
    /// so this is the first entry).
    pub fn most_recent(&self) -> Option<&Path> {
        self.entries.first().map(|(path, _)| path.as_path())
    }

    /// Record the position for `path`, moving it to the front.
    pub fn set(&mut self, path: &Path, page: usize) {
        self.entries.retain(|(entry, _)| entry != path);
        self.entries.insert(0, (path.to_path_buf(), page));
        self.entries.truncate(HISTORY_MAX);
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let mut raw = String::new();
        for (entry, page) in &self.entries {
            raw.push_str(&entry.display().to_string());
            raw.push('\t');
            raw.push_str(&page.to_string());
            raw.push('\n');
        }
        std::fs::write(&path, raw)
            .with_context(|| format!("failed to write {}", path.display()))
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
        assert_eq!(
            parse_binding(&keymap.scroll_bottom),
            Some((gdk::ModifierType::SHIFT_MASK, gdk::Key::G))
        );
        // The scroll_top chord must split into two valid key names.
        let mut tokens = keymap.scroll_top.split_whitespace();
        let leader = tokens.next().and_then(gdk::Key::from_name);
        let completer = tokens.next().and_then(gdk::Key::from_name);
        assert_eq!(leader, Some(gdk::Key::g));
        assert_eq!(completer, Some(gdk::Key::g));
        assert!(tokens.next().is_none());
        // Search bindings: `/` opens, `n` next, Shift+N previous.
        assert_eq!(
            parse_binding(&keymap.search),
            Some((gdk::ModifierType::empty(), gdk::Key::slash))
        );
        assert_eq!(
            parse_binding(&keymap.search_next),
            Some((gdk::ModifierType::empty(), gdk::Key::n))
        );
        assert_eq!(
            parse_binding(&keymap.search_prev),
            Some((gdk::ModifierType::SHIFT_MASK, gdk::Key::N))
        );
        assert_eq!(parse_binding("bogus_key"), None);
        assert_eq!(parse_binding(""), None);
    }

    #[test]
    fn history_roundtrip() {
        let base = std::env::temp_dir().join("yespanda-history-test");
        // SAFETY: test-only, single-threaded (--test-threads=1).
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &base) };

        let real = Path::new("/tmp/opencode/real.pdf");
        let other = Path::new("/tmp/opencode/test.pdf");

        let mut history = History::default();
        assert_eq!(history.page_for(real), None);
        history.set(real, 42);
        history.set(other, 7);
        history.set(real, 10); // re-open real.pdf: moves to front, updates page
        assert!(history.save().is_ok());

        let loaded = History::load();
        assert_eq!(loaded.page_for(real), Some(10));
        assert_eq!(loaded.page_for(other), Some(7));
        assert_eq!(loaded.entries[0].0, real);

        let _ = std::fs::remove_dir_all(&base);
    }
}
