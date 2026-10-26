//! Page text saved to disk, so searching a document again (even after a relaunch) reads it
//! from a small file instead of extracting it with PDFKit, which takes seconds for a book.
//!
//! Each document gets one file in `~/Library/Caches/Lightweight PDF/Search/`, named after
//! the PDF's identity (device, inode, size, modification time) and how text is folded
//! ([`text::rules`]), so an edited PDF or a new app version starts a new file and the old one
//! ages out. The header repeats that identity, and each page is a record with its own
//! checksum:
//!
//! ```text
//! header:  magic, format, rules, device, inode, size, mtime (s, ns), page count
//! record:  page, payload length, FNV-1a of payload, payload (see PageText::encode)
//! ```
//!
//! Pages are appended as the worker reads them, so a search closed halfway leaves the pages
//! it got through. A record cut short by a crash fails its checksum; reading stops there and
//! the next writer truncates it away. One worker writes at a time, holding an exclusive lock
//! on the file; another searching the same document reads what's there and takes over
//! writing once the lock is free.
//!
//! Only page text and its offset map are kept, never matches or highlight geometry. Files
//! are private to the user, unused ones are deleted after `max_days`, and the least recently
//! used go first when the folder is over `max_mb` ([`sweep`]).

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::text::{self, FNV_OFFSET, PageText, fnv};

const MAGIC: &[u8; 8] = b"LPDFTXT\0";
/// Bump when the header or record layout changes.
const FORMAT: u32 = 1;
const HEADER_LEN: u64 = 8 + 4 + 8 * 5 + 4 + 4;
const RECORD_HEADER_LEN: u64 = 4 + 4 + 8;
const EXTENSION: &str = "idx";
const MB: u64 = 1024 * 1024;
const DAY: u64 = 24 * 60 * 60;

/// How much page text to keep on disk, set with `"search_cache"` in the config.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// Total size of the folder; 0 turns the cache off.
    pub max_mb: u64,
    /// Days a document's text is kept after it was last searched.
    pub max_days: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_mb: 256, max_days: 30 }
    }
}

/// Where page text is saved and how much of it is kept.
#[derive(Clone, Debug)]
pub struct Store {
    pub dir: PathBuf,
    pub limits: Limits,
}

/// The user's folder for saved page text.
pub fn user_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Caches/Lightweight PDF/Search"))
}

impl Store {
    fn max_bytes(&self) -> u64 {
        self.limits.max_mb.saturating_mul(MB)
    }

    /// Deletes every saved document. Workers writing one carry on into a file that's gone
    /// once they close it.
    pub fn clear(&self) {
        for (path, _) in entries(&self.dir) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Opens the saved text of the PDF at `pdf` with `pages` pages, creating it if there is
    /// none. `None` if the PDF can't be read or the cache folder can't be written.
    pub fn open(&self, pdf: &Path, pages: usize) -> Option<Index> {
        let id = Identity::of(pdf, pages)?;
        create_private_dir(&self.dir)?;
        let path = self.dir.join(format!("{:016x}.{EXTENSION}", id.key()));
        let file =
            OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(&path).ok()?;
        // Opening counts as use, for expiry and for which documents go first.
        let _ = file.set_modified(SystemTime::now());
        let mut index = Index {
            file,
            path,
            id,
            pdf: pdf.to_owned(),
            pages: vec![None; pages],
            end: 0,
            writer: false,
            max_bytes: self.max_bytes(),
        };
        index.try_write();
        if !index.writer {
            index.scan();
        }
        Some(index)
    }
}

/// Which PDF, as it is on disk, and which rules its text was folded with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Identity {
    rules: u64,
    device: u64,
    inode: u64,
    size: u64,
    mtime: (i64, i64),
    pages: u32,
}

