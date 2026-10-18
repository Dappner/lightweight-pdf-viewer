# Rendering performance

What has been tried to make rendering cheaper on Apple Silicon, what it measured, and what was kept. Read this before changing how tiles are drawn or stored: the rejected experiments are as useful as the kept ones.

All numbers are from an M1 Pro (8 performance + 2 efficiency cores) on AC power, September 2026, rendering at Retina scale.

## Measuring

| Command | What it answers |
| --- | --- |
| `cargo bench --bench render_spike -- a.pdf …` | CoreGraphics alone: cost per page of each rendering option, and how many tiles have no colour. `RENDER_ONLY=variants\|quality\|qos\|parallel\|gray` runs one mode. |
| `BENCH_PPP=2 cargo bench --bench session -- a.pdf …` | The whole app, headless: time to sharp when opening, switching tabs, scrolling and zooming; CPU; footprint; texture memory. |

Run the session benchmark at least twice before believing a difference of a few milliseconds or megabytes: single runs vary by that much. With two or more files its search phase often fails its search-worker checks on `main`; that is an existing bug, unrelated to rendering.

## Kept

### One CoreGraphics draw per row of tiles

CoreGraphics walks a page's whole content stream on every draw, however small the clip. Drawing each 512 px tile separately therefore repeated that walk once per tile. The render thread now draws each run of adjacent wanted tiles in a row into one strip and slices it into tiles (`render_tiles` in `src/renderer.rs`).

- CPU per page: 37–72% of before (`render_spike`, four PDFs).
- Session benchmark: scrolling to sharp 57 / 130 ms → 45 / 78 ms (p50 / max), scrolling CPU 2.39 → 1.92 s, opening a document 71 → 61 ms.
- Cost: a strip is held while it is sliced, about 3 MB more peak footprint.
- Sliced tiles differ from tiles drawn alone by at most one level where images are resampled, plus a few pixels at a clip edge. The render test (`RENDER_TEST_PDF`) allows 0.01% of pixels to differ by more than one level.

### Several render threads, `fast` preset only

Each render thread takes one page's work from the shared queue and opens its own copy of the document (`Tuning::render_threads` in `src/memory.rs`). `fast` uses three; `low` and `balanced` use one.

- With two threads: scrolling to sharp p50 46 → 27–31 ms, p95 66 → 49–58 ms; two-window scrolling 52 → 31 ms; zoom 251 → 220 ms.
- Cost: up to ~13 MB more peak footprint (a second copy of CoreGraphics' per-document caches), and opening is ~5% slower because both threads open the document.
- Not on `balanced`: the default preset should not pay that memory.

### Single-channel textures for grey tiles

A tile whose every pixel is opaque with R = G = B is sent from the render thread as one byte a pixel and kept on the GPU as an R8 texture: a quarter of the memory. On Apple Silicon GPU memory is system memory. See `src/grey.rs`.

egui only uploads RGBA textures, so grey tiles are drawn with an egui-wgpu paint callback. Its shader, `src/grey.wgsl`, is egui-wgpu 0.33's `egui.wgsl` with only `sample_texture` changed to read one channel. The callback also copies egui's quad, sampler, blend state and uniforms, and draws in the whole screen's viewport rather than the rounded one egui gives callbacks. Only exact grey qualifies, so nothing draws differently:

- `grey_tiles_draw_exactly_as_rgba` draws a noise tile both ways on the real GPU (1:1, off the pixel grid, scaled up and down, clipped; gamma and sRGB targets) and requires identical bytes. Darkening the shader by 0.2% makes it fail.
- Screenshots of the app with grey tiles on and off had 0 differing pixels on four PDFs, including a page mixing text and colour figures at 272% zoom.

How many tiles are grey (`render_spike` gray mode, every page):

| Kind of PDF | Grey tiles | Texture memory vs RGBA |
| --- | --- | --- |
| Exam supplement, insurance certificate | 94–100% | 25–30% |
| 8-page text document | 88% | 35% |
| Thesis, course outline | 66–72% | 47–56% |
| Signed lease | 52% | 62% |
| Insurance policy | 27% | 80% |
| Colour slide decks | 0% | 100% |

In the session benchmark (`balanced`), texture memory when opening went 12.6 → 3.2 MB for a thesis, 25.2 → 6.3 MB for an exam supplement and 25.2 → 10.8 MB for lecture notes with figures; after a 3× zoom, 29.3 → 9.5 MB. Speed and process footprint were unchanged. The grey check costs 0.04 ms per 512 px tile.

The texture budget stays 48 MB: once a long read fills it, it holds up to four times as many grey tiles instead of using less memory.

When upgrading egui-wgpu, diff `grey.wgsl` against the new `egui.wgsl` and the callback against its `Renderer`; the GPU test fails if they drift apart.

## Tried and rejected

| Experiment | Result |
| --- | --- |
| 256 px tiles | 1.6–3.6× the CPU per page: more draws, each walking the whole page. |
| 1024 px tiles | As cheap as row draws (36–75% of before), but a page's first tile arrives up to 2.2× later, and textures are coarser. |
| One draw per whole page | Cheapest (19–62% of before), but no tile shows until the whole page is drawn (up to 42 ms for the first tile). |
| Default, Medium or Low interpolation instead of High | At most 5% faster; Medium and Low change up to 0.8% of pixels, by as much as 109 levels. |
| Opaque context (`NoneSkipLast`) | 0–31% slower, not faster. |
| Render thread QoS (user-initiated, utility, background) | No measurable difference on AC power. |
| Two render threads on `balanced` | Kept for `fast` only; see above. |

## Open

- Native two-window presentation: an October 18 live sample found 2,451 of 3,491
  main-thread samples in `CAMetalLayer nextDrawable`, while the PDF render worker
  was waiting for work in 3,489 samples. This identifies presentation waiting,
  but does not by itself prove the two-drawable limit is wrong: vsync also waits
  there normally. Keep that limit while fixing unnecessary cross-window wakeups.
  Render results now wake the document's owning viewport explicitly; search and
  text workers share a repaint destination that follows a moved tab. Closing the
  main window and handing another window over to it also reroutes those workers.
  Idle windows keep their last native frame and cached visible tiles; focusing
  and scrolling runs the selected window's deferred callback and prioritizes its
  render queue. The user confirmed that this fixed the slowdown after rebuilding
  and reopening. Native before/after timings remain unmeasured: the headless
  benchmark cannot measure drawable acquisition or input stalls.
- Thread QoS on battery power is unmeasured; see the TODO on `Renderer::spawn`.
- Zooming waits a fixed 150 ms (`ZOOM_SETTLE` in `src/view.rs`) of the ~250 ms zoom-to-sharp on `balanced`. Shortening it throws away more renders during a slow pinch.
- The snapshot's QOI encode runs on the render thread (TODO on `render_snapshot`).
