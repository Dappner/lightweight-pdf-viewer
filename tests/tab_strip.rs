//! Headless checks of tab strip mouse handling (no window; egui only).

use eframe::egui::{
    self, Event, Modifiers, PointerButton, Pos2, RawInput, Rect, ViewportCommand, ViewportId, ViewportInfo, pos2,
    vec2,
};
use lightweight_pdf_viewer::{app::App, config::Config, history::History};

/// Tab geometry for a 1000pt-wide window with two tabs (see `tab_bar.rs`): tabs start at
/// x = 122 (after the traffic lights and view button) and are 220pt wide; the strip's
/// vertical centre is y = 16.
const FIRST_TAB: Pos2 = pos2(122.0 + 110.0, 16.0);
const SECOND_TAB: Pos2 = pos2(122.0 + 220.0 + 110.0, 16.0);
const EMPTY_STRIP: Pos2 = pos2(700.0, 16.0);

struct Harness {
    ctx: egui::Context,
    app: App,
    time: f64,
}

impl Harness {
    fn new() -> Self {
        let ctx = egui::Context::default();
        let mut app = App::new(&ctx, Config::default(), History::default(), Vec::new());
        app.open(&ctx, "/nonexistent/a.pdf".into());
        app.open(&ctx, "/nonexistent/b.pdf".into());
        let mut h = Self { ctx, app, time: 0.0 };
        h.frame(vec![]);
        h
    }

    /// Runs one frame and returns the viewport commands the app issued.
    fn frame(&mut self, events: Vec<Event>) -> Vec<ViewportCommand> {
        self.time += 1.0 / 60.0;
        let mut raw = RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 800.0))),
            time: Some(self.time),
            events,
            ..Default::default()
        };
        raw.viewports.insert(ViewportId::ROOT, ViewportInfo { native_pixels_per_point: Some(1.0), ..Default::default() });
        let app = &mut self.app;
        let out = self.ctx.run(raw, |ctx| app.ui(ctx));
        out.viewport_output.get(&ViewportId::ROOT).map(|v| v.commands.clone()).unwrap_or_default()
    }

    fn button(&mut self, pos: Pos2, pressed: bool) -> Vec<ViewportCommand> {
        self.frame(vec![
            Event::PointerMoved(pos),
            Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE },
        ])
    }

    fn drag(&mut self, from: Pos2, to: Pos2) -> Vec<ViewportCommand> {
        let mut commands = self.button(from, true);
        for i in 1..=10 {
            commands.extend(self.frame(vec![Event::PointerMoved(from + (to - from) * i as f32 / 10.0)]));
        }
        commands.extend(self.button(to, false));
        commands
    }
}

fn starts_window_drag(commands: &[ViewportCommand]) -> bool {
    commands.iter().any(|c| matches!(c, ViewportCommand::StartDrag))
}

#[test]
fn clicking_a_tab_selects_it() {
    let mut h = Harness::new();
    assert_eq!(h.app.stats().active, 1, "the last opened tab starts active");
    h.button(FIRST_TAB, true);
    let commands = h.button(FIRST_TAB, false);
    assert_eq!(h.app.stats().active, 0);
    assert!(!starts_window_drag(&commands));
}

#[test]
fn dragging_a_tab_does_not_move_the_window() {
    let mut h = Harness::new();
    let commands = h.drag(SECOND_TAB, SECOND_TAB - vec2(200.0, 0.0));
    assert!(!starts_window_drag(&commands), "tab drag started a window drag");
}

#[test]
fn dragging_empty_strip_moves_the_window() {
    let mut h = Harness::new();
    let commands = h.drag(EMPTY_STRIP, EMPTY_STRIP + vec2(50.0, 0.0));
    assert!(starts_window_drag(&commands));
}
