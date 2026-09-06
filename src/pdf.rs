use std::ffi::CStr;
use std::path::Path;

use anyhow::{Context as _, Result};
use cairo::{Context as CairoContext, Format, ImageSurface};
use poppler::{
    Document, FindFlags,
    ffi::{
        POPPLER_ACTION_GOTO_DEST, POPPLER_ACTION_NAMED, PopplerIndexIter, poppler_action_free,
        poppler_dest_free, poppler_index_iter_free, poppler_index_iter_get_action,
        poppler_index_iter_get_child, poppler_index_iter_new, poppler_index_iter_next,
    },
};
use relm4::gtk::glib;
use relm4::gtk::glib::translate::ToGlibPtr;

pub struct PdfDoc {
    doc: Document,
}

// SAFETY: PdfDoc has no interior mutability and is always used by a single
// owner at a time (the app hands it to one dedicated renderer thread after
// opening). poppler-glib objects are not thread-safe for concurrent access,
// but sequential access from whichever thread owns the handle is fine.
unsafe impl Send for PdfDoc {}

#[derive(Clone, Debug)]
pub struct TocEntry {
    pub title: String,
    pub page: usize,
    pub depth: usize,
}

/// A text match on a page, in PDF points with the origin at the top-left
/// corner of the page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MatchRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl PdfDoc {
    pub fn open(path: &Path) -> Result<Self> {
        let uri = glib_uri(path);
        let doc = Document::from_file(&uri, None).context("failed to open PDF document")?;
        Ok(Self { doc })
    }

    pub fn n_pages(&self) -> usize {
        self.doc.n_pages().max(0) as usize
    }

    pub fn page_size(&self, index: usize) -> (f64, f64) {
        self.doc
            .page(index as i32)
            .map(|page| page.size())
            .unwrap_or((595.0, 842.0))
    }

    /// Flattened table of contents; named destinations resolved against this document.
    pub fn toc(&self) -> Vec<TocEntry> {
        unsafe {
            let root = poppler_index_iter_new(self.doc.to_glib_none().0);
            if root.is_null() {
                return Vec::new();
            }
            let mut entries = Vec::new();
            self.walk_toc(root, 0, &mut entries);
            let last = self.n_pages().saturating_sub(1);
            entries.retain_mut(|entry| {
                entry.title = entry.title.trim().to_owned();
                if entry.title.is_empty() {
                    return false;
                }
                entry.page = entry.page.min(last);
                true
            });
            entries
        }
    }

    unsafe fn walk_toc(
        &self,
        iter: *mut PopplerIndexIter,
        depth: usize,
        entries: &mut Vec<TocEntry>,
    ) {
        loop {
            unsafe {
                let action = poppler_index_iter_get_action(iter);
                if !action.is_null() {
                    let title_ptr = (*action).any.title;
                    let title = if title_ptr.is_null() {
                        String::new()
                    } else {
                        CStr::from_ptr(title_ptr).to_string_lossy().into_owned()
                    };

                    // The dest embedded in an action is owned by the action;
                    // only destinations returned by find_dest() have to be
                    // freed ourselves.
                    let action_type = (*action).type_;
                    let page = match action_type {
                        POPPLER_ACTION_GOTO_DEST => {
                            let dest = (*action).goto_dest.dest;
                            if dest.is_null() {
                                None
                            } else {
                                // PopplerDest::page_num is 1-based.
                                let page_num = (*dest).page_num;
                                if page_num > 0 {
                                    Some(page_num as usize - 1)
                                } else {
                                    self.resolve_named_dest((*dest).named_dest)
                                }
                            }
                        }
                        POPPLER_ACTION_NAMED => self.resolve_named_dest((*action).named.named_dest),
                        _ => None,
                    };
                    poppler_action_free(action);

                    entries.push(TocEntry {
                        title,
                        page: page.unwrap_or(0),
                        depth,
                    });
                }

                // Note: walk_toc takes ownership and frees the child iterator.
                let child = poppler_index_iter_get_child(iter);
                if !child.is_null() {
                    self.walk_toc(child, depth + 1, entries);
                }

                if poppler_index_iter_next(iter) == 0 {
                    poppler_index_iter_free(iter);
                    break;
                }
            }
        }
    }

    unsafe fn resolve_named_dest(&self, named: *mut std::ffi::c_char) -> Option<usize> {
        if named.is_null() {
            return None;
        }
        unsafe {
            let name = CStr::from_ptr(named).to_string_lossy().into_owned();
            let resolved: *mut poppler::ffi::PopplerDest =
                self.doc.find_dest(&name)?.to_glib_full();
            if resolved.is_null() {
                return None;
            }
            let page_num = (*resolved).page_num;
            poppler_dest_free(resolved);
            (page_num > 0).then_some(page_num as usize - 1)
        }
    }

    /// Case-insensitive text search on one page. Returns matches in
    /// document order, top-left-origin points (see [`MatchRect`]).
    pub fn find_text(&self, index: usize, needle: &str) -> Vec<MatchRect> {
        if needle.is_empty() {
            return Vec::new();
        }
        let Some(page) = self.doc.page(index as i32) else {
            return Vec::new();
        };
        // poppler reports rectangles in bottom-left-origin points; flip the
        // vertical axis so callers can work in widget-like coordinates.
        let page_height_pt = page.size().1;
        page.find_text_with_options(needle, FindFlags::DEFAULT)
            .into_iter()
            .map(|rect| {
                let x1 = rect.x1();
                let y1 = rect.y1();
                let x2 = rect.x2();
                let y2 = rect.y2();
                let y_lo = y1.min(y2);
                let y_hi = y1.max(y2);
                MatchRect {
                    x: x1.min(x2),
                    y: page_height_pt - y_hi,
                    w: (x2 - x1).abs(),
                    h: y_hi - y_lo,
                }
            })
            .filter(|m| m.w > 0.0 && m.h > 0.0)
            .collect()
    }

    /// Render a page to packed BGRA bytes `(width, height, data)`. Uses only
    /// cairo (no GDK objects), so it is safe to call off the main thread.
    pub fn render_page_bytes(&self, index: usize, zoom: f64) -> Result<(i32, i32, Vec<u8>)> {
        let page = self.doc.page(index as i32).context("missing page")?;
        let (width_pt, height_pt) = page.size();

        let width = ((width_pt * zoom).round() as i32).clamp(1, 16384);
        let height = ((height_pt * zoom).round() as i32).clamp(1, 16384);

        let mut surface = ImageSurface::create(Format::ARgb32, width, height)
            .context("failed to create render surface")?;
        {
            let ctx = CairoContext::new(&surface)?;
            ctx.set_source_rgb(1.0, 1.0, 1.0);
            ctx.paint()?;
            ctx.scale(zoom, zoom);
            page.render(&ctx);
        }
        surface.flush();

        // Pack rows in case cairo padded the stride.
        let stride = surface.stride() as usize;
        let row_len = width as usize * 4;
        let packed = {
            let data = surface.data().context("surface not writable")?;
            if stride == row_len {
                data.to_vec()
            } else {
                let mut packed = Vec::with_capacity(row_len * height as usize);
                for row in data.chunks(stride) {
                    packed.extend_from_slice(&row[..row_len.min(row.len())]);
                }
                packed
            }
        };

        Ok((width, height, packed))
    }
}

