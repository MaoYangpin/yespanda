use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Condvar, Mutex};

use adw::prelude::*;
use relm4::gtk::glib;
use relm4::gtk::glib::clone;
use relm4::gtk::gdk;
use relm4::gtk::prelude::AdjustmentExt;
use relm4::{
    Component, ComponentController, ComponentParts, ComponentSender, Controller,
    RelmRemoveAllExt, adw, gtk,
};

use crate::config::{Config, History, parse_binding};
use crate::pdf::{MatchRect, PdfDoc, TocEntry};
use crate::notes::Notes;
use crate::notes_dialog::{
    NotesList, NotesListInit, NotesListInput, NotesListItem, NotesListOutput,
};
use crate::picker::{PickerDialog, PickerInit, PickerOutput, expand_root};
use crate::toc::{SidebarBindings, TocInput, TocOutput, TocSidebar};

const PAGE_SPACING: i32 = 12;
/// Portion of the viewport scrolled by one `j`/`k` press.
const SCROLL_STEP_FRACTION: f64 = 0.25;
/// Window in which a chord's completing key must follow its leader.
const CHORD_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);

/// Chord state machine state: the armed leader key plus, while a count is
/// being typed after the leader (`g 1 2`), the accumulated page number.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct ChordState {
    leader: Option<(gdk::Key, std::time::Instant)>,
    /// Page number typed so far; 0 means not collecting.
    count: u32,
}

/// Advance the chord state machine on a key press. Returns the action to
/// fire if the press completed something.
///
/// Two behaviors share one leader key:
/// - two-key chords (`space e`, `space space`, `g g`) complete within
///   [`CHORD_WINDOW`], checked before arming because a key can be both
///   leader and completing key (`space space`);
/// - while the count leader (`g`) is armed, digit keys accumulate a page
///   number (no time limit), and `Enter` fires [`AppMsg::GoToPage`]. Any
///   other key cancels the collection and is processed as a fresh press.
fn chord_press(
    chords: &[(gdk::Key, gdk::Key, AppMsg)],
    count_leader: Option<gdk::Key>,
    state: &mut ChordState,
    keyval: gdk::Key,
    now: std::time::Instant,
) -> Option<AppMsg> {
    let digit = keyval.to_unicode().and_then(|char| char.to_digit(10));
    if state.count > 0 {
        if let Some(digit) = digit {
            state.count = state.count.saturating_mul(10).saturating_add(digit);
            return None;
        }
        if matches!(keyval, gdk::Key::Return | gdk::Key::KP_Enter) {
            let page = state.count;
            *state = ChordState::default();
            return Some(AppMsg::GoToPage(page));
        }
        // Any other key cancels the collection; continue below so it is
        // handled like a fresh press (bindings loop, re-arming, ...).
        *state = ChordState::default();
    } else if state.leader.is_some() && count_leader == state.leader.map(|(key, _)| key) {
        // First digit starts the collection (count was still 0).
        if let Some(digit) = digit {
            *state = ChordState {
                leader: state.leader,
                count: digit,
            };
            return None;
        }
    }
    if let Some((armed, pressed)) = state.leader
        && now.duration_since(pressed) <= CHORD_WINDOW
            && let Some((_, _, msg)) = chords
                .iter()
                .find(|(leader_key, chord_key, _)| *leader_key == armed && *chord_key == keyval)
            {
                *state = ChordState::default();
                return Some(msg.clone());
            }
    if chords.iter().any(|(leader_key, _, _)| *leader_key == keyval) {
        *state = ChordState {
            leader: Some((keyval, now)),
            count: 0,
        };
    } else {
        *state = ChordState::default();
    }
    None
}

/// One viewport batch of pages to render off the main thread. `order` is
/// pre-sorted center-out so workers paint the area the reader is looking
/// at first.
struct RenderJob {
    generation: u64,
    zoom: f64,
    order: Vec<usize>,
}

/// Shared work queue for the render pool. Each posted job fully replaces
/// the previous one (claims included): duplicate concurrent renders of the
/// same page are harmless because the main thread drops repeats.
#[derive(Default)]
struct Pool {
    inner: Mutex<PoolState>,
    work_available: Condvar,
}

#[derive(Default)]
struct PoolState {
    job: Option<std::sync::Arc<RenderJob>>,
    claimed: std::collections::HashSet<usize>,
    shutdown: bool,
}

