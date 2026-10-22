#!/bin/sh
# Builds a signed, notarized release of Screenshotr plus everything the in-app updater and
# Homebrew need. Publishes to GitHub and the Homebrew tap only with --publish.
#
#   scripts/release.sh            # build into dist/v<version>/ and stop
#   scripts/release.sh --publish  # also create the GitHub release and update the cask
set -e

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$ROOT/packaging/release.env"
PUBLISH=0
[ "$1" = "--publish" ] && PUBLISH=1

fail() { echo "release: $*" >&2; exit 1; }
case "$GITHUB_REPO" in OWNER/*|"") fail "set GITHUB_REPO in packaging/release.env" ;; esac
[ -f "$UPDATER_KEY" ] || fail "updater key not found at $UPDATER_KEY"
security find-identity -v -p codesigning | grep -qF "\"$SIGNING_IDENTITY\"" || fail "signing identity \"$SIGNING_IDENTITY\" not in keychain"

VERSION=$(python3 -c "import json;print(json.load(open('$ROOT/src-tauri/tauri.conf.json'))['package']['version'])")
ARCH=aarch64
BUNDLE="$ROOT/src-tauri/target/release/bundle"
APP="$BUNDLE/macos/Screenshotr.app"
OUT="$ROOT/dist/v$VERSION"
FEED="https://github.com/$GITHUB_REPO/releases/latest/download/latest.json"
rm -rf "$OUT"; mkdir -p "$OUT"
echo "==> Screenshotr $VERSION"

echo "==> Building"
(cd "$ROOT" && cargo tauri build --bundles app --config "{\"tauri\":{\"updater\":{\"endpoints\":[\"$FEED\"]}}}")

echo "==> Signing (hardened runtime, as notarization requires)"
codesign --force --options runtime --timestamp --sign "$SIGNING_IDENTITY" "$APP"
codesign --verify --strict --verbose=1 "$APP"

echo "==> Notarizing the app"
ditto -c -k --keepParent "$APP" "$OUT/notarize.zip"
xcrun notarytool submit "$OUT/notarize.zip" --keychain-profile "$NOTARY_PROFILE" --wait
xcrun stapler staple "$APP"
rm "$OUT/notarize.zip"

echo "==> Update archive"
UPDATE="$OUT/Screenshotr_${VERSION}_${ARCH}.app.tar.gz"
tar -czf "$UPDATE" -C "$BUNDLE/macos" Screenshotr.app
(cd "$ROOT" && cargo tauri signer sign -f "$UPDATER_KEY" -p "" "$UPDATE" >/dev/null)
[ -f "$UPDATE.sig" ] || fail "updater signature missing"

echo "==> Disk image"
DMG="$OUT/Screenshotr_${VERSION}_${ARCH}.dmg"
STAGE="$(mktemp -d)"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "Screenshotr" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
rm -rf "$STAGE"
codesign --force --timestamp --sign "$SIGNING_IDENTITY" "$DMG"
xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" --wait
xcrun stapler staple "$DMG"

echo "==> Update feed and cask"
python3 - "$OUT" "$VERSION" "$GITHUB_REPO" "$ARCH" <<'PY'
import json, sys, datetime, pathlib
out, version, repo, arch = sys.argv[1:]
out = pathlib.Path(out)
archive = f"Screenshotr_{version}_{arch}.app.tar.gz"
feed = {
    "version": version,
    "notes": f"Screenshotr {version}",
    "pub_date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "platforms": {
        f"darwin-{arch}": {
            "signature": (out / f"{archive}.sig").read_text().strip(),
            "url": f"https://github.com/{repo}/releases/download/v{version}/{archive}",
        }
    },
}
(out / "latest.json").write_text(json.dumps(feed, indent=2) + "\n")
PY
SHA=$(shasum -a 256 "$DMG" | awk '{print $1}')
sed -e "s/__VERSION__/$VERSION/" -e "s/__SHA256__/$SHA/" -e "s#__GITHUB_REPO__#$GITHUB_REPO#g" \
  "$ROOT/packaging/homebrew/screenshotr.rb" > "$OUT/screenshotr.rb"
ls -1 "$OUT"

if [ "$PUBLISH" -ne 1 ]; then
  echo "==> Built. Review $OUT, then run again with --publish."
  exit 0
fi

echo "==> Publishing v$VERSION to $GITHUB_REPO"
gh release create "v$VERSION" --repo "$GITHUB_REPO" --title "Screenshotr $VERSION" --generate-notes \
  "$DMG" "$UPDATE" "$UPDATE.sig" "$OUT/latest.json"

echo "==> Updating the cask in $TAP_REPO"
CASK_PATH="Casks/screenshotr.rb"
EXISTING=$(gh api "repos/$TAP_REPO/contents/$CASK_PATH" --jq .sha 2>/dev/null || true)
gh api -X PUT "repos/$TAP_REPO/contents/$CASK_PATH" \
  -f message="screenshotr $VERSION" \
  -f content="$(base64 < "$OUT/screenshotr.rb")" \
  ${EXISTING:+-f sha="$EXISTING"} >/dev/null
echo "==> Done: brew install --cask ${TAP_REPO%%/*}/${TAP_REPO#*/homebrew-}/screenshotr"
