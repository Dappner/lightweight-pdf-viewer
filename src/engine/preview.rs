//! Save-time compatibility for Preview's editable FreeText representation.
//!
//! PDFKit writes the font in PDF points, but AnnotationKit can multiply that font by its
//! archived `originalModelBaseScaleFactor` when Preview starts editing (12 becomes 6.77
//! points on the display used to reproduce this). Normalize only boxes we just styled.
//! This is an undocumented archive field: an unfamiliar archive leaves PDFKit's output
//! intact and reports a compatibility warning. We parse data, never unarchive ObjC classes.
//! An incremental PDF update leaves every appearance stream and untouched object intact.

use std::io::Cursor;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use lopdf::{Dictionary, Document, IncrementalDocument, Object, ObjectId};
use plist::Value;

const EXTRAS: &[u8] = b"AAPL:AKExtras";
const ARCHIVE: &[u8] = b"AAPL:AKAnnotationObject";
const SCALE: &str = "originalModelBaseScaleFactor";

/// Identity in the outgoing PDFKit page, with the geometry and font we asked it to save.
pub(super) struct EditedBox {
    pub page: u32,
    pub ordinal: usize,
    pub size: f32,
    pub bounds: [f64; 4],
}

type Result<T> = std::result::Result<T, String>;

/// Best effort: even a failed write of the corrected revision keeps PDFKit's file intact.
pub(super) fn apply(path: &Path, boxes: &[EditedBox]) -> super::SaveOutcome {
    match normalize(path, boxes) {
        Ok(()) => super::SaveOutcome::Saved,
        Err(error) => {
            eprintln!("Saved without Preview text compatibility: {error}");
            super::SaveOutcome::PreviewMayResize
        }
    }
}

fn check<T, E: std::fmt::Display>(result: std::result::Result<T, E>) -> Result<T> {
    result.map_err(|e| e.to_string())
}

pub(super) fn normalize(path: &Path, boxes: &[EditedBox]) -> Result<()> {
    if boxes.is_empty() {
        return Ok(());
    }
    let bytes = check(std::fs::read(path))?;
    let doc = check(Document::load_mem(&bytes))?;
    // PDFKit's writer owns encryption. Never append an unencrypted revision to an
    // encrypted file; unsupported saves leave the original in place.
    // lopdf automatically decrypts empty-password PDFs while loading. These still need
    // the fallback: its decrypted dictionaries cannot be appended to encrypted bytes.
    if doc.is_encrypted() || doc.was_encrypted() {
        return Err("Preview compatibility cannot update an encrypted PDF".into());
    }
    let mut edits = Vec::new();
    for b in boxes {
        let (id, annotation) = annotation(&doc, b)?;
        verify(annotation, b)?;
        let extras = check(doc.dereference(check(annotation.get(EXTRAS))?))?.1;
        let mut extras = check(extras.as_dict())?.clone();
        let blob = check(doc.dereference(check(extras.get(ARCHIVE))?))?.1;
        let encoded = check(blob.as_str())?;
        extras.set(ARCHIVE, Object::string_literal(normalize_archive(encoded)?));
        let mut annotation = annotation.clone();
        annotation.set(EXTRAS, extras);
        edits.push((id, annotation));
    }
    let mut update = IncrementalDocument::create_from(bytes, doc);
    for (id, annotation) in edits {
        update.new_document.set_object(id, annotation);
    }
    let mut output = Vec::new();
    check(update.save_to(&mut output))?;
    // Verify the revision the reader will actually see, including the geometry and font.
    let written = check(Document::load_mem(&output))?;
    for b in boxes {
        let (_, a) = annotation(&written, b)?;
        verify(a, b)?;
        let extras = check(written.dereference(check(a.get(EXTRAS))?))?.1;
        let blob = check(check(extras.as_dict())?.get(ARCHIVE))?;
        let blob = check(written.dereference(blob))?.1;
        let encoded = check(blob.as_str())?;
        if archive_scale(encoded)? != 1.0 {
            return Err("Preview annotation scale did not survive saving".into());
        }
    }
    // Never write over PDFKit's output in place: a partial write must also fall back safely.
    let name = path.file_name().ok_or("Not a file")?.to_string_lossy();
    let corrected = path.with_file_name(format!(".{name}.preview"));
    let result = std::fs::write(&corrected, output).and_then(|()| std::fs::rename(&corrected, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&corrected);
    }
    check(result)
}

