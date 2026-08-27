use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;
use relm4::gtk::prelude::AdjustmentExt;
use adw::prelude::*;
use relm4::{adw, Component, ComponentParts, ComponentSender};

const FILTER_DEBOUNCE_MS: u32 = 150;
const MAX_EMPTY_RESULTS: usize = 50;

/// Expand `~`, `~/...` or an empty string to a concrete directory.
pub fn expand_root(root: &str) -> String {
    if root.trim().is_empty() || root == "~" {
        return home_dir();
    }
    if let Some(rest) = root.strip_prefix("~/") {
        return format!("{}/{}", home_dir(), rest);
    }
    root.to_string()
}

fn home_dir() -> String {
    std::env::var_os("HOME")
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".into())
}

pub struct PickerInit {
    /// Directory fd searches (already expanded).
    pub root: String,
    /// Dialog content width in pixels.
    pub width: i32,
    /// Dialog content height in pixels.
    pub height: i32,
    /// Window this picker is transient for.
    pub parent: gtk::Window,
}

pub struct PickerDialog {
    candidates: Vec<String>,
    results: Vec<String>,
    done: bool,
    search_source: Option<glib::SourceId>,
    /// Result rows live here; built outside `view!` so rows can be
    /// `adw::ActionRow`s inside a `.boxed-list`.
    results_list: gtk::ListBox,
}

#[derive(Debug)]
pub enum PickerMsg {
    /// Full candidate list produced by `fd`.
    Candidates(Vec<String>),
    /// The filter text changed.
    Query(String),
    /// Debounced: run `fzf --filter` with this query.
    Filter(String),
    /// Matches produced by `fzf --filter`.
    Results(Vec<String>),
    /// Open the result at `index`.
    Confirm(usize),
    /// Open the currently selected result (Enter in the search entry).
    ConfirmCurrent,
    /// The window was closed (Esc or the close button).
    Closed,
}

#[derive(Debug)]
pub enum PickerOutput {
    Pick(PathBuf),
    Cancelled,
}

#[relm4::component(pub)]
impl Component for PickerDialog {
    type Init = PickerInit;
    type Input = PickerMsg;
    type Output = PickerOutput;
    type CommandOutput = ();

    view! {
        #[root]
        adw::Dialog {
            set_title: "Open PDF",
            connect_closed[sender] => move |_dialog| {
                sender.input(PickerMsg::Closed);
            },
            #[wrap(Some)]
            set_child = content_box = &gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                set_spacing: 12,
                set_margin_top: 12,
                set_margin_bottom: 12,
                set_margin_start: 12,
                set_margin_end: 12,
                append: search_entry = &gtk::SearchEntry {
                    set_placeholder_text: Some("Filter PDFs…"),
                    // Match the boxed-list's horizontal inset so the field
                    // lines up with the result card below it.
                    set_margin_top: 6,
                    set_margin_bottom: 6,
                    set_margin_start: 6,
                    set_margin_end: 6,
                    connect_changed[sender] => move |entry| {
                        sender.input(PickerMsg::Query(entry.text().into()));
                    },
                    connect_activate[sender] => move |_entry| {
                        sender.input(PickerMsg::ConfirmCurrent);
                    },
                },
                append: results_scroller = &gtk::ScrolledWindow {
                    set_vexpand: true,
                    #[wrap(Some)]
                    set_child = results_stack = &gtk::Stack {
                        set_vhomogeneous: false,
                    },
                },
            },
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // The result list is a boxed-list card of ActionRows; it lives on
        // the model because `view!` only declares the stack that hosts it.
        let results_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["boxed-list"])
            .valign(gtk::Align::Start)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        {
            let sender = sender.clone();
            results_list.connect_row_activated(move |_list, row| {
                sender.input(PickerMsg::Confirm(row.index().max(0) as usize));
            });
        }

        let model = PickerDialog {
            candidates: Vec::new(),
            results: Vec::new(),
            done: false,
            search_source: None,
            results_list: results_list.clone(),
        };

        let widgets = view_output!();