impl Identity {
    fn of(pdf: &Path, pages: usize) -> Option<Self> {
        let meta = std::fs::metadata(pdf).ok()?;
        Some(Self {
            rules: text::rules(),
            device: meta.dev(),
            inode: meta.ino(),
            size: meta.size(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            pages: pages as u32,
        })
    }

    fn header(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.extend(FORMAT.to_le_bytes());
        for n in [self.rules, self.device, self.inode, self.size, self.mtime.0 as u64] {
            out.extend(n.to_le_bytes());
        }
        out.extend((self.mtime.1 as u32).to_le_bytes());
        out.extend(self.pages.to_le_bytes());
        out
    }

    /// Names the file: every field of the header, hashed.
    fn key(&self) -> u64 {
        fnv(FNV_OFFSET, &self.header())
    }
}

/// One document's saved text, open for a search worker.
pub struct Index {
    file: File,
    path: PathBuf,
    id: Identity,
    pdf: PathBuf,
    /// Where each saved page's payload starts, and its length.
    pages: Vec<Option<(u64, u32)>>,
    /// End of the last good record.
    end: u64,
    /// Holds the lock: appends pages.
    writer: bool,
    max_bytes: u64,
}

impl Index {
    /// Takes the lock if it's free, then reads what's saved and cuts off anything after the
    /// last good record (or starts the file over if it's for something else).
    fn try_write(&mut self) {
        match self.file.try_lock() {
            Ok(()) => self.writer = true,
            Err(TryLockError::WouldBlock | TryLockError::Error(_)) => return,
        }
        if !self.scan() {
            self.pages.iter_mut().for_each(|p| *p = None);
            let header = self.id.header();
            let ok = self.file.set_len(0).is_ok() && self.file.write_all_at(&header, 0).is_ok();
            self.end = if ok { header.len() as u64 } else { 0 };
        }
        if self.end == 0 || self.file.set_len(self.end).is_err() {
            // Can't tell where records go: leave the file alone.
            let _ = self.file.unlock();
            self.writer = false;
        }
    }

    /// Reads the header and records; returns whether the header matched. Stops at the first
    /// bad record.
    fn scan(&mut self) -> bool {
        self.end = 0;
        let Ok(_) = self.file.seek(SeekFrom::Start(0)) else { return false };
        let mut reader = BufReader::with_capacity(256 * 1024, &self.file);
        let mut header = vec![0; HEADER_LEN as usize];
        if reader.read_exact(&mut header).is_err() || header != self.id.header() {
            return false;
        }
        let mut at = HEADER_LEN;
        let mut payload = Vec::new();
        loop {
            let mut head = [0u8; RECORD_HEADER_LEN as usize];
            if reader.read_exact(&mut head).is_err() {
                break;
            }
            let page = u32::from_le_bytes(head[0..4].try_into().unwrap()) as usize;
            let len = u32::from_le_bytes(head[4..8].try_into().unwrap());
            let sum = u64::from_le_bytes(head[8..16].try_into().unwrap());
            if page >= self.pages.len() || len as u64 > self.max_bytes {
                break;
            }
            payload.resize(len as usize, 0);
            if reader.read_exact(&mut payload).is_err() || fnv(FNV_OFFSET, &payload) != sum {
                break;
            }
            self.pages[page] = Some((at + RECORD_HEADER_LEN, len));
            at += RECORD_HEADER_LEN + len as u64;
        }
        self.end = at;
        true
    }

    /// Page `p`'s saved text, if it's there and intact.
    pub fn load(&mut self, p: usize) -> Option<PageText> {
        let (at, len) = self.pages[p]?;
        let mut payload = vec![0; len as usize];
        let text = self.file.read_exact_at(&mut payload, at).ok().and_then(|()| PageText::decode(&payload));
        if text.is_none() {
            self.pages[p] = None;
        }
        text
    }

    pub fn has(&self, p: usize) -> bool {
        self.pages[p].is_some()
    }

