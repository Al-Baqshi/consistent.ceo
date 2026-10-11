# Contributing

Thanks for helping. This is a self-hosted product that people run on their own
machines, so the bar is: it must keep working with no network, no account and
nothing of ours in the loop.

## Before you start

- Read [ARCHITECTURE.md](ARCHITECTURE.md) for how the pieces fit, and
  [STATUS.md](STATUS.md) for what is finished. If they disagree with the code,
  the code wins; fix the doc in the same pull request.
- Module header comments (`//!`) explain each subsystem's design and are the
  intended entry point.
- Touching `crates/credentials/`? Read [docs/VAULT.md](docs/VAULT.md) first.
- For anything bigger than a bug fix, open an issue to agree on the approach
  before writing it.

## Build and test

Rust **1.97** (pinned in `rust-toolchain.toml`) and Node 20.

```bash
cargo check  --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test   --workspace
scripts/repo-health.sh            # fast drift checks
```

These are exactly what CI runs. If linking the test binaries runs out of
memory, set `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`.

Frontend and apps:

```bash
cd sveltfront && npm ci && npm run check     # dashboard
cd mobile && npm install && npm run typecheck  # phone app
```

## Rules that bite

- **Do not run `cargo fmt --all`.** The tree is not rustfmt-clean and a
  whole-tree format buries your change. Format only the files you touched:
  `rustfmt --edition 2024 path/to/file.rs`. Never pass rustfmt a file that
  declares modules (`main.rs`, `lib.rs`, `control_api.rs`): it reformats the
  whole subtree.
- **New gateway source files** must be declared with `mod` in both
  `crates/gateway/src/lib.rs` and `main.rs`. CI fails otherwise, because an
  undeclared file is silently never compiled.
- **New HTTP routes** go in the matching `control_api/*_api.rs` and are
  registered in `control_routes()`.
- **Never rename internal identifiers.** User-facing text says "Consistent
  CEO"; crate names, `~/.consistent-world/`, database namespaces and bundle ids
  stay `consistent-world`.
- **No required service.** A change must not make startup, sign-in or pairing
  depend on a hosted service. Hosted features stay opt-in and off by default.
- **Several background loops can change a task.** If you add a loop-side task
  mutation, check the others (`orchestrator`, `mission_engine`,
  `task_executor`, `heartbeat_dispatcher`, `scheduler`).

## Pull requests

- One topic per pull request, with tests for behaviour you change.
- Commit messages: `type(scope): lowercase imperative subject`, e.g.
  `fix(pairing): refuse an expired challenge`. The body says *why*.
- Update `STATUS.md` when you change what is real, stubbed or shadow-mode.
- By contributing you agree your work is licensed under the
  [MIT license](LICENSE).

## Forks and unsigned builds

Release installers are signed and notarized in CI with the maintainer's
certificates. A fork's builds are unsigned: on macOS, right-click the app and
choose Open the first time; on Windows, choose "More info → Run anyway" in
SmartScreen. To publish signed builds from a fork, see
[docs/macos-signing.md](docs/macos-signing.md).
