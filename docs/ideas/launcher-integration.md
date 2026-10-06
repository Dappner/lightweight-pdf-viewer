# Working with the launcher

Idea date: 2026-09-27. Not built; nothing in the app depends on this.

How this viewer could work with [lightweight-launcher](../../../lightweight-launcher) when both are on one Mac.

## Not worth doing: reusing the launcher's file index

The launcher finds files with Spotlight by default. Its own index (`native/fileindex`) is in memory only while file search is in use. If the viewer built or read its own copy, it would pay that memory twice and depend on the launcher's private format. Keep the split: the launcher finds files (`fd type:pdf …`), and the viewer opens them as the default PDF app. If ⌘K ever needs PDFs you haven't opened, query Spotlight directly (`kMDItemContentTypeTree == "com.adobe.pdf"`).

## Worth doing, in order

1. **Open at a page.** Accept `--page N --find "q"` on launch, or a `lightweightpdf://open?path=…&page=…` URL. This is small and useful by itself, and the ideas below need it.

2. **Reading progress in the launcher.** `history.json` already stores frecency and where each file was left (page, position, zoom). The launcher could rank PDFs you read higher, show "p. 143/400 · read 2 days ago" as the subtitle, and reopen at that page.

3. **Search inside PDFs from the launcher.** The search cache (`src/search_index.rs`) holds page text for every PDF you've searched. `fd caq` could match text inside PDFs, and ↩ would open the page with the match highlighted. Spotlight's text index gives no page and often misses content. This would also serve the launcher's `docs/ideas/voice-file-search.md`. It needs:
   - A list saying which cache file belongs to which PDF: cache files are named by inode and modification time, not path.
   - Text extracted in the background when a PDF is opened, not only when it's searched. It costs CPU once per file.
   - A query command (e.g. `lightweight-pdf --query "caq" --json`), so the launcher never reads the binary cache files and their format can keep changing.

4. **One open history.** Each app only sees opens made through it. Pick one app to own the history rather than syncing both ways.

## For the viewer alone

- A document outline sidebar and working links.
- Register opened documents with the system (Handoff or Core Spotlight) so macOS Recents and Spotlight know the page you're on. That gives some of idea 2 to every app, not just the launcher.
