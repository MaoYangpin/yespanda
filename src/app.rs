use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use relm4::gtk::glib;
use relm4::gtk::glib::clone;
use relm4::gtk::gdk;
use relm4::{
    Component, ComponentController, ComponentParts, ComponentSender, Controller,
    RelmRemoveAllExt, adw, gtk,
};

use crate::config::{Config, History, parse_binding};
use crate::pdf::{PdfDoc, TocEntry};
use crate::picker::{PickerDialog, PickerInit, PickerOutput, expand_root};
use crate::toc::{SidebarBindings, TocInput, TocOutput, TocSidebar};

const PAGE_SPACING: i32 = 12;
/// Portion of the viewport scrolled by one `j`/`k` press.
const SCROLL_STEP_FRACTION: f64 = 0.25;
/// Window in which a chord's completing key must follow its leader.
const CHORD_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);

/// Advance the two-key chord state machine on a key press. Returns the
/// action to fire (and clears the leader) if the press completed a chord.
/// Completion is checked before arming because a key can be both leader and
/// completing key (`space space`).
fn chord_press(
    chords: &[(gdk::Key, gdk::Key, AppMsg)],
    leader: &mut Option<(gdk::Key, std::time::Instant)>,
    keyval: gdk::Key,
    now: std::time::Instant,
) -> Option<AppMsg> {
    if let Some((armed, pressed)) = *leader {
        if now.duration_since(pressed) <= CHORD_WINDOW {
            if let Some((_, _, msg)) = chords
                .iter()
                .find(|(leader_key, chord_key, _)| *leader_key == armed && *chord_key == keyval)
            {
                *leader = None;
                return Some(msg.clone());
            }
        }
    }
    if chords.iter().any(|(leader_key, _, _)| *leader_key == keyval) {
        *leader = Some((keyval, now));
    } else {
        *leader = None;
    }
    None
}

pub struct AppModel {
    doc: Option<PdfDoc>,
    path: Option<PathBuf>,
    zoom: f64,
    fit_mode: bool,
    last_viewport_width: i32,
    page_sizes_pt: Vec<(f64, f64)>,
    textures: HashMap<usize, gdk::MemoryTexture>,
    pictures: Vec<gtk::Picture>,
    pages_box: gtk::Box,
    pages_scroller: gtk::ScrolledWindow,
    status_page: adw::StatusPage,
    toc_entries: Vec<TocEntry>,
    highlighted_toc: Option<usize>,
    /// Entry highlighted by a sidebar click; kept while the viewport stays
    /// on that entry's page, otherwise replaced by scroll tracking.
    pinned_toc: Option<usize>,
    config: Rc<RefCell<Config>>,
    history: Rc<RefCell<History>>,
    /// Page to scroll to once the fit zoom settles, after opening a document
    /// that has a saved position.
    pending_restore: Option<usize>,
    /// True while a debounced history write is scheduled.
    history_save_pending: Rc<Cell<bool>>,
    window: gtk::Window,
    toc: Controller<TocSidebar>,
    picker: Option<Controller<PickerDialog>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppMsg {
    OpenFile(String),
    ZoomIn,
    ZoomOut,
    GoToEntry(usize),
    ViewportChanged,
    ScrollDown,
    ScrollUp,
    ScrollLeft,
    ScrollRight,
    ToggleSidebar,
    /// Move keyboard focus to the TOC sidebar (Ctrl+h). Never scrolls the
    /// PDF or changes the position highlight.
    FocusSidebar,
    /// Move keyboard focus to the PDF content (Ctrl+l).
    FocusPdf,
    /// Open the fd+fzf PDF picker dialog.
    PickFile,
    /// Result of the picker: `Some(path)` to open, `None` if cancelled.
    PickerResult(Option<PathBuf>),
}

impl AppModel {
    fn load_document(&mut self, widgets: &mut AppModelWidgets, path: PathBuf, doc: PdfDoc) {
        // Remember where we were in the document we are leaving, then persist.
        if let (Some(prev), Some(page)) = (self.path.as_ref(), self.current_page()) {
            self.history.borrow_mut().set(prev, page);
        }
        if let Err(error) = self.history.borrow().save() {
            eprintln!("failed to save history: {error:#}");
        }

        let entries: Vec<TocEntry> = doc.toc();
        let n_pages = doc.n_pages();
        let sizes_pt: Vec<(f64, f64)> = (0..n_pages).map(|i| doc.page_size(i)).collect();

        self.doc = Some(doc);
        self.path = Some(path.clone());
        self.zoom = 1.0;
        self.last_viewport_width = 0;
        self.page_sizes_pt = sizes_pt;
        self.toc_entries = entries.clone();
        self.highlighted_toc = None;
        self.pinned_toc = None;
        self.pending_restore = self.history.borrow().page_for(&path);
        self.rebuild_pages();

        // Reset the scroll position to the top. Otherwise the stale scroll
        // value from the previous document can lie beyond the new content,
        // making render_visible compute an empty range so nothing is drawn
        // until the viewport is scrolled.
        self.pages_scroller.vadjustment().set_value(0.0);

        if let Some(path) = path.to_str() {
            self.config.borrow_mut().last_file = Some(path.to_owned());
            self.persist_config();
        }

        let name = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Document".into());
        widgets.title_widget.set_title(&name);

        self.toc.sender().send(TocInput::Set(entries)).ok();
        widgets.content_stack.set_visible_child_name("doc");
    }

