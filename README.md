# Lightweight PDF

A small, fast PDF viewer for macOS, with tabs.

Open as many PDFs as you like. Tabs you aren't looking at use almost no memory or CPU.

## Why

Most PDF apps are heavy. This one does one thing: shows PDFs, quickly and cheaply.

- **Tabs.** Keep a stack of PDFs open in one window.
- **Idle tabs cost almost nothing.** When you switch away from a tab, it keeps a small compressed snapshot of what was on screen and closes the document. Switching back shows the snapshot straight away while the pages redraw.
- **Light on memory.** Rendered pages share one fixed 64 MB GPU budget across all tabs. Only the pages on screen are rendered.
- **Light on CPU.** Nothing redraws unless something changes. Pages are drawn with macOS's built-in PDF engine, the same one Preview uses.
- **Native.** Real macOS menu bar, Finder "Open With", drag and drop.

## Install

You need [Rust](https://rustup.rs) installed.

```sh
git clone <this repo>
cd lightweight-pdf-viewer
./scripts/bundle.sh --install
```

This builds `Lightweight PDF.app` and puts it in `~/Applications`. PDFs then show it under **Open With** in Finder. To make it your default PDF app, choose a PDF, open **Get Info**, pick Lightweight PDF under **Open with**, and click **Change All**.

Or run it straight from source:

```sh
cargo run --release -- some.pdf another.pdf
```

## Using it

Open PDFs with ⌘O, by dragging them onto the window, or from Finder. Drag the sidebar's edge to resize it.

The app reopens the tabs you had open last time. Settings live in `~/Library/Application Support/Lightweight PDF/config.json`; set `"restore_tabs": false` there to start empty instead.

| Shortcut | Action |
| --- | --- |
| ⌘O | Open |
| ⌘K | Open a recent file (type to filter; most-used and latest first) |
| ⌘W | Close tab |
| ⌘1 … ⌘9 | Go to tab (⌘9 is the last tab) |
| ⌘⇧[ / ⌘⇧] or ⌃⇧Tab / ⌃Tab | Previous / next tab |
| ⌘B | Show or hide page thumbnails |
| ⌘= / ⌘- | Zoom in / out |
| ⌘0 | Fit page width |
| Space / ⇧Space | Page down / up |
| ⌘↑ / ⌘↓ | First / last page |

## Benchmark

A headless benchmark opens real PDFs through the app and reports speed and memory:

```sh
cargo bench --bench session -- a.pdf b.pdf
```

## Requirements

macOS 11 or later. It uses macOS's own PDF engine, so it runs on macOS only.

## License

MIT
