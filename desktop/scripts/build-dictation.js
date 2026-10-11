#!/usr/bin/env node
// Builds the on-device dictation Swift sidecar and stages it for Tauri.
//
// Steps: `swift build -c release` → ad-hoc codesign (so the embedded Info.plist
// binds and TCC honors the mic/speech usage strings) → copy to
// src-tauri/binaries/consistent-dictate-<target-triple> with the exec bit.
//
// macOS-only: SpeechAnalyzer ships in macOS 26. On any other platform this is a
// no-op so cross-platform desktop builds still succeed.

const { execSync } = require('child_process');
const fs = require('fs');
const path = require('path');
const target = require('./target');

if (process.platform !== 'darwin') {
  console.log('[build-dictation] skipping — dictation sidecar is macOS-only.');
  process.exit(0);
}

const desktopDir = path.resolve(__dirname, '..');
const pkgDir = path.join(desktopDir, 'dictation');
const binariesDir = path.join(desktopDir, 'src-tauri', 'binaries');

// SwiftPM targets the host arch unless told otherwise, so a cross build has to
// pass --arch explicitly (and again when asking where the binary landed).
const archArgs = target.isCross && target.appleArch() ? ` --arch ${target.appleArch()}` : '';

console.log(`[build-dictation] swift build -c release for ${target.triple}...`);
execSync(`swift build -c release${archArgs}`, { cwd: pkgDir, stdio: 'inherit' });

// Locate the built binary (SwiftPM puts it under .build/release).
const built = execSync(`swift build -c release${archArgs} --show-bin-path`, {
  cwd: pkgDir,
  encoding: 'utf-8',
}).trim();
const srcBinary = path.join(built, 'consistent-dictate');
if (!fs.existsSync(srcBinary)) {
  console.error('[build-dictation] built binary not found at', srcBinary);
  process.exit(1);
}

// Ad-hoc sign so the __TEXT,__info_plist section binds — without this, TCC
// crashes the process the instant it touches the mic or speech APIs.
console.log('[build-dictation] ad-hoc codesign...');
execSync(`codesign --force --sign - "${srcBinary}"`, { stdio: 'inherit' });

const destBinary = path.join(binariesDir, `consistent-dictate-${target.triple}`);

if (!fs.existsSync(binariesDir)) {
  fs.mkdirSync(binariesDir, { recursive: true });
}
fs.copyFileSync(srcBinary, destBinary);
fs.chmodSync(destBinary, 0o755);
console.log('[build-dictation] copied', srcBinary, '->', destBinary);
