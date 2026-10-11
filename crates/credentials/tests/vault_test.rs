//! Vault behaviour: items, master-password rotation, sharing, on-disk safety,
//! and migration from the version 1 format.

use consistent_world_credentials::model::{ItemKind, ItemSecret, NewItem};
use consistent_world_credentials::vault::Vault;
use tempfile::TempDir;

// Shared test setup: a freshly initialized vault under a temp directory.
fn new_vault(dir: &TempDir, name: &str, password: &str) -> Vault {
    let vault = Vault::new(dir.path().join(name));
    vault.init(password).unwrap();
    vault
}

#[test]
fn items_round_trip_through_lock_and_unlock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.json");

    let vault = Vault::new(path.clone());
    vault.init("master").unwrap();
    let id = vault
        .add_item(NewItem::login("GitHub", "ahmad", "s3cret-pw"))
        .unwrap();

    // A fresh handle proves the item survived encryption to disk.
    let reopened = Vault::new(path);
    reopened.unlock("master").unwrap();

    let summaries = reopened.list_items(None).unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].name, "GitHub");
    assert_eq!(summaries[0].kind, ItemKind::Login);

    let secret = reopened.item_secret(&id).unwrap();
    assert_eq!(secret.username.as_deref(), Some("ahmad"));
    assert_eq!(secret.password.as_deref(), Some("s3cret-pw"));
}

#[test]
fn plaintext_never_appears_in_the_vault_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.json");
    let vault = Vault::new(path.clone());
    vault.init("master").unwrap();
    vault
        .add_item(NewItem::login("Bank", "ahmad", "hunter2-plaintext"))
        .unwrap();
    vault.set("OPENAI_API_KEY", "sk-do-not-leak").unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("hunter2-plaintext"));
    assert!(!raw.contains("sk-do-not-leak"));
    // Item names live inside the encrypted payload too.
    assert!(!raw.contains("Bank"));
    assert!(!raw.contains("OPENAI_API_KEY"));
}

#[test]
fn locked_vault_refuses_reads() {
    let dir = tempfile::tempdir().unwrap();
    let vault = new_vault(&dir, "v.json", "master");
    vault.set("k", "v").unwrap();
    assert!(vault.is_unlocked());

    vault.lock();
    assert!(!vault.is_unlocked());
    assert!(vault.get("k").is_err());
    assert!(vault.list_items(None).is_err());
    assert!(vault.add_item(NewItem::login("x", "y", "z")).is_err());
}

#[test]
fn secrets_can_be_deleted_and_listed() {
    let dir = tempfile::tempdir().unwrap();
    let vault = new_vault(&dir, "v.json", "master");
    vault.set("b", "2").unwrap();
    vault.set("a", "1").unwrap();
    assert_eq!(vault.list_keys().unwrap(), vec!["a", "b"]);

    vault.delete("a").unwrap();
    assert_eq!(vault.list_keys().unwrap(), vec!["b"]);
    // Deleting something absent is a no-op, not an error.
    vault.delete("a").unwrap();
}

#[test]
fn updating_an_item_secret_preserves_its_id() {
    let dir = tempfile::tempdir().unwrap();
    let vault = new_vault(&dir, "v.json", "master");
    let id = vault
        .add_item(NewItem::login("Mail", "ahmad", "old-pw"))
        .unwrap();

    vault
        .update_item_secret(
            &id,
            &ItemSecret {
                username: Some("ahmad".into()),
                password: Some("new-pw".into()),
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(vault.list_items(None).unwrap().len(), 1);
    assert_eq!(
        vault.item_secret(&id).unwrap().password.as_deref(),
        Some("new-pw")
    );
}

#[test]
fn deleting_a_missing_item_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let vault = new_vault(&dir, "v.json", "master");
    assert!(vault.delete_item("no-such-id").is_err());
}

#[test]
fn changing_the_master_password_keeps_the_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.json");
    let vault = Vault::new(path.clone());
    vault.init("old-master").unwrap();
    let id = vault
        .add_item(NewItem::login("Router", "admin", "admin-pw"))
        .unwrap();
    vault.set("TOKEN", "abc123").unwrap();

    vault
        .change_master_password("old-master", "new-master")
        .unwrap();

    let reopened = Vault::new(path);
    assert!(
        reopened.unlock("old-master").is_err(),
        "the old password must stop working"
    );
    reopened.unlock("new-master").unwrap();
    assert_eq!(reopened.get("TOKEN").unwrap(), "abc123");
    assert_eq!(
        reopened.item_secret(&id).unwrap().password.as_deref(),
        Some("admin-pw")
    );
}

#[test]
fn changing_the_master_password_requires_the_current_one() {
    let dir = tempfile::tempdir().unwrap();
    let vault = new_vault(&dir, "v.json", "right");
    assert!(vault.change_master_password("wrong", "new").is_err());
    // The original password must still work after a failed attempt.
    vault.unlock("right").unwrap();
}

#[test]
fn empty_master_passwords_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::new(dir.path().join("v.json"));
    assert!(vault.init("").is_err());

    let ok = new_vault(&dir, "ok.json", "master");
    assert!(ok.change_master_password("master", "").is_err());
}

