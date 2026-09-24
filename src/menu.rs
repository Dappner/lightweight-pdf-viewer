//! Native menu bar. Items queue [`Command`]s that the app runs on its next frame. AppKit
//! matches the ⌘ shortcuts shown in the menus before key events reach egui. The page layout
//! items show a checkmark next to the active tab's layout.

use std::sync::{Mutex, OnceLock};

use eframe::egui;
use objc2::rc::Retained;
use objc2::runtime::Sel;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSControlStateValueOff, NSControlStateValueOn, NSEventModifierFlags, NSMenu, NSMenuItem,
};
use objc2_foundation::{NSObject, NSString};

use crate::app::Command;
use crate::layout::PageLayout;

static PENDING: Mutex<Vec<Command>> = Mutex::new(Vec::new());
static CONTEXT: OnceLock<egui::Context> = OnceLock::new();
/// The command behind each app menu item, indexed by the item's tag.
static COMMANDS: Mutex<Vec<Command>> = Mutex::new(Vec::new());
/// The active tab's page layout; `None` disables the layout items.
static LAYOUT: Mutex<Option<PageLayout>> = Mutex::new(None);

define_class!(
    // SAFETY: NSObject has no subclassing requirements and MenuTarget does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "LightweightPdfMenuTarget"]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(perform:))]
        fn perform(&self, item: &NSMenuItem) {
            let command = COMMANDS.lock().unwrap().get(item.tag() as usize).copied();
            if let Some(command) = command {
                PENDING.lock().unwrap().push(command);
                if let Some(ctx) = CONTEXT.get() {
                    ctx.request_repaint();
                }
            }
        }

        /// AppKit asks before showing each item: this sets checkmarks and disables items.
        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, item: &NSMenuItem) -> bool {
            validate(item)
        }
    }
);

/// Whether `item` is enabled; checks the active layout's item.
fn validate(item: &NSMenuItem) -> bool {
    let command = COMMANDS.lock().unwrap().get(item.tag() as usize).copied();
    let Some(Command::SetLayout(layout)) = command else { return true };
    let current = *LAYOUT.lock().unwrap();
    item.setState(if current == Some(layout) { NSControlStateValueOn } else { NSControlStateValueOff });
    current.is_some()
}

impl MenuTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

enum Entry {
    /// Runs an app [`Command`].
    Item(&'static str, Command, &'static str, NSEventModifierFlags),
    /// Sends a standard AppKit action up the responder chain.
    System(&'static str, Sel, &'static str, NSEventModifierFlags),
    Separator,
}

/// Tells the menu which layout the active tab uses.
pub fn set_layout(layout: Option<PageLayout>) {
    *LAYOUT.lock().unwrap() = layout;
}

pub fn take() -> Vec<Command> {
    std::mem::take(&mut PENDING.lock().unwrap())
}

/// Replaces winit's default menu bar. Call on the main thread once the app has launched.
pub fn install(ctx: &egui::Context) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let _ = CONTEXT.set(ctx.clone());

    let cmd = NSEventModifierFlags::Command;
    let cmd_shift = cmd | NSEventModifierFlags::Shift;
    let cmd_option = cmd | NSEventModifierFlags::Option;
    let none = NSEventModifierFlags::empty();
    use Entry::*;
    let menus = [
        ("Lightweight PDF", vec![
            System("About Lightweight PDF", sel!(orderFrontStandardAboutPanel:), "", none),
            Separator,
            System("Hide Lightweight PDF", sel!(hide:), "h", cmd),
            System("Hide Others", sel!(hideOtherApplications:), "h", cmd_option),
            System("Show All", sel!(unhideAllApplications:), "", none),
            Separator,
            Item("Quit Lightweight PDF", Command::Quit, "q", cmd),
        ]),
        ("File", vec![
            Item("New Window", Command::NewWindow, "n", cmd),
            Item("Open…", Command::Open, "o", cmd),
            Item("Open Recent…", Command::QuickOpen, "k", cmd),
            Separator,
            Item("Close Tab", Command::CloseTab, "w", cmd),
        ]),
        ("Edit", vec![
            Item("Find…", Command::Find, "f", cmd),
            Item("Find Next", Command::FindNext, "g", cmd),
            Item("Find Previous", Command::FindPrevious, "g", cmd_shift),
            Separator,
            Item("Clear Search Cache", Command::ClearSearchCache, "", none),
        ]),
        ("View", vec![
            Item("Toggle Sidebar", Command::ToggleSidebar, "b", cmd),
            Item("Pages / Outline", Command::ToggleOutline, "b", cmd_option),
            Separator,
            Item("Single Page", Command::SetLayout(PageLayout::Single), "1", cmd_option),
            Item("Two Pages", Command::SetLayout(PageLayout::TwoPages), "2", cmd_option),
            Item("Book", Command::SetLayout(PageLayout::Book), "3", cmd_option),
            Separator,
            Item("Zoom In", Command::ZoomIn, "=", cmd),
            Item("Zoom Out", Command::ZoomOut, "-", cmd),
            Item("Fit Width", Command::ZoomToFit, "0", cmd),
        ]),
        ("Window", vec![
            System("Minimize", sel!(performMiniaturize:), "m", cmd),
            System("Zoom", sel!(performZoom:), "", none),
            Separator,
            Item("Show Previous Tab", Command::PreviousTab, "[", cmd_shift),
            Item("Show Next Tab", Command::NextTab, "]", cmd_shift),
            Separator,
            Item("Move Tab to New Window", Command::MoveTabToNewWindow, "", none),
            Item("Merge All Windows", Command::MergeAllWindows, "", none),
        ]),
    ];

    let target = MenuTarget::new(mtm);
    let app = NSApplication::sharedApplication(mtm);
    let bar = NSMenu::new(mtm);
    let mut commands = COMMANDS.lock().unwrap();
    for (title, entries) in menus {
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
        for entry in entries {
            let item = match entry {
                Separator => NSMenuItem::separatorItem(mtm),
                Item(title, command, key, modifiers) => {
                    let item = menu_item(mtm, title, sel!(perform:), key, modifiers);
                    // SAFETY: `target` is kept alive for the rest of the process below.
                    unsafe { item.setTarget(Some(&target)) };
                    item.setTag(commands.len() as isize);
                    commands.push(command);
                    item
                }
                // No target: AppKit sends these to the first responder / NSApplication.
                System(title, action, key, modifiers) => menu_item(mtm, title, action, key, modifiers),
            };
            menu.addItem(&item);
        }
        let root = NSMenuItem::new(mtm);
        root.setSubmenu(Some(&menu));
        bar.addItem(&root);
        if title == "Window" {
            app.setWindowsMenu(Some(&menu));
        }
    }
    app.setMainMenu(Some(&bar));
    // Menu items hold their target weakly.
    std::mem::forget(target);
}

fn menu_item(
    mtm: MainThreadMarker,
    title: &str,
    action: Sel,
    key: &str,
    modifiers: NSEventModifierFlags,
) -> Retained<NSMenuItem> {
    // SAFETY: `action` is a valid selector taking the sender as its only argument.
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(key),
        )
    };
    item.setKeyEquivalentModifierMask(modifiers);
    item
}
