use adw::prelude::*;
use gtk::glib;
use relm4::RelmRemoveAllExt;
use relm4::{adw, gtk, Component, ComponentParts, ComponentSender};

// ---- list ---------------------------------------------------------------

/// One row of the notes browser.
#[derive(Debug, Clone)]
pub struct NotesListItem {
    pub id: u64,
    pub title: String,
    pub subtitle: String,
}

pub struct NotesListInit {
    pub parent: gtk::Window,
}

impl NotesListInit {}

#[derive(Debug)]
pub enum NotesListInput {
    /// Replace the displayed rows.
    Update(Vec<NotesListItem>),
    /// Rebuild rows and present the dialog.
    Show,
    /// A row was activated: jump to its anchor and close.
    Activate(usize),
    /// The selected row should be removed.
    Delete(usize),
}

#[derive(Debug)]
pub enum NotesListOutput {
    Jumped(u64),
    Deleted(u64),
    Closed,
}

/// Picker-style browser for the current document's notes:
/// Enter jumps to the anchor, Delete removes the note.
pub struct NotesList {
    items: Vec<NotesListItem>,
    /// Result rows; lives on the model so `update_with_view` and signal
    /// closures share it (same pattern as `PickerDialog::results_list`).
    rows: gtk::ListBox,
    parent: gtk::Window,
}

#[relm4::component(pub)]
impl Component for NotesList {
    type Init = NotesListInit;
    type Input = NotesListInput;
    type Output = NotesListOutput;
    type CommandOutput = ();

    view! {
        #[root]
        adw::Dialog {
            set_title: "Notes",
            set_content_width: 560,
            set_content_height: 480,
            connect_closed[sender] => move |_dialog| {
                let _ = sender.output(NotesListOutput::Closed);
            },
            #[wrap(Some)]
            set_child = list_stack = &gtk::Stack {
                set_vexpand: true,
            },
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Result rows live outside the macro (same as the picker's list):
        // ActionRows built per item, hosted in a boxed-list card.
        let rows = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .valign(gtk::Align::Start)
            .css_classes(["boxed-list"])
            .build();
        {
            let sender = sender.clone();
            rows.connect_row_activated(move |_list, row| {
                sender.input(NotesListInput::Activate(row.index().max(0) as usize));
            });
        }

        let model = NotesList {
            items: Vec::new(),
            rows: rows.clone(),
            parent: init.parent.clone(),
        };

        let widgets = view_output!();

        let empty_page = adw::StatusPage::builder()
            .icon_name("document-edit-symbolic")
            .title("No notes")
            .description("Press m while reading to add one.")
            .build();
        widgets.list_stack.add_named(&rows, Some("list"));
        widgets.list_stack.add_named(&empty_page, Some("empty"));
        widgets.list_stack.set_visible_child_name("empty");

        // Ctrl+n/p move the selection; Enter jumps; Delete removes; Esc closes.
        let keys = gtk::EventControllerKey::new();
        {
            let sender = sender.clone();
            let rows = rows.clone();
            let root_for_keys = root.clone();
            keys.connect_key_pressed(move |_, keyval, _, state| {
                eprintln!("[notes-key] key={keyval:?} state={state:?}");
                if state.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
                    let delta: i32 = match keyval {
                        gtk::gdk::Key::n => 1,
                        gtk::gdk::Key::p => -1,
                        _ => return glib::Propagation::Proceed,
                    };
                    eprintln!("[notes-key] move delta={delta}");
                    move_note_selection(&rows, delta);
                    return glib::Propagation::Stop;
                }
                if keyval == gtk::gdk::Key::Escape && state.is_empty() {
                    root_for_keys.close();
                    return glib::Propagation::Stop;
                }
                // Enter activates the *selected* row — selection is kept in
                // sync programmatically, so this works even before any row
                // held real keyboard focus.
                if matches!(keyval, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter)
                    && state.is_empty()
                    && let Some(row) = rows.selected_row()
                {
                    eprintln!("[notes-key] -> Activate");
                    sender.input(NotesListInput::Activate(row.index().max(0) as usize));
                    return glib::Propagation::Stop;
                }
                if keyval == gtk::gdk::Key::Delete
                    && let Some(row) = rows.selected_row()
                {
                    eprintln!("[notes-key] -> Delete");
                    sender.input(NotesListInput::Delete(row.index().max(0) as usize));
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
        }
        // Capture phase: run before any focused child (rows, inner lists)
        // can consume or reroute these keys, independent of what currently
        // holds keyboard focus inside the sheet.
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        root.add_controller(keys);


        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        msg: Self::Input,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        eprintln!("[notes-list] msg={msg:?}");
        match msg {
            NotesListInput::Update(items) => {
                self.items = items;
                self.rebuild_rows(widgets);
                if self.rows.is_mapped() {
                    self.focus_rows_soon(widgets);
                }
            }
            NotesListInput::Show => {
                self.rebuild_rows(widgets);
                root.present(Some(&self.parent));
                self.focus_rows_soon(widgets);
            }
            NotesListInput::Activate(index) => {
                if let Some(item) = self.items.get(index) {
                    let id = item.id;
                    eprintln!("[notes-list] emit Jumped {id}");
                    let _ = sender.output(NotesListOutput::Jumped(id));
                    root.close();
                }
            }
            NotesListInput::Delete(index) => {
                if let Some(item) = self.items.get(index) {
                    let id = item.id;
                    eprintln!("[notes-list] emit Deleted {id}");
                    let _ = sender.output(NotesListOutput::Deleted(id));
                }
            }
        }
    }
}

impl NotesList {
    fn rebuild_rows(&self, widgets: &NotesListWidgets) {
        let rows = &self.rows;
        rows.remove_all();
        for item in &self.items {
            let row = adw::ActionRow::new();
            // File names and note text may contain raw '&'.
            row.set_use_markup(false);
            row.set_title(&item.title);
            if !item.subtitle.is_empty() {
                row.set_subtitle(&item.subtitle);
            }
            rows.append(&row);
        }
        widgets
            .list_stack
            .set_visible_child_name(if self.items.is_empty() { "empty" } else { "list" });
        if let Some(first) = rows.row_at_index(0) {
            rows.select_row(Some(&first));
        }
    }

    /// Give keyboard focus to the selected (or first) row, but only once
    /// the dialog is actually on screen. Grabbing earlier would fail and
    /// swallow GTK's automatic initial focus, leaving every key dead.
    fn focus_rows_soon(&self, widgets: &NotesListWidgets) {
        let rows = self.rows.clone();
        glib::idle_add_local_once(move || {
            if !rows.is_mapped() {
                return;
            }
            let target = rows.selected_row().or_else(|| rows.row_at_index(0));
            if let Some(row) = target {
                let _ = row.grab_focus();
            }
        });
        let _ = widgets;
    }
}

/// Move the list selection by `delta` rows (Ctrl+n / Ctrl+p). Focusing the
/// new row also scrolls it into view.
fn move_note_selection(list: &gtk::ListBox, delta: i32) {
    let mut count: i32 = 0;
    while list.row_at_index(count).is_some() {
        count += 1;
    }
    if count == 0 {
        return;
    }
    let current = list.selected_row().map(|row| row.index()).unwrap_or(-1);
    let target = if current < 0 {
        if delta > 0 { 0 } else { count - 1 }
    } else {
        (current + delta).clamp(0, count - 1)
    };
    if let Some(row) = list.row_at_index(target) {
        list.select_row(Some(&row));
        row.grab_focus();
    }
}