/// Fixed-size pool of renderer threads. Every worker owns its own `PdfDoc`
/// handle (poppler allows multiple opens of one file), so pages render in
/// parallel with no locking while a render is running.
struct Renderer {
    pool: std::sync::Arc<Pool>,
    /// Worker threads for the current document. Stored so we can join them
    /// on drop — otherwise each thread keeps its `PdfDoc` (mmap'd PDF) alive
    /// until it happens to exit, leaking memory across file switches.
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl Renderer {
    /// Spawn the worker pool for `path`. The already-opened document (if
    /// any) is handed to the first worker; the rest open their own handle
    /// lazily on their first job.
    fn spawn(path: PathBuf, mut leader: Option<PdfDoc>, reply: relm4::Sender<AppMsg>) -> Self {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
            .clamp(1, 4);
        let pool = std::sync::Arc::new(Pool::default());
        let mut handles = Vec::with_capacity(threads);
        for worker in 0..threads {
            let pool = pool.clone();
            let path = path.clone();
            let reply = reply.clone();
            // The main-thread document can only feed ONE worker without
            // sharing; see the Send note on `PdfDoc`.
            let doc = if worker == 0 { leader.take() } else { None };
            handles.push(std::thread::spawn(move || run_worker(pool, path, doc, reply)));
        }
        Self { pool, handles }
    }

    fn request(&self, job: RenderJob) {
        let mut state = self.pool.inner.lock().unwrap();
        state.job = Some(std::sync::Arc::new(job));
        state.claimed.clear();
        self.pool.work_available.notify_all();
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        {
            let mut state = self.pool.inner.lock().unwrap();
            state.shutdown = true;
            state.job = None;
            self.pool.work_available.notify_all();
        }
        // Wait for every worker to release its PdfDoc before the new file's
        // pool starts, so mmap'd documents from previous files don't pile up.
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

/// Worker body: claim the next unclaimed page of the current job, render
/// it with this thread's private document handle, and post the result.
fn run_worker(
    pool: std::sync::Arc<Pool>,
    path: PathBuf,
    doc: Option<PdfDoc>,
    reply: relm4::Sender<AppMsg>,
) {
    let mut doc = doc;
    let mut open_warned = false;
    'outer: loop {
        let (job, index) = {
            let mut state = pool.inner.lock().unwrap();
            loop {
                if state.shutdown {
                    return;
                }
                if let Some(job) = state.job.clone()
                    && let Some(&index) = job.order.iter().find(|i| !state.claimed.contains(i))
                {
                    state.claimed.insert(index);
                    break (job, index);
                }
                state = pool.work_available.wait(state).unwrap();
            }
        };
        if doc.is_none() {
            match PdfDoc::open(&path) {
                Ok(opened) => doc = Some(opened),
                Err(error) => {
                    if !open_warned {
                        eprintln!("renderer failed to open {path:?}: {error:#}");
                        open_warned = true;
                    }
                    // Back off instead of spinning; other workers proceed.
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue 'outer;
                }
            }
        }
        let result = doc
            .as_ref()
            .unwrap()
            .render_page_bytes(index, job.zoom)
            .map_err(|error| format!("{error:#}"));
        let _ = reply.send(AppMsg::PageRendered {
            generation: job.generation,
            zoom: job.zoom,
            index,
            result,
        });
    }
}

/// Rendered page image. Stored as a `gdk::Texture` (not a `Pixbuf`) so the
/// page widget can paint it through `gtk::Snapshot::append_texture`. GSK keys
/// the GL upload on the texture's identity, which is stable across frames, so
/// the GPU texture cache stays bounded to the live pages. Painting a `Pixbuf`
/// from a cairo draw func forces a fresh per-frame GL upload with no identity
/// to dedupe on, which leaked GPU memory without bound while scrolling.
struct PageArt {
    texture: gdk::Texture,
}

/// One page widget. Paints its `gdk::Texture` plus vector overlays (note
/// tabs, search highlights) via `gtk::Snapshot`, which keeps the GPU texture
/// cache bounded instead of growing per redraw.
mod page_view {
    use super::*;
    use gtk::subclass::prelude::*;

    #[derive(Default)]
    pub struct PageViewPriv {
        view: std::cell::RefCell<Option<Rc<ViewState>>>,
        index: std::cell::Cell<usize>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PageViewPriv {
        const NAME: &'static str = "YespandaPageView";
        type Type = PageView;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for PageViewPriv {}
    impl WidgetImpl for PageViewPriv {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let view = self.view.borrow();
            let Some(view) = view.as_ref() else { return };
            let index = self.index.get();
            let zoom = view.zoom.get();
            let textures = view.textures.borrow();
            let Some(art) = textures.get(&index) else { return };
            let tex = &art.texture;
            let tex_w = tex.width() as f64;
            let tex_h = tex.height() as f64;
            let w = widget.width() as f64;
            let h = widget.height() as f64;
            // Rendered at exactly `zoom`, so it matches the page box; center
            // horizontally when the allocation is wider (hexpand fills it).
            let ox = ((w - tex_w) / 2.0).max(0.0);
            let oy = ((h - tex_h) / 2.0).max(0.0);
            snapshot.append_texture(
                tex,
                &gtk::graphene::Rect::new(ox as f32, oy as f32, tex_w as f32, tex_h as f32),
            );

            // Note markers: slim blue triangle tabs on the page's right edge.
            if let Some(fracs) = view.note_marks.borrow().get(&index).cloned() {
                let right = (ox + tex_w) as f32;
                for frac in fracs {
                    let y = (oy + frac * tex_h) as f32;
                    let b = gsk4::PathBuilder::new();
                    b.move_to(right - 9.0, y - 7.0);
                    b.line_to(right, y);
                    b.line_to(right - 9.0, y + 7.0);
                    b.close();
                    snapshot.append_fill(
                        &b.to_path(),
                        gsk4::FillRule::Winding,
                        &gdk::RGBA::new(0.20, 0.55, 0.95, 0.90),
                    );
                }
            }

            // Search highlights. The active match gets a stronger fill and an
            // orange outline; others are a soft yellow wash.
            let active = view.current_match.get();
            let rects = view.matches.borrow().get(&index).cloned().unwrap_or_default();
            for (i, rect) in rects.iter().enumerate() {
                let bounds = gtk::graphene::Rect::new(
                    (rect.x * zoom + ox) as f32,
                    (rect.y * zoom + oy) as f32,
                    (rect.w * zoom) as f32,
                    (rect.h * zoom) as f32,
                );
                let is_active = active == Some((index, i));
                let color = if is_active {
                    gdk::RGBA::new(1.0, 0.78, 0.1, 0.50)
                } else {
                    gdk::RGBA::new(1.0, 0.78, 0.1, 0.28)
                };
                snapshot.append_color(&color, &bounds);
                if is_active {
                    let b = gsk4::PathBuilder::new();
                    b.add_rect(&bounds);
                    snapshot.append_stroke(
                        &b.to_path(),
                        &gsk4::Stroke::new(2.0),
                        &gdk::RGBA::new(0.98, 0.45, 0.05, 0.95),
                    );
                }
            }
        }
    }

    glib::wrapper! {
        pub struct PageView(ObjectSubclass<PageViewPriv>)
            @extends gtk::Widget,
            @implements gtk::Buildable, gtk::Accessible, gtk::ConstraintTarget;
    }

    impl PageView {
        pub(crate) fn new(view: Rc<ViewState>, index: usize) -> Self {
            let obj = glib::Object::builder::<Self>().build();
            obj.imp().view.borrow_mut().replace(view);
            obj.imp().index.set(index);
            obj
        }

        /// Point this (pooled) widget at a different page. The caller is
        /// responsible for `queue_draw` afterwards.
        pub(crate) fn set_page(&self, index: usize) {
            self.imp().index.set(index);
        }
    }
}

/// Scan every page of `path` for `query` on a worker thread.
fn scan_document(path: &Path, n_pages: usize, query: &str) -> HashMap<usize, Vec<MatchRect>> {
    let Ok(doc) = PdfDoc::open(path) else {
        return HashMap::new();
    };
    let mut hits = HashMap::new();
    for index in 0..n_pages {
        let found = doc.find_text(index, query);
        if !found.is_empty() {
            hits.insert(index, found);
        }
    }
    hits
}

#[derive(Default)]
pub(crate) struct ViewState {
    textures: RefCell<HashMap<usize, PageArt>>,
    /// Search hits per page in top-left-origin points (see
    /// [`MatchRect`]); empty when no search results are shown.
    matches: RefCell<HashMap<usize, Vec<MatchRect>>>,
    /// Note anchor positions per 0-based page index, each a fraction of
    /// the page height; drives the blue edge tabs.
    note_marks: RefCell<HashMap<usize, Vec<f64>>>,
    /// Page + index-within-page of the active match.
    current_match: Cell<Option<(usize, usize)>>,
    zoom: Cell<f64>,
}

pub struct AppModel {
    /// Owns the document on the renderer threads; `None` until a file opens.
    renderer: Option<Renderer>,
    /// Bumped whenever cached textures become invalid (new document, zoom
    /// change) so late worker replies can be recognized as stale.
    render_gen: u64,
    path: Option<PathBuf>,
    zoom: f64,
    fit_mode: bool,
    last_viewport_width: i32,
    page_sizes_pt: Vec<(f64, f64)>,
    /// Textures, search matches, and zoom shared with the page draw
    /// handlers (all mutated on the main thread only).
    view: Rc<ViewState>,
    /// A small, fixed pool of reusable page widgets. Only the pages in the
    /// visible window (plus a prefetch margin) are ever assigned to a pool
    /// widget, so the number of widgets — and thus the GSK-retained page
    /// textures — stays bounded no matter how long the document is.
    pages_pool: Vec<page_view::PageView>,
    /// Pool slot -> page index currently shown there (`None` = free slot).
    slot_page: Vec<Option<usize>>,
    /// Page index -> pool slot showing it (inverse of `slot_page`).
    page_slot: HashMap<usize, usize>,
    /// The single scrolled container holding the pool widgets, sized to the
    /// full document so the scrollbar tracks the whole file.
    pages_fixed: gtk::Fixed,
    pages_scroller: gtk::ScrolledWindow,
    status_page: adw::StatusPage,
    /// Lives inside the header bar's swap stack; hidden while searching.
    title_widget: adw::WindowTitle,
    /// Match counter shown next to the search entry in the header bar.
    match_label: gtk::Label,
    /// The search input inside the header bar's swap stack.
    search_entry: gtk::SearchEntry,
    /// Search bar state.
    search_open: bool,
    search_query: String,
    /// Bumped per committed query so late scan replies are dropped.
    search_gen: u64,
    search_debounce: Option<glib::SourceId>,
    /// Flattened match count and active index across all pages.
    match_total: usize,
    current_flat: usize,
    /// Shared with the window key controller so bindings stay off while
    /// the user is typing in the search entry.
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
    /// All saved notes across documents (write-through on changes).
    notes: Rc<RefCell<Notes>>,
    /// Top-right overlay card hosting the note text editor.
    editor_slot: gtk::Box,
    editor_heading: gtk::Label,
    editor_text: gtk::TextView,
    /// Anchor frozen when the editor was opened: (page 0-based, y_frac).
    pending_note_anchor: Option<(usize, f64)>,
    /// Set while revising an existing note instead of creating one.
    editing_note_id: Option<u64>,
    /// Persistent notes browser for the current document.
    notes_list: Controller<NotesList>,
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
    /// Jump to the start of the document (`g g` chord).
    ScrollTop,
    /// Jump to the end of the document (`G`).
    ScrollBottom,
    /// Jump to a 1-based page number typed after the `g` leader
    /// (`g 4 2 <Enter>`). Clamped to the document's page count.
    GoToPage(u32),
    ToggleSidebar,
    /// A renderer-thread page render finished. `gen`/`zoom` tag the batch
    /// so results from an older document state are dropped on arrival.
    PageRendered {
        generation: u64,
        zoom: f64,
        index: usize,
        result: Result<(i32, i32, Vec<u8>), String>,
    },
    /// Move keyboard focus to the TOC sidebar (Ctrl+h). Never scrolls the
    /// PDF or changes the position highlight.
    FocusSidebar,
    /// Move keyboard focus to the PDF content (Ctrl+l).
    FocusPdf,
    /// Open the fd+fzf PDF picker dialog.
    PickFile,
    /// Show the search bar and focus its entry (`/`).
    OpenSearch,
    /// Enter in the entry: flush any pending scan, then collapse back to
    /// the title. Matches stay highlighted for `n` / `N`.
    SearchConfirm,
    /// Hide the search bar (Esc). Also clears match highlights and resets
    /// the input so the next `/` starts fresh.
    CloseSearch,
    /// The query text changed; schedules a debounced scan.
    SearchQuery(String),
    /// Debounced: scan the whole document for the query.
    SearchRun(String),
    /// A finished scan's hits, tagged with its generation.
    SearchResults {
        generation: u64,
        hits: HashMap<usize, Vec<MatchRect>>,
    },
    SearchNext,
    SearchPrev,
    /// Add a margin note anchored at the viewport center (`m`).
    AddNote,
    /// Browse the current document's notes (`M`).
    BrowseNotes,
    /// Editor finished: `Some(text)` saved, `None` discarded.
    NoteEdited(Option<String>),
    /// Browser asks to jump to a note's anchor.
    JumpToNote(u64),
    /// Browser deleted a note.
    NoteDeleted(u64),
    /// Result of the picker: `Some(path)` to open, `None` if cancelled.
    PickerResult(Option<PathBuf>),
}

impl AppModel {
    fn load_document(
        &mut self,
        widgets: &mut AppModelWidgets,
        path: PathBuf,
        doc: PdfDoc,
        sender: ComponentSender<Self>,
    ) {
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

        self.renderer = Some(Renderer::spawn(
            path.clone(),
            Some(doc),
            sender.input_sender().clone(),
        ));
        self.path = Some(path.clone());
        self.zoom = 1.0;
        self.last_viewport_width = 0;
        self.page_sizes_pt = sizes_pt;
        self.render_gen += 1;
        self.view.textures.borrow_mut().clear();
        // A new document invalidates any running search completely.
        self.reset_search(widgets);
        self.toc_entries = entries.clone();
        self.highlighted_toc = None;
        self.pinned_toc = None;
        self.pending_restore = self.history.borrow().page_for(&path);
        self.rebuild_note_marks();
        self.rebuild_pages();

        // Reset the scroll position to the top. Otherwise the stale scroll
        // value from the previous document can lie beyond the new content,
        // making render_visible compute an empty range so nothing is drawn
        // until the viewport is scrolled.
        self.pages_scroller.vadjustment().set_value(0.0);

        let name = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Document".into());
        self.title_widget.set_title(&name);

        self.toc.sender().send(TocInput::Set(entries)).ok();
        widgets.content_stack.set_visible_child_name("doc");
    }

    fn show_error(&mut self, widgets: &mut AppModelWidgets, error: anyhow::Error) {
        widgets.content_stack.set_visible_child_name("empty");
        self.title_widget.set_title("Error");
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

    /// Drop all cached page images and tag the next render batch so stale
    /// worker replies (old zoom or old document) are discarded on arrival.
    fn invalidate_textures(&mut self) {
        self.render_gen += 1;
        self.view.textures.borrow_mut().clear();
        for page in &self.pages_pool {
            page.queue_draw();
        }
    }

    /// Content width the pages are laid out within (== scroller viewport).
    fn content_width(&self) -> i32 {
        self.pages_scroller.width().max(self.last_viewport_width)
    }

    /// Redraw the page's widget if it is currently mapped to a pool slot.
    fn queue_page_draw(&self, index: usize) {
        if let Some(&slot) = self.page_slot.get(&index) {
            if let Some(widget) = self.pages_pool.get(slot) {
                widget.queue_draw();
            }
        }
    }

    /// The page range to keep on screen, derived from the scroll position
    /// and inflated by a prefetch margin on each side.
    fn visible_window(&self) -> (usize, usize) {
        let adjustment = self.pages_scroller.vadjustment();
        let top = adjustment.value();
        let bottom = top + adjustment.page_size();
        let offsets = self.offsets();
        let first = offsets.partition_point(|start| *start < top - PAGE_SPACING as f64);
        let last = offsets
            .partition_point(|start| *start < bottom)
            .min(self.page_sizes_pt.len());
        const PREFETCH: usize = 2;
        (
            first.saturating_sub(PREFETCH),
            (last + PREFETCH).min(self.page_sizes_pt.len()),
        )
    }

    /// Assign the pooled widgets to `first..last`, growing the pool if the
    /// window is larger than the current pool, freeing slots that scrolled
    /// out, and repositioning every assigned widget for the current zoom.
    fn layout_pages(&mut self, first: usize, last: usize) {
        if self.page_sizes_pt.is_empty() {
            return;
        }
        let offsets = self.offsets();
        let content_width = self.content_width();
        let window_len = last.saturating_sub(first);

        // Grow the pool so it can cover the whole window at once.
        while self.slot_page.len() < window_len {
            let widget = page_view::PageView::new(self.view.clone(), 0);
            self.pages_fixed.put(&widget, 0.0, 0.0);
            self.pages_pool.push(widget);
            self.slot_page.push(None);
        }

        // Free slots whose page left the window.
        for slot in 0..self.slot_page.len() {
            if let Some(p) = self.slot_page[slot] {
                if p < first || p >= last {
                    self.slot_page[slot] = None;
                    self.page_slot.remove(&p);
                    self.pages_pool[slot].set_visible(false);
                }
            }
        }

        // Assign each wanted page to a free slot.
        for p in first..last {
            if self.page_slot.contains_key(&p) {
                continue;
            }
            let slot = self
                .slot_page
                .iter()
                .position(|s| s.is_none())
                .unwrap_or_else(|| {
                    let widget = page_view::PageView::new(self.view.clone(), 0);
                    self.pages_fixed.put(&widget, 0.0, 0.0);
                    self.pages_pool.push(widget);
                    self.slot_page.push(None);
                    self.slot_page.len() - 1
                });
            self.slot_page[slot] = Some(p);
            self.page_slot.insert(p, slot);
            self.place_widget(slot, p, &offsets, content_width);
        }
    }

    /// Size, position and paint a single pool widget for `page`.
    fn place_widget(&self, slot: usize, page: usize, offsets: &[f64], content_width: i32) {
        let widget = &self.pages_pool[slot];
        let (_, height_pt) = self.page_sizes_pt[page];
        widget.set_page(page);
        widget.set_size_request(content_width, (height_pt * self.zoom).round() as i32);
        let y = offsets[page];
        self.pages_fixed.move_(widget, 0.0, y);
        widget.set_visible(true);
        widget.queue_draw();
    }

    /// Resize the scrolled content to span the full document.
    fn update_fixed_size(&self) {
        if self.page_sizes_pt.is_empty() {
            return;
        }
        let offsets = self.offsets();
        let height = offsets
            .last()
            .copied()
            .unwrap_or(0.0)
            + self.page_sizes_pt.last().map(|(_, h)| *h).unwrap_or(0.0) * self.zoom
            + PAGE_SPACING as f64;
        self.pages_fixed
            .set_size_request(self.content_width(), height.round() as i32);
    }

    fn rebuild_pages(&mut self) {
        // Tear down the previous pool and its slot bookkeeping; the pool is
        // rebuilt lazily by `layout_pages` on the next `render_visible`.
        for widget in &self.pages_pool {
            self.pages_fixed.remove(widget);
        }
        self.pages_pool.clear();
        self.slot_page.clear();
        self.page_slot.clear();
        // New document: any in-flight reply targets dead indexes.
        self.invalidate_textures();
    }

    fn resize_pages(&mut self) {
        self.view.zoom.set(self.zoom);
        // Re-place every currently assigned pool widget at the new scale.
        // Changing the sizes nudges the scrolled content extent, which fires
        // `ViewportChanged` and drives the follow-up texture re-render.
        let offsets = self.offsets();
        let content_width = self.content_width();
        for (slot, page) in self.slot_page.iter().enumerate() {
            if let Some(p) = *page {
                self.place_widget(slot, p, &offsets, content_width);
            }
        }
        self.update_fixed_size();
    }

    /// Top-edge pixel offset of every page at the current zoom.
    fn offsets(&self) -> Vec<f64> {
        let mut offsets = Vec::with_capacity(self.page_sizes_pt.len());
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
        if !self.fit_mode || self.renderer.is_none() {
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
        self.invalidate_textures();
    }

    /// Map the pooled widgets over the visible window, then ask the renderer
    /// pool for any page in that window that is not yet painted. Results come
    /// back as [`AppMsg::PageRendered`].
    fn render_visible(&mut self) {
        if self.page_sizes_pt.is_empty() {
            return;
        }
        let (first, last) = self.visible_window();
        self.layout_pages(first, last);
        self.update_fixed_size();

        let textures = self.view.textures.borrow();
        let missing: Vec<usize> = (first..last)
            .filter(|index| !textures.contains_key(index))
            .collect();
        drop(textures);
        if !missing.is_empty()
            && let Some(renderer) = self.renderer.as_ref()
        {
            // Paint outward from the viewport center so the area under the
            // reader's eyes appears first, regardless of scroll direction.
            let center = (first + last) / 2;
            let mut order = missing;
            order.sort_by_key(|index| index.abs_diff(center));
            renderer.request(RenderJob {
                generation: self.render_gen,
                zoom: self.zoom,
                order,
            });
        }

        // Drop textures for pages that scrolled out of the windowed range so
        // the cache — and the widgets' retained snapshots — stay bounded.
        self.view
            .textures
            .borrow_mut()
            .retain(|index, _| *index >= first && *index < last);
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

    // ---- search ---------------------------------------------------------

    // ---- notes ----------------------------------------------------------

    /// Recompute per-page note anchor fractions for the open document.
    fn rebuild_note_marks(&mut self) {
        let mut marks: HashMap<usize, Vec<f64>> = HashMap::new();
        if let Some(path) = self.path.as_ref() {
            for note in self.notes.borrow().for_doc(path) {
                if note.page >= 1
                    && let Some(slot) =
                        marks.get_mut(&(note.page - 1))
                {
                    slot.push(note.y_frac);
                    continue;
                }
                marks.entry(note.page - 1).or_default().push(note.y_frac);
            }
        }
        for fracs in marks.values_mut() {
            fracs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        }
        *self.view.note_marks.borrow_mut() = marks;
    }

    /// (page 0-based, fraction down that page) of the viewport center —
    /// the anchor a new note gets when `m` is pressed.
    fn note_anchor_at_center(&self) -> Option<(usize, f64)> {
        let page = self.current_page()?;
        let offsets = self.offsets();
        let adjustment = self.pages_scroller.vadjustment();
        let center = adjustment.value() + adjustment.page_size() / 2.0;
        let top = *offsets.get(page)?;
        let (_, height_pt) = self.page_sizes_pt.get(page).copied()?;
        let frac = ((center - top) / (height_pt * self.zoom)).clamp(0.0, 1.0);
        Some((page, frac))
    }

    /// Scroll so that `y_frac` of `page` sits in the upper third of the
    /// viewport. Also drives prefetch renders via value_changed.
    fn scroll_to_offset(&mut self, page: usize, y_frac: f64) {
        let offsets = self.offsets();
        let Some(offset) = offsets.get(page).copied() else {
            return;
        };
        let height_pt = self.page_sizes_pt.get(page).map(|(_, h)| *h).unwrap_or(0.0);
        let adjustment = self.pages_scroller.vadjustment();
        let max = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
        let target = offset + y_frac * height_pt * self.zoom - adjustment.page_size() * 0.35;
        adjustment.set_value(target.clamp(adjustment.lower(), max));
    }

    /// Rows describing the open document's notes.
    fn build_note_items(&self) -> Vec<NotesListItem> {
        self.path
            .as_ref()
            .map(|path| {
                self.notes
                    .borrow()
                    .for_doc(path)
                    .iter()
                    .map(|note| NotesListItem {
                        id: note.id,
                        title: note.text.lines().next().unwrap_or("").to_string(),
                        subtitle: format!(
                            "p.{} · {}% · {}",
                            note.page,
                            (note.y_frac * 100.0).round() as i32,
                            note.created
                        ),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Refresh and present the notes browser for the open document.
    fn show_notes_list(&mut self) {
        let items = self.build_note_items();
        self.notes_list.emit(NotesListInput::Update(items));
        self.notes_list.emit(NotesListInput::Show);
    }

    /// Forget everything about the current search (document switch).
    fn reset_search(&mut self, widgets: &mut AppModelWidgets) {
        if let Some(source) = self.search_debounce.take() {
            source.remove();
        }
        self.search_open = false;
        widgets.header_stack.set_visible_child_name("title");
        self.search_query.clear();
        self.search_gen += 1;
        self.match_total = 0;
        self.current_flat = 0;
        self.view.matches.borrow_mut().clear();
        self.view.current_match.set(None);
        self.match_label.set_label("");
        self.search_entry.set_text("");
    }

    /// Drop results (empty query) without hiding the bar.
    fn clear_matches(&mut self) {
        let redrawn: Vec<usize> = self.view.matches.borrow().keys().copied().collect();
        self.view.matches.borrow_mut().clear();
        self.view.current_match.set(None);
        self.match_total = 0;
        self.current_flat = 0;
        self.match_label.set_label("");
        for index in redrawn {
            self.queue_page_draw(index);
        }
    }

    /// Install fresh scan results, select the first match and jump to it.
    fn apply_search_results(&mut self, hits: HashMap<usize, Vec<MatchRect>>) {
        let previous: Vec<usize> = self.view.matches.borrow().keys().copied().collect();
        let total: usize = hits.values().map(|rects| rects.len()).sum();
        *self.view.matches.borrow_mut() = hits;

        let first = if total > 0 { Some((0, 0)) } else { None };
        self.view.current_match.set(first);
        self.match_total = total;
        self.current_flat = 0;
        let label = if total > 0 {
            format!("1/{total}")
        } else {
            "No matches".to_string()
        };
        self.match_label.set_label(&label);

        for index in previous.iter().copied().chain(self.view.matches.borrow().keys().copied()) {
            self.queue_page_draw(index);
        }
        if let Some((page, index_in_page)) = first {
            let rect = {
                let matches = self.view.matches.borrow();
                matches
                    .get(&page)
                    .and_then(|rects| rects.get(index_in_page))
                    .copied()
            };
            if let Some(rect) = rect {
                self.scroll_to_match(page, rect);
            }
        }
    }

    /// Move the active match by `delta` (wrapping) and scroll to it.
    fn step_match(&mut self, delta: isize) {
        if self.match_total == 0 {
            return;
        }
        let total = self.match_total as isize;
        let next = (self.current_flat as isize + delta).rem_euclid(total) as usize;
        let Some((page, index_in_page)) = self.nth_match(next) else {
            return;
        };
        let old_current = self.view.current_match.get();
        self.view.current_match.set(Some((page, index_in_page)));
        self.current_flat = next;
        self.match_label
            .set_label(&format!("{}/{}", next + 1, self.match_total));
        // Redraw only the two affected pages.
        let mut redraw = Vec::new();
        if let Some((p, _)) = old_current {
            redraw.push(p);
        }
        redraw.push(page);
        for p in redraw {
            self.queue_page_draw(p);
        }
        let rect = {
            let matches = self.view.matches.borrow();
            matches
                .get(&page)
                .and_then(|rects| rects.get(index_in_page))
                .copied()
        };
        if let Some(rect) = rect {
            self.scroll_to_match(page, rect);
        }
    }

    /// Flattened `n`-th match across pages in ascending order.
    fn nth_match(&self, n: usize) -> Option<(usize, usize)> {
        let matches = self.view.matches.borrow();
        let mut walked = 0usize;
        for page in 0..self.page_sizes_pt.len() {
            let count = matches.get(&page).map_or(0, |rects| rects.len());
            if n < walked + count {
                return Some((page, n - walked));
            }
            walked += count;
        }
        None
    }

    /// Vertically center-ish a match rect in the viewport.
    fn scroll_to_match(&mut self, page: usize, rect: MatchRect) {
        let height_pt = self.page_sizes_pt.get(page).map(|(_, h)| *h).unwrap_or(1.0);
        self.scroll_to_offset(page, rect.y / height_pt);
    }

    /// Index of the page under the viewport's vertical centre.
    fn current_page(&self) -> Option<usize> {
        if self.page_sizes_pt.is_empty() {
            return None;
        }
        let adjustment = self.pages_scroller.vadjustment();
        let centre = adjustment.value() + adjustment.page_size() / 2.0;
        let offsets = self.offsets();
        let page = offsets.partition_point(|start| *start <= centre).max(1) - 1;
        Some(page.min(self.page_sizes_pt.len() - 1))
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
        if page >= self.page_sizes_pt.len() {
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
                            // Children are attached in init(): while
                            // searching the title morphs into the search
                            // field (Evince style); otherwise the plain
                            // document title shows.
                            #[wrap(Some)]
                            set_title_widget = header_stack = &gtk::Stack {
                                set_transition_type: gtk::StackTransitionType::Crossfade,
                            },
                        },
                        #[wrap(Some)]
                        set_content = content_overlay = &gtk::Overlay {
                            #[wrap(Some)]
                            set_child = content_stack = &gtk::Stack {
                                set_vexpand: true,
                                set_transition_type: gtk::StackTransitionType::Crossfade,
                            },
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

        let pages_fixed = gtk::Fixed::builder().build();
        let pages_scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&pages_fixed)
            .build();

        let status_page = adw::StatusPage::builder()
            .icon_name("application-pdf")
            .title("No document")
            .description("Start yespanda with a PDF file to view it.")
            .build();

        // Header-bar search: the title swaps for an entry + match counter
        // while a search is open.
        let title_widget = adw::WindowTitle::builder()
            .title("No document")
            .build();
        let search_entry = gtk::SearchEntry::builder()
            .hexpand(true)
            .width_chars(30)
            .placeholder_text("Find in document…")
            .build();
        {
            let tx = sender.input_sender().clone();
            search_entry.connect_changed(move |entry| {
                tx.send(AppMsg::SearchQuery(entry.text().into())).ok();
            });
            let tx = sender.input_sender().clone();
            search_entry.connect_activate(move |_| {
                tx.send(AppMsg::SearchConfirm).ok();
            });
            let tx = sender.input_sender().clone();
            search_entry.connect_stop_search(move |_| {
                tx.send(AppMsg::CloseSearch).ok();
            });
        }
        let match_label = gtk::Label::builder()
            .valign(gtk::Align::Center)
            .width_chars(10)
            .build();

        let view = Rc::new(ViewState::default());
        let notes = Rc::new(RefCell::new(Notes::load()));
        let notes_list = NotesList::builder()
            .launch(NotesListInit {
                parent: root.clone().upcast::<gtk::Window>(),
            })
            .forward(sender.input_sender(), |out| match out {
                NotesListOutput::Jumped(id) => AppMsg::JumpToNote(id),
                NotesListOutput::Deleted(id) => AppMsg::NoteDeleted(id),
                NotesListOutput::Closed => AppMsg::FocusPdf,
            });

        let mut model = AppModel {
            renderer: None,
            render_gen: 0,
            path: None,
            zoom: 1.0,
            fit_mode: config.borrow().fit_width,
            last_viewport_width: 0,
            page_sizes_pt: Vec::new(),
            view: view.clone(),
            pages_pool: Vec::new(),
            slot_page: Vec::new(),
            page_slot: HashMap::new(),
            pages_fixed: pages_fixed.clone(),
            pages_scroller: pages_scroller.clone(),
            status_page: status_page.clone(),
            title_widget: title_widget.clone(),
            match_label: match_label.clone(),
            search_entry: search_entry.clone(),
            search_open: false,
            search_query: String::new(),
            search_gen: 0,
            search_debounce: None,
            match_total: 0,
            current_flat: 0,
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
            notes,
            editor_slot: gtk::Box::default(),
            editor_heading: gtk::Label::default(),
            editor_text: gtk::TextView::default(),
            pending_note_anchor: None,
            editing_note_id: None,
            notes_list,
        };

        let widgets = view_output!();

        // Populate the header-bar swap stack: title first, search second.
        {
            let search_row = gtk::Box::builder()
                .spacing(8)
                .build();
            let entry = search_entry.clone();
            search_row.append(&entry);
            search_row.append(&match_label);
            widgets.header_stack.add_named(&title_widget, Some("title"));
            widgets.header_stack.add_named(&search_row, Some("search"));
            widgets.header_stack.set_visible_child_name("title");
        }

        // Note editor: a card pinned to the top-right of the page area.
        let editor_heading = gtk::Label::builder()
            .xalign(0.0)
            .css_classes(["heading"])
            .build();
        let editor_text = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .height_request(140)
            .hexpand(true)
            .vexpand(true)
            .build();
        {
            let tx = sender.input_sender().clone();
            let view = editor_text.clone();
            let keys = gtk::EventControllerKey::new();
            keys.connect_key_pressed(move |_, keyval, _, state| {
                if state.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                    && matches!(keyval, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter)
                {
                    let buffer = view.buffer();
                    let (start, end) = buffer.bounds();
                    let text = buffer.text(&start, &end, true).trim().to_string();
                    // Empty buffer behaves like a discard.
                    if text.is_empty() {
                        tx.send(AppMsg::NoteEdited(None)).ok();
                    } else {
                        tx.send(AppMsg::NoteEdited(Some(text))).ok();
                    }
                    return glib::Propagation::Stop;
                }
                if keyval == gtk::gdk::Key::Escape && state.is_empty() {
                    tx.send(AppMsg::NoteEdited(None)).ok();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            editor_text.add_controller(keys);
        }
        let editor_hint = gtk::Label::builder()
            .xalign(0.0)
            .label("Ctrl+Enter saves · Esc discards")
            .css_classes(["dim-label"])
            .build();
        let editor_scroll = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&editor_text)
            .build();
        let editor_slot = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_top(12)
            .margin_end(12)
            .width_request(360)
            .height_request(220)
            .valign(gtk::Align::Start)
            .halign(gtk::Align::End)
            .css_classes(["card", "background"])
            .build();
        editor_slot.append(&editor_heading);
        editor_slot.append(&editor_scroll);
        editor_slot.append(&editor_hint);
        editor_slot.set_visible(false);
        widgets.content_overlay.add_overlay(&editor_slot);
        model.editor_slot = editor_slot.clone();
        model.editor_heading = editor_heading.clone();
        model.editor_text = editor_text.clone();

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
        {
            let cfg = config.clone();
            let sv = widgets.split_view.clone();
            sv.connect_notify_local(Some("show-content"), move |split_view, _| {
                if !split_view.shows_content() && split_view.is_collapsed() {
                    let cfg = cfg.clone();
                    let sv = split_view.clone();
                    glib::idle_add_local(move || {
                        sv.set_collapsed(false);
                        sv.set_sidebar_width_fraction(cfg.borrow().sidebar.width_fraction);
                        glib::ControlFlow::Break
                    });
                }
            });
        }
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
            (AppMsg::ScrollBottom, &keymap.scroll_bottom),
            (AppMsg::FocusSidebar, &keymap.focus_sidebar),
            (AppMsg::FocusPdf, &keymap.focus_pdf),
            (AppMsg::ZoomIn, &keymap.zoom_in),
            (AppMsg::ZoomOut, &keymap.zoom_out),
            (AppMsg::PickFile, &keymap.pick_file),
            (AppMsg::OpenSearch, &keymap.search),
            (AppMsg::SearchNext, &keymap.search_next),
            (AppMsg::SearchPrev, &keymap.search_prev),
            (AppMsg::AddNote, &keymap.note_add),
            (AppMsg::BrowseNotes, &keymap.note_list),
        ]
        .into_iter()
        .filter_map(|(msg, spec)| parse_binding(spec).map(|(mods, key)| (msg, mods, key)))
        .collect();

        // Two-key chords from `[keymap]` (e.g. `space e`, `space space`). The
        // leader is pressed first, then the completing key within the window.
        let chords: Vec<(gdk::Key, gdk::Key, AppMsg)> = [
            (&keymap.sidebar_toggle, AppMsg::ToggleSidebar),
            (&keymap.pick_file_chord, AppMsg::PickFile),
            (&keymap.scroll_top, AppMsg::ScrollTop),
        ]
        .into_iter()
        .filter_map(|(spec, msg)| {
            let mut tokens = spec.split_whitespace();
            let leader = tokens.next().and_then(gdk::Key::from_name)?;
            let key = tokens.next().and_then(gdk::Key::from_name)?;
            Some((leader, key, msg))
        })
        .collect();
        let chord_state: Rc<std::cell::Cell<ChordState>> = Rc::default();
        // The leader that accepts digit counts for page jumps comes from
        // the same `[keymap]` entry as the scroll-top chord (`g g`).
        let count_leader = keymap
            .scroll_top
            .split_whitespace()
            .next()
            .and_then(gdk::Key::from_name);
        let key_controller = gtk::EventControllerKey::new();
        {
            let chord_state = chord_state.clone();
            let forward = sender.clone();
            key_controller.connect_key_pressed(move |_, keyval, _, state| {
                // Esc clears any active search (highlights + input). With
                // nothing to clear this is a no-op, so other Esc consumers
                // (sidebar, dialogs) keep working untouched.
                if keyval == gdk::Key::Escape && state.is_empty() {
                    forward.input(AppMsg::CloseSearch);
                }
                let now = std::time::Instant::now();
                let mut chord = chord_state.get();
                if state.is_empty() {
                    if let Some(msg) =
                        chord_press(&chords, count_leader, &mut chord, keyval, now)
                    {
                        forward.input(msg);
                        chord_state.set(chord);
                        return glib::Propagation::Stop;
                    }
                } else {
                    chord = ChordState::default();
                }
                chord_state.set(chord);
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

        // Reopen the most recently viewed PDF (newest history entry).
        if let Some(path) = model.history.borrow().most_recent().map(Path::to_path_buf) {
            sender.input(AppMsg::OpenFile(path.to_string_lossy().into_owned()));
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
                // open, and again from the history reopen on startup.
                // Reloading the same document mid-flight resets the zoom to
                // 1.0 and rebuilds the pages, so page jumps computed in that
                // window land on the wrong page. Skip the duplicate.
                if self.renderer.is_some() && self.path.as_deref() == Some(path.as_path()) {
                    return;
                }
                match PdfDoc::open(&path) {
                    Ok(doc) => {
                        self.load_document(widgets, path, doc, sender.clone());
                        let retry = sender.clone();
                        glib::idle_add_local_once(move || retry.input(AppMsg::ViewportChanged));
                    }
                    Err(error) => self.show_error(widgets, error),
                }
            }
            AppMsg::ZoomIn => {
                if self.renderer.is_some() {
                    self.set_manual_zoom();
                    self.zoom = (self.zoom * 1.25).min(4.0);
                    self.resize_pages();
                    self.invalidate_textures();
                }
            }
            AppMsg::ZoomOut => {
                if self.renderer.is_some() {
                    self.set_manual_zoom();
                    self.zoom = (self.zoom / 1.25).max(0.25);
                    self.resize_pages();
                    self.invalidate_textures();
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
            AppMsg::GoToPage(page) => {
                // 1-based as typed; `scroll_to_page` ignores out-of-range
                // pages, so clamping is all that is needed here.
                let index = (page as usize).saturating_sub(1);
                if index < self.page_sizes_pt.len() {
                    self.scroll_to_page(index);
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
            AppMsg::ScrollTop => {
                let adjustment = self.pages_scroller.vadjustment();
                adjustment.set_value(adjustment.lower());
            }
            AppMsg::ScrollBottom => {
                let adjustment = self.pages_scroller.vadjustment();
                // Never clamp below the lower bound; a short document
                // (content shorter than the viewport) has no room to move.
                let max = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
                adjustment.set_value(max);
            }
            AppMsg::PageRendered {
                generation,
                zoom,
                index,
                result,
            } => {
                // Drop replies from an older document state or zoom, and
                // replies for pages that were rebuilt meanwhile.
                if generation != self.render_gen
                    || zoom != self.zoom
                    || index >= self.page_sizes_pt.len()
                    || self.view.textures.borrow().contains_key(&index)
                {
                    return;
                }
                match result {
                    Ok((width, height, bytes)) => {
                        let texture = gdk::MemoryTexture::new(
                            width,
                            height,
                            gdk::MemoryFormat::B8g8r8a8Premultiplied,
                            &glib::Bytes::from_owned(bytes),
                            width as usize * 4,
                        );
                        self.view
                            .textures
                            .borrow_mut()
                            .insert(index, PageArt { texture: texture.into() });
                        self.queue_page_draw(index);
                    }
                    Err(error) => eprintln!("failed to render page {}: {error}", index + 1),
                }
            }
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
            AppMsg::OpenSearch => {
                self.search_open = true;
                widgets.header_stack.set_visible_child_name("search");
                let entry = self.search_entry.clone();
                glib::idle_add_local_once(move || {
                    entry.grab_focus();
                });
            }
            AppMsg::SearchConfirm => {
                // Enter finalizes the query: if typing settled less than
                // the debounce ago, run the scan now instead of dropping it.
                let query = self.search_query.clone();
                if !query.trim().is_empty() && self.search_debounce.take().is_some() {
                    sender.input(AppMsg::SearchRun(query));
                }
                // Collapse back to the title but KEEP the results: the
                // highlights stay on screen and n/N keep navigating.
                self.search_open = false;
                widgets.header_stack.set_visible_child_name("title");
                self.pages_scroller.grab_focus();
            }
            AppMsg::CloseSearch => {
                // Runs both for "close the open bar" and for a bare Esc
                // after Enter-confirmed results: no-op only when there is
                // neither an open bar nor anything highlighted.
                if !self.search_open && self.match_total == 0 {
                    return;
                }
                self.search_open = false;
                if let Some(source) = self.search_debounce.take() {
                    source.remove();
                }
                widgets.header_stack.set_visible_child_name("title");
                self.pages_scroller.grab_focus();
                // Esc means "forget this search": drop highlights and the
                // input text so the next `/` starts fresh.
                self.clear_matches();
                self.search_query.clear();
                self.search_entry.set_text("");
            }
            AppMsg::SearchQuery(text) => {
                self.search_query = text.clone();
                if let Some(source) = self.search_debounce.take() {
                    source.remove();
                }
                // Empty query clears results immediately; otherwise wait
                // for typing to settle before scanning the whole document.
                if text.trim().is_empty() {
                    self.clear_matches();
                    return;
                }
                let tx = sender.input_sender().clone();
                self.search_debounce = Some(glib::timeout_add_local_once(
                    std::time::Duration::from_millis(250),
                    move || {
                        tx.send(AppMsg::SearchRun(text)).ok();
                    },
                ));
            }
            AppMsg::SearchRun(query) => {
                self.search_debounce = None;
                let Some(path) = self.path.clone() else {
                    return;
                };
                let n_pages = self.page_sizes_pt.len();
                if n_pages == 0 {
                    return;
                }
                self.search_gen += 1;
                let generation = self.search_gen;
                let tx = sender.input_sender().clone();
                std::thread::spawn(move || {
                    let hits = scan_document(&path, n_pages, &query);
                    let _ = tx.send(AppMsg::SearchResults { generation, hits });
                });
            }
            AppMsg::SearchResults { generation, hits } => {
                if generation != self.search_gen {
                    return;
                }
                self.apply_search_results(hits);
            }
            AppMsg::SearchNext => self.step_match(1),
            AppMsg::SearchPrev => self.step_match(-1),
            AppMsg::AddNote => {
                // Only over a real document.
                if self.editor_slot.is_visible() || self.renderer.is_none() {
                    return;
                }
                let Some((page, y_frac)) = self.note_anchor_at_center() else {
                    return;
                };
                self.pending_note_anchor = Some((page, y_frac));
                self.editing_note_id = None;
                self.editor_heading
                    .set_label(&format!("New note · page {}", page + 1));
                self.editor_text.buffer().set_text("");
                self.editor_slot.set_visible(true);
                let view = self.editor_text.clone();
                glib::idle_add_local_once(move || {
                    view.grab_focus();
                });
            }
            AppMsg::BrowseNotes => self.show_notes_list(),
            AppMsg::NoteEdited(text) => {
                if !self.editor_slot.is_visible() {
                    return;
                }
                self.editor_slot.set_visible(false);
                let Some((page, y_frac)) = self.pending_note_anchor.take() else {
                    return;
                };
                // `None` means discard (Esc, or Ctrl+Enter on empty text);
                // `Some(text)` carries the already-read buffer content.
                self.pages_scroller.grab_focus();
                let editing_id = self.editing_note_id.take();
                let Some(doc) = self.path.clone() else {
                    return;
                };
                if let Some(text) = text
                    && !text.is_empty()
                {
                    let mut notes = self.notes.borrow_mut();
                    match editing_id {
                        Some(id) => {
                            notes.update_text(id, text);
                        }
                        None => {
                            notes.add(&doc, page + 1, y_frac, text);
                        }
                    }
                    if let Err(error) = notes.save() {
                        eprintln!("failed to save notes: {error:#}");
                    }
                }
                self.rebuild_note_marks();
                self.queue_page_draw(page);
            }
            AppMsg::JumpToNote(id) => {
                let note = self.notes.borrow().get(id).map(|note| {
                    (note.page.saturating_sub(1), note.y_frac)
                });
                if let Some((page, y_frac)) = note
                    && page < self.page_sizes_pt.len()
                {
                    self.scroll_to_offset(page, y_frac);
                    self.update_history_position();
                }
            }
            AppMsg::NoteDeleted(id) => {
                let removed_page = self.notes.borrow().get(id).and_then(|note| {
                    (note.doc == *self.path.as_ref()?).then(|| note.page - 1)
                });
                if self.notes.borrow_mut().remove(id) {
                    if let Err(error) = self.notes.borrow().save() {
                        eprintln!("failed to save notes: {error:#}");
                    }
                    self.rebuild_note_marks();
                    // Keep the open dialog in sync instead of leaving a
                    // stale row behind.
                    let items = self.build_note_items();
                    self.notes_list.emit(NotesListInput::Update(items));
                    if let Some(page) = removed_page {
                        self.queue_page_draw(page);
                    }
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
            (gdk::Key::g, gdk::Key::g, AppMsg::ScrollTop),
        ]
    }

    #[test]
    fn gg_fires_scroll_top() {
        let chords = chords();
        let t0 = std::time::Instant::now();
        let mut state = ChordState::default();
        // First g arms the leader; it must NOT fire yet.
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::g, t0), None);
        assert!(state.leader.is_some());
        // Second g within the window completes the chord.
        assert_eq!(
            chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::g, t0 + Duration::from_millis(100)),
            Some(AppMsg::ScrollTop)
        );
        assert!(state.leader.is_none());
    }

    #[test]
    fn g_then_capital_g_leaves_jump_to_bindings() {
        let chords = chords();
        let t0 = std::time::Instant::now();
        let mut state = ChordState::default();
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::g, t0), None);
        // G (Shift+g) is not a chord completer and not a leader: the chord
        // state machine must drop the leader so the plain Shift+G binding
        // can fire in the bindings loop instead.
        assert_eq!(
            chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::G, t0 + Duration::from_millis(100)),
            None
        );
        assert!(state.leader.is_none());
    }

    #[test]
    fn space_space_fires_pick_file() {
        let chords = chords();
        let t0 = std::time::Instant::now();
        let mut state = ChordState::default();
        // First Space arms the leader; it must NOT fire yet.
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::space, t0), None);
        assert!(state.leader.is_some());
        // Second Space within the window completes the chord.
        assert_eq!(
            chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::space, t0 + Duration::from_millis(100)),
            Some(AppMsg::PickFile)
        );
        assert!(state.leader.is_none());
    }

    #[test]
    fn space_e_fires_toggle_sidebar() {
        let chords = chords();
        let t0 = std::time::Instant::now();
        let mut state = ChordState::default();
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::space, t0), None);
        assert_eq!(
            chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::e, t0 + Duration::from_millis(100)),
            Some(AppMsg::ToggleSidebar)
        );
        assert!(state.leader.is_none());
    }

    #[test]
    fn chord_expires_but_leader_rearms() {
        let chords = chords();
        let t0 = std::time::Instant::now();
        let mut state = ChordState::default();
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::space, t0), None);
        // Too slow: no fire, but Space is still a leader so it re-arms.
        assert_eq!(
            chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::space, t0 + Duration::from_millis(1000)),
            None
        );
        assert!(state.leader.is_some());
    }

    #[test]
    fn unrelated_key_clears_leader() {
        let chords = chords();
        let t0 = std::time::Instant::now();
        let mut state = ChordState::default();
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::space, t0), None);
        assert_eq!(
            chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::j, t0 + Duration::from_millis(50)),
            None
        );
        assert!(state.leader.is_none());
    }

    #[test]
    fn g_digits_enter_fires_go_to_page() {
        let chords = chords();
        let t0 = std::time::Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut state = ChordState::default();
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::g, t0), None);
        // Digits collect with no time limit and do not fire.
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::_4, at(100)), None);
        assert_eq!(state.count, 4);
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::_2, at(2000)), None);
        assert_eq!(state.count, 42);
        // Enter commits as a 1-based page jump.
        assert_eq!(
            chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::Return, at(2500)),
            Some(AppMsg::GoToPage(42))
        );
        assert_eq!(state, ChordState::default());
    }

    #[test]
    fn go_to_page_leading_zeros_and_keypad_enter() {
        let chords = chords();
        let t0 = std::time::Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut state = ChordState::default();
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::g, t0), None);
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::_0, at(50)), None);
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::_7, at(100)), None);
        assert_eq!(
            chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::KP_Enter, at(150)),
            Some(AppMsg::GoToPage(7))
        );
    }

    #[test]
    fn digit_without_leader_is_ignored() {
        let chords = chords();
        let t0 = std::time::Instant::now();
        let mut state = ChordState::default();
        // Plain typing never starts collecting; nothing fires or stores.
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::_5, t0), None);
        assert_eq!(state, ChordState::default());
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::Return, t0), None);
    }

    #[test]
    fn other_key_cancels_collection_and_falls_through() {
        let chords = chords();
        let t0 = std::time::Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut state = ChordState::default();
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::g, t0), None);
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::_1, at(50)), None);
        // j cancels the collection (returns None so the bindings loop can
        // scroll) and resets the count.
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::j, at(100)), None);
        assert_eq!(state, ChordState::default());
    }

    #[test]
    fn escape_clears_collection() {
        let chords = chords();
        let t0 = std::time::Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut state = ChordState::default();
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::g, t0), None);
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::_9, at(50)), None);
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::Escape, at(100)), None);
        assert_eq!(state, ChordState::default());
    }

    #[test]
    fn long_digit_run_saturates_without_panic() {
        let chords = chords();
        let t0 = std::time::Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut state = ChordState::default();
        assert_eq!(chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::g, t0), None);
        for ms in 1..40u64 {
            chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::_9, at(ms * 10));
        }
        assert_eq!(state.count, u32::MAX);
        assert_eq!(
            chord_press(&chords, Some(gdk::Key::g), &mut state, gdk::Key::Return, at(400)),
            Some(AppMsg::GoToPage(u32::MAX))
        );
    }
}