        // Two stack pages: the result card and an empty-state notice.
        let empty_page = adw::StatusPage::builder()
            .icon_name("system-search-symbolic")
            .title("No matching files")
            .description("Try a different search term.")
            .build();
        widgets.results_stack.add_named(&results_list, Some("results"));
        widgets.results_stack.add_named(&empty_page, Some("empty"));
        widgets.results_stack.set_visible_child_name("empty");

        // GtkSearchEntry consumes Escape itself (emitting `stop-search`)
        // before the event can reach the window-level key controller, so
        // Esc while typing must be handled here.
        {
            let window = root.clone();
            widgets.search_entry.connect_stop_search(move |_| {
                window.close();
            });
        }

        root.set_content_width(init.width);
        root.set_content_height(init.height);
        root.present(Some(&init.parent));

        // Esc closes the dialog; Ctrl+n / Ctrl+p move through the input line
        // and the result rows (the entry is the "first line"); Enter or
        // space opens the selected row.
        let key_controller = gtk::EventControllerKey::new();
        {
            let window = root.clone();
            let entry = widgets.search_entry.clone();
            let list = results_list.clone();
            let scroller = widgets.results_scroller.clone();
            let forward = sender.input_sender().clone();
            key_controller.connect_key_pressed(move |_, keyval, _, state| {
                if keyval == gdk::Key::Escape {
                    // Closing fires `close_request`, which emits Cancelled.
                    window.close();
                    return glib::Propagation::Stop;
                }
                if state.contains(gdk::ModifierType::CONTROL_MASK) {
                    if keyval == gdk::Key::n {
                        move_selection(&entry, &scroller, &list, 1);
                        return glib::Propagation::Stop;
                    }
                    if keyval == gdk::Key::p {
                        move_selection(&entry, &scroller, &list, -1);
                        return glib::Propagation::Stop;
                    }
                    return glib::Propagation::Proceed;
                }
                // AdwActionRow consumes Enter/space for its own activation
                // before GtkListBox can emit row-activated, so open the
                // selected row here instead.
                if matches!(keyval, gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::space)
                    && !entry.has_focus()
                    && let Some(row) = list.selected_row()
                {
                    forward.send(PickerMsg::Confirm(row.index().max(0) as usize)).ok();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
        }
        root.add_controller(key_controller);

        // Emacs-style line editing in the input: Ctrl+a/e move to start/end
        // of the line, Ctrl+f/b move one character forward/backward. GTK
        // binds Ctrl+a to "select all" on the entry's text widget, so this
        // controller runs in the capture phase to intercept those keys
        // before the default binding handles them.
        let entry_keys = gtk::EventControllerKey::new();
        entry_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let entry = widgets.search_entry.clone();
            entry_keys.connect_key_pressed(move |_, keyval, _, state| {
                if !state.contains(gdk::ModifierType::CONTROL_MASK) {
                    return glib::Propagation::Proceed;
                }
                let length = entry.text().chars().count() as i32;
                let position = match keyval {
                    gdk::Key::a => Some(0),
                    gdk::Key::e => Some(length),
                    gdk::Key::f => Some((entry.position() + 1).min(length)),
                    gdk::Key::b => Some((entry.position() - 1).max(0)),
                    _ => return glib::Propagation::Proceed,
                };
                if let Some(position) = position {
                    entry.set_position(position);
                }
                glib::Propagation::Stop
            });
        }
        widgets.search_entry.add_controller(entry_keys);

        // Kick off the full candidate listing on a worker thread.
        let tx = sender.input_sender().clone();
        let search_root = init.root.clone();
        std::thread::spawn(move || {
            let _ = tx.send(PickerMsg::Candidates(list_candidates(&search_root)));
        });

        let entry = widgets.search_entry.clone();
        glib::idle_add_local_once(move || {
            entry.grab_focus();
        });

        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        msg: Self::Input,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match msg {
            PickerMsg::Candidates(paths) => {
                self.candidates = paths;
                self.results = filter(&self.candidates, "");
                rebuild_list(&self.results_list, &self.results);
                update_stack_page(widgets, self.results.len());
            }
            PickerMsg::Query(text) => {
                if let Some(source) = self.search_source.take() {
                    source.remove();
                }
                let tx = sender.input_sender().clone();
                let source = glib::timeout_add_local_once(
                    Duration::from_millis(FILTER_DEBOUNCE_MS as u64),
                    move || {
                        let _ = tx.send(PickerMsg::Filter(text));
                    },
                );
                self.search_source = Some(source);
            }
            PickerMsg::Filter(query) => {
                self.search_source = None;
                let candidates = self.candidates.clone();
                let tx = sender.input_sender().clone();
                std::thread::spawn(move || {
                    let results = filter(&candidates, &query);
                    let _ = tx.send(PickerMsg::Results(results));
                });
            }
            PickerMsg::Results(results) => {
                self.results = results;
                rebuild_list(&self.results_list, &self.results);
                update_stack_page(widgets, self.results.len());
            }
            PickerMsg::Confirm(index) => {
                if !self.done
                    && let Some(path) = self.results.get(index) {
                        self.done = true;
                        let _ = sender.output(PickerOutput::Pick(PathBuf::from(path)));
                        root.close();
                    }
            }
            PickerMsg::ConfirmCurrent => {
                if let Some(row) = self.results_list.selected_row() {
                    let index = row.index().max(0) as usize;
                    if !self.done
                        && let Some(path) = self.results.get(index) {
                            self.done = true;
                            let _ = sender.output(PickerOutput::Pick(PathBuf::from(path)));
                            root.close();
                        }
                }
            }
            PickerMsg::Closed => {
                if !self.done {
                    self.done = true;
                    let _ = sender.output(PickerOutput::Cancelled);
                }
            }
        }
        self.update_view(widgets, sender);
    }
}

/// List every PDF under `root`, NUL-separated, via `fd`.
fn list_candidates(root: &str) -> Vec<String> {
    let output = match Command::new("fd")
        .args(["-0", "-e", "pdf", ".", root])
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(_) => return Vec::new(),
    };
    parse_nul(&output.stdout)
}