#[test]
fn sharing_an_item_to_another_user() {
    let dir = tempfile::tempdir().unwrap();
    let alice = new_vault(&dir, "alice.json", "alice-master");
    let bob = new_vault(&dir, "bob.json", "bob-master");

    let item_id = alice
        .add_item(NewItem::login("Stripe prod", "ops@x.io", "rotate-me"))
        .unwrap();

    let envelope = alice
        .create_share(
            &item_id,
            &bob.public_key_b64().unwrap(),
            "alice",
            Some("on-call handover".to_string()),
            None,
        )
        .unwrap();

    // The envelope is what would travel through the gateway; it must not leak.
    let wire = serde_json::to_string(&envelope).unwrap();
    assert!(!wire.contains("rotate-me"));

    let new_id = bob.accept_share(&envelope).unwrap();
    let secret = bob.item_secret(&new_id).unwrap();
    assert_eq!(secret.password.as_deref(), Some("rotate-me"));

    let summary = bob
        .list_items(None)
        .unwrap()
        .into_iter()
        .find(|i| i.id == new_id)
        .unwrap();
    assert_eq!(summary.name, "Stripe prod");
    assert_eq!(summary.shared_from.as_deref(), Some("alice"));

    // Alice keeps her copy, under her own id.
    assert_eq!(alice.list_items(None).unwrap().len(), 1);
    assert_ne!(new_id, item_id);
}

#[test]
fn a_share_cannot_be_opened_by_the_wrong_recipient() {
    let dir = tempfile::tempdir().unwrap();
    let alice = new_vault(&dir, "alice.json", "a");
    let bob = new_vault(&dir, "bob.json", "b");
    let mallory = new_vault(&dir, "mallory.json", "m");

    let id = alice
        .add_item(NewItem::login("Prod DB", "root", "top-secret"))
        .unwrap();
    let envelope = alice
        .create_share(&id, &bob.public_key_b64().unwrap(), "alice", None, None)
        .unwrap();

    let err = mallory.accept_share(&envelope).unwrap_err().to_string();
    assert!(
        err.contains("addressed to a different vault"),
        "unexpected error: {err}"
    );
}

#[test]
fn a_share_readdressed_to_mallory_still_cannot_be_decrypted() {
    let dir = tempfile::tempdir().unwrap();
    let alice = new_vault(&dir, "alice.json", "a");
    let bob = new_vault(&dir, "bob.json", "b");
    let mallory = new_vault(&dir, "mallory.json", "m");

    let id = alice
        .add_item(NewItem::login("Prod DB", "root", "top-secret"))
        .unwrap();
    let mut envelope = alice
        .create_share(&id, &bob.public_key_b64().unwrap(), "alice", None, None)
        .unwrap();

    // Defeat the addressing check; the crypto must still hold, because the item
    // key was sealed to Bob's public key.
    envelope.to_public_key_b64 = mallory.public_key_b64().unwrap();
    assert!(mallory.accept_share(&envelope).is_err());
}

#[test]
fn expired_shares_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let alice = new_vault(&dir, "alice.json", "a");
    let bob = new_vault(&dir, "bob.json", "b");

    let id = alice
        .add_item(NewItem::login("Temp access", "u", "p"))
        .unwrap();
    let yesterday = (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339();
    let envelope = alice
        .create_share(
            &id,
            &bob.public_key_b64().unwrap(),
            "alice",
            None,
            Some(yesterday),
        )
        .unwrap();

    let err = bob.accept_share(&envelope).unwrap_err().to_string();
    assert!(err.contains("expired"), "unexpected error: {err}");

    // A future expiry is accepted.
    let tomorrow = (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339();
    let ok = alice
        .create_share(
            &id,
            &bob.public_key_b64().unwrap(),
            "alice",
            None,
            Some(tomorrow),
        )
        .unwrap();
    assert!(bob.accept_share(&ok).is_ok());
}

#[test]
fn a_shared_item_survives_the_recipient_rotating_their_master_password() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bob.json");
    let alice = new_vault(&dir, "alice.json", "a");
    let bob = Vault::new(path.clone());
    bob.init("bob-old").unwrap();

    let id = alice
        .add_item(NewItem::login("Shared", "u", "p-shared"))
        .unwrap();
    let envelope = alice
        .create_share(&id, &bob.public_key_b64().unwrap(), "alice", None, None)
        .unwrap();
    let bob_item = bob.accept_share(&envelope).unwrap();

    bob.change_master_password("bob-old", "bob-new").unwrap();

    let reopened = Vault::new(path);
    reopened.unlock("bob-new").unwrap();
    assert_eq!(
        reopened.item_secret(&bob_item).unwrap().password.as_deref(),
        Some("p-shared")
    );
}

#[cfg(unix)]
#[test]
fn vault_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.json");
    let vault = Vault::new(path.clone());
    vault.init("master").unwrap();
    vault.set("k", "v").unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "vault must not be group- or world-readable");
}

