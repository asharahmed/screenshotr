#!/bin/sh
# Builds Screenshotr, signs it with a Developer ID certificate, and installs it to /Applications.
# An Apple-issued certificate gives the app a designated requirement tied to the team ID rather than
# to one build's hash, so macOS privacy grants such as Screen Recording survive rebuilds.
set -e

IDENTITY="${SIGNING_IDENTITY:-Developer ID Application: Ashar Ahmed (AXZA9Y8669)}"
BUNDLE_ID="ca.aahmed.screenshotr"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUILT="$ROOT/src-tauri/target/release/bundle/macos/Screenshotr.app"
DEST="/Applications/Screenshotr.app"

if ! security find-identity -v -p codesigning | grep -qF "\"$IDENTITY\""; then
  echo "Signing identity \"$IDENTITY\" not found. Set SIGNING_IDENTITY to one listed by:" >&2
  echo "  security find-identity -v -p codesigning" >&2
  exit 1
fi

(cd "$ROOT" && cargo tauri build --bundles app)

codesign --force --deep --sign "$IDENTITY" "$BUILT"
codesign --verify "$BUILT"

# Quit through a normal quit request so the app restores the system screenshot shortcuts.
if pgrep -qf "$DEST/Contents/MacOS/Screenshotr"; then
  osascript -e "tell application id \"$BUNDLE_ID\" to quit" >/dev/null 2>&1 || true
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    pgrep -qf "$DEST/Contents/MacOS/Screenshotr" || break
    sleep 0.5
  done
  pkill -TERM -f "$DEST/Contents/MacOS/Screenshotr" 2>/dev/null || true
  sleep 1
fi

rm -rf "$DEST"
ditto "$BUILT" "$DEST"
open "$DEST"
echo "Installed $DEST (signed with \"$IDENTITY\")."
