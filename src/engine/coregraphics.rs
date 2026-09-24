//! The macOS PDF engine: pages are rasterized with CoreGraphics (Preview's engine) and
//! document structure (outline, links) is read with PDFKit.

use std::marker::PhantomData;
use std::path::Path;

use eframe::egui::{Pos2, Rect, Vec2, pos2, vec2};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_core_foundation::{CFRetained, CGAffineTransform, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo, CGInterpolationQuality,
    CGPDFBox, CGPDFDocument, CGPDFPage, kCGColorSpaceSRGB,
};
use objc2_foundation::NSURL;
use objc2_pdf_kit::{PDFActionGoTo, PDFActionURL, PDFDestination, PDFDocument, PDFOutline};

use crate::renderer::{Link, OutlineEntry, Target};

/// An open document: PDFKit for structure, and the CoreGraphics document it wraps for
/// rendering. Closing it (dropping) frees CoreGraphics' per-document caches.
/// Runs `f`, releasing what CoreGraphics autoreleased during it.
pub fn autoreleasepool<R>(f: impl FnOnce() -> R) -> R {
    objc2::rc::autoreleasepool(|_| f())
}

pub struct Document {
    pdf: Retained<PDFDocument>,
    cg: Retained<CGPDFDocument>,
}

/// One page of an open [`Document`].
pub struct Page<'a> {
    doc: &'a Document,
    index: u32,
    page: CFRetained<CGPDFPage>,
    geometry: PageGeometry,
}

impl Document {
    pub fn open(path: &Path) -> Option<Self> {
        let url = NSURL::from_file_path(path)?;
        // SAFETY: `url` is a valid file URL.
        let pdf = unsafe { PDFDocument::initWithURL(PDFDocument::alloc(), &url) }?;
        let cg = unsafe { pdf.documentRef() }?;
        (CGPDFDocument::number_of_pages(Some(&cg)) > 0).then_some(Document { pdf, cg })
    }

    pub fn page_count(&self) -> u32 {
        CGPDFDocument::number_of_pages(Some(&self.cg)) as u32
    }

    /// `index` is zero-based; CoreGraphics numbers pages from 1.
    pub fn page(&self, index: u32) -> Option<Page<'_>> {
        let page = CGPDFDocument::page(Some(&self.cg), index as usize + 1)?;
        let geometry = PageGeometry::of(&page);
        Some(Page { doc: self, index, page, geometry })
    }

    pub fn outline(&self) -> Vec<OutlineEntry> {
        fn walk(doc: &Document, node: &PDFOutline, depth: usize, out: &mut Vec<OutlineEntry>) {
            // SAFETY: plain PDFKit getters on objects owned by `doc`.
            for i in 0..unsafe { node.numberOfChildren() } {
                let Some(child) = (unsafe { node.childAtIndex(i) }) else { continue };
                let title = unsafe { child.label() }.map(|l| l.to_string()).unwrap_or_default();
                let destination = unsafe { child.destination() }.or_else(|| {
                    unsafe { child.action() }?.downcast::<PDFActionGoTo>().ok().map(|g| unsafe { g.destination() })
                });
                let target = destination.and_then(|d| doc.destination_target(&d));
                out.push(OutlineEntry { title: title.trim().to_owned(), depth, target });
                walk(doc, &child, depth + 1, out);
            }
        }
        let mut entries = Vec::new();
        if let Some(root) = unsafe { self.pdf.outlineRoot() } {
            walk(self, &root, 0, &mut entries);
        }
        entries
    }

    /// Resolves a PDFKit destination to a page and vertical position.
    fn destination_target(&self, destination: &PDFDestination) -> Option<Target> {
        // SAFETY: plain PDFKit getters on objects owned by `self`.
        let page = unsafe { destination.page() }?;
        let index = unsafe { self.pdf.indexForPage(&page) } as u32;
        let point = unsafe { destination.point() };
        // PDFKit uses a huge sentinel (kPDFDestinationUnspecifiedValue) for "no position".
        let y = (point.y.abs() < 1e6)
            .then(|| self.page(index).map(|p| p.geometry.to_fraction(point.x.min(1e6), point.y).y))
            .flatten()
            .map(|y| y.clamp(0.0, 1.0));
        Some(Target::Page { page: index, y })
    }
}