    /// Saves page `p`'s text, if this index holds the lock (or can take it now) and the
    /// document's file stays within the cache's size.
    pub fn save(&mut self, p: usize, text: &PageText) {
        if self.pages[p].is_some() {
            return;
        }
        if !self.writer {
            self.try_write();
            if !self.writer || self.pages[p].is_some() {
                return;
            }
        }
        let mut record = vec![0; RECORD_HEADER_LEN as usize];
        text.encode(&mut record);
        let len = record.len() as u64 - RECORD_HEADER_LEN;
        if self.end + record.len() as u64 > self.max_bytes {
            return;
        }
        let sum = fnv(FNV_OFFSET, &record[RECORD_HEADER_LEN as usize..]);
        record[0..4].copy_from_slice(&(p as u32).to_le_bytes());
        record[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        record[8..16].copy_from_slice(&sum.to_le_bytes());
        if self.file.write_all_at(&record, self.end).is_ok() {
            self.pages[p] = Some((self.end + RECORD_HEADER_LEN, len as u32));
            self.end += record.len() as u64;
        } else {
            // Disk full or similar: stop, leaving the good records.
            let _ = self.file.set_len(self.end);
            let _ = self.file.unlock();
            self.writer = false;
            self.max_bytes = 0;
        }
    }

    /// Whether the PDF is still the file this text came from.
    pub fn is_current(&self) -> bool {
        Identity::of(&self.pdf, self.pages.len()) == Some(self.id)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn create_private_dir(dir: &Path) -> Option<()> {
    std::fs::create_dir_all(dir).ok()?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).ok()
}

/// Saved documents in `dir`, with when each was last used and its size.
fn entries(dir: &Path) -> Vec<(PathBuf, (SystemTime, u64))> {
    let Ok(read) = std::fs::read_dir(dir) else { return vec![] };
    read.flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == EXTENSION))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            Some((e.path(), (meta.modified().ok()?, meta.len())))
        })
        .collect()
}

/// Deletes documents unused for longer than `max_days`, then the least recently used until
/// the folder fits `max_mb`. Documents a search is writing are skipped.
pub fn sweep(store: &Store) {
    sweep_at(store, SystemTime::now());
}

