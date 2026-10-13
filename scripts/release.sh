#!/usr/bin/env bash
#
# Ship a new version of Consistent CEO for macOS, in one command:
#
#   scripts/release.sh 0.2.1               # notes generated from commits
#   scripts/release.sh 0.2.1 notes.md      # your own release notes
#
# It sets the version everywhere, builds the Intel and Apple Silicon apps,
# signs them with the Developer ID, has Apple notarize them, packages the
# auto-update archives and the update feed (latest.json), commits and tags
# the version, and publishes the GitHub release. Installed apps pick the
# update up on their own within six hours (desktop/src-tauri/src/updater.rs),
# and the website's Download buttons always point at the latest release.
#
# Needs, on this Mac (one-time setup, docs/macos-signing.md):
#   - the "Developer ID Application" certificate in the login keychain
#   - ~/.consistent-signing/updater.key   (update signing; back it up)
#   - mobile/AuthKey_<id>.p8              (notarization)
#   - rustup target aarch64-apple-darwin, cmake, and `gh` signed in
set -euo pipefail

VERSION="${1:-}"
NOTES="${2:-}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST="$ROOT/dist"

say() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }
die() { printf '\n\033[31merror:\033[0m %s\n' "$1" >&2; exit 1; }

[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "usage: scripts/release.sh <major.minor.patch> [notes-file]"
[[ -z "$NOTES" || -f "$NOTES" ]] || die "no notes file at $NOTES"
[[ -n "$NOTES" ]] && NOTES="$(cd "$(dirname "$NOTES")" && pwd)/$(basename "$NOTES")"
cd "$ROOT"
trap 'echo; echo "If this stopped after \"Setting the version\": run  git checkout -- .  and start again."' ERR

say "Checking the ground"
[[ "$(git branch --show-current)" == "main" ]] || die "release from main"
[[ -z "$(git status --porcelain)" ]] || die "commit or stash your changes first"
git fetch -q origin
[[ "$(git rev-parse HEAD)" == "$(git rev-parse origin/main)" ]] || die "main is not level with origin/main"
! git rev-parse -q --verify "refs/tags/v$VERSION" >/dev/null || die "v$VERSION already exists"
gh auth status >/dev/null 2>&1 || die "gh is not signed in (gh auth login)"
desktop/scripts/signing-identity.sh >/dev/null || die "no Developer ID certificate"
[[ -f "$HOME/.consistent-signing/updater.key" ]] || die "no update-signing key at ~/.consistent-signing/updater.key"
echo "    ok"

say "Setting the version to $VERSION"
python3 - "$VERSION" <<'PY'
import json, pathlib, re, sys
v = sys.argv[1]
def toml_version(path, section):
    p = pathlib.Path(path); s = p.read_text()
    head, sep, rest = s.partition(f"[{section}]")
    assert sep, f"no [{section}] in {path}"
    rest, n = re.subn(r'(?m)^version = "[^"]*"', f'version = "{v}"', rest, count=1)
    assert n == 1, f"no version in [{section}] of {path}"
    p.write_text(head + sep + rest)
toml_version("Cargo.toml", "workspace.package")
toml_version("desktop/src-tauri/Cargo.toml", "package")
for path in ["desktop/src-tauri/tauri.conf.json", "desktop/package.json"]:
    p = pathlib.Path(path); s = p.read_text()
    s, n = re.subn(r'"version": "[^"]*"', f'"version": "{v}"', s, count=1)
    assert n == 1, f"no version in {path}"
    p.write_text(s)
PY
# Refresh only the workspace's own entries in each lockfile. (`cargo metadata`
# would also fetch every Windows-only dependency, which this Mac never needs.)
for dir in . desktop/src-tauri desktop/whisper; do
  (cd "$dir" && cargo update --workspace --offline --quiet)
done
echo "    $(git diff --stat | tail -1)"

say "Building, signing and notarizing both Macs"
# Keep package managers' libraries out of the link: an app that links them
# fails to start on a Mac without them (release-macos.sh also refuses it).
PATH="$(echo "$PATH" | tr ':' '\n' | grep -v -E '^/opt/(local|homebrew)' | paste -sd: -)"
[[ -d "$HOME/Library/Python/3.9/bin" ]] && PATH="$HOME/Library/Python/3.9/bin:$PATH"
export PATH CMAKE_POLICY_VERSION_MINIMUM=3.5
mkdir -p "$DIST"
for f in Consistent-CEO-macOS-Intel.dmg Consistent-CEO-macOS-AppleSilicon.dmg \
  Consistent-CEO-macOS-Intel.app.tar.gz Consistent-CEO-macOS-Intel.app.tar.gz.sig \
  Consistent-CEO-macOS-AppleSilicon.app.tar.gz Consistent-CEO-macOS-AppleSilicon.app.tar.gz.sig \
  consistent-world-x86_64-apple-darwin.tar.gz consistent-world-aarch64-apple-darwin.tar.gz \
  SHA256SUMS.txt latest.json; do
  rm -f "$DIST/$f"
done
desktop/scripts/release-macos.sh
CONSISTENT_TARGET=aarch64-apple-darwin desktop/scripts/release-macos.sh

say "Packaging the command-line gateway (install.sh downloads these)"
for pair in "x86_64-apple-darwin:desktop/src-tauri/target/release/bundle/macos" \
            "aarch64-apple-darwin:desktop/src-tauri/target/aarch64-apple-darwin/release/bundle/macos"; do
  triple="${pair%%:*}"; bundle="${pair#*:}"
  tmp="$(mktemp -d)"
  cp "$bundle/Consistent CEO.app/Contents/MacOS/consistent-world" "$tmp/consistent-world"
  # No `._consistent-world` metadata entry for Linux tar to unpack as junk.
  COPYFILE_DISABLE=1 tar -czf "$DIST/consistent-world-$triple.tar.gz" -C "$tmp" consistent-world
  echo "    consistent-world-$triple.tar.gz"
done

say "Checksums and the update feed"
(cd "$DIST" && shasum -a 256 Consistent-CEO-macOS-*.dmg consistent-world-*.tar.gz > SHA256SUMS.txt)
node desktop/scripts/make-latest-json.js "$VERSION" ${NOTES:+"$NOTES"}

say "Committing and tagging v$VERSION"
git add Cargo.toml Cargo.lock desktop/src-tauri/Cargo.toml desktop/src-tauri/Cargo.lock \
  desktop/src-tauri/tauri.conf.json desktop/package.json desktop/whisper/Cargo.lock
git commit -q -m "release: $VERSION"
git tag -a "v$VERSION" -m "Consistent CEO $VERSION"
git push -q origin main "v$VERSION"

say "Publishing the GitHub release"
notes_args=(--generate-notes)
[[ -n "$NOTES" ]] && notes_args=(--notes-file "$NOTES")
gh release create "v$VERSION" \
  "$DIST/Consistent-CEO-macOS-AppleSilicon.dmg" "$DIST/Consistent-CEO-macOS-Intel.dmg" \
  "$DIST/Consistent-CEO-macOS-AppleSilicon.app.tar.gz" "$DIST/Consistent-CEO-macOS-AppleSilicon.app.tar.gz.sig" \
  "$DIST/Consistent-CEO-macOS-Intel.app.tar.gz" "$DIST/Consistent-CEO-macOS-Intel.app.tar.gz.sig" \
  "$DIST/consistent-world-aarch64-apple-darwin.tar.gz" "$DIST/consistent-world-x86_64-apple-darwin.tar.gz" \
  "$DIST/SHA256SUMS.txt" "$DIST/latest.json" \
  --title "Consistent CEO $VERSION" "${notes_args[@]}" --latest

say "Checking the public links"
base="https://github.com/$(gh repo view --json nameWithOwner --jq .nameWithOwner)/releases/latest/download"
for f in latest.json Consistent-CEO-macOS-AppleSilicon.dmg Consistent-CEO-macOS-Intel.dmg; do
  code="$(curl -s -o /dev/null -L -w '%{http_code}' -r 0-0 "$base/$f")"
  echo "    $f -> $code"
done
say "Released $VERSION. Installed apps will offer it within six hours."
