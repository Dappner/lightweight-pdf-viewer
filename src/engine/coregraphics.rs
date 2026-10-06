//! The macOS engine: PDF pages are rasterized with CoreGraphics (Preview's engine) and
//! document structure (outline, links) and text are read with PDFKit. Images open as a
//! document of one page, decoded with ImageIO and drawn as they are, not wrapped in a PDF.

use std::marker::PhantomData;
use std::path::Path;

use eframe::egui::{Pos2, Rect, Vec2, pos2, vec2};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CFURL, CGAffineTransform, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo, CGInterpolationQuality, CGPDFBox,
    CGPDFDocument, CGPDFPage, kCGColorSpaceSRGB,
};
use objc2_foundation::{NSRange, NSURL};
use objc2_image_io::{
    CGImageSource, kCGImagePropertyDPIHeight, kCGImagePropertyDPIWidth, kCGImageSourceShouldCacheImmediately,
};
use objc2_pdf_kit::{PDFActionGoTo, PDFActionURL, PDFDestination, PDFDocument, PDFOutline, PDFPage};

use crate::renderer::{Link, OutlineEntry, Target};

/// Runs `f`, releasing what CoreGraphics autoreleased during it.
pub fn autoreleasepool<R>(f: impl FnOnce() -> R) -> R {
    objc2::rc::autoreleasepool(|_| f())
}

/// An open document: a PDF, or an image shown as its only page.
pub struct Document(Source);

enum Source {
    Pdf(Pdf),
    Image(Image),
}

/// One page of an open [`Document`].
pub struct Page<'a>(PageSource<'a>);

enum PageSource<'a> {
    Pdf(PdfPage<'a>),
    Image(&'a Image),
}

impl Document {
    /// Opens `path` as a PDF, or failing that as an image.
    pub fn open(path: &Path) -> Option<Self> {
        Pdf::open(path).map(Source::Pdf).or_else(|| Image::open(path).map(Source::Image)).map(Self)
    }

    pub fn page_count(&self) -> u32 {
        match &self.0 {
            Source::Pdf(pdf) => pdf.page_count(),
            Source::Image(_) => 1,
        }
    }

    /// `index` is zero-based.
    pub fn page(&self, index: u32) -> Option<Page<'_>> {
        match &self.0 {
            Source::Pdf(pdf) => pdf.page(index).map(PageSource::Pdf),
            Source::Image(image) => (index == 0).then_some(PageSource::Image(image)),
        }
        .map(Page)
    }

    pub fn outline(&self) -> Vec<OutlineEntry> {
        match &self.0 {
            Source::Pdf(pdf) => pdf.outline(),
            Source::Image(_) => Vec::new(),
        }
    }
}

impl Page<'_> {
    /// Displayed size in PDF points, after rotation.
    pub fn size(&self) -> Vec2 {
        match &self.0 {
            PageSource::Pdf(page) => page.geometry.size(),
            PageSource::Image(image) => image.size,
        }
    }

    pub fn links(&self) -> Vec<Link> {
        match &self.0 {
            PageSource::Pdf(page) => page.links(),
            PageSource::Image(_) => Vec::new(),
        }
    }
}

/// An image, decoded when opened and kept decoded for as long as the document is open, so
/// draws never go back to the file (which may since have changed or gone).
struct Image {
    image: CFRetained<CGImage>,
    /// In points: pixels at the image's resolution (72 dpi when it gives none), as Preview sizes it.
    size: Vec2,
}

