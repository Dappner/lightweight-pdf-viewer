#!/bin/sh
# Regenerates assets/AppIcon.icns from the artwork in assets/icon-source.png.
set -eu
cd "$(dirname "$0")/.."

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
swift scripts/make-icon.swift assets/icon-source.png "$WORK/master.png"

SET="$WORK/AppIcon.iconset"
mkdir "$SET"
for s in 16 32 128 256 512; do
    sips -z $s $s "$WORK/master.png" --out "$SET/icon_${s}x${s}.png" >/dev/null
    sips -z $((s * 2)) $((s * 2)) "$WORK/master.png" --out "$SET/icon_${s}x${s}@2x.png" >/dev/null
done
mkdir -p assets
iconutil -c icns "$SET" -o assets/AppIcon.icns
echo "Wrote assets/AppIcon.icns"
