#!/usr/bin/env node
// Copies the consistent-world-gateway binary to src-tauri/binaries/ with the target-triple suffix
// so Tauri can bundle it as a sidecar. Run from desktop/ with: node scripts/copy-sidecar.js
// Prerequisite: cargo build -p consistent-world-gateway --release (from repo root)

const { execSync } = require('child_process');
const fs = require('fs');
const path = require('path');
const target = require('./target');

const repoRoot = path.resolve(__dirname, '../..');
const desktopDir = path.resolve(__dirname, '..');
const srcTauri = path.join(desktopDir, 'src-tauri');
const binariesDir = path.join(srcTauri, 'binaries');

const isWindows = process.platform === 'win32';
const ext = isWindows ? '.exe' : '';

// 1. Build the SvelteKit frontend (static SPA) so build.rs can embed it.
const frontendDir = path.join(repoRoot, 'sveltfront');
console.log('Building sveltfront (static SPA dashboard)...');
if (!fs.existsSync(path.join(frontendDir, 'node_modules'))) {
  console.log('  installing sveltfront deps (npm ci)...');
  execSync('npm ci', { cwd: frontendDir, stdio: 'inherit' });
}
execSync('npm run build', { cwd: frontendDir, stdio: 'inherit' });

// 2. Build gateway with the dashboard feature so build.rs embeds sveltfront/build.
console.log(`Building consistent-world-gateway (with dashboard feature) for ${target.triple}...`);
execSync(
  `cargo build -p consistent-world-gateway --release --features dashboard${target.cargoTargetArgs}`,
  {
    cwd: repoRoot,
    stdio: 'inherit',
    env: { ...process.env, ...target.crossBuildEnv() },
  }
);

const targetTriple = target.triple;
const releaseDir = target.releaseDir(path.join(repoRoot, 'target'));
const possibleNames = [`consistent-world${ext}`, `consistent-world-gateway${ext}`];
let srcBinary = null;
for (const name of possibleNames) {
  const p = path.join(releaseDir, name);
  if (fs.existsSync(p)) {
    srcBinary = p;
    break;
  }
}

const destName = `consistent-world-${targetTriple}${ext}`;
const destBinary = path.join(binariesDir, destName);

if (!srcBinary) {
  console.error('Gateway binary not found. Run from repo root: cargo build -p consistent-world-gateway --release');
  console.error('Looked for:', possibleNames.map((n) => path.join(releaseDir, n)));
  process.exit(1);
}

if (!fs.existsSync(binariesDir)) {
  fs.mkdirSync(binariesDir, { recursive: true });
}

fs.copyFileSync(srcBinary, destBinary);
console.log('Copied', srcBinary, '->', destBinary);
