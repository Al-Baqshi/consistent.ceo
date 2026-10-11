//! Example WASM plugin for Consistent. Canonical ABI reference.
//! Build: cargo build -p consistent-world-plugin-hello-world --target wasm32-wasip1 --release

// Host imports (provided by the Consistent host at runtime). These are
// the WASM host ABI surface — unused from native builds, linked by the guest.
#[allow(dead_code)]
#[link(wasm_import_module = "env")]
unsafe extern "C" {
    fn host_log(level: i32, msg: *const u8, len: u32);
    fn host_read_file(path: *const u8, path_len: u32) -> i32;
    fn host_http_request(req: *const u8, req_len: u32) -> i32;
}

/// Result buffer: [len: u32 LE][data]. Host reads from this.
const RESULT_CAP: usize = 16384;
static mut RESULT_BUF: [u8; RESULT_CAP] = [0; RESULT_CAP];

/// WASM export: called once when the host loads the plugin.
#[unsafe(no_mangle)]
pub extern "C" fn plugin_init() -> i32 {
    0 // success
}

/// Returns pointer to length-prefixed JSON manifest (4 bytes LE length + JSON bytes).
#[unsafe(no_mangle)]
pub extern "C" fn plugin_info() -> *const u8 {
    let manifest = r#"{"name":"hello-world","version":"0.1.0","description":"Hello world plugin","wasm_url":"","permissions":[]}"#;
    let bytes = manifest.as_bytes();
    let len = bytes.len().min(RESULT_CAP - 4);
    unsafe {
        RESULT_BUF[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        RESULT_BUF[4..4 + len].copy_from_slice(&bytes[..len]);
        std::ptr::addr_of!(RESULT_BUF).cast::<u8>()
    }
}

/// Execute: args are JSON bytes at (args_ptr, args_len). Returns ptr to length-prefixed result in RESULT_BUF.
// WASM export ABI: the host guarantees (ptr, len) validity; null/len are checked below.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn plugin_execute(args_ptr: *const u8, args_len: u32) -> *const u8 {
    let args_len = args_len as usize;
    if args_ptr.is_null() || args_len > RESULT_CAP {
        return result_ptr(b"{\"error\":\"invalid args\"}");
    }
    let args = unsafe { std::slice::from_raw_parts(args_ptr, args_len) };
    let args_str = std::str::from_utf8(args).unwrap_or("");
    let out: String = if args_str.is_empty() || args_str == "{}" {
        "{\"message\":\"Hello from hello-world plugin\"}".to_string()
    } else {
        format!(
            "{{\"echo\":\"{}\",\"message\":\"Hello\"}}",
            escape_json(args_str)
        )
    };
    result_ptr(out.as_bytes())
}

// Encode `data` into RESULT_BUF as [len: u32 LE][bytes] (truncated to fit)
// and return a pointer to it, for the host to read across the WASM boundary.
fn result_ptr(data: &[u8]) -> *const u8 {
    let len = data.len().min(RESULT_CAP - 4);
    unsafe {
        RESULT_BUF[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        RESULT_BUF[4..4 + len].copy_from_slice(&data[..len]);
        std::ptr::addr_of!(RESULT_BUF).cast::<u8>()
    }
}

fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}
