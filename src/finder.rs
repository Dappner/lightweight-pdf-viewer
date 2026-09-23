//! Files opened from Finder: double-click, "Open With", or dropping onto the Dock icon.
//!
//! macOS delivers these to the app delegate's `application:openURLs:`. winit owns the
//! delegate and does not implement that method, so it is added to winit's delegate class at
//! runtime. It must exist before AppKit dispatches the launch-time open event, which happens
//! right after `NSApplicationWillFinishLaunchingNotification`, so that is when it's added.
//!
//! Paths are queued until the app picks them up, since the launch-time event arrives before
//! the egui app exists.

use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::{Mutex, OnceLock};

use block2::RcBlock;
use eframe::egui;
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{ffi, sel};
use objc2_foundation::{NSArray, NSNotification, NSNotificationCenter, NSString, NSURL};

static PENDING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static CONTEXT: OnceLock<egui::Context> = OnceLock::new();

/// Must be called before the event loop starts.
pub fn install() {
    let block = RcBlock::new(|_: NonNull<NSNotification>| add_open_urls_method());
    let name = NSString::from_str("NSApplicationWillFinishLaunchingNotification");
    let center = NSNotificationCenter::defaultCenter();
    // SAFETY: no object filter; the block runs on the posting (main) thread.
    let observer = unsafe { center.addObserverForName_object_queue_usingBlock(Some(&name), None, None, &block) };
    // The observer lives for the whole process.
    std::mem::forget(observer);
}

/// Lets queued files wake the UI.
pub fn set_context(ctx: &egui::Context) {
    let _ = CONTEXT.set(ctx.clone());
}

pub fn take() -> Vec<PathBuf> {
    std::mem::take(&mut PENDING.lock().unwrap())
}

fn add_open_urls_method() {
    let Some(class) = AnyClass::get(c"WinitApplicationDelegate") else {
        eprintln!("Opening files from Finder is unavailable: winit's app delegate not found");
        return;
    };
    let imp = open_urls as unsafe extern "C-unwind" fn(&AnyObject, Sel, &AnyObject, &NSArray<NSURL>);
    // SAFETY: the signature matches the "v@:@@" type encoding of
    // `-[NSApplicationDelegate application:openURLs:]`.
    unsafe {
        ffi::class_addMethod(
            class as *const AnyClass as *mut AnyClass,
            sel!(application:openURLs:),
            std::mem::transmute::<_, Imp>(imp),
            c"v@:@@".as_ptr(),
        );
    }
}

unsafe extern "C-unwind" fn open_urls(_this: &AnyObject, _cmd: Sel, _app: &AnyObject, urls: &NSArray<NSURL>) {
    PENDING.lock().unwrap().extend(urls.iter().filter_map(|url| url.to_file_path()));
    if let Some(ctx) = CONTEXT.get() {
        ctx.request_repaint();
    }
}
