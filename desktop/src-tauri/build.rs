//! Cargo build script for the Tauri desktop shell: runs tauri-build's
//! codegen (parsing tauri.conf.json, embedding icons, generating the
//! permission manifest) before the app itself compiles.
fn main() {
    tauri_build::build()
}
