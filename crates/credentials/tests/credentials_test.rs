//! Phase 14 tests: keyring set/get, vault init/unlock, vault wrong password.

use consistent_world_credentials::keyring::KeyringStore;
use consistent_world_credentials::vault::EncryptedVault;

#[test]
fn test_keyring_set_get() {
    let store = KeyringStore::new("consistent-world-test");
    let key = "test-key-set-get";
    let value = "secret-value-123";
    if store.set(key, value).is_err() {
        eprintln!("keyring set failed (no keychain in env); skipping keyring test");
        return;
    }
    let got = match store.get(key) {
        Ok(v) => v,
        Err(e) => {
            if e.to_string().contains("No matching") || e.to_string().contains("secure storage") {
                eprintln!("keyring get failed (keychain not persisting); skipping keyring test");
                let _ = store.delete(key);
                return;
            }
            panic!("keyring get failed: {}", e);
        }
    };
    assert_eq!(got, value);
    store.delete(key).unwrap();
    assert!(store.get(key).is_err());
}

#[test]
fn test_vault_init_and_unlock() {
    let temp = tempfile::tempdir().unwrap();
    let vault_path = temp.path().join("vault.bin");
    let vault = EncryptedVault::new(vault_path.clone());
    vault.init("master-pass").unwrap();
    vault.set("mykey", "myvalue").unwrap();

    let vault2 = EncryptedVault::new(vault_path);
    vault2.unlock("master-pass").unwrap();
    let v = vault2.get("mykey").unwrap();
    assert_eq!(v, "myvalue");
}

#[test]
fn test_vault_wrong_password() {
    let temp = tempfile::tempdir().unwrap();
    let vault_path = temp.path().join("vault2.bin");
    let vault = EncryptedVault::new(vault_path.clone());
    vault.init("correct-pass").unwrap();
    vault.set("k", "v").unwrap();

    let vault2 = EncryptedVault::new(vault_path);
    let err = vault2.unlock("wrong-pass").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("Decryption failed") || msg.contains("wrong password"),
        "expected decryption error, got: {}",
        msg
    );
}
