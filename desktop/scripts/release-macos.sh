#!/usr/bin/env bash
#
# Build, sign, notarize and staple the macOS desktop app, then stage the
# installer under the stable name the landing site links to.
#
# Why this exists: an ad-hoc signed build makes macOS warn every downloader that
# the app "could not be verified", and the only way to clear that is Apple's
# notary service — which will not look at a build unless it is signed with a
# Developer ID Application certificate under the hardened runtime.
#
# One-time setup (see docs/macos-signing.md):
#   1. A "Developer ID Application" certificate in the login keychain.
#   2. Notary credentials stored in a keychain profile:
#        xcrun notarytool store-credentials consistent-notary \
#          --apple-id <your-apple-id> --team-id <TEAM_ID> \
#          --password <app-specific-password>
#      or an App Store Connect API key via APPLE_API_KEY/APPLE_API_ISSUER.
#
# Usage:  desktop/scripts/release-macos.sh [--skip-notarize]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DESKTOP="$REPO_ROOT/desktop"
BUNDLE_DIR="$DESKTOP/src-tauri/target/release/bundle"
APP="$BUNDLE_DIR/macos/Consistent CEO.app"
NOTARY_PROFILE="${NOTARY_PROFILE:-consistent-notary}"
# Where the landing site expects the installer. Stable name on purpose: the
# download URL must not change per release.
SITE_ASSET="${SITE_ASSET:-$HOME/consistent/Consistent/public/downloads/Consistent-CEO-macOS-Intel.dmg}"

skip_notarize=false
[[ "${1:-}" == "--skip-notarize" ]] && skip_notarize=true

say() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }
die() { printf '\n\033[31merror:\033[0m %s\n' "$1" >&2; exit 1; }

say "Locating the Developer ID Application certificate"
IDENTITY="$("$DESKTOP/scripts/signing-identity.sh")" || die "cannot sign without a
certificate. For a signed build without notarization, use
  cd desktop && npm run build:signed
For an unsigned build, use
  cd desktop && npm run build
— but note an unsigned app cannot hold a macOS keychain grant across rebuilds."
echo "    $IDENTITY"

# Tauri signs every binary in the bundle during bundling when this is set, which
# matters: the three sidecars (gateway, whisper, dictation) each need their own
# signature or the notary service rejects the whole app.
export APPLE_SIGNING_IDENTITY="$IDENTITY"

# A DMG left mounted from a previous install collides with the bundler and the
# build fails late, after the expensive compile. Cheap to rule out up front.
if [[ -d "/Volumes/Consistent CEO" ]]; then
  say "Ejecting a previously mounted Consistent CEO volume"
  hdiutil detach "/Volumes/Consistent CEO" -quiet || true
fi

say "Building and signing"
(cd "$DESKTOP" && npm run build)

DMG="$(ls -t "$BUNDLE_DIR"/dmg/*.dmg 2>/dev/null | head -1)"
[[ -n "$DMG" && -f "$DMG" ]] || die "no .dmg was produced under $BUNDLE_DIR/dmg"

say "Verifying the signature before handing it to Apple"
codesign --verify --deep --strict --verbose=2 "$APP"
codesign -dv "$APP" 2>&1 | grep -E "Authority|TeamIdentifier|flags" || true

if $skip_notarize; then
  say "Skipping notarization (--skip-notarize)"
else
  say "Submitting to Apple's notary service (a few minutes)"
  # Prefer the App Store Connect API key the mobile project already submits
  # with: it needs no app-specific password and no Team ID, and it is already
  # on this machine. Fall back to a stored keychain profile.
  ASC_KEY="${APPLE_API_KEY_PATH:-$REPO_ROOT/mobile/AuthKey_3J4W8Y9522.p8}"
  ASC_KEY_ID="${APPLE_API_KEY:-3J4W8Y9522}"
  ASC_ISSUER="${APPLE_API_ISSUER:-fd5e0565-896c-44eb-939b-392369d7cc54}"
  if [[ -f "$ASC_KEY" ]]; then
    xcrun notarytool submit "$DMG" --wait \
      --key "$ASC_KEY" --key-id "$ASC_KEY_ID" --issuer "$ASC_ISSUER"
  else
    xcrun notarytool submit "$DMG" --wait --keychain-profile "$NOTARY_PROFILE"
  fi

  say "Stapling the ticket so the app validates offline"
  xcrun stapler staple "$DMG"
  # Staple the .app too: a user who drags it out of the DMG keeps the ticket.
  xcrun stapler staple "$APP" || true

  say "Gatekeeper assessment (this is what a downloader's Mac will do)"
  spctl -a -vvv -t install "$DMG"
fi

say "Staging the installer for the site"
if [[ -d "$(dirname "$SITE_ASSET")" ]]; then
  cp "$DMG" "$SITE_ASSET"
  echo "    $SITE_ASSET"
  shasum -a 256 "$SITE_ASSET"
else
  echo "    landing repo not found at $(dirname "$SITE_ASSET") — leaving it at:"
  echo "    $DMG"
fi

say "Done"
