#!/usr/bin/env bash
#
# Resolve this machine's "Developer ID Application" identity and echo it.
#
# Shared by `build-signed.sh` and `release-macos.sh` so there is exactly one
# definition of what "the signing identity" means. Sourcing scripts should treat
# a non-zero exit as "no certificate here" and decide for themselves whether
# that is fatal.
#
# Usage:  IDENTITY="$(desktop/scripts/signing-identity.sh)" || die "..."
set -euo pipefail

IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null \
  | grep "Developer ID Application" | head -1 | sed -E 's/.*"(.+)"/\1/')" || true

if [[ -z "${IDENTITY:-}" ]]; then
  cat >&2 <<'EOF'
no "Developer ID Application" certificate in the login keychain.

Import one with:  desktop/scripts/import-signing-cert.sh
Full setup:       docs/macos-signing.md
EOF
  exit 1
fi

printf '%s\n' "$IDENTITY"