    fn show_error(&mut self, widgets: &mut AppModelWidgets, error: anyhow::Error) {
        widgets.content_stack.set_visible_child_name("empty");
        widgets.title_widget.set_title("Error");
        self.status_page.set_title("Could not open document");
        self.status_page.set_description(Some(&format!("{error:#}")));
    }

    fn persist_config(&self) {
        if let Err(error) = self.config.borrow().save() {
            eprintln!("failed to save config: {error:#}");
        }
    }

    /// Leave fit-to-width and remember the choice for future sessions.
    fn set_manual_zoom(&mut self) {
        self.fit_mode = false;
        self.config.borrow_mut().fit_width = false;
        self.persist_config();
    }

    fn rebuild_pages(&mut self) {
        self.pages_box.remove_all();
        self.pictures.clear();
        self.textures.clear();

        for (width_pt, height_pt) in &self.page_sizes_pt {
            let picture = gtk::Picture::new();
            picture.set_size_request(
                (width_pt * self.zoom).round() as i32,
                (height_pt * self.zoom).round() as i32,
            );
            self.pages_box.append(&picture);
            self.pictures.push(picture);
        }
    }

    fn resize_pages(&mut self) {
        for (index, picture) in self.pictures.iter().enumerate() {
            let (width_pt, height_pt) = self.page_sizes_pt[index];
            picture.set_size_request(
                (width_pt * self.zoom).round() as i32,
                (height_pt * self.zoom).round() as i32,
            );
        }
    }

    /// Top-edge pixel offset of every page at the current zoom.
    fn offsets(&self) -> Vec<f64> {
        let mut offsets = Vec::with_capacity(self.pictures.len());
        let mut cursor = 0.0;
        for (_, height_pt) in &self.page_sizes_pt {
            offsets.push(cursor);
            cursor += height_pt * self.zoom + PAGE_SPACING as f64;
        }
        offsets
    }

    /// Fit the document to the viewport width once GTK has allocated it.
    /// Recompute the zoom so the widest page spans the content width.
    /// Active while fit mode is on (the default); manual zoom leaves it.
    fn apply_fit_width(&mut self) {
        if !self.fit_mode || self.doc.is_none() {
            return;
        }
        let width = self.pages_scroller.width();
        // Ignore sub-pixel churn and wait for a real allocation.
        if width < 50 || (width - self.last_viewport_width).abs() < 8 {
            return;
        }
        let viewport_width = width as f64 - 16.0;
        let max_width_pt = self
            .page_sizes_pt
            .iter()
            .map(|(width, _)| *width)
            .fold(1.0_f64, f64::max);
        self.zoom = (viewport_width / max_width_pt).clamp(0.25, 4.0);
        self.last_viewport_width = width;
        self.resize_pages();
        self.textures.clear();
    }

