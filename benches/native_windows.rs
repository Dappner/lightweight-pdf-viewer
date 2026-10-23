//! Native Metal benchmark: real windows, paired resizes, and zooming with both open.
//!
//! cargo bench --bench native_windows
//! BENCH_REPORT=/tmp/native.json cargo bench --bench native_windows
//! BENCH_FRAME_LATENCY=2 uses three Metal buffers per window (default 1 = two).
//! Compare the old presentation path by adding:
//!   --features egui-wgpu/macos-window-resize-jitter-fix
//!
//! Opens temporary windows for ~24 seconds. Run when disrupting desktop focus is OK.
//! UI callback intervals include presentation/event-loop delays between frames. The
//! callback's own duration is recorded separately; neither is GPU execution time.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{Context, ViewportCommand, ViewportId, pos2, vec2};
use lightweight_pdf_viewer::{
    app::{App, Command},
    config::Config,
    grey,
    history::History,
};
use serde::Serialize;

const PHASES: [(&str, f64); 6] = [
    ("idle", 3.0),
    ("paired_resize", 6.0),
    ("after_resize", 3.0),
    ("zoom_first", 4.0),
    ("zoom_second", 4.0),
    ("after_zoom", 3.0),
];

#[derive(Default)]
struct Samples {
    intervals: Vec<f64>,
    callbacks: Vec<f64>,
    actions: usize,
    sizes: std::collections::HashSet<(ViewportId, u32, u32)>,
    footprint_peak: u64,
}

#[derive(Serialize)]
struct Distribution {
    count: usize,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
    over_100_ms: usize,
    over_500_ms: usize,
}

fn distribution(values: &[f64]) -> Distribution {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let percentile =
        |p: f64| sorted.get(((sorted.len().saturating_sub(1)) as f64 * p) as usize).copied().unwrap_or(0.0);
    Distribution {
        count: sorted.len(),
        p50_ms: percentile(0.5),
        p95_ms: percentile(0.95),
        max_ms: percentile(1.0),
        over_100_ms: sorted.iter().filter(|v| **v > 100.0).count(),
        over_500_ms: sorted.iter().filter(|v| **v > 500.0).count(),
    }
}

#[derive(Serialize)]
struct PhaseReport {
    phase: &'static str,
    ui_intervals: Distribution,
    ui_callbacks: Distribution,
    actions: usize,
    distinct_sizes: usize,
    footprint_peak_bytes: u64,
}

struct Benchmark {
    app: App,
    started: Instant,
    phase_started: Option<Instant>,
    phase: usize,
    last_frame: Option<Instant>,
    last_action: Instant,
    samples: Vec<Samples>,
    report: PathBuf,
    finished: bool,
    frame_latency: u32,
}

impl Benchmark {
    fn finish(&mut self, ctx: &Context) {
        let reports: Vec<_> = PHASES
            .iter()
            .zip(&self.samples)
            .map(|((name, _), samples)| PhaseReport {
                phase: name,
                ui_intervals: distribution(&samples.intervals),
                ui_callbacks: distribution(&samples.callbacks),
                actions: samples.actions,
                distinct_sizes: samples.sizes.len(),
                footprint_peak_bytes: samples.footprint_peak,
            })
            .collect();
        let json = serde_json::to_string_pretty(&serde_json::json!({
            "metal_buffers_per_window": self.frame_latency + 1,
            "phases": reports,
        }))
        .unwrap();
        std::fs::write(&self.report, &json).unwrap();
        println!("{json}\nReport: {}", self.report.display());
        self.finished = true;
        self.app.run(ctx, 0, Command::Quit);
    }
}

impl eframe::App for Benchmark {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        if self.finished {
            return;
        }
        if self.phase_started.is_none() {
            self.app.ui(ctx);
            let stats = self.app.stats();
            if stats.windows == 2 && stats.windows_ready && self.started.elapsed().as_secs_f64() >= 2.0 {
                self.phase_started = Some(now);
                self.last_frame = None;
                self.last_action = now;
            } else if self.started.elapsed().as_secs() > 20 {
                eprintln!("Native benchmark could not prepare two rendered windows");
                std::process::exit(2);
            }
            ctx.request_repaint_after(Duration::from_millis(16));
            return;
        }

        // Charge the interval to the phase that submitted the previous frame, so a
        // long presentation wait at a phase boundary remains in that phase's report.
        if let Some(previous) = self.last_frame.replace(now) {
            self.samples[self.phase].intervals.push(now.duration_since(previous).as_secs_f64() * 1000.0);
        }
        if self.phase_started.unwrap().elapsed().as_secs_f64() >= PHASES[self.phase].1 {
            self.phase += 1;
            if self.phase == PHASES.len() {
                self.finish(ctx);
                return;
            }
            self.phase_started = Some(now);
            self.last_action = now;
        }

