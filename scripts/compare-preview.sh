#!/bin/sh
# Compares memory and idle CPU of Lightweight PDF and Preview with the same PDFs open.
#
#   scripts/compare-preview.sh a.pdf b.pdf ...
#
# Opens real windows for about a minute and cycles through the documents with keystrokes
# (needs Accessibility permission for the terminal). Preview runs as a separate, fresh
# instance that ignores saved window state, so an already-running Preview is left alone.
set -eu
cd "$(dirname "$0")/.."

[ $# -gt 0 ] || { echo "usage: $0 <pdf> [<pdf> ...]" >&2; exit 2; }
SETTLE=${SETTLE:-6}
cargo build --release -q
VIEWER="$PWD/target/release/lightweight-pdf-viewer"

# Fails loudly if `footprint` fails or prints nothing parseable, rather than leaving a blank column.
footprint_of() {
    out=$(footprint "$1") || { echo "footprint failed for pid $1" >&2; exit 1; }
    mb=$(printf '%s\n' "$out" | sed -n 's/.*Footprint: \([0-9.]* [KMG]B\).*/\1/p' | head -1)
    [ -n "$mb" ] || { echo "no footprint in output for pid $1" >&2; exit 1; }
    printf '%s\n' "$mb"
}
# Average CPU over 5 s of idling.
cpu_of() { top -l 2 -s 5 -pid "$1" -stats cpu | tail -1 | tr -d ' '; }

# Brings the process forward and shows its next document: ⌃Tab moves between tabs,
# ⌘` between windows (Preview may use either, depending on the "prefer tabs" setting).
next_document() {
    osascript -e "tell application \"System Events\"
        set frontmost of (first process whose unix id is $1) to true
        delay 0.3
        key code 48 using control down
        keystroke \"\`\" using command down
    end tell" >/dev/null
    sleep 1.5
}

newest_pid() { pgrep -n -f "$1"; }

measure() { # name pid ndocs
    sleep "$SETTLE"
    opened=$(footprint_of "$2")
    i=1
    while [ "$i" -lt "$3" ]; do next_document "$2"; i=$((i + 1)); done
    sleep 2
    visited=$(footprint_of "$2")
    cpu=$(cpu_of "$2")
    printf "%-16s %6s docs   opened %9s   after visiting all %9s   idle CPU %s%%\n" "$1" "$3" "$opened" "$visited" "$cpu"
}

run_viewer() { # files...
    "$VIEWER" "$@" >/dev/null 2>&1 &
    pid=$!
    measure "Lightweight PDF" "$pid" "$#"
    kill "$pid"
    wait "$pid" 2>/dev/null || true
}

run_preview() { # files...
    open -n -a Preview "$@" --args -ApplePersistenceIgnoreState YES
    sleep 1
    pid=$(newest_pid "Preview.app/Contents/MacOS/Preview")
    measure "Preview" "$pid" "$#"
    kill "$pid"
    sleep 1
}

echo "== 1 document"
run_viewer "$1"
run_preview "$1"
echo
echo "== $# documents"
run_viewer "$@"
run_preview "$@"
