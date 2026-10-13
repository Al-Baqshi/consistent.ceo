#!/usr/bin/env node
// Write dist/latest.json, the feed the installed app's updater reads
// (desktop/src-tauri/src/updater.rs, endpoint in tauri.conf.json).
//
//   node desktop/scripts/make-latest-json.js <version> [notes-file]
//
// Reads the signed update archives release-macos.sh staged in dist/ and points
// each platform at that version's GitHub release asset. A platform whose
// archive is missing is left out, so its users simply see no update.
const fs = require('fs');
const path = require('path');

const [version, notesFile] = process.argv.slice(2);
if (!version) {
  console.error('usage: make-latest-json.js <version> [notes-file]');
  process.exit(1);
}
const repo = process.env.CONSISTENT_REPO || 'Al-Baqshi/consistent.ceo';
const dist = path.join(__dirname, '..', '..', 'dist');
const targets = {
  'darwin-aarch64': 'Consistent-CEO-macOS-AppleSilicon.app.tar.gz',
  'darwin-x86_64': 'Consistent-CEO-macOS-Intel.app.tar.gz',
};
const platforms = {};
for (const [platform, asset] of Object.entries(targets)) {
  const sig = path.join(dist, `${asset}.sig`);
  if (!fs.existsSync(path.join(dist, asset)) || !fs.existsSync(sig)) {
    console.warn(`  no ${asset}; ${platform} gets no update`);
    continue;
  }
  platforms[platform] = {
    signature: fs.readFileSync(sig, 'utf8').trim(),
    url: `https://github.com/${repo}/releases/download/v${version}/${asset}`,
  };
}
if (Object.keys(platforms).length === 0) {
  console.error('no signed update archives in dist/; nothing to publish');
  process.exit(1);
}
const notes = notesFile ? fs.readFileSync(notesFile, 'utf8').trim() : `Consistent CEO ${version}`;
const feed = { version, notes, pub_date: new Date().toISOString(), platforms };
fs.writeFileSync(path.join(dist, 'latest.json'), JSON.stringify(feed, null, 2) + '\n');
console.log(`wrote dist/latest.json for ${version}: ${Object.keys(platforms).join(', ')}`);