fn glib_uri(path: &Path) -> String {
    match glib::filename_to_uri(path, None) {
        Ok(uri) => uri.into(),
        Err(_) => format!("file://{}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairo::{FontSlant, FontWeight, PdfSurface};

    #[test]
    fn outline_structure() {
        let Ok(doc) = PdfDoc::open(Path::new("/tmp/opencode/real.pdf")) else {
            eprintln!("real.pdf missing, skipping");
            return;
        };
        assert_eq!(doc.n_pages(), 288);

        let toc = doc.toc();
        assert_eq!(toc.len(), 154);

        let summary: Vec<_> = toc
            .iter()
            .map(|e| (e.title.as_str(), e.page, e.depth))
            .collect();
        assert_eq!(summary[0], ("Contents", 1, 0));
        assert_eq!(summary[1], ("Preface", 7, 0));
        assert_eq!(summary[2], ("Overview of this Book", 9, 0));
        assert_eq!(*summary.last().unwrap(), ("Index", 285, 0));

        // Outline walk order is document order: pages are non-decreasing.
        let pages: Vec<usize> = toc.iter().map(|e| e.page).collect();
        assert!(
            pages.windows(2).all(|w| w[0] <= w[1]),
            "outline pages must be monotonic: {pages:?}"
        );
    }

    #[test]
    fn render_page_texture() {
        let Ok(doc) = PdfDoc::open(Path::new("/tmp/opencode/real.pdf")) else {
            eprintln!("real.pdf missing, skipping");
            return;
        };
        eprintln!("rendering...");
        let (width, height, _bytes) = doc.render_page_bytes(0, 1.0).unwrap();
        assert_eq!(width, 595);
        assert_eq!(height, 842);
    }

    #[test]
    fn stress_toc_and_render() {
        for _ in 0..200 {
            let doc = PdfDoc::open(Path::new("/tmp/opencode/real.pdf")).unwrap();
            let toc = doc.toc();
            assert_eq!(toc.len(), 154);
            let _t2 = doc.toc();
            assert_eq!(_t2.len(), 154);
            let _ = doc.render_page_bytes(1, 0.5).unwrap();
            drop(doc);
        }
    }

    /// Build a tiny two-page PDF with known text placement so search
    /// behavior and match coordinates can be asserted deterministically.
    fn write_search_fixture(path: &Path) {
        let surface = PdfSurface::new(595.0, 842.0, path).expect("pdf surface");
        let ctx = CairoContext::new(&surface).expect("cairo context");
        ctx.set_source_rgb(1.0, 1.0, 1.0);
        ctx.paint().unwrap();
        ctx.select_font_face("Sans", FontSlant::Normal, FontWeight::Normal);
        ctx.set_font_size(24.0);
        // Page 1: one match near the top, one near the bottom.
        ctx.move_to(72.0, 100.0);
        ctx.show_text("needle at the top").unwrap();
        ctx.move_to(72.0, 760.0);
        ctx.show_text("another NEEDLE near the bottom").unwrap();
        ctx.show_page().unwrap();
        // Page 2: no matches.
        ctx.move_to(72.0, 100.0);
        ctx.show_text("nothing to find here").unwrap();
        drop(ctx);
        drop(surface); // flushes the file
    }

    #[test]
    fn find_text_finds_and_coordinates_are_top_left() {
        let path = std::env::temp_dir().join("yespanda-search-fixture.pdf");
        write_search_fixture(&path);

        let doc = PdfDoc::open(&path).expect("fixture opens");
        let matches = doc.find_text(0, "needle");
        assert_eq!(matches.len(), 2, "case-insensitive: both spellings found");

        // Document order: the top occurrence comes first, which also pins
        // down that y grows downward from the top-left corner (a
        // bottom-left origin would return ~742 for this rect instead).
        let top = matches[0];
        assert!(
            top.y < 200.0,
            "first match should be near the top, got {top:?}"
        );
        assert!((top.x - 72.0).abs() < 5.0, "x starts at the pen position");
        let bottom = matches[1];
        assert!(bottom.y > 700.0, "second match should be near the bottom");

        // Whole-word containment across a sentence, and page 2 has none.
        assert!(doc.find_text(1, "needle").is_empty());
        assert!(doc.find_text(0, "zzz-absent").is_empty());
        assert!(doc.find_text(0, "").is_empty());

        let _ = std::fs::remove_file(&path);
    }
}
