//! Plugin registry client: search a remote registry, download and verify
//! WASM plugins by SHA256, and track what's installed locally.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Metadata describing one plugin, as returned by the registry and stored
/// alongside each installed plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub description: String,
    pub wasm_url: String,
    #[serde(default)]
    pub permissions: Vec<String>,
    /// SHA256 hex of the WASM file for verification on install.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// Client for a remote plugin registry, paired with the local directory
/// installed plugins are unpacked into.
pub struct PluginRegistry {
    pub registry_url: String,
    pub local_plugins_dir: String,
}

impl PluginRegistry {
    pub fn new(registry_url: String, local_plugins_dir: String) -> Self {
        Self {
            registry_url,
            local_plugins_dir,
        }
    }

    /// GET `{registry_url}/search?q={query}` and deserialize `Vec<PluginManifest>`.
    pub async fn search(&self, query: &str) -> anyhow::Result<Vec<PluginManifest>> {
        let url = format!(
            "{}/search?q={}",
            self.registry_url.trim_end_matches('/'),
            urlencoding::encode(query)
        );
        let client = reqwest::Client::new();
        let res = client
            .get(&url)
            .send()
            .await
            .context("registry search request")?;
        let manifests: Vec<PluginManifest> = res.json().await.context("registry search JSON")?;
        Ok(manifests)
    }

    /// Download WASM from manifest's wasm_url, verify SHA256 if present, write to local_plugins_dir/{name}/.
    pub async fn install(&self, manifest: &PluginManifest) -> anyhow::Result<()> {
        let client = reqwest::Client::new();
        let bytes = client
            .get(&manifest.wasm_url)
            .send()
            .await
            .context("download wasm")?
            .bytes()
            .await
            .context("download wasm body")?;
        if let Some(ref expected) = manifest.sha256 {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            let got = format!("{:x}", hasher.finalize());
            if !eq_ignore_ascii_case(&got, expected) {
                anyhow::bail!("SHA256 mismatch: expected {} got {}", expected, got);
            }
        }
        let dir = Path::new(&self.local_plugins_dir).join(&manifest.name);
        std::fs::create_dir_all(&dir).context("create plugin dir")?;
        std::fs::write(dir.join("plugin.wasm"), &bytes).context("write plugin.wasm")?;
        let manifest_path = dir.join("manifest.json");
        std::fs::write(manifest_path, serde_json::to_string_pretty(manifest)?)
            .context("write manifest.json")?;
        Ok(())
    }

    /// Read manifest JSONs from local_plugins_dir (each subdir with manifest.json).
    pub fn list_installed(&self) -> anyhow::Result<Vec<PluginManifest>> {
        let root = Path::new(&self.local_plugins_dir);
        if !root.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(root).context("read local_plugins_dir")? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                let manifest_path = path.join("manifest.json");
                if manifest_path.is_file() {
                    let s =
                        std::fs::read_to_string(&manifest_path).context("read manifest.json")?;
                    let m: PluginManifest =
                        serde_json::from_str(&s).context("parse manifest.json")?;
                    out.push(m);
                }
            }
        }
        Ok(out)
    }
}

// Case-insensitive hex comparison for the SHA256 check in `install`.
fn eq_ignore_ascii_case(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}
