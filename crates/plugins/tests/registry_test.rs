//! Tests for plugin registry: list_installed with temp dir.

use consistent_world_plugins::registry::{PluginManifest, PluginRegistry};

#[test]
fn test_plugin_registry_list_installed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    // Pre-populate as if a plugin was installed: {name}/manifest.json + plugin.wasm
    let name = "test-plugin";
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = PluginManifest {
        name: name.to_string(),
        version: "0.1.0".to_string(),
        description: "A test plugin".to_string(),
        wasm_url: "https://example.com/plugin.wasm".to_string(),
        permissions: vec![],
        sha256: None,
    };
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
    // Minimal wasm bytes (valid wasm magic + version)
    let wasm: Vec<u8> = [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00].to_vec();
    std::fs::write(dir.join("plugin.wasm"), &wasm).unwrap();

    let registry = PluginRegistry::new(
        "https://registry.example.com".to_string(),
        root.to_string_lossy().to_string(),
    );
    let installed = registry.list_installed().unwrap();
    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0].name, name);
    assert_eq!(installed[0].version, "0.1.0");
}

#[test]
fn test_plugin_registry_list_installed_empty_dir() {
    let temp = tempfile::tempdir().unwrap();
    let registry = PluginRegistry::new(
        "https://registry.example.com".to_string(),
        temp.path().to_string_lossy().to_string(),
    );
    let installed = registry.list_installed().unwrap();
    assert!(installed.is_empty());
}
