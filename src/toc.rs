use std::sync::OnceLock;

use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::glib;
use relm4::gtk::glib::object::{Cast, IsA};
use relm4::gtk::pango::EllipsizeMode;
use relm4::gtk::prelude::{AdjustmentExt, ListBoxRowExt, WidgetExt};
use relm4::{Component, ComponentParts, ComponentSender};

use crate::pdf::TocEntry;

/// Background matching adwaita's row hover, derived from the theme
/// foreground so it adapts to light/dark automatically.
const HIGHLIGHT_STYLE: &str =
    "row.toc-active { background-color: color-mix(in srgb, currentColor 10%, transparent); }";

static STYLE_LOADED: OnceLock<()> = OnceLock::new();

fn ensure_style_loaded() {
    if STYLE_LOADED.set(()).is_ok() {
        relm4::set_global_css(HIGHLIGHT_STYLE);
    }
}

pub struct TocSidebar {
    entries: Vec<TocEntry>,
    collapsed: Vec<bool>,
    /// Visible rows, each paired with the index of its entry in `entries`.
    /// Collapsing an entry hides its subtree, so visible position != entry
    /// index; the app's Highlight/GoTo messages always use entry indices.
    rows: Vec<(gtk::ListBoxRow, usize)>,
}

/// Keyboard bindings for sidebar navigation, built from `[keymap]` in the
/// config. `None` disables a binding.
#[derive(Debug, Clone, Copy, Default)]
pub struct SidebarBindings {
    pub down: Option<(gdk::ModifierType, gdk::Key)>,
    pub up: Option<(gdk::ModifierType, gdk::Key)>,
    pub activate: Option<(gdk::ModifierType, gdk::Key)>,
    pub collapse: Option<(gdk::ModifierType, gdk::Key)>,
}

#[derive(Debug)]
pub enum TocInput {
    Set(Vec<TocEntry>),
    /// Highlight the row for the section currently on screen, styled like
    /// mouse hover. `None` clears the highlight.
    Highlight(Option<usize>),
    /// Grab keyboard focus and select the entry (None: first visible row).
    Focus(Option<usize>),
    /// Move the keyboard cursor (ListBox selection) by a signed delta.
    MoveCursor(isize),
    /// Open a leaf entry, or expand/collapse an entry with children.
    Activate,
    /// Collapse the entry with children under the cursor.
    Collapse,
    /// Open the entry at a visible row position (from a mouse click).
    OpenAt(usize),
}

#[derive(Debug)]
pub enum TocOutput {
    GoTo(usize),
}

#[relm4::component(pub)]
impl Component for TocSidebar {
    type Init = SidebarBindings;
    type Input = TocInput;
    type Output = TocOutput;
    type CommandOutput = ();

    view! {
        gtk::ScrolledWindow {
            set_hscrollbar_policy: gtk::PolicyType::Never,
            set_vexpand: true,
            #[wrap(Some)]
            set_child = toc_list = &gtk::ListBox {
                set_css_classes: &["navigation-sidebar"],
                set_selection_mode: gtk::SelectionMode::Single,
                connect_row_activated[sender] => move |_list, row| {
                    // The row's visible position must be mapped back to its
                    // entry index (collapsed subtrees shift positions), which
                    // needs the model, so route it through an input message.
                    let position = row.index().max(0) as usize;
                    sender.input(TocInput::OpenAt(position));
                },
            },
        }
    }

