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

Anyone with write access can do this.

1. Make sure `main` is green in CI.
2. Raise the version in the root `Cargo.toml` (`[workspace.package]`) and in
   `desktop/src-tauri/tauri.conf.json`, and commit.
3. Tag and push: `git tag v1.2.3 && git push origin v1.2.3`.
4. `release-desktop.yml` builds the desktop installers into a draft release
   and publishes it when every platform succeeds. `ci.yml` attaches the
   command-line binaries to the same release and pushes the container image
   to `ghcr.io`.
5. Check the release page: three installers, the `consistent-world-*`
   binaries, and the image under Packages.
