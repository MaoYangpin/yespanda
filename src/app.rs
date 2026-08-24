use std::collections::HashMap;
use std::path::PathBuf;

use adw::prelude::*;
use relm4::gtk::glib;
use relm4::gtk::glib::clone;
use relm4::gtk::gdk;
use relm4::gtk::gio::{SimpleAction, SimpleActionGroup};
use relm4::{
    Component, ComponentController, ComponentParts, ComponentSender, Controller,
    RelmRemoveAllExt, adw, gtk,
};

use crate::pdf::{PdfDoc, TocEntry};
use crate::toc::{TocInput, TocOutput, TocSidebar};

const PAGE_SPACING: i32 = 12;

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
    toc: Controller<TocSidebar>,
}

#[derive(Debug, Clone)]
pub enum AppMsg {
    OpenFile(String),
    ZoomIn,
    ZoomOut,
    GoToPage(usize),
    ViewportChanged,
}

impl AppModel {
    fn load_document(&mut self, widgets: &mut AppModelWidgets, path: PathBuf, doc: PdfDoc) {
        let entries: Vec<TocEntry> = doc.toc();
        let n_pages = doc.n_pages();
        let sizes_pt: Vec<(f64, f64)> = (0..n_pages).map(|i| doc.page_size(i)).collect();

        self.doc = Some(doc);
        self.path = Some(path);
        self.zoom = 1.0;
        self.fit_mode = true;
        self.last_viewport_width = 0;
        self.page_sizes_pt = sizes_pt;
        self.toc_entries = entries.clone();
        self.highlighted_toc = None;
        self.rebuild_pages();

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

    /// Index of the page under the viewport's vertical centre.
    fn current_page(&self) -> Option<usize> {
        if self.pictures.is_empty() {
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
    fn update_toc_highlight(&mut self) {
        let target = self
            .current_page()
            .and_then(|page| self.active_toc_entry(page));
        if target == self.highlighted_toc {
            return;
        }
        self.highlighted_toc = target;
        self.toc.sender().send(TocInput::Highlight(target)).ok();
    }

    /// Sidebar entry covering `page`: the last one starting at or before it.
    fn active_toc_entry(&self, page: usize) -> Option<usize> {
        let index = self.toc_entries.partition_point(|entry| entry.page <= page);
        index.checked_sub(1)
    }

    fn scroll_to_page(&mut self, page: usize) {
        if page >= self.pictures.len() {
            return;
        }
        let offsets = self.offsets();
        let adjustment = self.pages_scroller.vadjustment();
        let target = offsets[page];
        adjustment.set_value(target.min(adjustment.upper() - adjustment.page_size()));
    }
}

#[relm4::component(pub)]
impl Component for AppModel {
    type Init = ();
    type Input = AppMsg;
    type Output = ();
    type CommandOutput = ();

    view! {
        #[root]
        adw::ApplicationWindow {
            set_default_width: 1100,
            set_default_height: 760,

            #[wrap(Some)]
            set_content = &adw::NavigationSplitView {
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
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let toc = TocSidebar::builder()
            .launch(())
            .forward(sender.input_sender(), |msg| match msg {
                TocOutput::GoTo(page) => AppMsg::GoToPage(page),
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
            fit_mode: true,
            last_viewport_width: 0,
            page_sizes_pt: Vec::new(),
            textures: HashMap::new(),
            pictures: Vec::new(),
            pages_box: pages_box.clone(),
            pages_scroller: pages_scroller.clone(),
            status_page: status_page.clone(),
            toc_entries: Vec::new(),
            highlighted_toc: None,
            toc,
        };

        let widgets = view_output!();

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

        let actions = SimpleActionGroup::new();
        for (name, msg) in [("zoom-in", AppMsg::ZoomIn), ("zoom-out", AppMsg::ZoomOut)] {
            let action = SimpleAction::new(name, None);
            let forward = sender.clone();
            action.connect_activate(move |_, _| forward.input(msg.clone()));
            actions.add_action(&action);
        }
        root.insert_action_group("win", Some(&actions));
        if let Some(application) = root.application() {
            application.set_accels_for_action("win.zoom-in", &["<Primary>plus", "<Primary>equal"]);
            application.set_accels_for_action("win.zoom-out", &["<Primary>minus"]);
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
                    self.fit_mode = false;
                    self.zoom = (self.zoom * 1.25).min(4.0);
                    self.resize_pages();
                    self.textures.clear();
                }
            }
            AppMsg::ZoomOut => {
                if self.doc.is_some() {
                    self.fit_mode = false;
                    self.zoom = (self.zoom / 1.25).max(0.25);
                    self.resize_pages();
                    self.textures.clear();
                }
            }
            AppMsg::GoToPage(page) => self.scroll_to_page(page),
            AppMsg::ViewportChanged => {
                self.apply_fit_width();
                self.render_visible();
                self.update_toc_highlight();
            }
        }
        self.update_view(widgets, sender);
    }
}