impl Page<'_> {
    /// Displayed size in PDF points, after rotation.
    pub fn size(&self) -> Vec2 {
        self.geometry.size()
    }

    pub fn links(&self) -> Vec<Link> {
        let doc = self.doc;
        // SAFETY: plain PDFKit getters on objects owned by `doc`.
        let Some(pdf_page) = (unsafe { doc.pdf.pageAtIndex(self.index as usize) }) else { return vec![] };
        let annotations = unsafe { pdf_page.annotations() };
        annotations
            .iter()
            .filter(|a| unsafe { a.r#type() }.is_some_and(|t| t.to_string() == "Link"))
            .filter_map(|a| {
                let action = unsafe { a.action() };
                let url = unsafe { a.URL() }
                    .or_else(|| action.clone()?.downcast::<PDFActionURL>().ok().and_then(|u| unsafe { u.URL() }));
                let target = match url.and_then(|u| u.absoluteString()) {
                    Some(url) => Target::Url(url.to_string()),
                    None => {
                        let destination = unsafe { a.destination() }
                            .or_else(|| action?.downcast::<PDFActionGoTo>().ok().map(|g| unsafe { g.destination() }))?;
                        doc.destination_target(&destination)?
                    }
                };
                let b = unsafe { a.bounds() };
                let corners = [
                    self.geometry.to_fraction(b.origin.x, b.origin.y),
                    self.geometry.to_fraction(b.origin.x + b.size.width, b.origin.y + b.size.height),
                ];
                Some(Link { rect: Rect::from_two_pos(corners[0], corners[1]), target })
            })
            .collect()
    }
}

/// Crop box and rotation of a page, which together define what is displayed.
pub(crate) struct PageGeometry {
    crop: CGRect,
    /// Clockwise rotation in degrees: 0, 90, 180 or 270.
    rotation: i32,
}

impl PageGeometry {
    pub(crate) fn of(page: &CGPDFPage) -> Self {
        let crop = CGPDFPage::box_rect(Some(page), CGPDFBox::CropBox);
        let rotation = CGPDFPage::rotation_angle(Some(page)).rem_euclid(360) / 90 * 90;
        Self { crop, rotation }
    }

    fn size(&self) -> Vec2 {
        let (w, h) = (self.crop.size.width as f32, self.crop.size.height as f32);
        if self.rotation % 180 == 90 { vec2(h, w) } else { vec2(w, h) }
    }

    /// A point in PDF user space as fractions of the displayed page, top-left origin.
    pub(crate) fn to_fraction(&self, x: f64, y: f64) -> Pos2 {
        let t = self.page_to_display();
        let size = self.size();
        let u = t.a * x + t.c * y + t.tx;
        let v = t.b * x + t.d * y + t.ty;
        pos2(u as f32 / size.x, 1.0 - v as f32 / size.y)
    }

    /// Maps PDF user space onto the displayed page, with the origin at its bottom-left.
    fn page_to_display(&self) -> CGAffineTransform {
        let CGRect { origin: CGPoint { x, y }, size: CGSize { width: w, height: h } } = self.crop;
        let (a, b, c, d, tx, ty) = match self.rotation {
            90 => (0.0, -1.0, 1.0, 0.0, -y, w + x),
            180 => (-1.0, 0.0, 0.0, -1.0, w + x, h + y),
            270 => (0.0, 1.0, -1.0, 0.0, h + y, -x),
            _ => (1.0, 0.0, 0.0, 1.0, -x, -y),
        };
        CGAffineTransform { a, b, c, d, tx, ty }
    }
}

/// Pages drawn into a premultiplied RGBA (sRGB) pixel buffer.
pub struct Bitmap<'a> {
    ctx: CFRetained<CGContext>,
    height: f64,
    /// The context writes into the buffer, so it must not outlive it.
    _pixels: PhantomData<&'a mut [u8]>,
}

impl<'a> Bitmap<'a> {
    pub fn new(rgba: &'a mut [u8], width: u32, height: u32) -> Option<Self> {
        assert_eq!(rgba.len(), width as usize * height as usize * 4);
        let srgb = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
        // SAFETY: `rgba` holds width * height * 4 bytes and, by the lifetime on `Bitmap`,
        // outlives the context.
        let ctx = unsafe {
            CGBitmapContextCreate(
                rgba.as_mut_ptr().cast(),
                width as usize,
                height as usize,
                8,
                width as usize * 4,
                Some(&srgb),
                CGImageAlphaInfo::PremultipliedLast.0,
            )
        }?;
        CGContext::set_interpolation_quality(Some(&ctx), CGInterpolationQuality::High);
        CGContext::set_allows_font_smoothing(Some(&ctx), true);
        CGContext::set_should_smooth_fonts(Some(&ctx), true);
        Some(Self { ctx, height: height as f64, _pixels: PhantomData })
    }

    /// Draws the page on a white background, scaled to `page_px` pixels with its top-left
    /// corner at `top_left` (pixels from the bitmap's top-left; CoreGraphics' origin is the
    /// bottom-left).
    pub fn draw(&self, page: &Page, page_px: [f64; 2], top_left: [f64; 2]) {
        let display = page.geometry.size();
        let (dw, dh) = (display.x as f64, display.y as f64);
        let c = Some(&*self.ctx);
        CGContext::save_g_state(c);
        CGContext::translate_ctm(c, top_left[0], self.height - top_left[1] - page_px[1]);
        CGContext::scale_ctm(c, page_px[0] / dw, page_px[1] / dh);
        let bounds = CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: dw, height: dh } };
        CGContext::clip_to_rect(c, bounds);
        CGContext::set_rgb_fill_color(c, 1.0, 1.0, 1.0, 1.0);
        CGContext::fill_rect(c, bounds);
        CGContext::concat_ctm(c, page.geometry.page_to_display());
        CGContext::draw_pdf_page(c, Some(&page.page));
        CGContext::restore_g_state(c);
    }
}
