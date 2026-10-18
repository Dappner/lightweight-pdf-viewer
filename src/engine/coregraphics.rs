//! The macOS engine: PDF pages are rasterized with CoreGraphics (Preview's engine) and
//! document structure (outline, links), text and other annotation appearances use PDFKit. Text boxes and
//! highlights are read from the page's annotations with CoreGraphics and written back with
//! PDFKit. Images open as
//! a document of one page, decoded with ImageIO and drawn as they are, not wrapped in a PDF.

use std::cell::OnceCell;
use std::ffi::{CStr, c_char};
use std::marker::PhantomData;
use std::path::Path;
use std::ptr::{NonNull, null, null_mut};

use eframe::egui::{Pos2, Rect, Vec2, pos2, vec2};
use objc2::AnyThread;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSApplication, NSColor, NSFont, NSPrintInfo};
use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFMutableData, CFNumber, CFRetained, CFString, CFType, CFURL, CGAffineTransform, CGPoint,
    CGRect, CGSize,
};
use objc2_core_graphics::{
    CGAffineTransformInvert, CGBitmapContextCreate, CGColorSpace, CGContext, CGDataConsumer, CGImage, CGImageAlphaInfo,
    CGInterpolationQuality, CGPDFArray, CGPDFArrayRef, CGPDFBox, CGPDFContextClose, CGPDFContextCreate,
    CGPDFDictionary, CGPDFDictionaryRef, CGPDFDocument, CGPDFPage, CGPDFReal, CGPDFString, CGPDFStringRef,
    kCGColorSpaceSRGB,
};
use objc2_foundation::{NSArray, NSData, NSPoint, NSRange, NSString, NSURL, NSValue};
use objc2_image_io::{
    CGImageSource, kCGImagePropertyDPIHeight, kCGImagePropertyDPIWidth, kCGImagePropertyOrientation,
    kCGImageSourceShouldCacheImmediately,
};
use objc2_pdf_kit::{
    PDFActionGoTo, PDFActionURL, PDFAnnotation, PDFAnnotationSubtypeFreeText, PDFAnnotationSubtypeHighlight,
    PDFDestination, PDFDisplayBox, PDFDocument, PDFOutline, PDFPage, PDFPrintScalingMode,
};

use super::preview::{self, EditedBox};
use crate::layout::{Rotation, Rotations};
use crate::renderer::{Link, OutlineEntry, Target};
use crate::text_boxes::{self, Highlight, PageSave, TextBox};

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
    /// Opens `path` as a PDF, or failing that as an image, with its pages turned by `rotations`.
    pub fn open(path: &Path, rotations: &Rotations) -> Option<Self> {
        Pdf::open(path, rotations)
            .map(Source::Pdf)
            .or_else(|| Image::open(path, rotations.of(0)).map(Source::Image))
            .map(Self)
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
            PageSource::Image(image) => image.geometry.size(),
        }
    }

    pub fn links(&self) -> Vec<Link> {
        match &self.0 {
            PageSource::Pdf(page) => page.links(),
            PageSource::Image(_) => Vec::new(),
        }
    }

    pub fn text_boxes(&self) -> Vec<TextBox> {
        match &self.0 {
            PageSource::Pdf(page) => page.text_boxes(),
            PageSource::Image(_) => Vec::new(),
        }
    }

    pub fn highlights(&self) -> Vec<Highlight> {
        match &self.0 {
            PageSource::Pdf(page) => page.highlights(),
            PageSource::Image(_) => Vec::new(),
        }
    }
}

/// An image, decoded when opened and kept decoded for as long as the document is open, so
/// draws never go back to the file (which may since have changed or gone).
struct Image {
    image: CFRetained<CGImage>,
    /// The image as a page: its size in points (pixels at the image's resolution, 72 dpi when
    /// it gives none, as Preview sizes it), turned as its EXIF orientation says and then by
    /// the document's rotation.
    geometry: PageGeometry,
    /// Flipped left to right before turning, for the mirrored EXIF orientations.
    mirrored: bool,
}

impl Image {
    fn open(path: &Path, rotation: Rotation) -> Option<Self> {
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
        let number = |key: &CFString| properties.as_ref()?.get(key)?.downcast::<CFNumber>().ok()?.as_f64();
        // SAFETY: ImageIO's property keys are immutable statics.
        let (dpi_x, dpi_y, orientation) = unsafe {
            (number(kCGImagePropertyDPIWidth), number(kCGImagePropertyDPIHeight), number(kCGImagePropertyOrientation))
        };
        let points = |pixels: usize, dpi: Option<f64>| pixels as f64 * 72.0 / dpi.filter(|d| *d > 0.0).unwrap_or(72.0);
        let size = CGSize {
            width: points(CGImage::width(Some(&image)), dpi_x),
            height: points(CGImage::height(Some(&image)), dpi_y),
        };
        // EXIF orientations 1-8: how the stored pixels are turned (clockwise) and flipped to show.
        let (turn, mirrored) = match orientation.map(|o| o as i32) {
            Some(2) => (0, true),
            Some(3) => (180, false),
            Some(4) => (180, true),
            Some(5) => (270, true),
            Some(6) => (90, false),
            Some(7) => (90, true),
            Some(8) => (270, false),
            _ => (0, false),
        };
        let crop = CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size };
        let geometry = PageGeometry { crop, rotation: (turn + rotation.degrees()) % 360 };
        Some(Self { image, geometry, mirrored })
    }

    /// Draws the image as its page shows it, in a context whose origin is the page's
    /// bottom-left corner, one unit a point.
    fn draw(&self, ctx: &CGContext) {
        let c = Some(ctx);
        CGContext::concat_ctm(c, self.geometry.page_to_display());
        let crop = self.geometry.crop;
        if self.mirrored {
            CGContext::translate_ctm(c, crop.size.width, 0.0);
            CGContext::scale_ctm(c, -1.0, 1.0);
        }
        CGContext::draw_image(c, crop, Some(&self.image));
    }

    /// The image as a one-page PDF, in memory, as its page shows it.
    fn to_pdf(&self) -> Option<Retained<PDFDocument>> {
        let size = self.geometry.size();
        let media =
            CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: size.x as f64, height: size.y as f64 } };
        let data = CFMutableData::new(None, 0)?;
        let consumer = CGDataConsumer::with_cf_data(Some(&data))?;
        // SAFETY: `media` outlives the calls that read it.
        let ctx = unsafe { CGPDFContextCreate(Some(&consumer), &media, None) }?;
        unsafe { CGContext::begin_page(Some(&ctx), &media) };
        self.draw(&ctx);
        CGContext::end_page(Some(&ctx));
        CGPDFContextClose(Some(&ctx));
        // SAFETY: CFData is toll-free bridged with NSData.
        let data: &NSData = unsafe { &*(&*data as *const CFMutableData).cast() };
        unsafe { PDFDocument::initWithData(PDFDocument::alloc(), data) }
    }
}

/// A PDF: PDFKit for structure, and the CoreGraphics document it wraps for rendering.
/// Closing it (dropping) frees CoreGraphics' per-document caches.
struct Pdf {
    pdf: Retained<PDFDocument>,
    cg: Retained<CGPDFDocument>,
    rotations: Rotations,
}

