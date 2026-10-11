#!/usr/bin/env bash
# Import the issued Developer ID Application certificate, its private key, and
# Apple's intermediate CA into the login keychain, then prove codesign can see
# the resulting identity and report the Team ID.
#
# Run after ~/.consistent-signing/create_devid_cert.py has produced
# developerID_application.cer. See docs/macos-signing.md.
set -euo pipefail

D="$HOME/.consistent-signing"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"
CER="$D/developerID_application.cer"
KEY="$D/developer_id.key"

[[ -f "$CER" ]] || { echo "error: $CER not found — run create_devid_cert.py first" >&2; exit 1; }
[[ -f "$KEY" ]] || { echo "error: $KEY not found (the CSR's private half)" >&2; exit 1; }

echo "==> Fetching Apple's Developer ID intermediate CA"
# Without the intermediate the chain does not validate, and codesign refuses the
# identity even though the leaf certificate imported fine.
curl -fsSL -o "$D/DeveloperIDG2CA.cer" \
  https://www.apple.com/certificateauthority/DeveloperIDG2CA.cer
curl -fsSL -o "$D/AppleRootCA-G2.cer" \
  https://www.apple.com/certificateauthority/AppleRootCA-G2.cer || true

echo "==> Importing into the login keychain"
security import "$D/AppleRootCA-G2.cer" -k "$KEYCHAIN" 2>/dev/null || true
security import "$D/DeveloperIDG2CA.cer" -k "$KEYCHAIN" 2>/dev/null || true
security import "$CER" -k "$KEYCHAIN" 2>/dev/null || echo "   (certificate already present)"
security import "$KEY" -k "$KEYCHAIN" -T /usr/bin/codesign -T /usr/bin/security \
  2>/dev/null || echo "   (private key already present)"

# Let codesign use the key without a GUI prompt on every build. Needs the login
# password, so it may no-op under automation — the prompt on first sign is the
# fallback; choose "Always Allow" there.
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "" "$KEYCHAIN" \
  >/dev/null 2>&1 || echo "   note: if signing prompts for keychain access, click 'Always Allow'"

echo
echo "==> Identities codesign can now use"
security find-identity -v -p codesigning

IDENTITY="$(security find-identity -v -p codesigning \
  | grep "Developer ID Application" | head -1 | sed -E 's/.*"(.+)"/\1/')" || true
[[ -n "$IDENTITY" ]] || { echo; echo "error: no Developer ID Application identity yet" >&2; exit 1; }

echo
echo "Developer ID ready: $IDENTITY"
# The Team ID is the OU of the certificate subject, so it never needs looking up.
echo "Team ID: $(echo "$IDENTITY" | sed -E 's/.*\(([A-Z0-9]{10})\)$/\1/')"