impl Image {
    fn open(path: &Path) -> Option<Self> {
        let url = CFURL::from_file_path(path)?;
        // SAFETY: `url` is a valid file URL, and ImageIO's property dictionary has CFString keys.
        let (image, properties) = unsafe {
            let source = CGImageSource::with_url(&url, None)?;
            let decode_now =
                CFDictionary::from_slices(&[kCGImageSourceShouldCacheImmediately], &[CFBoolean::new(true)]);
            let image = source.image_at_index(0, Some(decode_now.as_ref()))?;
            let properties = source
                .properties_at_index(0, None)
                .map(|p| CFRetained::cast_unchecked::<CFDictionary<CFString, CFType>>(p));
            (image, properties)
        };
        let dpi = |key: &CFString| {
            let number = properties.as_ref()?.get(key)?.downcast::<CFNumber>().ok()?;
            number.as_f64().filter(|dpi| *dpi > 0.0)
        };
        // SAFETY: ImageIO's property keys are immutable statics.
        let (dpi_x, dpi_y) = unsafe { (dpi(kCGImagePropertyDPIWidth), dpi(kCGImagePropertyDPIHeight)) };
        let points = |pixels: usize, dpi: Option<f64>| (pixels as f64 * 72.0 / dpi.unwrap_or(72.0)) as f32;
        let size = vec2(points(CGImage::width(Some(&image)), dpi_x), points(CGImage::height(Some(&image)), dpi_y));
        Some(Self { image, size })
    }
}

/// A PDF: PDFKit for structure, and the CoreGraphics document it wraps for rendering.
/// Closing it (dropping) frees CoreGraphics' per-document caches.
struct Pdf {
    pdf: Retained<PDFDocument>,
    cg: Retained<CGPDFDocument>,
}

struct PdfPage<'a> {
    doc: &'a Pdf,
    index: u32,
    page: CFRetained<CGPDFPage>,
    geometry: PageGeometry,
}

impl Pdf {
    fn open(path: &Path) -> Option<Self> {
        let pdf = open_pdf(path)?;
        // SAFETY: plain PDFKit getter.
        let cg = unsafe { pdf.documentRef() }?;
        (CGPDFDocument::number_of_pages(Some(&cg)) > 0).then_some(Self { pdf, cg })
    }

    fn page_count(&self) -> u32 {
        CGPDFDocument::number_of_pages(Some(&self.cg)) as u32
    }