    /// Render every page overlapping the viewport, plus one above and below.
    fn render_visible(&mut self) {
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        let adjustment = self.pages_scroller.vadjustment();
        let top = adjustment.value();
        let bottom = top + adjustment.page_size();
        let offsets = self.offsets();

        let first = offsets.partition_point(|start| *start < top - PAGE_SPACING as f64);
        let last = offsets
            .partition_point(|start| *start < bottom)
            .min(self.pictures.len());

        for index in first..last {
            if self.textures.contains_key(&index) {
                continue;
            }
            match doc.render_page(index, self.zoom) {
                Ok(texture) => {
                    self.pictures[index].set_paintable(Some(&texture));
                    self.textures.insert(index, texture);
                }
                Err(error) => eprintln!("failed to render page {}: {error:#}", index + 1),
            }
        }

        self.textures
            .retain(|index, _| index + 4 >= first && *index <= last + 4);
    }

    /// Record the current page for the open document in the in-memory
    /// history and schedule a debounced write, so the position survives even
    /// if the window is never closed gracefully.
    fn update_history_position(&mut self) {
        if self.pending_restore.is_some() {
            return;
        }
        if let (Some(path), Some(page)) = (self.path.as_ref(), self.current_page()) {
            self.history.borrow_mut().set(path, page);
        }
        if self.history_save_pending.get() {
            return;
        }
        self.history_save_pending.set(true);
        let history = self.history.clone();
        let pending = self.history_save_pending.clone();
        glib::timeout_add_local_once(std::time::Duration::from_secs(2), move || {
            pending.set(false);
            if let Err(error) = history.borrow().save() {
                eprintln!("failed to save history: {error:#}");
            }
        });
    }

    /// Scroll to the page saved in the history, once the fit zoom has
    /// settled so the offsets are computed at the correct scale.
    fn restore_if_pending(&mut self) {
        let Some(page) = self.pending_restore else {
            return;
        };
        if self.fit_mode && self.last_viewport_width == 0 {
            return;
        }
        self.pending_restore = None;
        if let Some(path) = self.path.clone() {
            self.history.borrow_mut().set(&path, page);
        }
        self.scroll_to_page(page);
    }

    /// Index of the page under the viewport's vertical centre.
    fn current_page(&self) -> Option<usize> {        if self.pictures.is_empty() {
            return None;
        }
        let adjustment = self.pages_scroller.vadjustment();
        let centre = adjustment.value() + adjustment.page_size() / 2.0;
        let offsets = self.offsets();
        let page = offsets.partition_point(|start| *start <= centre).max(1) - 1;
        Some(page.min(self.pictures.len() - 1))
    }

    /// Push the hover-highlight to the sidebar entry covering the visible
    /// section; only messages the sidebar when the entry actually changed.
    /// A clicked entry stays pinned while the viewport is on its page, so
    /// entries sharing a page (e.g. a section and a later appendix) don't
    /// steal the highlight from the one the user chose.
    fn update_toc_highlight(&mut self) {
        if let Some(pinned) = self.pinned_toc {
            let pinned_page = self.toc_entries.get(pinned).map(|entry| entry.page);
            if self.current_page() != pinned_page {
                self.pinned_toc = None;
            }
        }
        let tracked = self
            .current_page()
            .and_then(|page| self.active_toc_entry(page));
        let target = self.pinned_toc.or(tracked);
        if target == self.highlighted_toc {
            return;
        }
        self.highlighted_toc = target;
        self.toc.sender().send(TocInput::Highlight(target)).ok();
    }

    /// Sidebar entry covering `page`: the entry whose page is the greatest
    /// one at or before it. Outline order is not page-sorted (a later chapter
    /// can start on an earlier page), so a linear scan is required instead of
    /// a binary search; on ties the last occurrence wins, which is the most
    /// specific section.
    fn active_toc_entry(&self, page: usize) -> Option<usize> {
        self.toc_entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.page <= page)
            .max_by_key(|(_, entry)| entry.page)
            .map(|(index, _)| index)
    }

    fn scroll_to_page(&mut self, page: usize) {
        if page >= self.pictures.len() {
            return;
        }
        let offsets = self.offsets();
        let adjustment = self.pages_scroller.vadjustment();
        // Never clamp below the lower bound; a short document (content
        // shorter than the viewport) would otherwise scroll back to the top.
        let max = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
        let target = offsets[page].clamp(adjustment.lower(), max);
        adjustment.set_value(target);
    }

