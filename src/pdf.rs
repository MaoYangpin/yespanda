use std::ffi::CStr;
use std::path::Path;

use anyhow::{Context as _, Result};
use cairo::{Context as CairoContext, Format, ImageSurface};
use poppler::{
    Document,
    ffi::{
        PopplerIndexIter, POPPLER_ACTION_GOTO_DEST, POPPLER_ACTION_NAMED, poppler_action_free,
        poppler_dest_free, poppler_index_iter_free, poppler_index_iter_get_action,
        poppler_index_iter_get_child, poppler_index_iter_new, poppler_index_iter_next,
    },
};
use relm4::gtk::gdk;
use relm4::gtk::glib;
use relm4::gtk::glib::translate::ToGlibPtr;

pub struct PdfDoc {
    doc: Document,
}

#[derive(Clone, Debug)]
pub struct TocEntry {
    pub title: String,
    pub page: usize,
    pub depth: usize,
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

    unsafe fn walk_toc(&self, iter: *mut PopplerIndexIter, depth: usize, entries: &mut Vec<TocEntry>) {
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
                        POPPLER_ACTION_NAMED => {
                            self.resolve_named_dest((*action).named.named_dest)
                        }
                        _ => None,
                    };
                    poppler_action_free(action);

                    entries.push(TocEntry { title, page: page.unwrap_or(0), depth });
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

    pub fn render_page(&self, index: usize, zoom: f64) -> Result<gdk::MemoryTexture> {
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

        Ok(gdk::MemoryTexture::new(
            width,
            height,
            gdk::MemoryFormat::B8g8r8a8Premultiplied,
            &glib::Bytes::from_owned(packed),
            row_len,
        ))
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
    use relm4::gtk::prelude::TextureExt;

    #[test]
    fn outline_structure() {
        let Ok(doc) = PdfDoc::open(Path::new("/tmp/opencode/real.pdf")) else {
            eprintln!("real.pdf missing, skipping");
            return;
        };
        assert_eq!(doc.n_pages(), 288);

        let toc = doc.toc();
        assert_eq!(toc.len(), 154);

        let summary: Vec<_> =
            toc.iter().map(|e| (e.title.as_str(), e.page, e.depth)).collect();
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
        let tex = doc.render_page(0, 1.0).unwrap();
        assert_eq!(tex.width(), 595);
        assert_eq!(tex.height(), 842);
    }

    #[test]
    fn stress_toc_and_render() {
        for _ in 0..200 {
            let doc = PdfDoc::open(Path::new("/tmp/opencode/real.pdf")).unwrap();
            let toc = doc.toc();
            assert_eq!(toc.len(), 154);
            let _t2 = doc.toc();
            assert_eq!(_t2.len(), 154);
            let _ = doc.render_page(1, 0.5).unwrap();
            drop(doc);
        }
    }
}