/// Fuzzy-filter `candidates` with `fzf --filter`; empty query lists a slice.
fn filter(candidates: &[String], query: &str) -> Vec<String> {
    let query = query.trim();
    if query.is_empty() {
        return candidates.iter().take(MAX_EMPTY_RESULTS).cloned().collect();
    }
    let mut child = match Command::new("fzf")
        .args(["--read0", "--print0", "--filter", query])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return Vec::new(),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let mut data = Vec::new();
        for candidate in candidates {
            data.extend_from_slice(candidate.as_bytes());
            data.push(0);
        }
        let _ = stdin.write_all(&data);
    }
    match child.wait_with_output() {
        Ok(output) => parse_nul(&output.stdout),
        Err(_) => Vec::new(),
    }
}

fn parse_nul(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|&b| b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

/// Move the active line by `delta` (+1 down, -1 up). The search entry counts
/// as the line above the first result row, so Ctrl+p on the first result
/// returns focus to the input, and Ctrl+n on the input selects the first.
fn move_selection(
    entry: &gtk::SearchEntry,
    scroller: &gtk::ScrolledWindow,
    list: &gtk::ListBox,
    delta: isize,
) {
    let current = if entry.has_focus() {
        None
    } else {
        list.selected_row().map(|row| row.index().max(0) as usize)
    };
    match current {
        None => {
            if delta > 0
                && let Some(row) = list.row_at_index(0) {
                    list.select_row(Some(&row));
                    row.grab_focus();
                    scroll_row_into_view(scroller, list, &row);
                }
        }
        Some(index) => {
            let target = index as isize + delta;
            if target < 0 {
                entry.grab_focus();
            } else if let Some(row) = list.row_at_index(target as i32) {
                list.select_row(Some(&row));
                row.grab_focus();
                scroll_row_into_view(scroller, list, &row);
            }
        }
    }
}

fn scroll_row_into_view(
    scroller: &gtk::ScrolledWindow,
    list: &gtk::ListBox,
    row: &gtk::ListBoxRow,
) {
    let y = row
        .compute_point(list, &gtk::graphene::Point::new(0.0, 0.0))
        .map(|p| p.y() as f64)
        .unwrap_or(0.0);
    let top = y.max(0.0);
    let bottom = top + row.height() as f64;
    let adjustment = scroller.vadjustment();
    let view = adjustment.page_size();
    let value = if top < adjustment.value() {
        top
    } else if bottom > adjustment.value() + view {
        bottom - view
    } else {
        adjustment.value()
    };
    let max = (adjustment.upper() - view).max(adjustment.lower());
    adjustment.set_value(value.clamp(adjustment.lower(), max));
}

/// Split a path into `(file name, parent directory)`, abbreviating a
/// `$HOME` prefix in the directory as `~`. The directory is empty when the
/// path has no parent.
fn split_path(path: &str) -> (String, String) {
    let path = std::path::Path::new(path);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let dir = path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let home = home_dir();
    let dir = if dir == home {
        "~".to_string()
    } else if let Some(rest) = dir.strip_prefix(&format!("{home}/")) {
        format!("~/{rest}")
    } else {
        dir
    };
    (name, dir)
}

/// Fill the result list with one ActionRow per match: the file name as the
/// title, its (tilde-abbreviated) directory as the dimmed subtitle.
fn rebuild_list(list: &gtk::ListBox, results: &[String]) {
    list.remove_all();
    for path in results {
        let (name, dir) = split_path(path);
        let row = adw::ActionRow::new();
        // Titles are parsed as Pango markup by default; file names with a
        // raw `&` (e.g. `html & css.pdf`) would fail to render otherwise.
        row.set_use_markup(false);
        row.set_title(&name);
        if !dir.is_empty() {
            row.set_subtitle(&dir);
        }
        list.append(&row);
    }
}

/// Show the empty-state page when a filter yields nothing.
fn update_stack_page(widgets: &PickerDialogWidgets, result_count: usize) {
    widgets.results_stack.set_visible_child_name(if result_count == 0 {
        "empty"
    } else {
        "results"
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools_available() -> bool {
        Command::new("fd").arg("--version").output().is_ok()
            && Command::new("fzf").arg("--version").output().is_ok()
    }

    #[test]
    fn expand_root_handles_tilde() {
        let home = home_dir();
        assert_eq!(expand_root("~"), home);
        assert_eq!(expand_root(""), home);
        assert_eq!(expand_root("~/docs"), format!("{home}/docs"));
        assert_eq!(expand_root("/tmp/opencode"), "/tmp/opencode".to_string());
    }

    #[test]
    fn split_path_abbreviates_home() {
        let home = home_dir();
        assert_eq!(
            split_path(&format!("{home}/Books/ai/math.pdf")),
            ("math.pdf".to_string(), "~/Books/ai".to_string())
        );
        // Directly inside $HOME collapses to just `~`.
        assert_eq!(
            split_path(&format!("{home}/x.pdf")),
            ("x.pdf".to_string(), "~".to_string())
        );
        // Paths outside $HOME stay absolute.
        assert_eq!(
            split_path("/mnt/data/report.pdf"),
            ("report.pdf".to_string(), "/mnt/data".to_string())
        );
        // A bare file name has no directory part.
        assert_eq!(
            split_path("solo.pdf"),
            ("solo.pdf".to_string(), String::new())
        );
    }

    #[test]
    fn fd_and_fzf_agree_on_candidates() {
        if !tools_available() {
            eprintln!("fd/fzf missing, skipping");
            return;
        }
        let candidates = list_candidates("/tmp/opencode");
        assert!(!candidates.is_empty(), "fd should find PDFs in /tmp/opencode");
        assert!(candidates.iter().any(|p| p.contains("real.pdf")));

        let matches = filter(&candidates, "real");
        assert!(matches.iter().any(|p| p.contains("real.pdf")));

        let limited = filter(&candidates, "");
        assert_eq!(limited, candidates.into_iter().take(MAX_EMPTY_RESULTS).collect::<Vec<_>>());
    }
}
