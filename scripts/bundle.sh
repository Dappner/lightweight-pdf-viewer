#!/bin/sh
# Builds "Lightweight PDF.app" into target/release/.
# Pass --install to also copy it into ~/Applications.
set -eu
cd "$(dirname "$0")/.."

NAME="Lightweight PDF"
BUNDLE_ID="com.dappner.lightweight-pdf"
EXECUTABLE="lightweight-pdf-viewer"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister

cargo build --release

APP="target/release/$NAME.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/release/$EXECUTABLE" "$APP/Contents/MacOS/"
cp assets/AppIcon.icns "$APP/Contents/Resources/"

# LSHandlerRank "Alternate" lists the app under "Open With" without taking over as the
# default PDF viewer; pick it as the default in Finder's Get Info if you want that.
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>$NAME</string>
    <key>CFBundleDisplayName</key><string>$NAME</string>
    <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
    <key>CFBundleExecutable</key><string>$EXECUTABLE</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key><string>PDF Document</string>
            <key>CFBundleTypeRole</key><string>Viewer</string>
            <key>LSHandlerRank</key><string>Alternate</string>
            <key>LSItemContentTypes</key><array><string>com.adobe.pdf</string></array>
        </dict>
    </array>
</dict>
</plist>
EOF

codesign --force --sign - "$APP"

if [ "${1:-}" = "--install" ]; then
    rm -rf "$HOME/Applications/$NAME.app"
    mkdir -p "$HOME/Applications"
    ditto "$APP" "$HOME/Applications/$NAME.app"
    # Keep only the installed copy registered, so "Open With" lists the app once.
    "$LSREGISTER" -u "$APP"
    APP="$HOME/Applications/$NAME.app"
fi

"$LSREGISTER" -f "$APP"
echo "Built $APP"
