//! The OS-keychain backend for credential storage.
//!
//! A thin wrapper over the `keyring` crate, which talks to macOS Keychain,
//! Windows Credential Manager, or Linux's Secret Service depending on
//! platform. Preferred over [`crate::vault`] wherever an OS keychain exists,
//! since it needs no master password of its own.

/// One named keychain, identified by `service_name` (e.g. the app's bundle id).
pub struct KeyringStore {
    pub service_name: String,
}

impl KeyringStore {
    pub fn new(service_name: &str) -> Self {
        Self {
            service_name: service_name.to_string(),
        }
    }

    /// Store `value` under `key`, overwriting any existing entry.
    pub fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let entry = keyring::Entry::new(&self.service_name, key)?;
        entry.set_password(value)?;
        Ok(())
    }

    /// Read back a previously stored value.
    pub fn get(&self, key: &str) -> anyhow::Result<String> {
        let entry = keyring::Entry::new(&self.service_name, key)?;
        let secret = entry.get_password()?;
        Ok(secret)
    }

    /// Remove an entry. Idempotent: deleting an absent key is not an error.
    pub fn delete(&self, key: &str) -> anyhow::Result<()> {
        let entry = keyring::Entry::new(&self.service_name, key)?;
        // Ignore "no entry" errors to make delete idempotent.
        let _ = entry.delete_credential();
        Ok(())
    }

    pub fn list_keys(&self) -> anyhow::Result<Vec<String>> {
        // The `keyring` crate does not expose an API to list all entries for a service
        // in a portable way across backends, so this is intentionally left as a stub.
        // Callers should track known keys separately (e.g. in a config file).
        Ok(Vec::new())
    }
}
