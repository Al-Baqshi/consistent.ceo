/// Defines the ABI (Application Binary Interface) between the WASM plugin and the host.
///
/// Plugins export these functions:
///   - `plugin_init() -> i32`
///   - `plugin_info() -> i32`  (ptr to length-prefixed JSON PluginManifest, or 0)
///   - `plugin_execute(args: *const u8, len: u32) -> *const u8`
///
/// Host provides these imports to plugins:
///   - `host_log(level: i32, msg: *const u8, len: u32)`
///   - `host_read_file(path: *const u8, len: u32) -> *const u8`  (if filesystem allowed)
///   - `host_http_request(req: *const u8, len: u32) -> *const u8` (if network allowed)
pub const ABI_VERSION: u32 = 1;

/// Names of the functions plugins must export.
pub mod export_names {
    pub const INIT: &str = "plugin_init";
    pub const INFO: &str = "plugin_info";
    pub const EXECUTE: &str = "plugin_execute";
}

/// Names of the functions the host provides to plugins as imports.
pub mod import_names {
    pub const LOG: &str = "host_log";
    pub const READ_FILE: &str = "host_read_file";
    pub const HTTP_REQUEST: &str = "host_http_request";
}