    fn init(
        bindings: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        ensure_style_loaded();
        let model = TocSidebar {
            entries: Vec::new(),
            collapsed: Vec::new(),
            rows: Vec::new(),
        };

        // Keyboard navigation, active only while focus is inside the sidebar:
        // GtkEventControllerKey fires for the focused widget's ancestors, so
        // this controller sees keys only when the sidebar has focus, and
        // returning Stop keeps the window-level PDF scrolling from firing.
        let key_controller = gtk::EventControllerKey::new();
        {
            let input = sender.clone();
            let b = bindings;
            key_controller.connect_key_pressed(move |_, keyval, _, state| {
                if let Some((modifiers, key)) = b.down
                    && state == modifiers && keyval == key {
                        input.input(TocInput::MoveCursor(1));
                        return glib::Propagation::Stop;
                    }
                if let Some((modifiers, key)) = b.up
                    && state == modifiers && keyval == key {
                        input.input(TocInput::MoveCursor(-1));
                        return glib::Propagation::Stop;
                    }
                if let Some((modifiers, key)) = b.activate
                    && state == modifiers && keyval == key {
                        input.input(TocInput::Activate);
                        return glib::Propagation::Stop;
                    }
                if let Some((modifiers, key)) = b.collapse
                    && state == modifiers && keyval == key {
                        input.input(TocInput::Collapse);
                        return glib::Propagation::Stop;
                    }
                glib::Propagation::Proceed
            });
        }
        root.add_controller(key_controller);

        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        msg: Self::Input,
        sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        match msg {
            TocInput::Set(entries) => {
                self.entries = entries;
                self.collapsed = vec![false; self.entries.len()];
                rebuild(&widgets.toc_list, &self.entries, &self.collapsed, &mut self.rows);
            }
            TocInput::Highlight(index) => {
                // A row clicked for navigation stays selected with an accent
                // background, which would linger as a second, stale highlight
                // next to the position tracking. Clear it whenever the
                // position highlight moves so only `toc-active` shows.
                widgets.toc_list.unselect_all();
                for (row, entry_index) in &self.rows {
                    let active = Some(*entry_index) == index;
                    if active {
                        row.add_css_class("toc-active");
                    } else {
                        row.remove_css_class("toc-active");
                    }
                }
            }
            TocInput::Focus(target) => {
                if self.rows.is_empty() {
                    return;
                }
                let position = target
                    .and_then(|entry| {
                        self.rows.iter().position(|(_, entry_index)| *entry_index == entry)
                    })
                    .unwrap_or(0);
                self.select_row(&widgets.toc_list, position);
            }
            TocInput::MoveCursor(delta) => {
                if self.rows.is_empty() {
                    return;
                }
                let current = widgets
                    .toc_list
                    .selected_row()
                    .map(|row| row.index().max(0) as usize)
                    .unwrap_or(0);
                let target = (current as isize + delta)
                    .clamp(0, self.rows.len() as isize - 1) as usize;
                self.select_row(&widgets.toc_list, target);
            }
            TocInput::Activate => {
                if let Some(entry_index) = self.current_entry(&widgets.toc_list) {
                    let has_kids = has_children(&self.entries, entry_index);
                    if has_kids && self.collapsed[entry_index] {
                        // "l" expands a collapsed entry with children; it
                        // never collapses (use "h" for that).
                        self.collapsed[entry_index] = false;
                        let entries = self.entries.clone();
                        let collapsed = self.collapsed.clone();
                        self.rebuild_preserve(
                            &widgets.toc_list, &entries, &collapsed);
                        if let Some(position) = self
                            .rows
                            .iter()
                            .position(|(_, idx)| *idx == entry_index)
                        {
                            self.select_row(&widgets.toc_list, position);
                        }
                    } else {
                        // A leaf or an already-expanded entry: open it.
                        let _ = sender.output(TocOutput::GoTo(entry_index));
                    }
                }
            }
            TocInput::Collapse => {
                if let Some(entry_index) = self.current_entry(&widgets.toc_list)
                    && has_children(&self.entries, entry_index) && !self.collapsed[entry_index] {
                        self.collapsed[entry_index] = true;
                        let entries = self.entries.clone();
                        let collapsed = self.collapsed.clone();
                        self.rebuild_preserve(
                            &widgets.toc_list, &entries, &collapsed);
                        if let Some(position) = self
                            .rows
                            .iter()
                            .position(|(_, idx)| *idx == entry_index)
                        {
                            self.select_row(&widgets.toc_list, position);
                        }
                    }
            }
            TocInput::OpenAt(position) => {
                if let Some((_, entry_index)) = self.rows.get(position) {
                    let _ = sender.output(TocOutput::GoTo(*entry_index));
                }
            }
        }
        self.update_view(widgets, sender);
    }
}

impl TocSidebar {
    /// Entry index under the keyboard cursor (visible selection).
    fn current_entry(&self, list: &gtk::ListBox) -> Option<usize> {
        let position = list
            .selected_row()
            .map(|row| row.index().max(0) as usize)?;
        self.rows.get(position).map(|(_, entry_index)| *entry_index)
    }

    /// Rebuild visible rows while preserving the scrolled window's
    /// scroll position, so expanding/collapsing an entry does not
    /// snap the view back to the top of the TOC.
    fn rebuild_preserve(
        &mut self,
        toc_list: &gtk::ListBox,
        entries: &[TocEntry],
        collapsed: &[bool],
    ) {
        let scroller = find_scrolled_window(toc_list);
        let saved = scroller.as_ref().map(|s| s.vadjustment().value());
        rebuild(toc_list, entries, collapsed, &mut self.rows);
        // Restoring in an idle lets the layout pass triggered by removing and
        // re-adding the rows settle first; a synchronous set_value right here
        // gets clobbered by that re-layout and the view snaps to the top.
        if let Some((v, s)) = saved.zip(scroller) {
            glib::idle_add_local(move || {
                s.vadjustment().set_value(v);
                glib::ControlFlow::Break
            });
        }
    }