fn sweep_at(store: &Store, now: SystemTime) {
    let max_age = Duration::from_secs(store.limits.max_days.saturating_mul(DAY));
    let mut entries = entries(&store.dir);
    entries.sort_by_key(|(_, (used, _))| *used);
    let mut total: u64 = entries.iter().map(|(_, (_, len))| len).sum();
    for (path, (used, len)) in entries {
        let expired = now.duration_since(used).is_ok_and(|age| age > max_age);
        if !expired && total <= store.max_bytes() {
            continue;
        }
        let Ok(file) = File::open(&path) else { continue };
        if file.try_lock().is_ok() && std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: PathBuf,
        pdf: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("lpdf-index-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let pdf = dir.join("doc.pdf");
            std::fs::write(&pdf, b"%PDF-1.4 stand-in").unwrap();
            Self { dir, pdf }
        }

        fn store(&self, max_mb: u64) -> Store {
            Store { dir: self.dir.join("cache"), limits: Limits { max_mb, max_days: 30 } }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn page(n: usize) -> PageText {
        PageText::new(&format!("Page {n}: Straße ﬁne text"))
    }

    fn same(a: &PageText, b: &PageText) -> bool {
        let (mut x, mut y) = (Vec::new(), Vec::new());
        a.encode(&mut x);
        b.encode(&mut y);
        x == y
    }

    #[test]
    fn saved_pages_come_back_after_reopening() {
        let f = Fixture::new("reopen");
        let store = f.store(16);
        let mut index = store.open(&f.pdf, 4).unwrap();
        index.save(2, &page(2));
        index.save(0, &page(0));
        drop(index);
        let mut index = store.open(&f.pdf, 4).unwrap();
        assert!(same(&index.load(2).unwrap(), &page(2)));
        assert!(same(&index.load(0).unwrap(), &page(0)));
        assert!(index.load(1).is_none() && index.load(3).is_none());
        let mode = std::fs::metadata(index.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "private to the user");
    }

    #[test]
    fn a_cut_off_record_is_dropped_and_writing_carries_on() {
        let f = Fixture::new("truncated");
        let store = f.store(16);
        let mut index = store.open(&f.pdf, 3).unwrap();
        index.save(0, &page(0));
        index.save(1, &page(1));
        let path = index.path().to_owned();
        drop(index);
        let len = std::fs::metadata(&path).unwrap().len();
        OpenOptions::new().write(true).open(&path).unwrap().set_len(len - 3).unwrap();

        let mut index = store.open(&f.pdf, 3).unwrap();
        assert!(index.load(0).is_some());
        assert!(!index.has(1), "the cut-off page is gone");
        index.save(2, &page(2));
        drop(index);
        let mut index = store.open(&f.pdf, 3).unwrap();
        assert!(index.load(0).is_some() && !index.has(1) && index.load(2).is_some());
    }

    #[test]
    fn corrupt_bytes_stop_reading_there() {
        let f = Fixture::new("corrupt");
        let store = f.store(16);
        let mut index = store.open(&f.pdf, 3).unwrap();
        for p in 0..3 {
            index.save(p, &page(p));
        }
        let (at, _) = index.pages[1].unwrap();
        let path = index.path().to_owned();
        drop(index);
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all_at(b"X", at + 2).unwrap();
        let mut index = store.open(&f.pdf, 3).unwrap();
        assert!(index.load(0).is_some());
        assert!(!index.has(1) && !index.has(2), "nothing after a bad record is trusted");
    }

    #[test]
    fn a_changed_pdf_gets_new_text() {
        let f = Fixture::new("changed");
        let store = f.store(16);
        let mut index = store.open(&f.pdf, 2).unwrap();
        index.save(0, &page(0));
        assert!(index.is_current());
        std::fs::write(&f.pdf, b"%PDF-1.4 edited, and longer").unwrap();
        assert!(!index.is_current());
        drop(index);
        let index = store.open(&f.pdf, 2).unwrap();
        assert!(!index.has(0));
        // A different page count is a different document too.
        assert!(!store.open(&f.pdf, 3).unwrap().has(0));
    }

    #[test]
    fn a_second_reader_takes_over_writing() {
        let f = Fixture::new("two");
        let store = f.store(16);
        let mut first = store.open(&f.pdf, 3).unwrap();
        first.save(0, &page(0));
        let mut second = store.open(&f.pdf, 3).unwrap();
        assert!(first.writer && !second.writer);
        assert!(second.load(0).is_some());
        second.save(1, &page(1));
        assert!(!second.has(1), "only one writes at a time");
        first.save(1, &page(1));
        drop(first);
        second.save(2, &page(2));
        assert!(second.writer && second.load(1).is_some() && second.load(2).is_some());
    }

    #[test]
    fn a_document_stops_growing_at_the_limit() {
        let f = Fixture::new("limit");
        let mut index = f.store(1).open(&f.pdf, 1000).unwrap();
        let big = PageText::new(&"word ".repeat(20_000));
        for p in 0..1000 {
            index.save(p, &big);
        }
        let len = std::fs::metadata(index.path()).unwrap().len();
        assert!(len <= MB && len > MB / 2, "{len}");
    }

    #[test]
    fn sweep_drops_expired_then_least_recently_used() {
        let f = Fixture::new("sweep");
        let store = f.store(1);
        std::fs::create_dir_all(&store.dir).unwrap();
        let now = SystemTime::now();
        let file = |name: &str, kb: usize, days_ago: u64| {
            let path = store.dir.join(format!("{name}.{EXTENSION}"));
            std::fs::write(&path, vec![0; kb * 1024]).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(now - Duration::from_secs(days_ago * DAY))
                .unwrap();
            path
        };
        let expired = file("expired", 10, 31);
        let old = file("old", 400, 5);
        let middle = file("middle", 400, 3);
        let recent = file("recent", 400, 1);
        let other = store.dir.join("notes.txt");
        std::fs::write(&other, b"not ours").unwrap();
        sweep_at(&store, now);
        assert!(!expired.exists(), "unused for more than 30 days");
        assert!(!old.exists(), "least recently used, to fit 1 MB");
        assert!(middle.exists() && recent.exists() && other.exists());

        // A document being written is never deleted.
        let locked = File::open(&middle).unwrap();
        locked.try_lock().unwrap();
        let store = Store { limits: Limits { max_mb: 0, ..store.limits }, ..store };
        sweep_at(&store, now);
        assert!(middle.exists() && !recent.exists());
        drop(locked);
        store.clear();
        assert!(!middle.exists() && other.exists());
    }
}