    /// Scroll the document vertically by `dy` (positive = down).
    fn scroll_vertical(&self, dy: f64) {
        let adjustment = self.pages_scroller.vadjustment();
        let max = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
        let value = (adjustment.value() + dy).clamp(adjustment.lower(), max);
        adjustment.set_value(value);
    }

    /// Scroll the document horizontally by `dx` (positive = right).
    /// A no-op while fit-width keeps the content exactly viewport-sized.
    fn scroll_horizontal(&self, dx: f64) {
        let adjustment = self.pages_scroller.hadjustment();
        let max = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
        let value = (adjustment.value() + dx).clamp(adjustment.lower(), max);
        adjustment.set_value(value);
    }
}

#[relm4::component(pub)]
impl Component for AppModel {
    type Init = Config;
    type Input = AppMsg;
    type Output = ();
    type CommandOutput = ();

    view! {
        #[root]
        adw::ApplicationWindow {
            set_default_width: 1100,
            set_default_height: 760,

            #[wrap(Some)]
            set_content = split_view = &adw::NavigationSplitView {
                set_min_sidebar_width: 180.0,
                set_max_sidebar_width: 320.0,
                set_sidebar_width_fraction: 0.24,

                #[wrap(Some)]
                set_sidebar = &adw::NavigationPage {
                    set_title: "Contents",
                    #[wrap(Some)]
                    set_child = &adw::ToolbarView {
                        add_top_bar = &adw::HeaderBar {},
                        #[wrap(Some)]
                        set_content = &toc_widget.clone(),
                    },
                },

                #[wrap(Some)]
                set_content = &adw::NavigationPage {
                    set_title: "Yespanda",
                    #[wrap(Some)]
                    set_child = &adw::ToolbarView {
                        add_top_bar = content_headerbar = &adw::HeaderBar {
                            #[wrap(Some)]
                            set_title_widget = title_widget = &adw::WindowTitle {
                                set_title: "No document",
                            },
                        },
                        #[name = "content_stack"]
                        #[wrap(Some)]
                        set_content = &gtk::Stack {
                            set_transition_type: gtk::StackTransitionType::Crossfade,
                        },
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
        let config = Rc::new(RefCell::new(init));
        let history = Rc::new(RefCell::new(History::load()));

        let keymap = config.borrow().keymap.clone();
        let sidebar_bindings = SidebarBindings {
            down: parse_binding(&keymap.sidebar_down),
            up: parse_binding(&keymap.sidebar_up),
            activate: parse_binding(&keymap.sidebar_activate),
            collapse: parse_binding(&keymap.sidebar_collapse),
        };
        let toc = TocSidebar::builder()
            .launch(sidebar_bindings)
            .forward(sender.input_sender(), |msg| match msg {
                TocOutput::GoTo(index) => AppMsg::GoToEntry(index),
            });
        let toc_widget = toc.widget().clone();

        let pages_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(PAGE_SPACING)
            .build();
        let pages_scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&pages_box)
            .build();

        let status_page = adw::StatusPage::builder()
            .icon_name("application-pdf")
            .title("No document")
            .description("Start yespanda with a PDF file to view it.")
            .build();

        let model = AppModel {
            doc: None,
            path: None,
            zoom: 1.0,
            fit_mode: config.borrow().fit_width,
            last_viewport_width: 0,
            page_sizes_pt: Vec::new(),
            textures: HashMap::new(),
            pictures: Vec::new(),
            pages_box: pages_box.clone(),
            pages_scroller: pages_scroller.clone(),
            status_page: status_page.clone(),
            toc_entries: Vec::new(),
            highlighted_toc: None,
            pinned_toc: None,
            config: config.clone(),
            history: history.clone(),
            pending_restore: None,
            history_save_pending: Rc::new(Cell::new(false)),
            window: root.clone().upcast::<gtk::Window>(),
            toc,
            picker: None,
        };

        let widgets = view_output!();

        // Focus the PDF content once the window maps, so j/k/h/l scroll the
        // document instead of navigating the sidebar right after startup.
        let content = pages_scroller.clone();
        root.connect_map(move |_| {
            content.grab_focus();
        });

        // Restore persisted window geometry.
        root.set_default_size(
            config.borrow().window.width,
            config.borrow().window.height,
        );
        if config.borrow().window.maximized {
            root.maximize();
        }
        widgets
            .split_view
            .set_sidebar_width_fraction(config.borrow().sidebar.width_fraction);
        // show-content is FALSE by default, so collapsing would show the
        // sidebar full-screen. Force content to be the visible page instead,
        // making collapse hide the sidebar and give the content full width.
        widgets.split_view.set_show_content(true);
        widgets
            .split_view
            .set_collapsed(config.borrow().sidebar.collapsed);
        adw::StyleManager::default().set_color_scheme(config.borrow().theme.into());

        // Persist geometry, sidebar state, and the history when the window
        // closes.
        {
            let cfg = config.clone();
            let hist = history.clone();
            let split_view = widgets.split_view.clone();
            root.connect_close_request(move |window| {
                let (width, height) = window.default_size();
                {
                    let mut state = cfg.borrow_mut();
                    state.window.width = width;
                    state.window.height = height;
                    state.window.maximized = window.is_maximized();
                    state.sidebar.width_fraction = split_view.sidebar_width_fraction();
                    state.sidebar.collapsed = split_view.is_collapsed();
                }
                if let Err(error) = cfg.borrow().save() {
                    eprintln!("failed to save config: {error:#}");
                }
                if let Err(error) = hist.borrow().save() {
                    eprintln!("failed to save history: {error:#}");
                }
                glib::Propagation::Proceed
            });
        }

        widgets.content_stack.add_named(&status_page, Some("empty"));
        widgets.content_stack.add_named(&pages_scroller, Some("doc"));
        widgets.content_stack.set_visible_child_name("empty");

        let adjustment = pages_scroller.vadjustment();
        adjustment
            .connect_value_changed(clone!(#[strong] sender, move |_| {
                sender.input(AppMsg::ViewportChanged);
            }));
        // Fires whenever the scroll extent changes (initial allocation,
        // page resizing): drives fit-width and re-rendering.
        adjustment.connect_changed(clone!(#[strong] sender, move |_| {
            sender.input(AppMsg::ViewportChanged);
        }));
        // The horizontal adjustment reports every width allocation, so
        // resizing the window keeps re-fitting the document to the new
        // content width (the vertical one only fires when page heights
        // actually change).
        pages_scroller.hadjustment().connect_changed(
            clone!(#[strong] sender, move |_| {
                sender.input(AppMsg::ViewportChanged);
            }),
        );

        // All bindings live in `[keymap]` and are handled by a key controller:
        // GTK4 does not activate accelerators for bare (unmodified) character
        // keys like "j", so they would never fire as accels.
        let keymap = config.borrow().keymap.clone();
        let bindings: Vec<(AppMsg, gdk::ModifierType, gdk::Key)> = [
            (AppMsg::ScrollDown, &keymap.scroll_down),
            (AppMsg::ScrollUp, &keymap.scroll_up),
            (AppMsg::ScrollLeft, &keymap.scroll_left),
            (AppMsg::ScrollRight, &keymap.scroll_right),
            (AppMsg::FocusSidebar, &keymap.focus_sidebar),
            (AppMsg::FocusPdf, &keymap.focus_pdf),
            (AppMsg::ZoomIn, &keymap.zoom_in),
            (AppMsg::ZoomOut, &keymap.zoom_out),
            (AppMsg::PickFile, &keymap.pick_file),
        ]
        .into_iter()
        .filter_map(|(msg, spec)| parse_binding(spec).map(|(mods, key)| (msg, mods, key)))
        .collect();

        // Two-key chords from `[keymap]` (e.g. `space e`, `space space`). The
        // leader is pressed first, then the completing key within the window.
        let chords: Vec<(gdk::Key, gdk::Key, AppMsg)> = [
            (&keymap.sidebar_toggle, AppMsg::ToggleSidebar),
            (&keymap.pick_file_chord, AppMsg::PickFile),
        ]
        .into_iter()
        .filter_map(|(spec, msg)| {
            let mut tokens = spec.split_whitespace();
            let leader = tokens.next().and_then(|token| gdk::Key::from_name(token))?;
            let key = tokens.next().and_then(|token| gdk::Key::from_name(token))?;
            Some((leader, key, msg))
        })
        .collect();
        let leader: Rc<std::cell::Cell<Option<(gdk::Key, std::time::Instant)>>> = Rc::default();
        let key_controller = gtk::EventControllerKey::new();
        {
            let leader = leader.clone();
            let forward = sender.clone();
            key_controller.connect_key_pressed(move |_, keyval, _, state| {
                let now = std::time::Instant::now();
                let mut leader_state = leader.get();
                if state.is_empty() {
                    if let Some(msg) = chord_press(&chords, &mut leader_state, keyval, now) {
                        forward.input(msg);
                        leader.set(leader_state);
                        return glib::Propagation::Stop;
                    }
                } else {
                    leader_state = None;
                }
                leader.set(leader_state);
                for (msg, modifiers, key) in &bindings {
                    if state == *modifiers && keyval == *key {
                        forward.input(msg.clone());
                        return glib::Propagation::Stop;
                    }
                }
                glib::Propagation::Proceed
            });
        }
        root.add_controller(key_controller);

        // Reopen the PDF viewed in the previous session.
        if let Some(path) = config.borrow().last_file.clone() {
            sender.input(AppMsg::OpenFile(path));
        }

        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut AppModelWidgets,
        msg: Self::Input,
        sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        match msg {
            AppMsg::OpenFile(uri) => {
                let path = glib::filename_from_uri(&uri)
                    .map(|(path, _)| path)
                    .unwrap_or_else(|_| PathBuf::from(&uri));
                // A second launch hands its file to this (primary) instance
                // while the terminal keeps focus; raise and focus our window
                // so the newly opened document is immediately visible.
                self.window.present();
                // A file can arrive twice: once from the command line / gio
                // open, and again from the saved `last_file` on startup.
                // Reloading the same document mid-flight resets the zoom to
                // 1.0 and rebuilds the pages, so page jumps computed in that
                // window land on the wrong page. Skip the duplicate.
                if self.doc.is_some() && self.path.as_deref() == Some(path.as_path()) {
                    return;
                }
                match PdfDoc::open(&path) {
                    Ok(doc) => {
                        self.load_document(widgets, path, doc);
                        let retry = sender.clone();
                        glib::idle_add_local_once(move || retry.input(AppMsg::ViewportChanged));
                    }
                    Err(error) => self.show_error(widgets, error),
                }
            }
            AppMsg::ZoomIn => {
                if self.doc.is_some() {
                    self.set_manual_zoom();
                    self.zoom = (self.zoom * 1.25).min(4.0);
                    self.resize_pages();
                    self.textures.clear();
                }
            }
            AppMsg::ZoomOut => {
                if self.doc.is_some() {
                    self.set_manual_zoom();
                    self.zoom = (self.zoom / 1.25).max(0.25);
                    self.resize_pages();
                    self.textures.clear();
                }
            }
            AppMsg::GoToEntry(index) => {
                if let Some(entry) = self.toc_entries.get(index) {
                    self.pinned_toc = Some(index);
                    self.highlighted_toc = Some(index);
                    self.toc.sender().send(TocInput::Highlight(Some(index))).ok();
                    self.scroll_to_page(entry.page);
                    // Returning focus to the content lets j/k/h/l scroll the
                    // PDF again after opening an entry with `l` or a click.
                    self.pages_scroller.grab_focus();
                }
            }
            AppMsg::ViewportChanged => {
                self.apply_fit_width();
                self.render_visible();
                self.update_toc_highlight();
                self.restore_if_pending();
                self.update_history_position();
            }
            AppMsg::ScrollDown => self
                .scroll_vertical(self.pages_scroller.vadjustment().page_size() * SCROLL_STEP_FRACTION),
            AppMsg::ScrollUp => self.scroll_vertical(
                -self.pages_scroller.vadjustment().page_size() * SCROLL_STEP_FRACTION,
            ),
            AppMsg::ScrollLeft => self.scroll_horizontal(-40.0),
            AppMsg::ScrollRight => self.scroll_horizontal(40.0),
            AppMsg::ToggleSidebar => {
                let collapsed = !widgets.split_view.is_collapsed();
                // Keep the content page visible while collapsed so the sidebar
                // is hidden instead of shown full-screen.
                widgets.split_view.set_show_content(true);
                widgets.split_view.set_collapsed(collapsed);
                self.config.borrow_mut().sidebar.collapsed = collapsed;
                self.persist_config();
            }
            AppMsg::FocusSidebar => {
                // Focus the sidebar on the item for the currently visible
                // page. `highlighted_toc` tracks exactly that: the clicked
                // entry after a click, else the section covering the page.
                // Front-matter before the first TOC entry anchors on the
                // first entry. No PDF scrolling or highlight change happens.
                let entry = self
                    .highlighted_toc
                    .or_else(|| (!self.toc_entries.is_empty()).then_some(0));
                self.toc.sender().send(TocInput::Focus(entry)).ok();
            }
            AppMsg::FocusPdf => {
                self.pages_scroller.grab_focus();
            }
            AppMsg::PickFile => {
                if self.picker.is_some() {
                    return;
                }
                let root = expand_root(&self.config.borrow().picker.root);
                let picker = PickerDialog::builder()
                    .launch(PickerInit {
                        root,
                        width: self.config.borrow().picker.width,
                        height: self.config.borrow().picker.height,
                        parent: self.window.clone(),
                    })
                    .forward(sender.input_sender(), |out| match out {
                        PickerOutput::Pick(path) => AppMsg::PickerResult(Some(path)),
                        PickerOutput::Cancelled => AppMsg::PickerResult(None),
                    });
                self.picker = Some(picker);
            }
            AppMsg::PickerResult(result) => {
                self.picker = None;
                if let Some(path) = result {
                    let uri = path.to_string_lossy().into_owned();
                    sender.input(AppMsg::OpenFile(uri));
                }
            }
        }
        self.update_view(widgets, sender);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn chords() -> Vec<(gdk::Key, gdk::Key, AppMsg)> {
        vec![
            (gdk::Key::space, gdk::Key::e, AppMsg::ToggleSidebar),
            (gdk::Key::space, gdk::Key::space, AppMsg::PickFile),
        ]
    }

    #[test]
    fn space_space_fires_pick_file() {
        let chords = chords();
        let t0 = std::time::Instant::now();
        let mut leader = None;
        // First Space arms the leader; it must NOT fire yet.
        assert_eq!(chord_press(&chords, &mut leader, gdk::Key::space, t0), None);
        assert!(leader.is_some());
        // Second Space within the window completes the chord.
        assert_eq!(
            chord_press(&chords, &mut leader, gdk::Key::space, t0 + Duration::from_millis(100)),
            Some(AppMsg::PickFile)
        );
        assert!(leader.is_none());
    }

    #[test]
    fn space_e_fires_toggle_sidebar() {
        let chords = chords();
        let t0 = std::time::Instant::now();
        let mut leader = None;
        assert_eq!(chord_press(&chords, &mut leader, gdk::Key::space, t0), None);
        assert_eq!(
            chord_press(&chords, &mut leader, gdk::Key::e, t0 + Duration::from_millis(100)),
            Some(AppMsg::ToggleSidebar)
        );
        assert!(leader.is_none());
    }

    #[test]
    fn chord_expires_but_leader_rearms() {
        let chords = chords();
        let t0 = std::time::Instant::now();
        let mut leader = None;
        assert_eq!(chord_press(&chords, &mut leader, gdk::Key::space, t0), None);
        // Too slow: no fire, but Space is still a leader so it re-arms.
        assert_eq!(
            chord_press(&chords, &mut leader, gdk::Key::space, t0 + Duration::from_millis(1000)),
            None
        );
        assert!(leader.is_some());
    }

    #[test]
    fn unrelated_key_clears_leader() {
        let chords = chords();
        let t0 = std::time::Instant::now();
        let mut leader = None;
        assert_eq!(chord_press(&chords, &mut leader, gdk::Key::space, t0), None);
        assert_eq!(chord_press(&chords, &mut leader, gdk::Key::j, t0 + Duration::from_millis(50)), None);
        assert!(leader.is_none());
    }
}

