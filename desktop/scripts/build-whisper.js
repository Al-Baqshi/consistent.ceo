#!/usr/bin/env node
// Builds the cross-platform Whisper dictation sidecar and stages it for Tauri.
//
// Unlike build-dictation.js (Apple Speech.framework, macOS-only), this engine
// runs everywhere — Intel Macs, Windows, Linux — so it builds on every platform.
//
// Steps: `cargo build --release` in desktop/whisper → (macOS) ad-hoc codesign so
// the embedded Info.plist section binds for TCC → copy to
// src-tauri/binaries/consistent-whisper-<target-triple>(.exe) with the exec bit.
//
// Requires cmake + a C/C++ compiler on the *build* machine (whisper.cpp is
// compiled from source via whisper-rs). The shipped binary is self-contained.

const { execSync } = require('child_process');
const fs = require('fs');
const path = require('path');
const target = require('./target');

const desktopDir = path.resolve(__dirname, '..');
const crateDir = path.join(desktopDir, 'whisper');
const binariesDir = path.join(desktopDir, 'src-tauri', 'binaries');

const isWindows = process.platform === 'win32';
const ext = isWindows ? '.exe' : '';

console.log(`[build-whisper] cargo build --release for ${target.triple} (compiles whisper.cpp)...`);
execSync(`cargo build --release${target.cargoTargetArgs}`, {
  cwd: crateDir,
  stdio: 'inherit',
  env: { ...process.env, ...target.crossBuildEnv() },
});

const srcBinary = path.join(
  target.releaseDir(path.join(crateDir, 'target')),
  `consistent-whisper${ext}`
);
if (!fs.existsSync(srcBinary)) {
  console.error('[build-whisper] built binary not found at', srcBinary);
  process.exit(1);
}

if (process.platform === 'darwin') {
  // Ad-hoc sign so the __TEXT,__info_plist section binds — without this TCC
  // crashes the process the instant cpal opens the microphone.
  console.log('[build-whisper] ad-hoc codesign...');
  execSync(`codesign --force --sign - "${srcBinary}"`, { stdio: 'inherit' });
}

const destBinary = path.join(binariesDir, `consistent-whisper-${target.triple}${ext}`);

if (!fs.existsSync(binariesDir)) {
  fs.mkdirSync(binariesDir, { recursive: true });
}
fs.copyFileSync(srcBinary, destBinary);
fs.chmodSync(destBinary, 0o755);
console.log('[build-whisper] copied', srcBinary, '->', destBinary);
