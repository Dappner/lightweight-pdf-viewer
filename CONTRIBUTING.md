# Contributing

Bug reports and pull requests are welcome. For anything larger than a fix, open an issue first so we can agree on the approach.

## Building

You need macOS, [Rust](https://rustup.rs) and the Xcode Command Line Tools. `cargo run --release -- some.pdf` runs the app; `./scripts/bundle.sh` builds `Lightweight PDF.app`.

On other platforms the crate builds with a synthetic engine in place of CoreGraphics and PDFKit, so the portable core (layout, caching, search, text boxes) can be tested there. `cargo check --target aarch64-apple-darwin` type-checks the macOS code from Linux.

## Before sending a pull request

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

The tests that open real PDFs through PDFKit run on macOS only. Changes to rendering, memory or search should come with numbers from the benchmarks in the README; [docs/performance.md](docs/performance.md) records what has already been tried.

The app's point is to stay small and cheap: an idle tab should cost next to nothing, and nothing should redraw unless something changed. Features that cost memory or CPU while idle are unlikely to be accepted.