    /// Select `position`, focus its row, and scroll it into view.
    fn select_row(&self, list: &gtk::ListBox, position: usize) {
        let (row, _) = &self.rows[position];
        list.select_row(Some(row));
        row.grab_focus();
        if let Some(scroller) = find_scrolled_window(list)
        {
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
    }
}

/// The `ScrolledWindow` containing `widget`, walking up the parent chain.
/// The `ListBox` lives inside the scroller's internal `GtkViewport`, so it is
/// not the direct parent and a single `parent()` lookup misses it.
fn find_scrolled_window(widget: &impl IsA<gtk::Widget>) -> Option<gtk::ScrolledWindow> {
    let mut current = widget.clone().upcast::<gtk::Widget>().parent();
    while let Some(w) = current {
        if let Ok(sw) = w.clone().downcast::<gtk::ScrolledWindow>() {
            return Some(sw);
        }
        current = w.parent();
    }
    None
}

/// Whether the entry has any children in the outline walk.
fn has_children(entries: &[TocEntry], index: usize) -> bool {
    let depth = entries[index].depth;
    for entry in &entries[index + 1..] {
        if entry.depth <= depth {
            return false;
        }
        if entry.depth > depth {
            return true;
        }
    }
    false
}

/// Entry indices that should be shown given the collapse state: a hidden
/// entry hides its whole subtree; a collapsed entry hides its subtree but
/// stays visible itself.
fn visible_indices(entries: &[TocEntry], collapsed: &[bool]) -> Vec<usize> {
    // Stack of entry indices for the visible ancestor path.
    let mut path: Vec<usize> = Vec::new();
    let mut visible = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        while path.last().is_some_and(|&ancestor| entries[ancestor].depth >= entry.depth) {
            path.pop();
        }
        let shown = path.last().is_none_or(|&ancestor| !collapsed[ancestor]);
        if shown {
            visible.push(index);
            path.push(index);
        }
    }
    visible
}

/// Rebuild the visible rows, hiding the subtree of every collapsed entry.
fn rebuild(
    list_box: &gtk::ListBox,
    entries: &[TocEntry],
    collapsed: &[bool],
    rows: &mut Vec<(gtk::ListBoxRow, usize)>,
) {
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }
    rows.clear();

    for entry_index in visible_indices(entries, collapsed) {
        let entry = &entries[entry_index];

        let label = gtk::Label::builder()
            .label(&entry.title)
            .ellipsize(EllipsizeMode::End)
            .xalign(0.0)
            .margin_top(2)
            .margin_bottom(2)
            .build();
        if entry.depth == 0 {
            label.add_css_class("heading");
        }
        let row = gtk::ListBoxRow::builder()
            .child(&label)
            .tooltip_text(format!("Page {}", entry.page + 1))
            .build();
        row.set_margin_start(entry.depth as i32 * 12 + 6);
        list_box.append(&row);
        rows.push((row, entry_index));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::TocEntry;

    fn entry(depth: usize) -> TocEntry {
        TocEntry {
            title: "x".into(),
            page: 0,
            depth,
        }
    }

    #[test]
    fn children_detection_stops_at_siblings() {
        // A(d0) with no children, followed by a sibling C(d0) that has a
        // child D(d1). A must NOT count C's subtree as its own children.
        let entries = [entry(0), entry(0), entry(1)];
        assert!(!has_children(&entries, 0));
        assert!(has_children(&entries, 1));
        assert!(!has_children(&entries, 2));
    }

    #[test]
    fn children_detection_nested() {
        // A(d0) with child B(d1), then sibling C(d0) with child D(d1).
        let entries = [entry(0), entry(1), entry(0), entry(1)];
        assert!(has_children(&entries, 0));
        assert!(!has_children(&entries, 1));
        assert!(has_children(&entries, 2));
        assert!(!has_children(&entries, 3));
    }

    #[test]
    fn collapse_hides_subtree() {
        // A(0), B(1) child of A, C(0) sibling, D(1) child of C.
        let entries = [entry(0), entry(1), entry(0), entry(1)];

        // Nothing collapsed: all four visible.
        assert_eq!(visible_indices(&entries, &[false; 4]), vec![0, 1, 2, 3]);

        // Collapse C (index 2): its child D (index 3) disappears, C stays.
        let mut collapsed = [false; 4];
        collapsed[2] = true;
        assert_eq!(visible_indices(&entries, &collapsed), vec![0, 1, 2]);

        // Collapse A (index 0): its child B (index 1) disappears; the
        // sibling C (index 2) remains with its child D (index 3).
        let mut collapsed = [false; 4];
        collapsed[0] = true;
        assert_eq!(visible_indices(&entries, &collapsed), vec![0, 2, 3]);

        // Both collapsed: only A and C show.
        let mut collapsed = [false; 4];
        collapsed[0] = true;
        collapsed[2] = true;
        assert_eq!(visible_indices(&entries, &collapsed), vec![0, 2]);
    }
}
