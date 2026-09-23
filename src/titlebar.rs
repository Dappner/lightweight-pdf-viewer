//! Keeps macOS from treating clicks in the tab strip as window drags.
//!
//! The tab strip lives in the transparent title bar. AppKit starts a native window drag on a
//! mouse-down there whenever the view under the pointer answers YES to
//! `mouseDownCanMoveWindow`, which NSView does by default for non-opaque views like winit's,
//! so tab clicks never reached the app. Answering NO hands every click to the app; the tab
//! strip starts window drags itself from its empty areas.

use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
use objc2::{ffi, sel};

/// Call once the window exists (winit registers its view class when creating it).
pub fn install() {
    let Some(class) = AnyClass::get(c"WinitView") else {
        eprintln!("Tab clicks may move the window: winit's view class not found");
        return;
    };
    let imp = mouse_down_cannot_move_window as unsafe extern "C-unwind" fn(&AnyObject, Sel) -> Bool;
    // SAFETY: the signature matches the "B@:" type encoding of `-[NSView mouseDownCanMoveWindow]`.
    unsafe {
        ffi::class_addMethod(
            class as *const AnyClass as *mut AnyClass,
            sel!(mouseDownCanMoveWindow),
            std::mem::transmute::<unsafe extern "C-unwind" fn(&AnyObject, Sel) -> Bool, Imp>(imp),
            c"B@:".as_ptr(),
        );
    }
}

unsafe extern "C-unwind" fn mouse_down_cannot_move_window(_this: &AnyObject, _cmd: Sel) -> Bool {
    Bool::NO
}
