//! Tests for WASM plugin host: load, init, execute, and sandbox (memory cap).

use consistent_world_plugins::host::{SandboxConfig, WasmHost};

/// Minimal WASM that exports plugin_init (return 0) and plugin_execute (return 0), imports host_log etc.
const MINIMAL_WAT: &str = r#"
(module
  (type (;0;) (func (result i32)))
  (type (;1;) (func (param i32 i32) (result i32)))
  (type (;2;) (func (param i32 i32 i32)))
  (type (;3;) (func (param i32 i32) (result i32)))
  (import "env" "host_log" (func (;0;) (type 2)))
  (import "env" "host_read_file" (func (;1;) (type 3)))
  (import "env" "host_http_request" (func (;2;) (type 3)))
  (func (;3;) (type 0) (result i32) i32.const 0)
  (func (;4;) (type 1) (param i32 i32) (result i32) i32.const 0)
  (memory (;0;) 1)
  (export "memory" (memory 0))
  (export "plugin_init" (func 3))
  (export "plugin_execute" (func 4))
)
"#;

#[test]
fn test_wasm_plugin_loads() {
    let config = SandboxConfig::default();
    let host = WasmHost::new(config).unwrap();
    let wasm_bytes = wat::parse_str(MINIMAL_WAT).unwrap();
    host.load_plugin_bytes("minimal", &wasm_bytes).unwrap();
    let out = host.call_plugin("minimal", b"{}").unwrap();
    assert!(
        out.is_empty()
            || out
                .get(0..4)
                .map(|x| u32::from_le_bytes(x.try_into().unwrap()))
                == Some(0)
    );
}

#[test]
fn test_wasm_plugin_execute() {
    let config = SandboxConfig::default();
    let host = WasmHost::new(config).unwrap();
    let wasm_bytes = wat::parse_str(MINIMAL_WAT).unwrap();
    host.load_plugin_bytes("minimal", &wasm_bytes).unwrap();
    let out = host.call_plugin("minimal", b"{\"test\":1}").unwrap();
    // Minimal plugin returns ptr 0 so we get empty or length-prefixed empty
    assert!(out.len() <= 4);
}

#[test]
fn test_wasm_plugin_list_loaded() {
    let config = SandboxConfig::default();
    let host = WasmHost::new(config).unwrap();
    assert!(host.list_loaded().is_empty());
    let wasm_bytes = wat::parse_str(MINIMAL_WAT).unwrap();
    host.load_plugin_bytes("minimal", &wasm_bytes).unwrap();
    let list = host.list_loaded();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0], "minimal");
}

/// WASM that grows memory by 1 page in plugin_init. With max_memory_bytes = 1 page, this exceeds the limit.
const MEMORY_GROW_WAT: &str = r#"
(module
  (type (;0;) (func (result i32)))
  (type (;1;) (func (param i32 i32) (result i32)))
  (type (;2;) (func (param i32 i32 i32)))
  (type (;3;) (func (param i32 i32) (result i32)))
  (import "env" "host_log" (func (;0;) (type 2)))
  (import "env" "host_read_file" (func (;1;) (type 3)))
  (import "env" "host_http_request" (func (;2;) (type 3)))
  (func (;3;) (type 0) (result i32)
    (memory.grow (i32.const 1))
    (drop)
    (i32.const 0))
  (func (;4;) (type 1) (param i32 i32) (result i32) (i32.const 0))
  (memory (;0;) 1)
  (export "memory" (memory 0))
  (export "plugin_init" (func 3))
  (export "plugin_execute" (func 4))
)
"#;

#[test]
fn test_wasm_sandbox_memory_cap() {
    // 1 page = 64KiB. Plugin starts with 1 page and tries to grow by 1 in plugin_init.
    let config = SandboxConfig {
        max_memory_bytes: 65536,
        ..SandboxConfig::default()
    };
    let host = WasmHost::new(config).unwrap();
    let wasm_bytes = wat::parse_str(MEMORY_GROW_WAT).unwrap();
    host.load_plugin_bytes("grower", &wasm_bytes).unwrap();
    let result = host.call_plugin("grower", b"{}");
    // Should fail: memory limit exceeded (trap or error).
    assert!(
        result.is_err(),
        "plugin that exceeds memory limit should be stopped"
    );
}
