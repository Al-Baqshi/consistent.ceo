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
#
# Cross-build the other Mac architecture with CONSISTENT_TARGET, e.g. on an
# Intel Mac:  CONSISTENT_TARGET=aarch64-apple-darwin desktop/scripts/release-macos.sh
# (needs `rustup target add aarch64-apple-darwin`).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DESKTOP="$REPO_ROOT/desktop"
HOST_TRIPLE="$(rustc --print host-tuple)"
TARGET_TRIPLE="${CONSISTENT_TARGET:-$HOST_TRIPLE}"
if [[ "$TARGET_TRIPLE" == "$HOST_TRIPLE" ]]; then
  BUNDLE_DIR="$DESKTOP/src-tauri/target/release/bundle"
  TAURI_TARGET_ARGS=()
else
  BUNDLE_DIR="$DESKTOP/src-tauri/target/$TARGET_TRIPLE/release/bundle"
  TAURI_TARGET_ARGS=(-- --target "$TARGET_TRIPLE")
fi
case "$TARGET_TRIPLE" in
  aarch64-apple-darwin) ASSET_NAME="Consistent-CEO-macOS-AppleSilicon.dmg" ;;
  *)                    ASSET_NAME="Consistent-CEO-macOS-Intel.dmg" ;;
esac
APP="$BUNDLE_DIR/macos/Consistent CEO.app"
NOTARY_PROFILE="${NOTARY_PROFILE:-consistent-notary}"
# Where the finished installer is staged, under the stable name the website
# and README link to (releases/latest/download/<name>). Upload it to the
# GitHub release for the tag.
OUT_DIR="${OUT_DIR:-$REPO_ROOT/dist}"
SITE_ASSET="$OUT_DIR/$ASSET_NAME"

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
# The ${a[@]+...} form keeps macOS's bash 3.2 from calling an empty array unbound.
(cd "$DESKTOP" && npm run build ${TAURI_TARGET_ARGS[@]+"${TAURI_TARGET_ARGS[@]}"})

DMG="$(ls -t "$BUNDLE_DIR"/dmg/*.dmg 2>/dev/null | head -1)"
[[ -n "$DMG" && -f "$DMG" ]] || die "no .dmg was produced under $BUNDLE_DIR/dmg"

say "Checking every binary links only macOS's own libraries"
# A library from Homebrew (/opt/homebrew, /usr/local) or MacPorts (/opt/local)
# exists only on the build machine; the app would fail to start on a user's Mac.
bad=0
for bin in "$APP"/Contents/MacOS/*; do
  foreign="$(otool -L "$bin" | tail -n +2 | awk '{print $1}' \
    | grep -v -E '^(/usr/lib/|/System/Library/|@rpath/|@executable_path/|@loader_path/)' || true)"
  if [[ -n "$foreign" ]]; then
    echo "    $(basename "$bin") links: $foreign"
    bad=1
  fi
done
[[ $bad -eq 0 ]] || die "the app links libraries that only exist on this machine (see above)"
echo "    ok"

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
  ASC_KEY="${APPLE_API_KEY_PATH:-$REPO_ROOT/mobile/AuthKey_38K86XR69G.p8}"
  ASC_KEY_ID="${APPLE_API_KEY:-38K86XR69G}"
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

  say "Stapling the app inside the image too, so an offline first launch works"
  "$DESKTOP/scripts/staple-app-in-dmg.sh" "$DMG"
fi

say "Staging the installer for the site"
mkdir -p "$OUT_DIR"
cp "$DMG" "$SITE_ASSET"
echo "    $SITE_ASSET"
shasum -a 256 "$SITE_ASSET"

# The auto-update package (desktop/src-tauri/src/updater.rs): the app as
# notarized and stapled above, archived and signed with the update key. The
# installed app verifies that signature against the public key in
# tauri.conf.json before installing anything.
say "Packaging and signing the auto-update archive"
UPDATER_KEY="${UPDATER_KEY:-$HOME/.consistent-signing/updater.key}"
[[ -f "$UPDATER_KEY" ]] || die "no update-signing key at $UPDATER_KEY (see docs/macos-signing.md)"
UPDATE_ASSET="$OUT_DIR/${ASSET_NAME%.dmg}.app.tar.gz"
# COPYFILE_DISABLE: macOS tar otherwise stores extended attributes (every
# built app carries com.apple.provenance) as hidden `._*` entries, which the
# updater refuses to unpack. Apple's tar hides them when listing, so check
# with a reader that does not.
COPYFILE_DISABLE=1 tar -czf "$UPDATE_ASSET" -C "$(dirname "$APP")" "$(basename "$APP")"
python3 -I -c 'import sys, tarfile; sys.exit(any("._" in m.name for m in tarfile.open(sys.argv[1])))' "$UPDATE_ASSET" \
  || die "the update archive contains macOS metadata entries the updater cannot unpack"
(cd "$DESKTOP" && npx tauri signer sign -f "$UPDATER_KEY" -p "${UPDATER_KEY_PASSWORD:-}" "$UPDATE_ASSET" </dev/null >/dev/null)
[[ -s "$UPDATE_ASSET.sig" ]] || die "signing the update archive produced no signature"
echo "    $UPDATE_ASSET (+ .sig)"

say "Done"
