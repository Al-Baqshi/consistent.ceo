// Resolves which target triple the sidecars are being built for.
//
// Tauri looks up each sidecar by the exact name `<name>-<target-triple>`, so the
// suffix these scripts write has to match the triple Tauri bundles for. Asking
// rustc for the *host* tuple is correct for a native build and silently wrong
// for a cross build: the app bundles happily and then ships an executable for
// the wrong architecture, which only fails on a user's machine.
//
// Set CONSISTENT_TARGET to cross-compile — e.g. building the Intel macOS app on
// an Apple Silicon runner, which is how CI has to do it now that GitHub retired
// the macos-13 (x86_64) image:
//
//   CONSISTENT_TARGET=x86_64-apple-darwin \
//     npm run tauri build -- --target x86_64-apple-darwin --bundles dmg
//
// Leave it unset for a native build so cargo's incremental cache stays warm.

const { execSync } = require('child_process');
const path = require('path');

const host = execSync('rustc --print host-tuple', { encoding: 'utf-8' }).trim();
const triple = (process.env.CONSISTENT_TARGET || '').trim() || host;
const isCross = triple !== host;

/** Args to append to a `cargo build` invocation. Empty when building natively. */
const cargoTargetArgs = isCross ? ` --target ${triple}` : '';

/**
 * Where cargo drops the release binary, given a crate's target directory.
 * Cross builds nest under the triple; native builds do not.
 */
function releaseDir(cargoTargetRoot) {
  return isCross
    ? path.join(cargoTargetRoot, triple, 'release')
    : path.join(cargoTargetRoot, 'release');
}

/** The Apple arch name (`arm64`/`x86_64`) for this triple, or null if not Apple. */
function appleArch() {
  if (!triple.endsWith('-apple-darwin')) return null;
  if (triple.startsWith('x86_64')) return 'x86_64';
  if (triple.startsWith('aarch64')) return 'arm64';
  return null;
}

/**
 * Extra env for compiling the C/C++ dependencies (whisper.cpp, via whisper-rs).
 * cmake targets the host arch by default, so without this a cross build produces
 * object files the Rust target cannot link against.
 */
function crossBuildEnv() {
  const arch = isCross ? appleArch() : null;
  return arch ? { CMAKE_OSX_ARCHITECTURES: arch } : {};
}

module.exports = { host, triple, isCross, cargoTargetArgs, releaseDir, appleArch, crossBuildEnv };
