#!/usr/bin/env bash
#
# Staple the notarization ticket to the app *inside* a notarized DMG, then
# re-sign, re-notarize and re-staple the DMG itself.
#
# Why: release-macos.sh notarizes and staples the DMG, but the copy of the app
# inside it was packed before any ticket existed. When a user drags that app
# out, macOS has to fetch the ticket online on first launch, so an offline
# first launch is refused and `syspolicy_check distribution` reports
# "Notary Ticket Missing". Stapling inside the image (keeping its window
# layout) closes that gap.
#
# Usage:  desktop/scripts/staple-app-in-dmg.sh <path/to/notarized.dmg>
# Notary credentials: same as release-macos.sh (App Store Connect API key, or
# NOTARY_PROFILE).
set -euo pipefail
DMG="${1:?usage: staple-app-in-dmg.sh <dmg>}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
IDENTITY="$("$REPO_ROOT/desktop/scripts/signing-identity.sh")"
WORK="$(mktemp -d)"
trap 'hdiutil detach "$WORK/mnt" -quiet 2>/dev/null || true; rm -rf "$WORK"' EXIT

echo "==> Opening $(basename "$DMG") read-write"
hdiutil convert "$DMG" -format UDRW -o "$WORK/rw.dmg" -quiet
mkdir "$WORK/mnt"
hdiutil attach "$WORK/rw.dmg" -nobrowse -noverify -mountpoint "$WORK/mnt" -quiet
APP="$(find "$WORK/mnt" -maxdepth 1 -name '*.app' | head -1)"
[[ -n "$APP" ]] || { echo "no .app inside $DMG" >&2; exit 1; }

echo "==> Stapling the app inside the image"
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"
hdiutil detach "$WORK/mnt" -quiet

echo "==> Compressing, signing and notarizing the image"
hdiutil convert "$WORK/rw.dmg" -format UDZO -imagekey zlib-level=9 -o "$WORK/final.dmg" -quiet
codesign --force --sign "$IDENTITY" --timestamp "$WORK/final.dmg"
ASC_KEY="${APPLE_API_KEY_PATH:-$REPO_ROOT/mobile/AuthKey_38K86XR69G.p8}"
ASC_KEY_ID="${APPLE_API_KEY:-38K86XR69G}"
ASC_ISSUER="${APPLE_API_ISSUER:-fd5e0565-896c-44eb-939b-392369d7cc54}"
if [[ -f "$ASC_KEY" ]]; then
  xcrun notarytool submit "$WORK/final.dmg" --wait \
    --key "$ASC_KEY" --key-id "$ASC_KEY_ID" --issuer "$ASC_ISSUER"
else
  xcrun notarytool submit "$WORK/final.dmg" --wait --keychain-profile "${NOTARY_PROFILE:-consistent-notary}"
fi
xcrun stapler staple "$WORK/final.dmg"
spctl -a -vvv -t install "$WORK/final.dmg"

mv "$WORK/final.dmg" "$DMG"
echo "==> Done: $DMG"
shasum -a 256 "$DMG"