fn annotation<'a>(doc: &'a Document, b: &EditedBox) -> Result<(ObjectId, &'a Dictionary)> {
    let page_id = doc.get_pages().get(&(b.page + 1)).copied().ok_or("Saved page is missing")?;
    let page = check(check(doc.get_object(page_id))?.as_dict())?;
    let annots = check(doc.dereference(check(page.get(b"Annots"))?))?.1;
    let mut ordinal = 0;
    for object in check(annots.as_array())? {
        let (id, object) = check(doc.dereference(object))?;
        let a = check(object.as_dict())?;
        if a.get(b"Subtype").and_then(Object::as_name).ok() != Some(b"FreeText") {
            continue;
        }
        if ordinal == b.ordinal {
            return Ok((id.ok_or("PDFKit wrote a direct annotation instead of an object")?, a));
        }
        ordinal += 1;
    }
    Err("Saved text box is missing".into())
}

fn verify(a: &Dictionary, b: &EditedBox) -> Result<()> {
    let rect = check(check(a.get(b"Rect"))?.as_array())?;
    if rect.len() != 4
        || rect
            .iter()
            .zip(b.bounds)
            .any(|(v, want)| v.as_float().map_or(true, |got| !got.is_finite() || (got as f64 - want).abs() > 0.002))
    {
        return Err("Saved text box bounds differ from the edit".into());
    }
    // PDFKit writes /DA as a PDF name, whereas other writers use a string.
    let object = check(a.get(b"DA"))?;
    let da = check(object.as_name().or_else(|_| object.as_str()))?;
    let da = check(std::str::from_utf8(da))?;
    // PDFKit escapes spaces inside its name before the PDF writer escapes '#' again.
    // lopdf has decoded the outer name escapes; these inner separators remain literal.
    let da = if matches!(object, Object::Name(_)) { da.replace("#20", " ") } else { da.to_owned() };
    let tokens: Vec<_> = da.split_whitespace().collect();
    let size = tokens.windows(2).find(|t| t[1] == "Tf").and_then(|t| t[0].parse::<f32>().ok());
    if size.is_none_or(|s| !s.is_finite() || (s - b.size).abs() > 0.001) {
        return Err(format!("Saved text box font size differs from the edit (expected {}, appearance {da:?})", b.size));
    }
    Ok(())
}

fn archive(encoded: &[u8]) -> Result<(Value, usize)> {
    let bytes = check(STANDARD.decode(encoded))?;
    let value = check(Value::from_reader(Cursor::new(bytes)))?;
    let dict = value.as_dictionary().ok_or("Expected an Apple annotation archive")?;
    if dict.get("$archiver").and_then(Value::as_string) != Some("NSKeyedArchiver") {
        return Err("Unrecognized Apple annotation archiver".into());
    }
    let root = dict
        .get("$top")
        .and_then(Value::as_dictionary)
        .and_then(|top| top.get("root"))
        .and_then(Value::as_uid)
        .ok_or("Apple annotation archive has no root")?
        .get();
    let root = check(usize::try_from(root))?;
    let objects = dict.get("$objects").and_then(Value::as_array).ok_or("Apple annotation archive has no objects")?;
    let root_dict = objects.get(root).and_then(Value::as_dictionary).ok_or("Invalid Apple annotation root")?;
    let class = root_dict
        .get("$class")
        .and_then(Value::as_uid)
        .and_then(|id| usize::try_from(id.get()).ok())
        .and_then(|id| objects.get(id))
        .and_then(Value::as_dictionary)
        .and_then(|d| d.get("$classname"))
        .and_then(Value::as_string);
    if class != Some("AKTextBoxAnnotation") {
        return Err("Unrecognized Apple text box archive class".into());
    }
    let scale = root_dict.get(SCALE).and_then(Value::as_real).ok_or("Apple annotation scale is missing")?;
    if !scale.is_finite() || scale < 0.0 {
        return Err("Invalid Apple annotation scale".into());
    }
    Ok((value, root))
}

