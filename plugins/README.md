# Consistent WASM Plugins

Plugins are WebAssembly modules that run sandboxed inside the Consistent host. They extend the agent with custom tools.

## Build target

- **Target:** `wasm32-wasip1` (WASI Preview 1)
- **Prerequisite:** `rustup target add wasm32-wasip1`
- **Build:**
  ```bash
  cargo build -p consistent-world-plugin-hello-world --target wasm32-wasip1 --release
  ```
- **Output:** `target/wasm32-wasip1/release/consistent_world_plugin_hello_world.wasm` (name may vary by crate name).

## ABI (Application Binary Interface)

### Exports (plugin must provide)

| Function | Signature | Description |
|----------|-----------|-------------|
| `plugin_init` | `() -> i32` | Initialize; return 0 on success. |
| `plugin_info` | `() -> i32` | Return pointer to length-prefixed JSON manifest (4 bytes LE length + JSON). 0 if not supported. |
| `plugin_execute` | `(args_ptr: i32, args_len: i32) -> i32` | Run with input JSON at (args_ptr, args_len). Return pointer to length-prefixed result (4 bytes LE length + payload). |

**Memory:** The plugin must export linear `memory`. The host writes args at the given offset and reads the result from the returned pointer (first 4 bytes = length, then data).

### Imports (host provides under `env`)

| Function | Signature | When |
|----------|-----------|------|
| `host_log` | `(level: i32, msg_ptr: i32, msg_len: i32) -> ()` | Always. |
| `host_read_file` | `(path_ptr: i32, path_len: i32) -> i32` | If sandbox allows filesystem. |
| `host_http_request` | `(req_ptr: i32, req_len: i32) -> i32` | If sandbox allows network. |

## Manifest format

Each installed plugin lives in a directory under `local_plugins_dir` with:

- `plugin.wasm` — the WASM binary
- `manifest.json` — plugin metadata

**manifest.json** (same as registry):

```json
{
  "name": "hello-world",
  "version": "0.1.0",
  "description": "Short description",
  "wasm_url": "https://...",
  "permissions": [],
  "sha256": "optional-hex-for-install-verification"
}
```

## Sandbox

The host applies:

- **Fuel:** Execution stops when fuel is consumed (roughly 1 unit per instruction).
- **Memory cap:** Linear memory growth is limited (e.g. 64MB).
- **Host calls:** `host_read_file` and `host_http_request` are only available when the sandbox config allows them.

## Example: hello-world

See `plugins/hello-world/`. It implements `plugin_init`, `plugin_info`, and `plugin_execute`, and declares the host imports so it links against the host.

## Adding a new plugin

1. Create a crate with `crate-type = ["cdylib"]`.
2. Implement the three exports and declare the `env` imports.
3. Build for `wasm32-wasip1`.
4. Put `plugin.wasm` and `manifest.json` in a subdir of the host’s `local_plugins_dir`.
5. On startup, the host scans that dir and registers each plugin as a tool.
