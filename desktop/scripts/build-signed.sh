#!/usr/bin/env bash
#
# A signed local build — the one to install on this machine.
#
# Why this exists separately from `release-macos.sh`: that script also notarizes,
# staples and stages the DMG for the landing site, which takes a trip through
# Apple's notary service and is pointless for a build you are only going to run
# yourself. This does the part that matters locally — sign every binary in the
# bundle with the real Developer ID — and stops.
#
# It matters more than it sounds. An ad-hoc signed app has no stable identity:
# every rebuild produces a different code directory hash, so macOS keychain ACLs
# can never trust it, and "Always Allow" on a keychain prompt does not stick. The
# app then asks for the login password again on the next launch, and the next.
# Signing with a real certificate is what makes that grant persist.
#
# Usage:  cd desktop && npm run build:signed
#         (or: desktop/scripts/build-signed.sh)
set -euo pipefail

DESKTOP="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

say() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }
die() { printf '\n\033[31merror:\033[0m %s\n' "$1" >&2; exit 1; }

say "Locating the Developer ID Application certificate"
IDENTITY="$("$DESKTOP/scripts/signing-identity.sh")" || exit 1
echo "    $IDENTITY"

# Tauri signs every binary in the bundle during bundling when this is set, which
# is the point: the sidecars (gateway, whisper, dictation) each need their own
# signature, and an unsigned sidecar taints the whole bundle.
export APPLE_SIGNING_IDENTITY="$IDENTITY"

# The gateway sidecar is copied in by a separate step; building without it
# produces an app that launches and then cannot reach a gateway.
say "Refreshing the gateway sidecar"
(cd "$DESKTOP" && npm run build:sidecar)

say "Building and signing"
(cd "$DESKTOP" && npm run build)

APP="$DESKTOP/src-tauri/target/release/bundle/macos/Consistent CEO.app"
[[ -d "$APP" ]] || die "no app bundle was produced at $APP"

say "Verifying the signature"
codesign --verify --deep --strict --verbose=2 "$APP"
codesign -dv "$APP" 2>&1 | grep -E "Authority|TeamIdentifier|flags" || true

cat <<EOF

Built and signed:
    $APP

To use it, replace the installed copy — quit the running app first, then:
    rm -rf "/Applications/Consistent CEO.app"
    cp -R "$APP" /Applications/

The first keychain prompt after that will be the last: "Always Allow" now
sticks, because the signature it is granted to stops changing between builds.

For a build other people can download without a Gatekeeper warning, use
desktop/scripts/release-macos.sh instead — it also notarizes and staples.
EOF
