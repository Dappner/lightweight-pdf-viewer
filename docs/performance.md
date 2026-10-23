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

- Native two-window presentation: the October 18 live resize capture found 1,033
  of 1,660 main-thread samples in `CAMetalLayer nextDrawable`, with the PDF worker
  idle throughout. Zooming showed the same path (871 of 1,258 main-thread samples;
  PDF worker idle in 1,234 of 1,253 samples). The native benchmark below reproduced
  the stalls and verified the presentation change.
- Thread QoS on battery power is unmeasured; see the TODO on `Renderer::spawn`.
- Zooming waits a fixed 150 ms (`ZOOM_SETTLE` in `src/view.rs`) of the ~250 ms zoom-to-sharp on `balanced`. Shortening it throws away more renders during a slow pinch.
- The snapshot's QOI encode runs on the render thread (TODO on `render_snapshot`).

## Unfocused windows

Open windows retain their rendered pages under every memory preset. Losing keyboard
focus no longer triggers app-wide tile release: a window may still be visible on
another monitor. The cache continues to protect the tiles used by each window's
latest frame and evict older pages under its budget. Inactive tabs still release
their caches after the tab-switch grace period.

Removing app-wide background sleep also removes its unsafe snapshot-to-tile handoff.
Tab-switch snapshots accept late results after switching back, preserve the previous
fallback if a replacement fails, and remain visible when sharp tiles fail to render.
Scrolling or zooming invalidates a snapshot because its geometry no longer matches.

## Native resize and zoom regression benchmark

`benches/native_windows.rs` opens two real windows with generated PDFs, alternates
paired window sizes, zooms each window, then closes. It measures root UI callback
intervals (including event-loop/presentation delays) and callback durations separately.
It also records action counts, observed window sizes and process footprint. Settings
and PDFs in the user's session are untouched. This opens and moves desktop windows:
warn the user before running it while they are working.

On macOS 27.0 / Apple Silicon, October 18 (one run per configuration):

| Configuration | Resize p95 / max UI interval | Second-window zoom max | After-zoom max |
| --- | --- | --- | --- |
| Transaction resize workaround, 2 buffers/window | 1009 / 1021 ms | 1024 ms | 1005 ms |
| Workaround disabled, 2 buffers/window | 19 / 28 ms | 15 ms | 11 ms |
| Workaround disabled, 3 buffers/window | 14 / 26 ms | 16 ms | 12 ms |

The old path completed only 11 resize steps in six seconds; the fixed path completed
157. Reports: [before](benchmarks/2026-10-18-native-windows/before.json),
[fixed](benchmarks/2026-10-18-native-windows/fixed.json),
[three buffers](benchmarks/2026-10-18-native-windows/three.json).

The enabled `macos-window-resize-jitter-fix` uses transaction-based Metal presentation,
but eframe 0.33.3 tracks only the last resized viewport. Interleaved window resizes
can replace that tracking slot without clearing the previous surface's transaction
mode. Disable this optional workaround and select the Metal backend explicitly.
Keep two buffers per window: three modestly improved resize p95 in this run but was
not needed to remove the second-long stalls. Possible tradeoff: visual jitter while
manually resizing, which the disabled workaround was intended to suppress. These
results measure this synthetic workload, not every PDF or tiling manager gesture.

Run after announcing the temporary desktop disruption:

```sh
BENCH_REPORT=/tmp/pdf-native-fixed.json cargo bench --bench native_windows
python3 scripts/check-native.py /tmp/pdf-native-fixed.json
# Reproduce the old path for comparison:
BENCH_REPORT=/tmp/pdf-native-before.json cargo bench --bench native_windows --features egui-wgpu/macos-window-resize-jitter-fix
python3 scripts/check-native.py /tmp/pdf-native-fixed.json --before /tmp/pdf-native-before.json
# Optional three-buffer comparison:
BENCH_FRAME_LATENCY=2 BENCH_REPORT=/tmp/pdf-native-three.json cargo bench --bench native_windows
```

The checker rejects insufficient activity or a UI update gap over 500 ms by default;
`--max-ui-ms` adjusts that explicit threshold. The benchmark has a 60-second watchdog
so a presentation hang terminates the temporary process instead of leaving it running.

## Dropping tabs into another window

While a tab is dragged, sample the native pointer and button state before egui handles
the frame. `egui-winit` clears its pointer on `CursorLeft` and can omit a subsequent
mouse-up outside the source window. Supply the current coordinates and a release
when necessary so the ordinary drag response and screen-coordinate drop hit test
complete. Poll only during tab drags. The headless regression covers losing the pointer
and dropping into another strip from either the main or a secondary window, including
closing the emptied source and rerouting the destination viewport.