struct PdfPage<'a> {
    doc: &'a Pdf,
    index: u32,
    page: CFRetained<CGPDFPage>,
    geometry: PageGeometry,
    drawn_annotations: OnceCell<Vec<Retained<PDFAnnotation>>>,
}

impl Pdf {
    fn open(path: &Path, rotations: &Rotations) -> Option<Self> {
        let pdf = open_pdf(path)?;
        // SAFETY: plain PDFKit getter.
        let cg = unsafe { pdf.documentRef() }?;
        (CGPDFDocument::number_of_pages(Some(&cg)) > 0).then_some(Self { pdf, cg, rotations: rotations.clone() })
    }

    fn page_count(&self) -> u32 {
        CGPDFDocument::number_of_pages(Some(&self.cg)) as u32
    }

    /// `index` is zero-based; CoreGraphics numbers pages from 1.
    fn page(&self, index: u32) -> Option<PdfPage<'_>> {
        let page = CGPDFDocument::page(Some(&self.cg), index as usize + 1)?;
        let geometry = PageGeometry::of(&page, self.rotations.of(index));
        Some(PdfPage { doc: self, index, page, geometry, drawn_annotations: OnceCell::new() })
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
    /// CoreGraphics draws page content only. PDFKit draws the remaining annotation
    /// appearances; FreeText and Highlight are drawn by editable UI overlays instead.
    fn draw_annotations(&self, ctx: &CGContext) {
        let annotations = self.drawn_annotations.get_or_init(|| {
            let mut raw = null_mut();
            let dict = CGPDFPage::dictionary(Some(&self.page));
            // SAFETY: dictionaries and arrays are owned by this page; out-pointers are valid.
            let has_raw = !dict.is_null() && unsafe { CGPDFDictionary::array(dict, key(c"Annots"), &mut raw) };
            if !has_raw {
                return Vec::new();
            }
            // SAFETY: PDFKit getters on the document owned by this worker. Pages without
            // annotations stay entirely on the CoreGraphics rendering path.
            let Some(page) = (unsafe { self.doc.pdf.pageAtIndex(self.index as usize) }) else {
                return Vec::new();
            };
            unsafe { page.annotations() }
                .iter()
                .enumerate()
                .filter(|(i, a)| {
                    let mut annotation = null_mut();
                    let mut flags = 0;
                    if has_raw && unsafe { CGPDFArray::dictionary(raw, *i, &mut annotation) } {
                        unsafe { CGPDFDictionary::integer(annotation, key(c"F"), &mut flags) };
                    }
                    // Invisible, Hidden and NoView flags must also be respected: PDFKit's
                    // shouldDisplay getter alone does not exclude every hidden appearance.
                    flags & (1 | 2 | 32) == 0
                        && (unsafe { a.shouldDisplay() })
                        && !unsafe { a.r#type() }
                            .is_some_and(|t| matches!(t.to_string().as_str(), "FreeText" | "Highlight"))
                })
                .map(|(_, a)| a)
                .collect()
        });
        if annotations.is_empty() {
            return;
        }
        // PDFKit already applies the page's crop and intrinsic rotation. Remove the
        // PDF user-space transform and apply only the reader's additional rotation.
        let own = PageGeometry::of(&self.page, Rotation::default());
        let size = own.size();
        let reader = PageGeometry {
            crop: CGRect {
                origin: CGPoint { x: 0.0, y: 0.0 },
                size: CGSize { width: size.x as f64, height: size.y as f64 },
            },
            rotation: (self.geometry.rotation - own.rotation).rem_euclid(360),
        };
        CGContext::save_g_state(Some(ctx));
        CGContext::concat_ctm(Some(ctx), CGAffineTransformInvert(self.geometry.page_to_display()));
        CGContext::concat_ctm(Some(ctx), reader.page_to_display());
        for annotation in annotations {
            CGContext::save_g_state(Some(ctx));
            // SAFETY: the annotation and context belong to this render job.
            unsafe { annotation.drawWithBox_inContext(PDFDisplayBox::CropBox, ctx) };
            CGContext::restore_g_state(Some(ctx));
        }
        CGContext::restore_g_state(Some(ctx));
    }

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

impl PdfPage<'_> {
    /// The page's annotations of `subtype`, in the file's order: dictionaries owned by the
    /// page.
    fn annotations(&self, subtype: &str) -> Vec<CGPDFDictionaryRef> {
        let page = CGPDFPage::dictionary(Some(&self.page));
        let mut annots: CGPDFArrayRef = null_mut();
        // SAFETY: `page` is the page's dictionary or null, and the out-pointers are valid.
        if page.is_null() || !unsafe { CGPDFDictionary::array(page, key(c"Annots"), &mut annots) } {
            return Vec::new();
        }
        // SAFETY: `annots` is a valid array owned by the page, and the out-pointers are valid.
        (0..unsafe { CGPDFArray::count(annots) })
            .filter_map(|i| {
                let mut annot: CGPDFDictionaryRef = null_mut();
                (unsafe { CGPDFArray::dictionary(annots, i, &mut annot) }
                    && unsafe { name(annot, c"Subtype") }.as_deref() == Some(subtype))
                .then_some(annot)
            })
            .collect()
    }

    /// The page's FreeText annotations, read from its dictionary: PDFKit doesn't read back
    /// the font size and colour it writes (it saves them as a name, not a string).
    fn text_boxes(&self) -> Vec<TextBox> {
        // Positions are on the page as the file turns it, before the reader's rotation.
        let geometry = PageGeometry::of(&self.page, Rotation::default());
        let mut boxes = Vec::new();
        for (origin, annot) in self.annotations("FreeText").into_iter().enumerate() {
            let origin = origin as u32;
            // SAFETY: `annot` is a valid dictionary owned by the page.
            let Some([x0, y0, x1, y1]) = (unsafe { numbers::<4>(annot, c"Rect") }) else { continue };
            let corners = Rect::from_two_pos(geometry.to_fraction(x0, y0), geometry.to_fraction(x1, y1));
            let contents = unsafe { text(annot, c"Contents") }.unwrap_or_default();
            let appearance = unsafe { text(annot, c"DA").or_else(|| name(annot, c"DA")) }.unwrap_or_default();
            let (size, color) = default_appearance(&appearance);
            boxes.push(TextBox {
                text: contents.replace('\r', "\n"),
                at: [corners.min.x, corners.min.y],
                size,
                color,
                width: Some(corners.width() * geometry.size().x),
                origin: Some(origin),
                changed: false,
            });
        }
        boxes
    }

    /// The page's Highlight annotations: a rect per quadrilateral, or the annotation's rect
    /// when it has none.
    fn highlights(&self) -> Vec<Highlight> {
        let geometry = PageGeometry::of(&self.page, Rotation::default());
        let mut highlights = Vec::new();
        for (origin, annot) in self.annotations("Highlight").into_iter().enumerate() {
            // SAFETY: `annot` is a valid dictionary owned by the page.
            let quads = unsafe { all_numbers(annot, c"QuadPoints") }
                .filter(|q| !q.is_empty() && q.len() % 8 == 0)
                .or_else(|| {
                    let [x0, y0, x1, y1] = unsafe { numbers::<4>(annot, c"Rect") }?;
                    Some(vec![x0, y1, x1, y1, x0, y0, x1, y0])
                });
            let Some(quads) = quads else { continue };
            let rects = quads.chunks(8).map(|q| {
                let corners: Vec<Pos2> = q.chunks(2).map(|p| geometry.to_fraction(p[0], p[1])).collect();
                Rect::from_points(&corners)
            });
            let color = match unsafe { all_numbers(annot, c"C") }.as_deref() {
                Some(&[r, g, b]) => [r, g, b].map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8),
                _ => text_boxes::HIGHLIGHT_YELLOW,
            };
            highlights.push(Highlight { color, origin: Some(origin as u32), ..Highlight::new(rects) });
        }
        highlights
    }
}

