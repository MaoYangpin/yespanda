use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use relm4::gtk::glib;
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// A margin note anchored to a position inside one document. `page` is
/// 1-based as shown in the UI; `y_frac` is the anchor's vertical position
/// on that page (0 = top, 1 = bottom), independent of zoom.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub id: u64,
    pub doc: PathBuf,
    pub page: usize,
    pub y_frac: f64,
    pub created: String,
    pub text: String,
}

/// All notes across every document, persisted to
/// `~/.config/yespanda/notes.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Notes {
    #[serde(default)]
    notes: Vec<Note>,
    next_id: u64,
}

impl Notes {
    /// Path of the notes file inside the config directory.
    pub fn path() -> PathBuf {
        Config::config_dir().join("notes.toml")
    }

    /// Load from the config directory; a missing or malformed file yields
    /// an empty collection.
    pub fn load() -> Self {
        if let Err(error) = std::fs::create_dir_all(Config::config_dir()) {
            eprintln!("failed to create config dir: {error:#}");
        }
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|raw| toml::from_str(&raw).ok())
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn load_from(dir: &Path) -> Self {
        std::fs::read_to_string(dir.join("notes.toml"))
            .ok()
            .and_then(|raw| toml::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// Write through to the config directory.
    pub fn save(&self) -> Result<()> {
        self.save_to(&Config::config_dir())
    }

    fn save_to(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        let raw = toml::to_string_pretty(self)?;
        std::fs::write(dir.join("notes.toml"), raw)
            .context("failed to write notes.toml")
    }

    /// Append a note and return its id.
    pub fn add(&mut self, doc: &Path, page: usize, y_frac: f64, text: String) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let created = glib::DateTime::now_local()
            .and_then(|dt| dt.format("%Y-%m-%d %H:%M"))
            .map(|s| s.to_string())
            .unwrap_or_default();
        self.notes.push(Note {
            id,
            doc: doc.to_path_buf(),
            page,
            y_frac,
            created,
            text,
        });
        id
    }

    /// Replace the text of an existing note; false if `id` is unknown.
    pub fn update_text(&mut self, id: u64, text: String) -> bool {
        match self.notes.iter_mut().find(|note| note.id == id) {
            Some(note) => {
                note.text = text;
                true
            }
            None => false,
        }
    }

    /// Drop a note by id; true when something was removed.
    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.notes.len();
        self.notes.retain(|note| note.id != id);
        before != self.notes.len()
    }

    pub fn get(&self, id: u64) -> Option<&Note> {
        self.notes.iter().find(|note| note.id == id)
    }

    /// Notes of one document, reading order (page, then top-to-bottom).
    pub fn for_doc(&self, doc: &Path) -> Vec<&Note> {
        let mut found: Vec<&Note> = self
            .notes
            .iter()
            .filter(|note| note.doc == doc)
            .collect();
        found.sort_by(|a, b| {
            a.page
                .cmp(&b.page)
                .then(a.y_frac.partial_cmp(&b.y_frac).unwrap_or(std::cmp::Ordering::Equal))
                .then(a.id.cmp(&b.id))
        });
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("yespanda-notes-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn add_filter_remove_roundtrip() {
        let mut notes = Notes::default();
        let book = Path::new("/tmp/opencode/real.pdf");
        let other = Path::new("/tmp/opencode/test.pdf");

        let a = notes.add(book, 10, 0.5, "middle of page 10".into());
        let _b = notes.add(other, 1, 0.0, "other doc".into());
        let c = notes.add(book, 3, 0.9, "near bottom".into());

        // Reading order for one document only.
        let listed = notes.for_doc(book);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].page, 3);
        assert_eq!(listed[1].id, a);

        assert!(notes.update_text(a, "revised".into()));
        assert_eq!(notes.get(a).unwrap().text, "revised");

        assert!(notes.remove(c));
        assert!(!notes.remove(c));
        assert_eq!(notes.for_doc(book).len(), 1);
    }

    #[test]
    fn empty_text_still_stores_and_toml_roundtrips() {
        let dir = tmp_dir("roundtrip");
        let mut notes = Notes::default();
        let id = notes.add(Path::new("/tmp/x.pdf"), 7, 0.25, "hello".into());
        notes.save_to(&dir).expect("save");

        let loaded = Notes::load_from(&dir);
        assert_eq!(loaded.get(id).map(|n| n.text.as_str()), Some("hello"));
        assert_eq!(loaded.next_id, notes.next_id);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
