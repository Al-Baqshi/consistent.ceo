# Consistent CEO native shell (Tauri)

Runs the Consistent CEO gateway and opens the workspace UI in a native window. One download, plug and play.

## Prerequisites

- **Rust** (stable)
- **Node.js** 18+ and pnpm (or npm)
- **Tauri prerequisites**: [Tauri docs](https://v2.tauri.app/start/prerequisites/) (e.g. macOS: Xcode CLI tools; Windows: Microsoft C++ Build Tools; Linux: webkit2gtk, etc.)

## Build the gateway (sidecar)

The app bundles the `consistent-world` gateway binary as a sidecar with the **dashboard** feature so the window shows the embedded UI instead of a blank or fallback page.

**Build the gateway and copy sidecar** (from **desktop** directory):

```bash
cd desktop
npm run build:sidecar
```

This builds the `sveltfront` static SPA, then builds the gateway (`consistent-world`) with `--features dashboard` and copies the release binary to `src-tauri/binaries/consistent-world-<target-triple>` so Tauri can bundle it.
**Note:** `pnpm tauri build` runs `build:sidecar` automatically via `beforeBuildCommand`.

## Run in development

1. Build the gateway and sidecar (see above).
2. From `desktop/`:

```bash
pnpm install
pnpm tauri dev
```

The app will:

- Spawn the gateway with a config in your app data dir (`~/Library/Application Support/world.consistent.desktop/` on macOS).
- Wait for the gateway to be ready at `http://127.0.0.1:18800`.
- Open a window loading that URL (the gateway serves the dashboard when built with `--features dashboard`).

If the gateway is not bundled or fails to start, you’ll see an error in the terminal. You can still start the gateway manually (`cargo run -p consistent-world-gateway -- --config ~/.consistent-world/consistent_world.toml`) and the window will load once the gateway is up.

## Build for distribution

From `desktop/`:

```bash
pnpm tauri build
```

This runs `beforeBuildCommand` (builds the gateway and copies the sidecar), then builds the Tauri app. Outputs are under `src-tauri/target/release/bundle/` (e.g. `.app`, `.exe`, `.deb`).

## Config and data

- **Config:** The app writes a default `consistent_world.toml` to the app data directory on first run. Edit it there or run `consistent-world-gateway --onboard` to configure credentials (keyring).
- **Data:** Gateway uses `~/.consistent-world/` for memory and schedules by default (paths in the generated config).

## Architecture

- **Tauri (Rust):** Spawns the `consistent-world` binary as a sidecar, waits for `/health`, then opens a window to `http://127.0.0.1:18800`.
- **Frontend:** The window loads the URL served by the gateway (embedded SvelteKit dashboard when the gateway is built with `--features dashboard`).
- **On exit:** The app kills the gateway process.

So one app = one window + one gateway process; no separate browser or terminal needed.