/// A dictionary key, for the CGPDF getters.
fn key(k: &CStr) -> NonNull<c_char> {
    NonNull::from(k).cast()
}

/// # Safety
/// `dict` must be a valid dictionary.
unsafe fn name(dict: CGPDFDictionaryRef, k: &CStr) -> Option<String> {
    let mut value: *const c_char = null();
    // SAFETY: the out-pointer is valid; CoreGraphics returns a NUL-terminated name.
    unsafe { CGPDFDictionary::name(dict, key(k), &mut value) && !value.is_null() }
        .then(|| unescape_name(unsafe { CStr::from_ptr(value) }.to_bytes()))
}

/// A name with its `#xx` escapes decoded, which CoreGraphics leaves in.
fn unescape_name(name: &[u8]) -> String {
    let mut out = Vec::with_capacity(name.len());
    let mut i = 0;
    while i < name.len() {
        let hex = name.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
        match (name[i], hex) {
            (b'#', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// # Safety
/// `dict` must be a valid dictionary.
unsafe fn text(dict: CGPDFDictionaryRef, k: &CStr) -> Option<String> {
    let mut value: CGPDFStringRef = null_mut();
    // SAFETY: the out-pointer is valid.
    if !unsafe { CGPDFDictionary::string(dict, key(k), &mut value) } {
        return None;
    }
    unsafe { CGPDFString::text_string(value) }.map(|s| s.to_string())
}

/// An array of `N` numbers.
///
/// # Safety
/// `dict` must be a valid dictionary.
unsafe fn numbers<const N: usize>(dict: CGPDFDictionaryRef, k: &CStr) -> Option<[f64; N]> {
    unsafe { all_numbers(dict, k) }?.try_into().ok()
}

/// An array of numbers.
///
/// # Safety
/// `dict` must be a valid dictionary.
unsafe fn all_numbers(dict: CGPDFDictionaryRef, k: &CStr) -> Option<Vec<f64>> {
    let mut array: CGPDFArrayRef = null_mut();
    // SAFETY: the out-pointers are valid, and only `array`'s entries are read.
    if !unsafe { CGPDFDictionary::array(dict, key(k), &mut array) } {
        return None;
    }
    (0..unsafe { CGPDFArray::count(array) })
        .map(|i| {
            let mut value: CGPDFReal = 0.0;
            unsafe { CGPDFArray::number(array, i, &mut value) }.then_some(value)
        })
        .collect()
}

/// Font size and colour from a FreeText's default appearance string, such as
/// `/Helv 12 Tf 0 0 1 rg`. Black at the default size for what it doesn't say.
fn default_appearance(da: &str) -> (f32, [u8; 3]) {
    let tokens: Vec<&str> = da.split_whitespace().collect();
    let number = |i: usize| tokens.get(i).and_then(|t| t.parse::<f32>().ok());
    let mut size = text_boxes::DEFAULT_SIZE;
    let mut color = [0.0f32; 3];
    for (i, &op) in tokens.iter().enumerate() {
        match op {
            "Tf" => size = i.checked_sub(1).and_then(number).filter(|s| *s > 0.0).unwrap_or(size),
            "g" if i >= 1 => color = [number(i - 1).unwrap_or(0.0); 3],
            "rg" if i >= 3 => color = [0, 1, 2].map(|k| number(i - 3 + k).unwrap_or(0.0)),
            "k" if i >= 4 => {
                let [c, m, y, k] = [0, 1, 2, 3].map(|j| number(i - 4 + j).unwrap_or(0.0));
                color = [c, m, y].map(|v| (1.0 - v) * (1.0 - k));
            }
            _ => {}
        }
    }
    (size, color.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8))
}

/// Writes text boxes and highlights into the PDF at `path`: changes and removes the file's
/// own as `pages` says, and adds the new ones. Writes a copy and moves it over the file, so a failed save
/// leaves the file as it was. PDFKit rewrites the whole document, so this can take seconds.
pub fn save_annotations(path: &Path, pages: &[PageSave]) -> Result<super::SaveOutcome, String> {
    autoreleasepool(|| {
        let pdf = open_pdf(path).ok_or("The file couldn't be opened")?;
        let (edited_boxes, identified) = apply_edits(&pdf, pages);
        let name = path.file_name().ok_or("Not a file")?.to_string_lossy();
        let temp = path.with_file_name(format!(".{name}.saving"));
        let url = NSURL::from_file_path(&temp).ok_or("Bad file name")?;
        // SAFETY: plain PDFKit call.
        if !unsafe { pdf.writeToURL(&url) } {
            let _ = std::fs::remove_file(&temp);
            return Err("PDFKit couldn't write the file".into());
        }
        let outcome =
            if identified { preview::apply(&temp, &edited_boxes) } else { super::SaveOutcome::PreviewMayResize };
        if let Ok(meta) = std::fs::metadata(path) {
            let _ = std::fs::set_permissions(&temp, meta.permissions());
        }
        std::fs::rename(&temp, path).map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            format!("The file couldn't be replaced: {e}")
        })?;
        Ok(outcome)
    })
}

/// Shows the print sheet on the key window for the PDF or image at `path` as its tab shows
/// it: pages turned by `rotations`, with the edits in `pages` not yet saved. The sheet runs on its own;
/// the print operation keeps the document alive until it's done.
pub fn print(path: &Path, rotations: &Rotations, pages: &[PageSave]) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("Printing needs the main thread")?;
    let window = NSApplication::sharedApplication(mtm).keyWindow().ok_or("No window to print from")?;
    autoreleasepool(|| {
        let pdf = match open_pdf(path) {
            Some(pdf) => {
                apply_edits(&pdf, pages);
                // SAFETY: plain PDFKit calls on objects owned by `pdf`.
                for i in 0..unsafe { pdf.pageCount() } {
                    let turn = rotations.of(i as u32).degrees() as isize;
                    if turn != 0
                        && let Some(page) = unsafe { pdf.pageAtIndex(i) }
                    {
                        unsafe { page.setRotation((page.rotation() + turn).rem_euclid(360)) };
                    }
                }
                pdf
            }
            // Images draw turned as they show, so they need no rotation of their own.
            None => Image::open(path, rotations.of(0))
                .and_then(|image| image.to_pdf())
                .ok_or("The file couldn't be opened")?,
        };
        let info = NSPrintInfo::sharedPrintInfo();
        // SAFETY: plain PDFKit and AppKit calls on the main thread.
        let operation = unsafe {
            pdf.printOperationForPrintInfo_scalingMode_autoRotate(
                Some(&info),
                PDFPrintScalingMode::PageScaleDownToFit,
                true,
                mtm,
            )
        }
        .ok_or("PDFKit couldn't print the file")?;
        let title = path.file_stem().map(|n| NSString::from_str(&n.to_string_lossy()));
        operation.setJobTitle(title.as_deref());
        // SAFETY: no delegate, so no selector or context.
        unsafe {
            operation.runOperationModalForWindow_delegate_didRunSelector_contextInfo(&window, None, None, null_mut())
        };
        Ok(())
    })
}

