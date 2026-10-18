//! A worker's repaint destination follows its tab when it moves between windows.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui::{Context, ViewportId};

#[derive(Clone)]
pub(crate) struct Repaint {
    ctx: Context,
    viewport: Arc<Mutex<ViewportId>>,
}

impl Repaint {
    pub fn new(ctx: &Context) -> Self {
        Self { ctx: ctx.clone(), viewport: Arc::new(Mutex::new(ctx.viewport_id())) }
    }

    pub fn set_viewport(&self, viewport: ViewportId) {
        *self.viewport.lock().unwrap() = viewport;
    }

    pub fn request(&self) {
        self.after(Duration::ZERO);
    }

    pub fn after(&self, delay: Duration) {
        let viewport = *self.viewport.lock().unwrap();
        self.ctx.request_repaint_after_for(delay, viewport);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_worker_follows_its_tab_without_waking_the_main_window() {
        let ctx = Context::default();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        ctx.set_request_repaint_callback(move |info| captured.lock().unwrap().push(info.viewport_id));
        let owner = Repaint::new(&ctx);
        let worker = owner.clone();
        let first = ViewportId::from_hash_of("first");
        let second = ViewportId::from_hash_of("second");
        owner.set_viewport(first);
        worker.request();
        owner.set_viewport(second);
        worker.after(Duration::from_millis(50));
        assert_eq!(*requests.lock().unwrap(), [first, second]);
    }
}