fn archive_scale(encoded: &[u8]) -> Result<f64> {
    let (value, root) = archive(encoded)?;
    Ok(value.as_dictionary().unwrap()["$objects"].as_array().unwrap()[root].as_dictionary().unwrap()[SCALE]
        .as_real()
        .unwrap())
}

fn normalize_archive(encoded: &[u8]) -> Result<Vec<u8>> {
    let (mut value, root) = archive(encoded)?;
    value.as_dictionary_mut().unwrap().get_mut("$objects").unwrap().as_array_mut().unwrap()[root]
        .as_dictionary_mut()
        .unwrap()
        .insert(SCALE.into(), Value::Real(1.0));
    let mut bytes = Vec::new();
    check(value.to_writer_binary(&mut bytes))?;
    Ok(STANDARD.encode(bytes).into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{Stream, dictionary};
    use plist::Uid;

    fn blob(scale: f64) -> Vec<u8> {
        let root = Value::Dictionary(
            [
                (SCALE.to_owned(), Value::Real(scale)),
                ("$class".to_owned(), Value::Uid(Uid::new(2))),
                ("rectangle".to_owned(), Value::Data(vec![1, 2, 3])),
                ("rotationAngle".to_owned(), Value::Real(0.0)),
            ]
            .into_iter()
            .collect(),
        );
        let class = Value::Dictionary(
            [("$classname".to_owned(), Value::String("AKTextBoxAnnotation".into()))].into_iter().collect(),
        );
        let archive = Value::Dictionary(
            [
                ("$archiver".to_owned(), Value::String("NSKeyedArchiver".into())),
                ("$version".to_owned(), Value::Integer(100000.into())),
                (
                    "$top".to_owned(),
                    Value::Dictionary([("root".to_owned(), Value::Uid(Uid::new(1)))].into_iter().collect()),
                ),
                ("$objects".to_owned(), Value::Array(vec![Value::String("$null".into()), root, class])),
            ]
            .into_iter()
            .collect(),
        );
        let mut bytes = Vec::new();
        archive.to_writer_binary(&mut bytes).unwrap();
        STANDARD.encode(bytes).into_bytes()
    }

    fn fixture(name: &str) -> (std::path::PathBuf, EditedBox) {
        let path = std::env::temp_dir().join(format!("preview-compat-{}-{name}.pdf", std::process::id()));
        let mut doc = Document::with_version("1.7");
        let pages = doc.new_object_id();
        let appearance = doc.add_object(Stream::new(
            dictionary! {"Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 160.into(), 22.into()]},
            b"q Q".to_vec(),
        ));
        let mut annots = Vec::new();
        let link =
            doc.add_object(dictionary! {"Subtype" => "Link", "Rect" => vec![0.into(),0.into(),10.into(),10.into()]});
        annots.push(link.into());
        for contents in ["Edited", "Untouched"] {
            let a = doc.add_object(dictionary! {
                "Subtype" => "FreeText", "Contents" => Object::string_literal(contents),
                "DA" => Object::Name(b"Helvetica#2012#20Tf#200#20g".to_vec()),
                "Rect" => vec![72.into(),500.into(),232.into(),522.into()],
                "AP" => dictionary! {"N" => appearance},
                "AAPL:AKExtras" => dictionary! {"AAPL:AKAnnotationObject" => Object::string_literal(blob(0.56424))},
            });
            annots.push(a.into());
        }
        let page = doc.add_object(dictionary! {"Type" => "Page", "Parent" => pages, "MediaBox" => vec![0.into(),0.into(),612.into(),792.into()], "Annots" => annots});
        doc.set_object(pages, dictionary! {"Type" => "Pages", "Count" => 1, "Kids" => vec![page.into()]});
        let catalog = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
        doc.trailer.set("Root", catalog);
        doc.save(&path).unwrap();
        (path, EditedBox { page: 0, ordinal: 0, size: 12.0, bounds: [72.0, 500.0, 232.0, 522.0] })
    }

    #[test]
    fn corrects_only_selected_annotations_and_preserves_original_revision() {
        let (path, selected) = fixture("selected");
        let original = std::fs::read(&path).unwrap();
        let before = Document::load_mem(&original).unwrap();
        let untouched = EditedBox { ordinal: 1, ..selected };
        let (id, _) = annotation(&before, &untouched).unwrap();
        let original_untouched = before.get_object(id).unwrap().clone();
        normalize(&path, &[selected]).unwrap();
        let output = std::fs::read(&path).unwrap();
        assert!(output.starts_with(&original), "original streams and objects must stay byte-for-byte intact");
        let after = Document::load_mem(&output).unwrap();
        assert_eq!(after.get_object(id).unwrap(), &original_untouched);
        let a = annotation(&after, &EditedBox { ordinal: 0, ..untouched }).unwrap().1;
        let extras = a.get(EXTRAS).unwrap().as_dict().unwrap();
        assert_eq!(archive_scale(extras.get(ARCHIVE).unwrap().as_str().unwrap()).unwrap(), 1.0);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn normalization_preserves_every_other_archive_field() {
        for scale in [0.0, 0.564240790655885, 1.0, 2.0] {
            let original = blob(scale);
            let corrected = normalize_archive(&original).unwrap();
            let (mut expected, root) = archive(&original).unwrap();
            expected.as_dictionary_mut().unwrap().get_mut("$objects").unwrap().as_array_mut().unwrap()[root]
                .as_dictionary_mut()
                .unwrap()
                .insert(SCALE.into(), Value::Real(1.0));
            assert_eq!(archive(&corrected).unwrap().0, expected);
        }
        assert!(normalize_archive(b"not an archive").is_err());
    }

    #[test]
    fn unexpected_font_or_geometry_leaves_file_unchanged() {
        let (path, mut selected) = fixture("rejected");
        let before = std::fs::read(&path).unwrap();
        selected.size = 16.0;
        assert!(normalize(&path, std::slice::from_ref(&selected)).is_err());
        selected.size = 12.0;
        selected.bounds[0] = 80.0;
        assert!(normalize(&path, &[selected]).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn unsupported_archives_save_pdfkit_output_unchanged() {
        for missing_scale in [false, true] {
            let (path, selected) = fixture(if missing_scale { "missing-scale" } else { "unknown-class" });
            let mut doc = Document::load(&path).unwrap();
            let (id, _) = annotation(&doc, &selected).unwrap();
            let (mut value, root) = archive(&blob(0.56424)).unwrap();
            let objects = value.as_dictionary_mut().unwrap().get_mut("$objects").unwrap().as_array_mut().unwrap();
            if missing_scale {
                objects[root].as_dictionary_mut().unwrap().remove(SCALE);
            } else {
                objects[2]
                    .as_dictionary_mut()
                    .unwrap()
                    .insert("$classname".into(), Value::String("FutureTextBox".into()));
            }
            let mut bytes = Vec::new();
            value.to_writer_binary(&mut bytes).unwrap();
            doc.get_object_mut(id)
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .set(EXTRAS, dictionary! {"AAPL:AKAnnotationObject" => Object::string_literal(STANDARD.encode(bytes))});
            doc.save(&path).unwrap();
            let before = std::fs::read(&path).unwrap();
            assert_eq!(apply(&path, &[selected]), super::super::SaveOutcome::PreviewMayResize);
            assert_eq!(std::fs::read(&path).unwrap(), before);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn encrypted_output_falls_back_without_changing_encryption() {
        use lopdf::{EncryptionState, EncryptionVersion, Permissions};
        let (path, selected) = fixture("encrypted");
        let mut doc = Document::load(&path).unwrap();
        doc.trailer.set("ID", vec![Object::string_literal(b"preview-test-id".to_vec()); 2]);
        let state = EncryptionState::try_from(EncryptionVersion::V1 {
            document: &doc,
            owner_password: "owner",
            user_password: "",
            permissions: Permissions::all(),
        })
        .unwrap();
        doc.encrypt(&state).unwrap();
        doc.save(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert_eq!(apply(&path, &[selected]), super::super::SaveOutcome::PreviewMayResize);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let mut saved = Document::load(&path).unwrap();
        if saved.is_encrypted() {
            saved.decrypt("").unwrap();
        }
        assert!(annotation(&saved, &EditedBox { page: 0, ordinal: 0, size: 12.0, bounds: [0.0; 4] }).is_ok());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn corrected_output_write_failure_keeps_pdfkit_output() {
        let (path, selected) = fixture("write-failure");
        let before = std::fs::read(&path).unwrap();
        let corrected = path.with_file_name(format!(".{}.preview", path.file_name().unwrap().to_string_lossy()));
        // A directory makes opening the correction file fail, even when running as root.
        std::fs::create_dir(&corrected).unwrap();
        assert_eq!(apply(&path, &[selected]), super::super::SaveOutcome::PreviewMayResize);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::remove_dir(corrected).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pdfkit_save_commits_edits_when_preview_correction_cannot_be_written() {
        use crate::text_boxes::{PageSave, TextBox};
        use eframe::egui::{Rect, pos2};

        let (path, _) = fixture("pdfkit-fallback");
        let corrected =
            path.with_file_name(format!("..{}.saving.preview", path.file_name().unwrap().to_string_lossy()));
        std::fs::create_dir(&corrected).unwrap();
        let b = TextBox::new(pos2(0.1, 0.4), "Saved despite Preview compatibility failure".into());
        let save = PageSave::boxes(0, vec![(b, Rect::from_min_max(pos2(0.1, 0.4), pos2(0.6, 0.44)))], true);
        assert_eq!(
            crate::engine::save_annotations(&path, &[save]).unwrap(),
            super::super::SaveOutcome::PreviewMayResize
        );
        let doc = Document::load(&path).unwrap();
        let a = annotation(&doc, &EditedBox { page: 0, ordinal: 0, size: 12.0, bounds: [0.0; 4] }).unwrap().1;
        assert_eq!(a.get(b"Contents").unwrap().as_str().unwrap(), b"Saved despite Preview compatibility failure");
        std::fs::remove_dir(corrected).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pdfkit_save_normalizes_new_and_changed_boxes_after_deletion() {
        use crate::text_boxes::{PageSave, TextBox};
        use eframe::egui::{Rect, pos2};

        let (path, _) = fixture("pdfkit-save");
        let mut doc = Document::load(&path).unwrap();
        let page = doc.get_pages()[&1];
        doc.get_object_mut(page).unwrap().as_dict_mut().unwrap().remove(b"Annots");
        doc.save(&path).unwrap();
        let rects = [0.2, 0.4, 0.6].map(|y| Rect::from_min_max(pos2(0.1, y), pos2(0.6, y + 0.04)));
        let boxes =
            ["Delete me", "Twelve point box", "Untouched box"].map(|text| TextBox::new(pos2(0.1, 0.2), text.into()));
        let save = PageSave::boxes(0, boxes.into_iter().zip(rects).collect(), false);
        crate::engine::save_annotations(&path, &[save]).unwrap();
        let doc = Document::load(&path).unwrap();
        for ordinal in 0..3 {
            let b = EditedBox { page: 0, ordinal, size: 12.0, bounds: [0.0; 4] };
            let a = annotation(&doc, &b).unwrap().1;
            let extras = a.get(EXTRAS).unwrap().as_dict().unwrap();
            assert_eq!(archive_scale(extras.get(ARCHIVE).unwrap().as_str().unwrap()).unwrap(), 1.0);
        }
        let changed =
            TextBox { origin: Some(1), size: 16.0, ..TextBox::new(pos2(0.1, 0.4), "Changed to sixteen points".into()) };
        let untouched =
            TextBox { origin: Some(2), changed: false, ..TextBox::new(pos2(0.1, 0.6), "Untouched box".into()) };
        let save = PageSave::boxes(0, vec![(changed, rects[1]), (untouched, rects[2])], true);
        crate::engine::save_annotations(&path, &[save]).unwrap();
        let doc = Document::load(&path).unwrap();
        let selected = EditedBox { page: 0, ordinal: 0, size: 16.0, bounds: [61.2, 443.52, 367.2, 475.2] };
        let a = annotation(&doc, &selected).unwrap().1;
        verify(a, &selected).unwrap();
        let extras = a.get(EXTRAS).unwrap().as_dict().unwrap();
        assert_eq!(archive_scale(extras.get(ARCHIVE).unwrap().as_str().unwrap()).unwrap(), 1.0);
        // Optional diagnostic artifact for an actual Preview edit/save round trip.
        if let Some(output) = std::env::var_os("PREVIEW_COMPAT_PROBE_PDF") {
            std::fs::copy(&path, output).unwrap();
        }
        std::fs::remove_file(path).unwrap();
    }
}