        let ids: Vec<_> = ctx.input(|i| i.raw.viewports.keys().copied().collect());
        let samples = &mut self.samples[self.phase];
        if let Some(usage) = lightweight_pdf_viewer::memory::usage() {
            samples.footprint_peak = samples.footprint_peak.max(usage.footprint);
        }
        ctx.input(|i| {
            for (id, viewport) in &i.raw.viewports {
                if let Some(rect) = viewport.inner_rect {
                    samples.sizes.insert((*id, rect.width() as u32, rect.height() as u32));
                }
            }
        });
        if now.duration_since(self.last_action) >= Duration::from_millis(if self.phase == 1 { 32 } else { 180 }) {
            self.last_action = now;
            match self.phase {
                1 => {
                    let width = 700.0 + (self.phase_started.unwrap().elapsed().as_secs_f32() * 4.0).sin() * 140.0;
                    for id in ids {
                        let (x, w) =
                            if id == ViewportId::ROOT { (30.0, width) } else { (30.0 + width, 1400.0 - width) };
                        ctx.send_viewport_cmd_to(id, ViewportCommand::OuterPosition(pos2(x, 60.0)));
                        ctx.send_viewport_cmd_to(id, ViewportCommand::InnerSize(vec2(w, 850.0)));
                    }
                    samples.actions += 1;
                }
                3 | 4 => {
                    let command = if samples.actions % 8 < 4 { Command::ZoomIn } else { Command::ZoomOut };
                    self.app.run(ctx, self.phase - 3, command);
                    samples.actions += 1;
                }
                _ => (),
            }
        }
        self.app.ui(ctx);
        self.samples[self.phase].callbacks.push(now.elapsed().as_secs_f64() * 1000.0);
        ctx.request_repaint_after(Duration::from_millis(16));
    }
}

fn fixture(path: &std::path::Path) {
    let content = "BT /F1 22 Tf 60 730 Td (Native window resize and zoom benchmark) Tj ET\n".to_string()
        + &(0..80).map(|i| format!("60 {} m 550 {} l S\n", i * 8 + 40, i * 8 + 45)).collect::<String>();
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Count 8 /Kids [5 0 R 6 0 R 7 0 R 8 0 R 9 0 R 10 0 R 11 0 R 12 0 R] >>".into(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        format!("<< /Length {} >>\nstream\n{content}endstream", content.len()),
    ];
    objects.extend((0..8).map(|_| "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents 4 0 R >>".into()));
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
    std::fs::write(path, pdf).unwrap();
}

fn main() -> eframe::Result {
    let dir = std::env::temp_dir().join(format!("lightweight-pdf-native-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let files = [dir.join("first.pdf"), dir.join("second.pdf")];
    for file in &files {
        fixture(file);
    }
    let report = std::env::var_os("BENCH_REPORT").map(PathBuf::from).unwrap_or_else(|| dir.join("report.json"));
    // A compositor stall must not leave benchmark windows around indefinitely.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(60));
        eprintln!("Native benchmark timed out");
        std::process::exit(2);
    });
    let options = eframe::NativeOptions {
        viewport: lightweight_pdf_viewer::app::window_at(pos2(30.0, 60.0), vec2(700.0, 850.0)),
        renderer: eframe::Renderer::Wgpu,
        wgpu_options: egui_wgpu::WgpuConfiguration { desired_maximum_frame_latency: Some(1), ..Default::default() },
        ..Default::default()
    };
    let frame_latency: u32 = std::env::var("BENCH_FRAME_LATENCY")
        .ok()
        .map(|v| v.parse().expect("BENCH_FRAME_LATENCY must be 1 or 2"))
        .unwrap_or(1);
    assert!((1..=2).contains(&frame_latency));
    let mut options = options;
    options.wgpu_options.desired_maximum_frame_latency = Some(frame_latency);
    let dithering = options.dithering;
    let result = eframe::run_native(
        "PDF native benchmark",
        options,
        Box::new(move |cc| {
            grey::install(cc.wgpu_render_state.as_ref().unwrap(), dithering);
            let mut app = App::new(&cc.egui_ctx, Config::default(), History::default(), files.into());
            app.run(&cc.egui_ctx, 0, Command::MoveTabToNewWindow);
            let now = Instant::now();
            Ok(Box::new(Benchmark {
                app,
                started: now,
                phase_started: None,
                phase: 0,
                last_frame: None,
                last_action: now,
                samples: PHASES.iter().map(|_| Samples::default()).collect(),
                report,
                finished: false,
                frame_latency,
            }))
        }),
    );
    for file in
        std::fs::read_dir(&dir).unwrap().flatten().filter(|f| f.path().extension().is_some_and(|ext| ext == "pdf"))
    {
        let _ = std::fs::remove_file(file.path());
    }
    result
}