    /// `index` is zero-based; CoreGraphics numbers pages from 1.
    fn page(&self, index: u32) -> Option<PdfPage<'_>> {
        let page = CGPDFDocument::page(Some(&self.cg), index as usize + 1)?;
        let geometry = PageGeometry::of(&page);
        Some(PdfPage { doc: self, index, page, geometry })
    }

    fn outline(&self) -> Vec<OutlineEntry> {
        fn walk(doc: &Pdf, node: &PDFOutline, depth: usize, out: &mut Vec<OutlineEntry>) {
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

impl PdfPage<'_> {
    fn links(&self) -> Vec<Link> {
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

fn open_pdf(path: &Path) -> Option<Retained<PDFDocument>> {
    let url = NSURL::from_file_path(path)?;
    // SAFETY: `url` is a valid file URL.
    unsafe { PDFDocument::initWithURL(PDFDocument::alloc(), &url) }
}

/// A document opened for its text alone, as the search worker reads it. PDFKit keeps each
/// page it touches parsed until the document is closed (dropped).
pub struct TextDocument {
    pdf: Retained<PDFDocument>,
}

/// One page of a [`TextDocument`].
pub struct TextPage {
    page: Retained<PDFPage>,
    geometry: Option<PageGeometry>,
}

impl TextDocument {
    pub fn open(path: &Path) -> Option<Self> {
        open_pdf(path).map(|pdf| Self { pdf })
    }

    /// Frees the pages PDFKit holds parsed, which it autoreleases.
    pub fn close(self) {
        autoreleasepool(|| drop(self));
    }

    pub fn page(&self, index: u32) -> Option<TextPage> {
        // SAFETY: plain PDFKit getters on objects owned by `self`.
        let page = unsafe { self.pdf.pageAtIndex(index as usize) }?;
        let geometry = unsafe { page.pageRef() }.map(|cg| PageGeometry::of(&cg));
        Some(TextPage { page, geometry })
    }
}

impl TextPage {
    /// The page's text, indexed in UTF-16 code units as the other methods take it.
    pub fn text(&self) -> Option<String> {
        // SAFETY: plain PDFKit getter.
        unsafe { self.page.string() }.map(|s| s.to_string())
    }

    /// The top of character `index`, as a fraction of the displayed page from its top.
    pub fn char_top(&self, index: u32) -> Option<f32> {
        let geometry = self.geometry.as_ref()?;
        // SAFETY: plain PDFKit getter.
        let b = unsafe { self.page.characterBoundsAtIndex(index as isize) };
        let a = geometry.to_fraction(b.origin.x, b.origin.y).y;
        Some(a.min(geometry.to_fraction(b.origin.x + b.size.width, b.origin.y + b.size.height).y))
    }

    /// Rectangles of the text `start..start + len`, one per line, as fractions of the
    /// displayed page with a top-left origin.
    pub fn line_rects(&self, start: u32, len: u32) -> Vec<Rect> {
        let Some(geometry) = &self.geometry else { return vec![] };
        // SAFETY: plain PDFKit getters on objects owned by `self`.
        let Some(selection) = (unsafe { self.page.selectionForRange(NSRange::new(start as usize, len as usize)) })
        else {
            return vec![];
        };
        let lines = unsafe { selection.selectionsByLine() };
        (0..lines.count())
            .map(|line| {
                let b = unsafe { lines.objectAtIndex(line).boundsForPage(&self.page) };
                let corners = [
                    geometry.to_fraction(b.origin.x, b.origin.y),
                    geometry.to_fraction(b.origin.x + b.size.width, b.origin.y + b.size.height),
                ];
                Rect::from_two_pos(corners[0], corners[1])
            })
            .collect()
    }
}

/// Crop box and rotation of a page, which together define what is displayed.
struct PageGeometry {
    crop: CGRect,
    /// Clockwise rotation in degrees: 0, 90, 180 or 270.
    rotation: i32,
}

impl PageGeometry {
    fn of(page: &CGPDFPage) -> Self {
        let crop = CGPDFPage::box_rect(Some(page), CGPDFBox::CropBox);
        let rotation = CGPDFPage::rotation_angle(Some(page)).rem_euclid(360) / 90 * 90;
        Self { crop, rotation }
    }

    fn size(&self) -> Vec2 {
        let (w, h) = (self.crop.size.width as f32, self.crop.size.height as f32);
        if self.rotation % 180 == 90 { vec2(h, w) } else { vec2(w, h) }
    }

    /// A point in PDF user space as fractions of the displayed page, top-left origin.
    fn to_fraction(&self, x: f64, y: f64) -> Pos2 {
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
        // Default, Medium or Low interpolation save at most 5%, and the latter two change
        // pixels; an opaque (NoneSkipLast) context is up to 30% slower (`render_spike`).
        CGContext::set_interpolation_quality(Some(&ctx), CGInterpolationQuality::High);
        CGContext::set_allows_font_smoothing(Some(&ctx), true);
        CGContext::set_should_smooth_fonts(Some(&ctx), true);
        Some(Self { ctx, height: height as f64, _pixels: PhantomData })
    }

    /// Draws the page on a white background, scaled to `page_px` pixels with its top-left
    /// corner at `top_left` (pixels from the bitmap's top-left; CoreGraphics' origin is the
    /// bottom-left).
    pub fn draw(&self, page: &Page, page_px: [f64; 2], top_left: [f64; 2]) {
        let display = page.size();
        let (dw, dh) = (display.x as f64, display.y as f64);
        let c = Some(&*self.ctx);
        CGContext::save_g_state(c);
        CGContext::translate_ctm(c, top_left[0], self.height - top_left[1] - page_px[1]);
        CGContext::scale_ctm(c, page_px[0] / dw, page_px[1] / dh);
        let bounds = CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: dw, height: dh } };
        CGContext::clip_to_rect(c, bounds);
        CGContext::set_rgb_fill_color(c, 1.0, 1.0, 1.0, 1.0);
        CGContext::fill_rect(c, bounds);
        match &page.0 {
            PageSource::Pdf(page) => {
                CGContext::concat_ctm(c, page.geometry.page_to_display());
                CGContext::draw_pdf_page(c, Some(&page.page));
            }
            PageSource::Image(image) => CGContext::draw_image(c, bounds, Some(&image.image)),
        }
        CGContext::restore_g_state(c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: [u8; 4] = [255, 0, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const CLEAR: [u8; 4] = [0, 0, 0, 0];

    /// Writes a 40x20 PNG: red top-left, blue top-right and green bottom quarters, with the
    /// green's last column transparent. `dpi` sets its resolution (pHYs), if any.
    fn quarters_png(name: &str, dpi: Option<f64>) -> std::path::PathBuf {
        let (w, h) = (40, 20);
        let pixels: Vec<u8> = (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .flat_map(|(x, y)| match (x, y) {
                (_, y) if y >= h / 2 && x == w - 1 => CLEAR,
                (_, y) if y >= h / 2 => GREEN,
                (x, _) if x < w / 2 => RED,
                _ => BLUE,
            })
            .collect();
        let path = std::env::temp_dir().join(format!("lightweight-pdf-engine-{}-{name}.png", std::process::id()));
        let mut enc = png::Encoder::new(std::fs::File::create(&path).unwrap(), w, h);
        enc.set_color(png::ColorType::Rgba);
        if let Some(dpi) = dpi {
            let ppm = (dpi / 0.0254).round() as u32;
            enc.set_pixel_dims(Some(png::PixelDimensions { xppu: ppm, yppu: ppm, unit: png::Unit::Meter }));
        }
        enc.write_header().unwrap().write_image_data(&pixels).unwrap();
        path
    }

    fn assert_near(actual: &[u8], expected: [u8; 4], at: (usize, usize)) {
        let close = actual.iter().zip(expected).all(|(a, e)| a.abs_diff(e) <= 3);
        assert!(close, "pixel {at:?} is {actual:?}, expected {expected:?}");
    }

    #[test]
    fn png_opens_as_one_page_of_its_pixel_size() {
        let path = quarters_png("plain", None);
        let doc = Document::open(&path).expect("open");
        let _ = std::fs::remove_file(&path);
        assert_eq!(doc.page_count(), 1);
        assert!(doc.page(1).is_none());
        let page = doc.page(0).unwrap();
        assert_eq!(page.size(), vec2(40.0, 20.0));
        assert!(page.links().is_empty());
        assert!(doc.outline().is_empty());
    }

    #[test]
    fn png_resolution_sets_its_size_in_points() {
        let path = quarters_png("retina", Some(144.0));
        let doc = Document::open(&path).expect("open");
        let _ = std::fs::remove_file(&path);
        let size = doc.page(0).unwrap().size();
        assert!((size - vec2(20.0, 10.0)).length() < 0.01, "size {size:?}");
    }

    #[test]
    fn png_draws_upright_on_white() {
        let path = quarters_png("draw", None);
        let doc = Document::open(&path).expect("open");
        let _ = std::fs::remove_file(&path);
        let (w, h) = (40, 20);
        let mut rgba = vec![0u8; w * h * 4];
        Bitmap::new(&mut rgba, w as u32, h as u32).unwrap().draw(&doc.page(0).unwrap(), [40.0, 20.0], [0.0, 0.0]);
        let pixel = |x: usize, y: usize| &rgba[(y * w + x) * 4..][..4];
        assert_near(pixel(5, 3), RED, (5, 3));
        assert_near(pixel(35, 3), BLUE, (35, 3));
        assert_near(pixel(20, 16), GREEN, (20, 16));
        assert_near(pixel(39, 16), [255, 255, 255, 255], (39, 16));
    }

    #[test]
    fn png_has_no_text_to_search() {
        let path = quarters_png("text", None);
        let text = TextDocument::open(&path);
        let _ = std::fs::remove_file(&path);
        assert!(text.is_none());
    }

    #[test]
    fn other_files_do_not_open() {
        let path = std::env::temp_dir().join(format!("lightweight-pdf-engine-{}-notes.txt", std::process::id()));
        std::fs::write(&path, "not a document").unwrap();
        let doc = Document::open(&path);
        let _ = std::fs::remove_file(&path);
        assert!(doc.is_none());
    }
}
