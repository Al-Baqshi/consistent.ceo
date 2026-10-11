/// Email skill plugin for Consistent
///
/// Provides: send_email, read_inbox, search_email
/// Requires: network permission in sandbox config

/// WASM export: called once when the host loads the plugin; always reports
/// success since there is no setup to do yet.
#[unsafe(no_mangle)]
pub extern "C" fn plugin_init() -> i32 {
    0
}

/// WASM export: run a plugin action (send/read/search email). Not yet
/// implemented, so any call currently panics.
#[unsafe(no_mangle)]
pub extern "C" fn plugin_execute(_args_ptr: *const u8, _args_len: u32) -> *const u8 {
    todo!("Implement email actions via IMAP/SMTP")
}
