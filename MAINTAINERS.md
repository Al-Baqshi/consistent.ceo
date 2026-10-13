# Maintainers

| Who | GitHub |
|---|---|
| Al Baqshi | [@Al-Baqshi](https://github.com/Al-Baqshi) |

## What to expect

I use Consistent CEO every day and maintain it in my own time. I read issues
and merge what I can, but there is no support schedule and no guarantee of a
reply. Forks are welcome; if you build something useful on top, tell us in
Discussions.

## Cutting a release

From the maintainer's Mac, on an up-to-date `main` with a clean tree:

```bash
scripts/release.sh 0.2.1            # notes generated from the commits
scripts/release.sh 0.2.1 notes.md   # or your own release notes
```

It sets the version, builds the Intel and Apple Silicon apps, signs them
with the Developer ID, has Apple notarize them, packages the auto-update
archives and `latest.json`, tags the version, and publishes the GitHub
release. Installed apps find the update within six hours and offer
"Restart to update" in the menu bar. The website's Download buttons always
point at the latest release, so nothing else needs touching.

One-time setup on a new Mac is in [docs/macos-signing.md](docs/macos-signing.md).
Keep a backup of `~/.consistent-signing/`: without `updater.key`, installed
apps can never be updated again, only reinstalled by hand.

Pushing the tag also runs CI, which attaches the Linux and Windows command-line
binaries and publishes the Docker image. It never builds macOS, so it cannot
overwrite the signed Mac files.