/// Writes the edits in `pages` into `pdf`, in memory. Returns the boxes it styled, and whether
/// each could be told apart in the file's FreeText order (see [`preview::apply`]).
fn apply_edits(pdf: &PDFDocument, pages: &[PageSave]) -> (Vec<EditedBox>, bool) {
    let mut edited_boxes = Vec::new();
    let mut identified = true;
    for save in pages {
        // SAFETY: plain PDFKit calls on objects owned by `pdf`.
        let Some(page) = (unsafe { pdf.pageAtIndex(save.page as usize) }) else { continue };
        let Some(cg) = (unsafe { page.pageRef() }) else { continue };
        let geometry = PageGeometry::of(&cg, Rotation::default());
        let mut styled = Vec::new();
        let theirs: Vec<Retained<PDFAnnotation>> = unsafe { page.annotations() }
            .iter()
            .filter(|a| unsafe { a.r#type() }.is_some_and(|t| t.to_string() == "FreeText"))
            .collect();
        for (ordinal, annotation) in theirs.iter().enumerate() {
            match save.boxes.iter().find(|(b, _)| b.origin == Some(ordinal as u32)) {
                Some((b, rect)) if b.changed => {
                    let bounds = geometry.fraction_rect_to_pdf(*rect);
                    style(annotation, b, bounds);
                    styled.push((annotation.clone(), b.size, bounds));
                }
                Some(_) => {}
                None if save.complete => unsafe { page.removeAnnotation(annotation) },
                None => {}
            }
        }
        for (b, rect) in save.boxes.iter().filter(|(b, _)| b.origin.is_none()) {
            let bounds = geometry.fraction_rect_to_pdf(*rect);
            // SAFETY: a new FreeText annotation, added to a page of `pdf`.
            let annotation = unsafe {
                PDFAnnotation::initWithBounds_forType_withProperties(
                    PDFAnnotation::alloc(),
                    bounds,
                    PDFAnnotationSubtypeFreeText,
                    None,
                )
            };
            style(&annotation, b, bounds);
            unsafe { page.addAnnotation(&annotation) };
            styled.push((annotation, b.size, bounds));
        }
        // Deletions shift ordinals, so record identity after all page edits, in the
        // same FreeText order the saved file exposes. Duplicate text is harmless.
        for (ordinal, annotation) in unsafe { page.annotations() }
            .iter()
            .filter(|a| unsafe { a.r#type() }.is_some_and(|t| t.to_string() == "FreeText"))
            .enumerate()
        {
            if let Some((_, size, bounds)) = styled.iter().find(|(a, _, _)| std::ptr::eq(&**a, &*annotation)) {
                edited_boxes.push(EditedBox {
                    page: save.page,
                    ordinal,
                    size: *size,
                    bounds: [
                        bounds.origin.x,
                        bounds.origin.y,
                        bounds.origin.x + bounds.size.width,
                        bounds.origin.y + bounds.size.height,
                    ],
                });
            }
        }
        if edited_boxes.iter().filter(|b| b.page == save.page).count() != styled.len() {
            identified = false;
        }
        save_highlights(&page, &geometry, save);
    }
    (edited_boxes, identified)
}

/// Removes the page's highlights `save` no longer has and adds its new ones.
fn save_highlights(page: &PDFPage, geometry: &PageGeometry, save: &PageSave) {
    // SAFETY: plain PDFKit and AppKit calls on objects this thread owns.
    let theirs: Vec<Retained<PDFAnnotation>> = unsafe { page.annotations() }
        .iter()
        .filter(|a| unsafe { a.r#type() }.is_some_and(|t| t.to_string() == "Highlight"))
        .collect();
    for (ordinal, annotation) in theirs.iter().enumerate() {
        if save.highlights_complete && !save.highlights.iter().any(|h| h.origin == Some(ordinal as u32)) {
            unsafe { page.removeAnnotation(annotation) };
        }
    }
    for h in save.highlights.iter().filter(|h| h.origin.is_none()) {
        let quads: Vec<CGRect> = h.rects().map(|r| geometry.fraction_rect_to_pdf(r)).collect();
        let Some(bounds) = quads.iter().copied().reduce(|a, b| {
            let (x0, y0) = (a.origin.x.min(b.origin.x), a.origin.y.min(b.origin.y));
            let x1 = (a.origin.x + a.size.width).max(b.origin.x + b.size.width);
            let y1 = (a.origin.y + a.size.height).max(b.origin.y + b.size.height);
            CGRect { origin: CGPoint { x: x0, y: y0 }, size: CGSize { width: x1 - x0, height: y1 - y0 } }
        }) else {
            continue;
        };
        // Each quadrilateral's top-left, top-right, bottom-left and bottom-right corners, from
        // the bounds' origin, as PDFKit takes them.
        let points: Vec<Retained<NSValue>> = quads
            .iter()
            .flat_map(|q| {
                let (x0, y0) = (q.origin.x - bounds.origin.x, q.origin.y - bounds.origin.y);
                let (x1, y1) = (x0 + q.size.width, y0 + q.size.height);
                [(x0, y1), (x1, y1), (x0, y0), (x1, y0)]
            })
            .map(|(x, y)| unsafe { NSValue::valueWithPoint(NSPoint { x, y }) })
            .collect();
        let [r, g, b] = h.color.map(|v| v as f64 / 255.0);
        unsafe {
            let annotation = PDFAnnotation::initWithBounds_forType_withProperties(
                PDFAnnotation::alloc(),
                bounds,
                PDFAnnotationSubtypeHighlight,
                None,
            );
            annotation.setQuadrilateralPoints(Some(&NSArray::from_retained_slice(&points)));
            annotation.setColor(&NSColor::colorWithCalibratedRed_green_blue_alpha(r, g, b, 1.0));
            annotation.setShouldPrint(true);
            page.addAnnotation(&annotation);
        }
    }
}

/// Sets a FreeText annotation to show `b` in `bounds` (PDF user space).
fn style(annotation: &PDFAnnotation, b: &TextBox, bounds: CGRect) {
    // Generic (calibrated) RGB is the one colour space PDFKit writes into the appearance
    // string as given; sRGB and device RGB it converts first.
    let [r, g, bl] = b.color.map(|v| v as f64 / 255.0);
    // SAFETY: plain PDFKit and AppKit calls on objects this thread owns.
    unsafe {
        annotation.setBounds(bounds);
        annotation.setContents(Some(&NSString::from_str(&b.text)));
        let font = NSFont::fontWithName_size(&NSString::from_str(text_boxes::FONT), b.size as f64);
        annotation.setFont(font.as_deref());
        annotation.setFontColor(Some(&NSColor::colorWithCalibratedRed_green_blue_alpha(r, g, bl, 1.0)));
        // Without this PDFKit fills the box yellow.
        annotation.setColor(&NSColor::clearColor());
        annotation.setShouldPrint(true);
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
    rotations: Rotations,
}

/// One page of a [`TextDocument`].
pub struct TextPage {
    page: Retained<PDFPage>,
    geometry: Option<PageGeometry>,
}

impl TextDocument {
    /// Opens `path` with its pages turned by `rotations`, as [`Document::open`] does.
    pub fn open(path: &Path, rotations: &Rotations) -> Option<Self> {
        open_pdf(path).map(|pdf| Self { pdf, rotations: rotations.clone() })
    }

    /// Frees the pages PDFKit holds parsed, which it autoreleases.
    pub fn close(self) {
        autoreleasepool(|| drop(self));
    }

    pub fn page(&self, index: u32) -> Option<TextPage> {
        // SAFETY: plain PDFKit getters on objects owned by `self`.
        let page = unsafe { self.pdf.pageAtIndex(index as usize) }?;
        let geometry = unsafe { page.pageRef() }.map(|cg| PageGeometry::of(&cg, self.rotations.of(index)));
        Some(TextPage { page, geometry })
    }
}

impl TextPage {
    /// Total displayed rotation, including the page's intrinsic PDF rotation.
    pub fn rotation(&self) -> Rotation {
        let degrees = self.geometry.as_ref().map_or(0, |g| g.rotation);
        (0..degrees / 90).fold(Rotation::default(), |r, _| r.clockwise())
    }

    /// The page's text, indexed in UTF-16 code units as the other methods take it.
    pub fn text(&self) -> Option<String> {
        // SAFETY: plain PDFKit getter.
        unsafe { self.page.string() }.map(|s| s.to_string())
    }

    /// The page's text as UTF-16 units, each with its box as fractions of the displayed page
    /// with a top-left origin; [`Rect::NOTHING`] for units without one. Both units of a
    /// surrogate pair get their character's box.
    pub fn boxes(&self) -> (Vec<u16>, Vec<Rect>) {
        // SAFETY: plain PDFKit getter.
        let Some(text) = (unsafe { self.page.string() }) else { return (vec![], vec![]) };
        let units: Vec<u16> = text.to_string().encode_utf16().collect();
        let Some(geometry) = &self.geometry else { return (vec![], vec![]) };
        let mut rects = Vec::with_capacity(units.len());
        while rects.len() < units.len() {
            let len = char_len(&units, rects.len());
            let rect = self.text_box(geometry, rects.len(), len).unwrap_or(Rect::NOTHING);
            rects.extend(std::iter::repeat_n(rect, len));
        }
        (units, rects)
    }

    /// The top of the character starting at UTF-16 unit `index`, as a fraction of the
    /// displayed page from its top.
    pub fn char_top(&self, index: u32) -> Option<f32> {
        let geometry = self.geometry.as_ref()?;
        // SAFETY: plain PDFKit getter.
        let text = unsafe { self.page.string() }?;
        let index = index as usize;
        let next = |i: usize| (i < text.length()).then(|| text.characterAtIndex(i));
        let len = if next(index).is_some_and(is_high) && next(index + 1).is_some_and(is_low) { 2 } else { 1 };
        Some(self.text_box(geometry, index, len)?.top())
    }

    /// The box of the page string's UTF-16 units `start..start + len`, as fractions of the
    /// displayed page with a top-left origin; `None` if they have none.
    ///
    /// From a selection of the units, about 1.5 µs a character, not from
    /// `characterBoundsAtIndex`: that counts glyphs, not units of the page string, skipping
    /// the line breaks PDFKit inserts and counting a ligature such as "fi" once, so its boxes
    /// drift further from the text with every line.
    fn text_box(&self, geometry: &PageGeometry, start: usize, len: usize) -> Option<Rect> {
        // SAFETY: plain PDFKit getters on objects owned by `self`.
        let selection = unsafe { self.page.selectionForRange(NSRange::new(start, len)) }?;
        let b = unsafe { selection.boundsForPage(&self.page) };
        (b.size.width > 0.0 && b.size.height > 0.0).then(|| {
            Rect::from_two_pos(
                geometry.to_fraction(b.origin.x, b.origin.y),
                geometry.to_fraction(b.origin.x + b.size.width, b.origin.y + b.size.height),
            )
        })
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

/// UTF-16 units in the character starting at `units[i]`: 2 for a surrogate pair, else 1.
fn char_len(units: &[u16], i: usize) -> usize {
    if is_high(units[i]) && units.get(i + 1).copied().is_some_and(is_low) { 2 } else { 1 }
}

fn is_high(unit: u16) -> bool {
    (0xD800..0xDC00).contains(&unit)
}

fn is_low(unit: u16) -> bool {
    (0xDC00..0xE000).contains(&unit)
}

/// Crop box and rotation of a page, which together define what is displayed.
struct PageGeometry {
    crop: CGRect,
    /// Clockwise rotation in degrees: 0, 90, 180 or 270.
    rotation: i32,
}

impl PageGeometry {
    /// The page as displayed: its own rotation, then `rotation`.
    fn of(page: &CGPDFPage, rotation: Rotation) -> Self {
        let crop = CGPDFPage::box_rect(Some(page), CGPDFBox::CropBox);
        let own = CGPDFPage::rotation_angle(Some(page)).rem_euclid(360) / 90 * 90;
        Self { crop, rotation: (own + rotation.degrees()) % 360 }
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

    /// The inverse of [`Self::to_fraction`], for a rect: PDF user space.
    fn fraction_rect_to_pdf(&self, rect: Rect) -> CGRect {
        let t = CGAffineTransformInvert(self.page_to_display());
        let size = self.size();
        let point = |p: Pos2| {
            let (u, v) = (p.x as f64 * size.x as f64, (1.0 - p.y as f64) * size.y as f64);
            (t.a * u + t.c * v + t.tx, t.b * u + t.d * v + t.ty)
        };
        let (x0, y0) = point(rect.min);
        let (x1, y1) = point(rect.max);
        CGRect {
            origin: CGPoint { x: x0.min(x1), y: y0.min(y1) },
            size: CGSize { width: (x1 - x0).abs(), height: (y1 - y0).abs() },
        }
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
                page.draw_annotations(&self.ctx);
            }
            PageSource::Image(image) => image.draw(&self.ctx),
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
    /// green's last column transparent. `dpi` sets its resolution (pHYs), and `orientation`
    /// its EXIF orientation (eXIf), if any.
    fn quarters_png(name: &str, dpi: Option<f64>, orientation: Option<u8>) -> std::path::PathBuf {
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
        // A big-endian TIFF header and one IFD holding only the Orientation tag (0x0112, a SHORT).
        let exif = orientation.map(|o| {
            let mut exif = b"MM\0*\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0".to_vec();
            exif.extend([o, 0, 0, 0, 0, 0, 0]);
            exif
        });
        let mut info = png::Info::with_size(w, h);
        info.exif_metadata = exif.map(Into::into);
        let mut enc = png::Encoder::with_info(std::fs::File::create(&path).unwrap(), info).unwrap();
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

    /// Every page turned a quarter, clockwise or not.
    fn turned_all(clockwise: bool) -> Rotations {
        let mut rotations = Rotations::default();
        rotations.turn_all(clockwise);
        rotations
    }

    /// Opens `path` turned by `rotations` and draws its first page at one pixel a point,
    /// returning the pixel at each of `at` (x, y from the top-left).
    fn pixels_at(path: &Path, rotations: &Rotations, at: &[(usize, usize)]) -> (Vec2, Vec<[u8; 4]>) {
        let doc = Document::open(path, rotations).expect("open");
        let page = doc.page(0).unwrap();
        let size = page.size();
        let (w, h) = (size.x as usize, size.y as usize);
        let mut rgba = vec![0u8; w * h * 4];
        Bitmap::new(&mut rgba, w as u32, h as u32).unwrap().draw(&page, [w as f64, h as f64], [0.0, 0.0]);
        let pixels = at.iter().map(|&(x, y)| rgba[(y * w + x) * 4..][..4].try_into().unwrap()).collect();
        (size, pixels)
    }

    /// The quarters image turned a quarter clockwise: 20x40, red top-right, blue bottom-right,
    /// green down the left.
    fn assert_turned_clockwise(size: Vec2, pixels: &[[u8; 4]]) {
        assert_eq!(size, vec2(20.0, 40.0));
        for (pixel, (expected, at)) in pixels.iter().zip([(RED, (15, 5)), (BLUE, (15, 35)), (GREEN, (4, 20))]) {
            assert_near(pixel, expected, at);
        }
    }

    const TURNED: [(usize, usize); 3] = [(15, 5), (15, 35), (4, 20)];

    #[test]
    fn annotation_appearances_follow_crop_rotation_and_tile_clip() {
        use lopdf::{Document as PdfFile, Object, Stream, dictionary};

        let path = text_pdf("annotation-appearances", &[]);
        let mut pdf = PdfFile::load(&path).unwrap();
        let page_id = pdf.get_pages()[&1];
        let mut annotations = Vec::new();
        for (kind, x, rgb, flags) in [
            ("Ink", 50, "1 0 0", 0),
            ("Stamp", 100, "1 0 0", 0),
            ("FreeText", 50, "0 1 0", 0),
            ("Highlight", 100, "0 0 1", 0),
            ("Stamp", 150, "1 0 0", 2),
        ] {
            let y = if matches!(kind, "FreeText" | "Highlight") { 120 } else { 60 };
            let appearance = pdf.add_object(Stream::new(
                dictionary! {
                    "Type" => "XObject", "Subtype" => "Form",
                    "BBox" => vec![0.into(), 0.into(), 30.into(), 30.into()],
                    "Resources" => dictionary! {},
                },
                format!("{rgb} rg 0 0 30 30 re f").into_bytes(),
            ));
            let annotation = pdf.add_object(dictionary! {
                "Type" => "Annot", "Subtype" => kind,
                "Rect" => vec![x.into(), y.into(), (x + 30).into(), (y + 30).into()],
                "AP" => dictionary! { "N" => appearance }, "F" => flags,
                "InkList" => vec![Object::Array(vec![x.into(), y.into(), (x+30).into(), (y+30).into()])],
            });
            annotations.push(annotation.into());
        }
        let page = pdf.get_object_mut(page_id).unwrap().as_dict_mut().unwrap();
        page.set("MediaBox", vec![10.into(), 20.into(), 210.into(), 220.into()]);
        page.set("CropBox", vec![30.into(), 40.into(), 190.into(), 200.into()]);
        page.set("Rotate", 90);
        page.set("Annots", annotations);
        pdf.save(&path).unwrap();

        for turns in 0..4 {
            let mut rotation = Rotations::default();
            for _ in 0..turns {
                rotation.turn_all(true);
            }
            let document = Document::open(&path, &rotation).unwrap();
            let page = document.page(0).unwrap();
            let PageSource::Pdf(native) = &page.0 else { panic!("PDF") };
            let text = TextDocument::open(&path, &rotation).unwrap();
            assert_eq!(text.page(0).unwrap().rotation().degrees(), native.geometry.rotation);
            let [w, h] = [page.size().x as u32, page.size().y as u32];
            let mut rgba = vec![0; (w * h * 4) as usize];
            Bitmap::new(&mut rgba, w, h).unwrap().draw(&page, [w as f64, h as f64], [0.0, 0.0]);
            for (x, y, expected) in [
                (65.0, 75.0, RED),
                (115.0, 75.0, RED),
                (65.0, 135.0, [255; 4]),
                (115.0, 135.0, [255; 4]),
                (165.0, 75.0, [255; 4]),
            ] {
                let at = native.geometry.to_fraction(x, y);
                let (px, py) = ((at.x * w as f32) as usize, (at.y * h as f32) as usize);
                assert_near(&rgba[(py * w as usize + px) * 4..][..4], expected, (px, py));
                if expected == RED {
                    let origin = [px as f64 - 8.0, py as f64 - 8.0];
                    let mut tile = vec![0; 16 * 16 * 4];
                    Bitmap::new(&mut tile, 16, 16).unwrap().draw(&page, [w as f64, h as f64], [-origin[0], -origin[1]]);
                    assert_near(&tile[(8 * 16 + 8) * 4..][..4], RED, (8, 8));
                }
            }
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn exif_orientation_turns_the_image() {
        let path = quarters_png("exif-6", None, Some(6));
        let (size, pixels) = pixels_at(&path, &Rotations::default(), &TURNED);
        let _ = std::fs::remove_file(&path);
        assert_turned_clockwise(size, &pixels);
    }

    #[test]
    fn exif_mirroring_flips_the_image() {
        let path = quarters_png("exif-2", None, Some(2));
        let (size, pixels) = pixels_at(&path, &Rotations::default(), &[(5, 3), (35, 3), (20, 16)]);
        let _ = std::fs::remove_file(&path);
        assert_eq!(size, vec2(40.0, 20.0));
        assert_near(&pixels[0], BLUE, (5, 3));
        assert_near(&pixels[1], RED, (35, 3));
        assert_near(&pixels[2], GREEN, (20, 16));
    }

    /// An image prints as a PDF page drawn as the image shows: here turned by its EXIF
    /// orientation.
    #[test]
    fn images_print_as_they_show() {
        let path = quarters_png("print", None, Some(6));
        let pdf = Image::open(&path, Rotation::default()).unwrap().to_pdf().unwrap();
        let printed = path.with_extension("pdf");
        assert!(unsafe { pdf.writeToURL(&NSURL::from_file_path(&printed).unwrap()) });
        let (size, pixels) = pixels_at(&printed, &Rotations::default(), &TURNED);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&printed);
        assert_turned_clockwise(size, &pixels);
    }

    #[test]
    fn rotation_turns_an_image_after_its_orientation() {
        let plain = quarters_png("turn", None, None);
        let (size, pixels) = pixels_at(&plain, &turned_all(true), &TURNED);
        let _ = std::fs::remove_file(&plain);
        assert_turned_clockwise(size, &pixels);

        // Turning an EXIF-turned image back brings it upright.
        let turned = quarters_png("turn-back", None, Some(6));
        let (size, pixels) = pixels_at(&turned, &turned_all(false), &[(5, 3), (35, 3)]);
        let _ = std::fs::remove_file(&turned);
        assert_eq!(size, vec2(40.0, 20.0));
        assert_near(&pixels[0], RED, (5, 3));
        assert_near(&pixels[1], BLUE, (35, 3));
    }

    /// Turning one page turns only that page.
    #[test]
    fn single_pages_turn_alone() {
        let path = text_pdf("one-page", &[("Alpha", 700)]);
        let mut rotations = Rotations::default();
        rotations.turn_page(1, true);
        let other = Document::open(&path, &rotations).expect("open").page(0).unwrap().size();
        rotations.turn_page(0, true);
        let turned = Document::open(&path, &rotations).expect("open").page(0).unwrap().size();
        let _ = std::fs::remove_file(&path);
        assert_eq!((other, turned), (vec2(612.0, 792.0), vec2(792.0, 612.0)));
    }

    /// A clockwise turn makes a portrait page landscape, and the text boxes turn with it:
    /// a line along the top runs down the right-hand side.
    #[test]
    fn rotation_turns_pdf_pages_and_their_text() {
        let path = text_pdf("turned", &[("Alpha", 700)]);
        let clockwise = turned_all(true);
        let size = Document::open(&path, &clockwise).expect("open").page(0).unwrap().size();
        let text = TextDocument::open(&path, &clockwise).expect("open text");
        let _ = std::fs::remove_file(&path);
        assert_eq!(size, vec2(792.0, 612.0));
        let (units, rects) = text.page(0).unwrap().boxes();
        let letters = String::from_utf16(&units).unwrap().find("Alpha").unwrap();
        let alpha = &rects[letters..letters + 5];
        // The line's top edge, 1 - 700/792 of the way down upright, is now that far from the right.
        let top = 1.0 - 700.0 / 792.0;
        assert!(alpha.iter().all(|r| (1.0 - r.center().x - top).abs() < 20.0 / 792.0), "{alpha:?}");
        assert!((alpha[0].top() - 72.0 / 612.0).abs() < 2.0 / 612.0, "starts at {}", alpha[0].top());
        assert!(alpha.windows(2).all(|w| w[1].top() >= w[0].top()), "letters run down the page: {alpha:?}");
    }

    #[test]
    fn png_opens_as_one_page_of_its_pixel_size() {
        let path = quarters_png("plain", None, None);
        let doc = Document::open(&path, &Rotations::default()).expect("open");
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
        let path = quarters_png("retina", Some(144.0), None);
        let doc = Document::open(&path, &Rotations::default()).expect("open");
        let _ = std::fs::remove_file(&path);
        let size = doc.page(0).unwrap().size();
        assert!((size - vec2(20.0, 10.0)).length() < 0.01, "size {size:?}");
    }

    #[test]
    fn png_draws_upright_on_white() {
        let path = quarters_png("draw", None, None);
        let doc = Document::open(&path, &Rotations::default()).expect("open");
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
        let path = quarters_png("text", None, None);
        let text = TextDocument::open(&path, &Rotations::default());
        let _ = std::fs::remove_file(&path);
        assert!(text.is_none());
    }

    fn read_boxes(path: &Path, rotations: &Rotations) -> Vec<TextBox> {
        Document::open(path, rotations).expect("open").page(0).unwrap().text_boxes()
    }

    #[test]
    fn saved_text_boxes_read_back() {
        let path = text_pdf("text-box", &[("Alpha", 700)]);
        let new =
            TextBox { size: 16.0, color: [24, 88, 214], ..TextBox::new(pos2(0.1, 0.2), "Note é\nline two".into()) };
        let rect = Rect::from_min_max(pos2(0.1, 0.2), pos2(0.4, 0.25));
        let save = PageSave::boxes(0, vec![(new.clone(), rect)], false);
        save_annotations(&path, &[save]).expect("save");

        let boxes = read_boxes(&path, &Rotations::default());
        assert_eq!(boxes.len(), 1);
        let read = &boxes[0];
        assert_eq!(read.text, new.text);
        assert_eq!(read.origin, Some(0));
        assert!(!read.changed);
        assert!((read.size - 16.0).abs() < 0.01, "size {}", read.size);
        for (got, want) in read.color.iter().zip(new.color) {
            assert!(got.abs_diff(want) <= 3, "colour {:?}", read.color);
        }
        assert!((read.at() - rect.min).length() < 1.0 / 612.0, "at {:?}", read.at);
        assert!((read.width.unwrap() - rect.width() * 612.0).abs() < 1.0, "width {:?}", read.width);
        // Where a box is doesn't depend on how the reader turns the pages.
        assert_eq!(read_boxes(&path, &turned_all(true))[0].at, read.at);

        // Changed, then deleted.
        let changed = TextBox { text: "Changed".into(), changed: true, ..read.clone() };
        let save = PageSave::boxes(0, vec![(changed, rect)], true);
        save_annotations(&path, &[save]).expect("save");
        assert_eq!(read_boxes(&path, &Rotations::default())[0].text, "Changed");
        let delete = PageSave::boxes(0, Vec::new(), true);
        save_annotations(&path, &[delete]).expect("save");
        assert!(read_boxes(&path, &Rotations::default()).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn saved_highlights_read_back() {
        let path = text_pdf("highlight", &[("Alpha", 700), ("Beta", 680)]);
        let lines =
            [Rect::from_min_max(pos2(0.1, 0.1), pos2(0.4, 0.12)), Rect::from_min_max(pos2(0.1, 0.13), pos2(0.3, 0.15))];
        let save = |highlights: Vec<Highlight>, complete: bool| PageSave {
            highlights,
            highlights_complete: complete,
            ..PageSave::boxes(0, Vec::new(), false)
        };
        save_annotations(&path, &[save(vec![Highlight::new(lines)], false)]).expect("save");

        let read = Document::open(&path, &Rotations::default()).expect("open").page(0).unwrap().highlights();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].origin, Some(0));
        let rects: Vec<Rect> = read[0].rects().collect();
        assert_eq!(rects.len(), 2);
        for (got, want) in rects.iter().zip(lines) {
            assert!((got.min - want.min).length() < 1e-3 && (got.max - want.max).length() < 1e-3, "{got:?} {want:?}");
        }
        for (got, want) in read[0].color.iter().zip(text_boxes::HIGHLIGHT_YELLOW) {
            assert!(got.abs_diff(want) <= 3, "colour {:?}", read[0].color);
        }
        // Text boxes are left alone, and a page with none of its highlights left has none.
        save_annotations(&path, &[save(Vec::new(), true)]).expect("save");
        assert!(Document::open(&path, &Rotations::default()).unwrap().page(0).unwrap().highlights().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn default_appearance_gives_size_and_colour() {
        assert_eq!(default_appearance("/Helv 14 Tf 1 0 0 rg"), (14.0, [255, 0, 0]));
        assert_eq!(default_appearance("0.5 g /Helvetica 9 Tf"), (9.0, [128, 128, 128]));
        assert_eq!(default_appearance("/F1 0 Tf 0 0 0 1 k"), (text_boxes::DEFAULT_SIZE, [0, 0, 0]));
        assert_eq!(default_appearance(""), (text_boxes::DEFAULT_SIZE, [0, 0, 0]));
        assert_eq!(unescape_name(b"Helvetica#2012#20Tf#200#20g"), "Helvetica 12 Tf 0 g");
    }

    /// Writes a one-page US Letter PDF drawing `content` with font F1 (`font`, a dictionary,
    /// whose `ToUnicode` refers to object 6 when `to_unicode` is given).
    fn pdf(name: &str, font: &str, content: &str, to_unicode: Option<&str>) -> std::path::PathBuf {
        let mut objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Count 1 /Kids [4 0 R] >>".to_string(),
            font.to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents 5 0 R >>"
                .to_string(),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        ];
        if let Some(cmap) = to_unicode {
            objects.push(format!("<< /Length {} >>\nstream\n{cmap}\nendstream", cmap.len()));
        }
        let mut pdf = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (i, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf += &format!("{} 0 obj\n{object}\nendobj\n", i + 1);
        }
        let xref = pdf.len();
        pdf += &format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
        for offset in offsets {
            pdf += &format!("{offset:010} 00000 n \n");
        }
        pdf += &format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1);
        let path = std::env::temp_dir().join(format!("lightweight-pdf-engine-{}-{name}.pdf", std::process::id()));
        std::fs::write(&path, pdf).unwrap();
        path
    }

    /// Writes a PDF with lines of 18 pt Helvetica at the given heights (points from the
    /// bottom), each starting 72 pt from the left.
    fn text_pdf(name: &str, lines: &[(&str, i32)]) -> std::path::PathBuf {
        let content: String =
            lines.iter().map(|(line, y)| format!("BT /F1 18 Tf 72 {y} Td ({line}) Tj ET\n")).collect();
        pdf(name, "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>", &content, None)
    }

    /// Each unit of the page string gets its own box, also after the line breaks PDFKit
    /// inserts, which `characterBoundsAtIndex` doesn't count.
    #[test]
    fn text_boxes_follow_the_page_string() {
        let path = text_pdf("boxes", &[("Alpha beta", 700), ("Gamma delta", 650), ("Epsilon", 600)]);
        let doc = TextDocument::open(&path, &Rotations::default()).expect("open");
        let _ = std::fs::remove_file(&path);
        let (units, rects) = doc.page(0).unwrap().boxes();
        let text = String::from_utf16(&units).unwrap();
        for (line, word) in ["Alpha", "Gamma", "Epsilon"].into_iter().enumerate() {
            let at = text[..text.find(word).unwrap()].encode_utf16().count();
            let first = rects[at];
            assert!((first.left() - 72.0 / 612.0).abs() < 1.0 / 612.0, "{word} starts at x {}", first.left());
            // Every letter of the word sits on its line, left to right.
            let top = 1.0 - (700 - 50 * line as i32) as f32 / 792.0;
            for i in at..at + word.len() {
                assert!(
                    (rects[i].center().y - top).abs() < 20.0 / 792.0,
                    "{word}[{}] at y {}",
                    i - at,
                    rects[i].center().y
                );
                assert!(i == at || rects[i].left() >= rects[i - 1].left(), "{word}[{}] left of the one before", i - at);
            }
        }
    }

    /// A ligature (one glyph, the two units "fi") and a character outside the Basic
    /// Multilingual Plane (one glyph, two units), as LaTeX writes them: the characters after
    /// each still get their own glyph's box.
    #[test]
    fn text_boxes_survive_ligatures_and_surrogate_pairs() {
        // Code 1 draws Helvetica's "fi" glyph and reads as "fi"; code 2 draws "A" and reads
        // as U+1D49C (𝒜).
        let font = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /ToUnicode 6 0 R \
                    /Encoding << /Type /Encoding /BaseEncoding /WinAnsiEncoding /Differences [1 /fi 2 /A] >> >>";
        let cmap = "/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
                    /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def \
                    /CMapName /Adobe-Identity-UCS def /CMapType 2 def \
                    1 begincodespacerange <00> <FF> endcodespacerange \
                    2 beginbfchar <01> <00660069> <02> <D835DC9C> endbfchar \
                    1 beginbfrange <20> <7E> <0020> endbfrange \
                    endcmap CMapName currentdict /CMap defineresource pop end end";
        // "the first 𝒜 end"
        let content = "BT /F1 18 Tf 72 700 Td <7468652001727374200220656E64> Tj ET";
        let path = pdf("ligature", font, content, Some(cmap));
        let doc = TextDocument::open(&path, &Rotations::default()).expect("open");
        let _ = std::fs::remove_file(&path);
        let (units, rects) = doc.page(0).unwrap().boxes();
        let text = String::from_utf16(&units).unwrap();
        assert_eq!(text.trim_end(), "the first 𝒜 end");

        // Helvetica's widths, in 1/1000 em, of the glyphs drawn: "the ", fi, "rst ", A, " end".
        let glyphs = [278, 556, 556, 278, 500, 333, 500, 278, 278, 667, 278, 556, 556, 556];
        let mut x = 72.0;
        let mut lefts = Vec::new();
        for w in glyphs {
            lefts.push(x);
            x += 18.0 * w as f32 / 1000.0;
        }
        // Each unit of the text, with the glyph it's drawn by.
        let glyph_of = [0, 1, 2, 3, 4, 4, 5, 6, 7, 8, 9, 9, 10, 11, 12, 13];
        for (unit, &glyph) in glyph_of.iter().enumerate() {
            let left = rects[unit].left() * 612.0;
            assert!(
                (left - lefts[glyph]).abs() < 1.0,
                "unit {unit} ({:?}) starts at {left:.1} pt, its glyph at {:.1}",
                String::from_utf16_lossy(&units[unit..unit + 1]),
                lefts[glyph]
            );
        }
        assert_eq!(rects[10], rects[11], "both halves of 𝒜 share its box");
    }

    #[test]
    fn other_files_do_not_open() {
        let path = std::env::temp_dir().join(format!("lightweight-pdf-engine-{}-notes.txt", std::process::id()));
        std::fs::write(&path, "not a document").unwrap();
        let doc = Document::open(&path, &Rotations::default());
        let _ = std::fs::remove_file(&path);
        assert!(doc.is_none());
    }
}