#[test]
fn no_temporary_files_are_left_behind() {
    let dir = tempfile::tempdir().unwrap();
    let vault = new_vault(&dir, "v.json", "master");
    for i in 0..5 {
        vault.set(&format!("k{i}"), "v").unwrap();
    }
    let strays: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(strays.is_empty(), "left temp files behind: {strays:?}");
}

#[test]
fn init_refuses_to_clobber_an_existing_vault() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.json");
    let vault = Vault::new(path.clone());
    vault.init("master").unwrap();
    vault.set("k", "v").unwrap();

    assert!(Vault::new(path.clone()).init("other").is_err());

    // The original contents are intact.
    let reopened = Vault::new(path);
    reopened.unlock("master").unwrap();
    assert_eq!(reopened.get("k").unwrap(), "v");
}

/// Build a version 1 file by hand (`salt | nonce | ciphertext`, Argon2 defaults,
/// flat JSON string map) exactly as the previous implementation wrote it.
fn write_v1_vault(path: &std::path::Path, password: &str, secrets: &[(&str, &str)]) {
    write_v1_vault_with_salt(path, password, secrets, rand::random());
}

/// Same, but with a caller-chosen salt — used to reproduce the case where a v1
/// file's leading bytes look like the start of a JSON document.
fn write_v1_vault_with_salt(
    path: &std::path::Path,
    password: &str,
    secrets: &[(&str, &str)],
    salt: [u8; 16],
) {
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit};
    use std::collections::HashMap;

    let mut key = [0u8; 32];
    argon2::Argon2::default()
        .hash_password_into(password.as_bytes(), &salt, &mut key)
        .unwrap();

    let map: HashMap<String, String> = secrets
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let plaintext = serde_json::to_vec(&map).unwrap();

    let nonce: [u8; 12] = rand::random();
    let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
    let ciphertext = cipher
        .encrypt(aes_gcm::Nonce::from_slice(&nonce), plaintext.as_ref())
        .unwrap();

    let mut out = Vec::new();
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    std::fs::write(path, out).unwrap();
}

#[test]
fn a_v1_vault_is_migrated_on_unlock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.bin");
    write_v1_vault(
        &path,
        "legacy-master",
        &[("OPENAI_API_KEY", "sk-legacy"), ("B", "2")],
    );

    let vault = Vault::new(path.clone());
    vault.unlock("legacy-master").unwrap();

    assert_eq!(vault.get("OPENAI_API_KEY").unwrap(), "sk-legacy");
    assert_eq!(vault.get("B").unwrap(), "2");

    // The file is now current-format JSON, and the original was preserved
    // alongside it.
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.trim_start().starts_with('{'));
    assert!(raw.contains("\"version\": 3"));
    assert!(path.with_extension("v1.bak").exists());

    // The migrated vault behaves like a natively created one, same password.
    let reopened = Vault::new(path);
    reopened.unlock("legacy-master").unwrap();
    assert_eq!(reopened.get("B").unwrap(), "2");
    let id = reopened
        .add_item(NewItem::login("Post migration", "u", "p"))
        .unwrap();
    assert_eq!(
        reopened.item_secret(&id).unwrap().password.as_deref(),
        Some("p")
    );
}

#[test]
fn a_v1_vault_whose_salt_looks_like_json_still_migrates() {
    // Regression: v1-vs-JSON detection once sniffed the first non-whitespace
    // byte for '{'. A v1 file starts with 16 random salt bytes, so ~1 in 256
    // vaults began with '{' (or whitespace then '{') and was misread as a
    // corrupt JSON file instead of being migrated. Force exactly that salt.
    let dir = tempfile::tempdir().unwrap();
    for salt_prefix in [
        [b'{'; 16],             // starts with '{'
        *b"  \t\n{ not json  ", // whitespace then '{'
    ] {
        let path = dir.path().join(format!("legacy-{}.bin", salt_prefix[0]));
        write_v1_vault_with_salt(&path, "legacy-master", &[("K", "V")], salt_prefix);

        let vault = Vault::new(path);
        vault.unlock("legacy-master").unwrap();
        assert_eq!(vault.get("K").unwrap(), "V");
    }
}

#[test]
fn a_v1_vault_with_the_wrong_password_is_not_migrated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.bin");
    write_v1_vault(&path, "right", &[("K", "V")]);
    let before = std::fs::read(&path).unwrap();

    let vault = Vault::new(path.clone());
    assert!(vault.unlock("wrong").is_err());

    // The file must be untouched, and the real password must still work.
    assert_eq!(std::fs::read(&path).unwrap(), before);
    Vault::new(path).unlock("right").unwrap();
}

#[test]
fn a_corrupted_vault_file_reports_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.json");
    let vault = Vault::new(path.clone());
    vault.init("master").unwrap();

    let mut raw = std::fs::read_to_string(&path).unwrap();
    raw = raw.replace("\"version\": 3", "\"version\": 99");
    std::fs::write(&path, raw).unwrap();

    let err = Vault::new(path).unlock("master").unwrap_err().to_string();
    assert!(err.contains("unsupported vault format version"), "{err}");
}
