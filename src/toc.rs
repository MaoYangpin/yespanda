use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;

use relm4::gtk;
use relm4::gtk::pango::EllipsizeMode;
use relm4::gtk::prelude::{ListBoxRowExt, WidgetExt};
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
    entries: Rc<RefCell<Vec<TocEntry>>>,
    rows: Vec<gtk::ListBoxRow>,
}

#[derive(Debug)]
pub enum TocInput {
    Set(Vec<TocEntry>),
    /// Highlight the row for the section currently on screen, styled like
    /// mouse hover. `None` clears the highlight.
    Highlight(Option<usize>),
}

#[derive(Debug)]
pub enum TocOutput {
    GoTo(usize),
}

#[relm4::component(pub)]
impl Component for TocSidebar {
    type Init = ();
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
                connect_row_activated[sender, entries] => move |_list, row| {
                    // A row's index is a TOC entry index; the page it jumps
                    // to is the entry's stored page number.
                    let index = row.index().max(0) as usize;
                    let page = entries.borrow().get(index).map(|e| e.page).unwrap_or(index);
                    let _ = sender.output(TocOutput::GoTo(page));
                },
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        ensure_style_loaded();
        let entries = Rc::new(RefCell::new(Vec::new()));
        let model = TocSidebar {
            entries: entries.clone(),
            rows: Vec::new(),
        };
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
                self.rows.clear();
                rebuild(&widgets.toc_list, &entries, &mut self.rows);
                *self.entries.borrow_mut() = entries;
            }
            TocInput::Highlight(index) => {
                // A row clicked for navigation stays selected with an accent
                // background, which would linger as a second, stale highlight
                // next to the position tracking. Clear it whenever the
                // position highlight moves so only `toc-active` shows.
                widgets.toc_list.unselect_all();
                highlight_row(&self.rows, index);
            }
        }
        self.update_view(widgets, sender);
    }
}

fn rebuild(list_box: &gtk::ListBox, entries: &[TocEntry], rows: &mut Vec<gtk::ListBoxRow>) {
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }
    rows.clear();

    for entry in entries {
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
        rows.push(row);
    }
}

/// Render `index` like the pointer is hovering it; every other row plain.
/// A CSS class is used instead of the PRELIGHT state flag because GTK
/// clears that state itself whenever the pointer leaves a row, which
/// would drop the highlight after hovering it once.
fn highlight_row(rows: &[gtk::ListBoxRow], index: Option<usize>) {
    for (position, row) in rows.iter().enumerate() {
        let active = Some(position) == index;
        let highlighted = row.has_css_class("toc-active");
        if active == highlighted {
            continue;
        }
        if active {
            row.add_css_class("toc-active");
        } else {
            row.remove_css_class("toc-active");
        }
    }
}
