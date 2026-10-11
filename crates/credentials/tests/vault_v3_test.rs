//! The version 3 surface: many unlock methods, scoped environments, portable
//! backups, and the audit log.
//!
//! The theme running through these is that no single credential and no single
//! device is load-bearing. A passkey can be revoked without touching the data, a
//! master password can be changed without invalidating a recovery code, and a
//! backup opens with nothing but its passphrase.

use std::collections::BTreeMap;

use consistent_world_credentials::model::{ItemSecret, NewItem, VaultExport};
use consistent_world_credentials::recovery::random_key;
use consistent_world_credentials::scope::Scope;
use consistent_world_credentials::vault::{UnlockKind, Vault};
use tempfile::TempDir;

/// A fresh vault, unlocked, plus the recovery code minted at creation.
fn new_vault(dir: &TempDir, name: &str, password: &str) -> (Vault, String) {
    let vault = Vault::new(dir.path().join(format!("{name}.json")));
    let outcome = vault.init(password).unwrap();
    let code = outcome.recovery_code.formatted().to_string();
    (vault, code)
}

// Shorthand for building a valid entity scope in a test.
fn entity(id: &str) -> Scope {
    Scope::entity(id).unwrap()
}

// ---- recovery codes ---------------------------------------------------------

#[test]
fn every_new_vault_is_born_with_a_working_recovery_code() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, code) = new_vault(&dir, "v", "master");
    vault.set("K", "V").unwrap();
    vault.lock();

    // The code alone opens it. No password, no device.
    let reopened = Vault::new(vault.path().to_path_buf());
    reopened.unlock_with_recovery_code(&code).unwrap();
    assert_eq!(reopened.get("K").unwrap(), "V");
}

#[test]
fn a_wrong_recovery_code_does_not_open_the_vault() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault.lock();

    let (_, other_code) = new_vault(&dir, "other", "master");
    assert!(vault.unlock_with_recovery_code(&other_code).is_err());
    // Malformed input is refused too, without panicking.
    assert!(vault.unlock_with_recovery_code("not-a-code").is_err());
    assert!(!vault.is_unlocked());
}

#[test]
fn a_recovery_code_survives_a_master_password_change() {
    // The scenario this exists for: you rotate your password, then months later
    // lose every device and reach for the card in the drawer.
    let dir = tempfile::tempdir().unwrap();
    let (vault, code) = new_vault(&dir, "v", "old-master");
    vault.set("K", "V").unwrap();

    vault
        .change_master_password("old-master", "new-master")
        .unwrap();
    vault.lock();

    let reopened = Vault::new(vault.path().to_path_buf());
    reopened.unlock_with_recovery_code(&code).unwrap();
    assert_eq!(reopened.get("K").unwrap(), "V");
}

#[test]
fn a_second_recovery_code_can_be_minted_and_both_work() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, first) = new_vault(&dir, "v", "master");
    vault.set("K", "V").unwrap();

    let second = vault.add_recovery_code().unwrap().formatted().to_string();
    assert_ne!(first, second);

    for code in [&first, &second] {
        let v = Vault::new(vault.path().to_path_buf());
        v.unlock_with_recovery_code(code).unwrap();
        assert_eq!(v.get("K").unwrap(), "V");
    }
}

// ---- passkey and device-key unlock methods ----------------------------------

#[test]
fn a_passkey_key_can_be_enrolled_and_then_opens_the_vault() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault.set("K", "V").unwrap();

    // Stands in for a WebAuthn PRF evaluation: 32 uniform bytes the client
    // reproduces on demand from the authenticator.
    let prf_output = random_key();
    let mut params = BTreeMap::new();
    params.insert("prfSaltB64".to_string(), "c2FsdA==".to_string());

    let id = vault
        .add_key_unlock_method(UnlockKind::Passkey, "iPhone", &prf_output, params)
        .unwrap();
    vault.lock();

    let reopened = Vault::new(vault.path().to_path_buf());
    reopened.unlock_with_key(&id, &prf_output).unwrap();
    assert_eq!(reopened.get("K").unwrap(), "V");
}

#[test]
fn the_wrong_key_does_not_open_an_enrolled_method() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    let id = vault
        .add_key_unlock_method(
            UnlockKind::DeviceKey,
            "Laptop",
            &random_key(),
            BTreeMap::new(),
        )
        .unwrap();
    vault.lock();

    assert!(vault.unlock_with_key(&id, &random_key()).is_err());
    assert!(!vault.is_unlocked());
}

#[test]
fn a_password_method_cannot_be_opened_with_a_raw_key() {
    // Otherwise a caller could bypass Argon2 by handing over 32 bytes directly.
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault.lock();

    let err = vault
        .unlock_with_key("master-password", &random_key())
        .unwrap_err()
        .to_string();
    assert!(err.contains("expects a password"), "{err}");
}

#[test]
fn an_unknown_method_id_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault.lock();
    assert!(
        vault
            .unlock_with_key("no-such-method", &random_key())
            .is_err()
    );
}

#[test]
fn enrolling_a_new_method_requires_the_vault_to_be_open() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault.lock();

    assert!(
        vault
            .add_key_unlock_method(
                UnlockKind::DeviceKey,
                "Rogue",
                &random_key(),
                BTreeMap::new()
            )
            .is_err(),
        "a locked vault must not accept a new way in"
    );
}

#[test]
fn revoking_a_device_key_stops_it_opening_the_vault() {
    // The lost-laptop path.
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault.set("K", "V").unwrap();

    let laptop = random_key();
    let id = vault
        .add_key_unlock_method(
            UnlockKind::DeviceKey,
            "Old laptop",
            &laptop,
            BTreeMap::new(),
        )
        .unwrap();

    // It works, then it is revoked, then it does not.
    let check = Vault::new(vault.path().to_path_buf());
    check.unlock_with_key(&id, &laptop).unwrap();

    vault.remove_unlock_method(&id).unwrap();

    let after = Vault::new(vault.path().to_path_buf());
    assert!(after.unlock_with_key(&id, &laptop).is_err());
    // Everyone else still gets in, and the data is untouched.
    after.unlock("master").unwrap();
    assert_eq!(after.get("K").unwrap(), "V");
}

#[test]
fn the_last_way_in_can_never_be_removed() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");

    let methods = vault.unlock_methods().unwrap();
    // A vault starts with a password and a recovery code.
    assert_eq!(methods.len(), 2);

    vault.remove_unlock_method(&methods[0].id).unwrap();
    let err = vault
        .remove_unlock_method(&methods[1].id)
        .unwrap_err()
        .to_string();
    assert!(err.contains("only way into the vault"), "{err}");

    // Still openable by whatever remains.
    assert_eq!(vault.unlock_methods().unwrap().len(), 1);
}

#[test]
fn revoking_a_method_requires_the_vault_to_be_open() {
    // A stolen session must not be able to lock the owner out.
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    let id = vault.unlock_methods().unwrap()[0].id.clone();
    vault.lock();

    assert!(vault.remove_unlock_method(&id).is_err());
    assert_eq!(vault.unlock_methods().unwrap().len(), 2);
}

#[test]
fn unlock_methods_are_listable_while_locked_and_leak_no_key_material() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault
        .add_key_unlock_method(
            UnlockKind::Passkey,
            "iPhone",
            &random_key(),
            BTreeMap::new(),
        )
        .unwrap();
    vault.lock();

    // A client has to know a passkey is enrolled *before* it can unlock.
    let methods = vault.unlock_methods().unwrap();
    assert!(methods.iter().any(|m| m.kind == UnlockKind::Passkey));

    let json = serde_json::to_string(&methods).unwrap();
    assert!(
        !json.contains("wrappedUserKey") && !json.contains("wrapped_user_key"),
        "the wrapped key must never reach a client: {json}"
    );
}

#[test]
fn using_a_method_records_when_it_was_last_used() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    assert!(vault.unlock_methods().unwrap()[0].last_used_at.is_none());

    vault.lock();
    vault.unlock("master").unwrap();

    let method = vault
        .unlock_methods()
        .unwrap()
        .into_iter()
        .find(|m| m.kind == UnlockKind::MasterPassword)
        .unwrap();
    assert!(
        method.last_used_at.is_some(),
        "an unlock should be visible in the method list"
    );
}

// ---- scoped environment variables -------------------------------------------

#[test]
fn two_entities_hold_different_values_for_the_same_key() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");

    vault
        .set_scoped(&entity("acme"), "STRIPE_SECRET_KEY", "sk-acme")
        .unwrap();
    vault
        .set_scoped(&entity("globex"), "STRIPE_SECRET_KEY", "sk-globex")
        .unwrap();

    assert_eq!(
        vault.resolve(&entity("acme"), "STRIPE_SECRET_KEY").unwrap(),
        "sk-acme"
    );
    assert_eq!(
        vault
            .resolve(&entity("globex"), "STRIPE_SECRET_KEY")
            .unwrap(),
        "sk-globex"
    );
    // Neither leaks into the global space.
    assert!(vault.get("STRIPE_SECRET_KEY").is_err());
}

#[test]
fn an_entity_inherits_global_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault.set("OPENAI_API_KEY", "sk-shared").unwrap();

    assert_eq!(
        vault.resolve(&entity("acme"), "OPENAI_API_KEY").unwrap(),
        "sk-shared"
    );
    // But only through resolution — the entity's own scope is genuinely empty.
    assert!(vault.get_scoped(&entity("acme"), "OPENAI_API_KEY").is_err());
}

#[test]
fn an_entity_value_shadows_the_global_one() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault.set("OPENAI_API_KEY", "sk-shared").unwrap();
    vault
        .set_scoped(&entity("acme"), "OPENAI_API_KEY", "sk-own")
        .unwrap();

    assert_eq!(
        vault.resolve(&entity("acme"), "OPENAI_API_KEY").unwrap(),
        "sk-own"
    );
    // Global is unchanged, and every other entity still sees it.
    assert_eq!(vault.get("OPENAI_API_KEY").unwrap(), "sk-shared");
    assert_eq!(
        vault.resolve(&entity("globex"), "OPENAI_API_KEY").unwrap(),
        "sk-shared"
    );
}

#[test]
fn the_environment_an_entity_runs_with_is_global_overlaid_with_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault.set("OPENAI_API_KEY", "sk-shared").unwrap();
    vault.set("LOG_LEVEL", "info").unwrap();
    vault
        .set_scoped(&entity("acme"), "OPENAI_API_KEY", "sk-own")
        .unwrap();
    vault
        .set_scoped(&entity("acme"), "DATABASE_URL", "postgres://acme")
        .unwrap();

    let env = vault.env_for(&entity("acme")).unwrap();
    assert_eq!(env["OPENAI_API_KEY"], "sk-own", "own value must win");
    assert_eq!(env["LOG_LEVEL"], "info", "global default inherited");
    assert_eq!(env["DATABASE_URL"], "postgres://acme");
    assert_eq!(env.len(), 3);

    // An entity with nothing of its own gets exactly the global set.
    let bare = vault.env_for(&entity("nobody")).unwrap();
    assert_eq!(bare.len(), 2);
    assert_eq!(bare["OPENAI_API_KEY"], "sk-shared");
}

#[test]
fn deleting_the_last_variable_removes_the_scope_entirely() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault.set_scoped(&entity("acme"), "K", "V").unwrap();
    assert!(vault.list_scopes().unwrap().contains(&entity("acme")));

    vault.delete_scoped(&entity("acme"), "K").unwrap();
    assert!(
        !vault.list_scopes().unwrap().contains(&entity("acme")),
        "an empty scope should not linger in the scope list"
    );
}

#[test]
fn scoped_values_survive_a_lock_and_unlock() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault
        .set_scoped(&entity("acme"), "DATABASE_URL", "postgres://acme")
        .unwrap();
    vault.lock();

    let reopened = Vault::new(vault.path().to_path_buf());
    reopened.unlock("master").unwrap();
    assert_eq!(
        reopened
            .get_scoped(&entity("acme"), "DATABASE_URL")
            .unwrap(),
        "postgres://acme"
    );
}

#[test]
fn a_hostile_env_key_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    for bad in ["A=B", "HAS SPACE", "", "1LEADING"] {
        assert!(
            vault.set_scoped(&Scope::Global, bad, "v").is_err(),
            "{bad:?} should be refused"
        );
    }
}

#[test]
fn scoped_secret_values_never_appear_in_the_vault_file() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault
        .set_scoped(&entity("acme"), "DATABASE_URL", "postgres://super-secret")
        .unwrap();

    let raw = std::fs::read_to_string(vault.path()).unwrap();
    assert!(!raw.contains("super-secret"));
    assert!(!raw.contains("DATABASE_URL"));
    // Not even the entity name is in the clear.
    assert!(!raw.contains("acme"));
}

// ---- portable backup --------------------------------------------------------

#[test]
fn a_backup_restores_into_an_empty_vault_with_only_its_passphrase() {
    // The ten-year test: a different machine, a different vault, no passkey, no
    // device, no copy of the original file.
    let dir = tempfile::tempdir().unwrap();
    let (source, _) = new_vault(&dir, "source", "master");
    source.set("OPENAI_API_KEY", "sk-global").unwrap();
    source
        .set_scoped(&entity("acme"), "DATABASE_URL", "postgres://acme")
        .unwrap();
    source
        .add_item(NewItem::login("GitHub", "ahmad", "hunter2"))
        .unwrap();

    let backup = source.export("backup-passphrase").unwrap();
    let json = serde_json::to_string(&backup).unwrap();

    // Somewhere else entirely.
    let elsewhere = tempfile::tempdir().unwrap();
    let (restored, _) = new_vault(&elsewhere, "restored", "a-different-password");
    let parsed: VaultExport = serde_json::from_str(&json).unwrap();
    let (items, vars) = restored.import(&parsed, "backup-passphrase").unwrap();

    assert_eq!(items, 1);
    assert_eq!(vars, 2);
    assert_eq!(restored.get("OPENAI_API_KEY").unwrap(), "sk-global");
    assert_eq!(
        restored
            .get_scoped(&entity("acme"), "DATABASE_URL")
            .unwrap(),
        "postgres://acme"
    );

    let item = &restored.list_items(None).unwrap()[0];
    assert_eq!(item.name, "GitHub");
    assert_eq!(
        restored.item_secret(&item.id).unwrap().password.as_deref(),
        Some("hunter2")
    );
}

#[test]
fn a_backup_does_not_open_with_the_wrong_passphrase() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault.set("K", "V").unwrap();
    let backup = vault.export("correct-passphrase").unwrap();

    let (target, _) = new_vault(&dir, "target", "master");
    assert!(target.import(&backup, "wrong-passphrase").is_err());
    assert!(target.get("K").is_err(), "nothing should have been written");
}

#[test]
fn a_backup_holds_no_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault
        .set_scoped(&entity("acme"), "DATABASE_URL", "postgres://super-secret")
        .unwrap();
    vault
        .add_item(NewItem::login("GitHub", "ahmad", "hunter2"))
        .unwrap();

    let json = serde_json::to_string(&vault.export("backup-passphrase").unwrap()).unwrap();
    for leak in ["super-secret", "hunter2", "GitHub", "ahmad", "acme"] {
        assert!(!json.contains(leak), "{leak:?} leaked into the backup");
    }
    // The passphrase itself certainly must not be in there.
    assert!(!json.contains("backup-passphrase"));
}

#[test]
fn importing_never_overwrites_what_is_already_there() {
    // At 2am, restoring the wrong backup must not destroy the current one.
    let dir = tempfile::tempdir().unwrap();
    let (source, _) = new_vault(&dir, "source", "master");
    source.set("SHARED", "from-backup").unwrap();
    source.set("ONLY_IN_BACKUP", "yes").unwrap();
    let backup = source.export("passphrase").unwrap();

    let (target, _) = new_vault(&dir, "target", "master");
    target.set("SHARED", "current-value").unwrap();

    let (_, vars) = target.import(&backup, "passphrase").unwrap();

    assert_eq!(
        target.get("SHARED").unwrap(),
        "current-value",
        "must not clobber"
    );
    assert_eq!(
        target.get("ONLY_IN_BACKUP").unwrap(),
        "yes",
        "must still add"
    );
    assert_eq!(vars, 1, "only the genuinely new variable counts");
}

#[test]
fn a_weak_export_passphrase_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    assert!(vault.export("short").is_err());
    assert!(vault.export("").is_err());
}

#[test]
fn a_backup_from_a_newer_version_is_refused_rather_than_misread() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    let mut backup = vault.export("passphrase-here").unwrap();
    backup.version = VaultExport::VERSION + 1;

    let err = vault
        .import(&backup, "passphrase-here")
        .unwrap_err()
        .to_string();
    assert!(err.contains("newer version"), "{err}");
}

#[test]
fn a_file_that_is_not_a_backup_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    let mut backup = vault.export("passphrase-here").unwrap();
    backup.format = "some/other-tool".to_string();

    assert!(vault.import(&backup, "passphrase-here").is_err());
}

#[test]
fn exporting_requires_an_open_vault() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault.lock();
    assert!(vault.export("passphrase-here").is_err());
}

// ---- audit log ---------------------------------------------------------------

#[test]
fn the_audit_log_records_what_happened_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault.set("K", "V").unwrap();
    vault
        .add_item(NewItem::login("GitHub", "ahmad", "hunter2"))
        .unwrap();

    let log = vault.audit_log().unwrap();
    let actions: Vec<&str> = log.iter().map(|e| e.action.as_str()).collect();

    assert_eq!(actions[0], "item.created", "newest first");
    assert!(actions.contains(&"secret.set"));
    assert!(actions.contains(&"vault.created"));
}

#[test]
fn the_audit_log_names_things_but_never_their_values() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault
        .set_scoped(&entity("acme"), "DATABASE_URL", "postgres://super-secret")
        .unwrap();
    vault
        .add_item(NewItem::login("GitHub", "ahmad", "hunter2"))
        .unwrap();

    let log = serde_json::to_string(&vault.audit_log().unwrap()).unwrap();
    assert!(log.contains("DATABASE_URL"), "the key name is useful");
    assert!(log.contains("GitHub"), "the item name is useful");
    for secret in ["super-secret", "hunter2"] {
        assert!(!log.contains(secret), "{secret:?} must not be logged");
    }
}

#[test]
fn unlocking_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, code) = new_vault(&dir, "v", "master");
    vault.lock();
    vault.unlock_with_recovery_code(&code).unwrap();

    let log = vault.audit_log().unwrap();
    let unlock = log.iter().find(|e| e.action == "vault.unlocked").unwrap();
    assert_eq!(
        unlock.detail.as_deref(),
        Some("Recovery code"),
        "which door was used matters"
    );
}

#[test]
fn the_audit_log_needs_an_open_vault() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault.lock();
    assert!(vault.audit_log().is_err());
}

// ---- per-entity password sets -----------------------------------------------

#[test]
fn each_entity_has_its_own_password_set() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");

    // The CEO's own login, plus one for each of two entities.
    vault
        .add_item(NewItem::login("Personal email", "me", "p1"))
        .unwrap();
    vault
        .add_item(NewItem::login("Acme Stripe", "ops", "p2").in_scope(entity("acme")))
        .unwrap();
    vault
        .add_item(NewItem::login("Globex AWS", "root", "p3").in_scope(entity("globex")))
        .unwrap();

    // Listing an entity returns only that entity's items — no fallback, no
    // bleed-through from the global set.
    let acme = vault.list_items(Some(&entity("acme"))).unwrap();
    assert_eq!(acme.len(), 1);
    assert_eq!(acme[0].name, "Acme Stripe");
    assert_eq!(acme[0].scope, "entity:acme");

    let personal = vault.list_items(Some(&Scope::Global)).unwrap();
    assert_eq!(personal.len(), 1);
    assert_eq!(personal[0].name, "Personal email");

    // Passing None returns everything, each tagged with its own scope.
    let all = vault.list_items(None).unwrap();
    assert_eq!(all.len(), 3);
}

#[test]
fn the_entity_switcher_sees_which_entities_have_items() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault.add_item(NewItem::login("A", "u", "p")).unwrap();
    vault
        .add_item(NewItem::login("B", "u", "p").in_scope(entity("acme")))
        .unwrap();
    vault
        .add_item(NewItem::login("C", "u", "p").in_scope(entity("acme")))
        .unwrap();

    let scopes = vault.list_item_scopes().unwrap();
    // Global has one, acme has two.
    assert!(scopes.contains(&(Scope::Global, 1)));
    assert!(scopes.contains(&(entity("acme"), 2)));
}

#[test]
fn an_item_can_be_moved_between_entities_keeping_its_secret() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    let id = vault
        .add_item(NewItem::login("Shared tool", "u", "keep-me"))
        .unwrap();

    // Starts global, moves to an entity.
    vault.set_item_scope(&id, entity("acme")).unwrap();

    assert!(vault.list_items(Some(&Scope::Global)).unwrap().is_empty());
    let acme = vault.list_items(Some(&entity("acme"))).unwrap();
    assert_eq!(acme.len(), 1);
    // The secret and id survive the move — a re-filing, not a re-creation.
    assert_eq!(acme[0].id, id);
    assert_eq!(
        vault.item_secret(&id).unwrap().password.as_deref(),
        Some("keep-me")
    );
}

#[test]
fn an_entity_scoped_item_survives_a_lock_and_unlock() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault
        .add_item(NewItem::login("Acme DB", "root", "pw").in_scope(entity("acme")))
        .unwrap();
    vault.lock();

    let reopened = Vault::new(vault.path().to_path_buf());
    reopened.unlock("master").unwrap();
    let acme = reopened.list_items(Some(&entity("acme"))).unwrap();
    assert_eq!(acme.len(), 1);
    assert_eq!(acme[0].name, "Acme DB");
}

#[test]
fn a_backup_preserves_which_entity_each_item_belongs_to() {
    let dir = tempfile::tempdir().unwrap();
    let (source, _) = new_vault(&dir, "source", "master");
    source
        .add_item(NewItem::login("Personal", "me", "p1"))
        .unwrap();
    source
        .add_item(NewItem::login("Acme", "ops", "p2").in_scope(entity("acme")))
        .unwrap();

    let backup = source.export("backup-passphrase").unwrap();

    let elsewhere = tempfile::tempdir().unwrap();
    let (restored, _) = new_vault(&elsewhere, "restored", "other");
    restored.import(&backup, "backup-passphrase").unwrap();

    // Each item comes back to the entity it left from.
    assert_eq!(restored.list_items(Some(&entity("acme"))).unwrap().len(), 1);
    assert_eq!(restored.list_items(Some(&Scope::Global)).unwrap().len(), 1);
}

// ---- password history --------------------------------------------------------

#[test]
fn changing_a_password_records_the_previous_one() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    let id = vault
        .add_item(NewItem::login("Bank", "u", "first-pw"))
        .unwrap();

    // No history until something is superseded.
    assert!(vault.item_secret(&id).unwrap().password_history.is_empty());

    vault
        .update_item_secret(
            &id,
            &ItemSecret {
                username: Some("u".into()),
                password: Some("second-pw".into()),
                ..Default::default()
            },
        )
        .unwrap();

    let secret = vault.item_secret(&id).unwrap();
    assert_eq!(secret.password.as_deref(), Some("second-pw"));
    assert_eq!(secret.password_history.len(), 1);
    assert_eq!(secret.password_history[0].password, "first-pw");

    // A third change stacks newest-first.
    vault
        .update_item_secret(
            &id,
            &ItemSecret {
                password: Some("third-pw".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let history = vault.item_secret(&id).unwrap().password_history;
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].password, "second-pw");
    assert_eq!(history[1].password, "first-pw");
}

#[test]
fn re_saving_without_changing_the_password_does_not_grow_history() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    let id = vault
        .add_item(NewItem::login("Site", "u", "same-pw"))
        .unwrap();

    // Change only the username; password is unchanged.
    vault
        .update_item_secret(
            &id,
            &ItemSecret {
                username: Some("new-username".into()),
                password: Some("same-pw".into()),
                ..Default::default()
            },
        )
        .unwrap();

    assert!(
        vault.item_secret(&id).unwrap().password_history.is_empty(),
        "an unchanged password must not be recorded as history"
    );
}

#[test]
fn a_caller_cannot_forge_password_history() {
    // The vault owns history; a value supplied by the caller is ignored in favour
    // of what the vault itself recorded.
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    let id = vault
        .add_item(NewItem::login("Site", "u", "real-pw"))
        .unwrap();

    vault
        .update_item_secret(
            &id,
            &ItemSecret {
                password: Some("changed-pw".into()),
                password_history: vec![consistent_world_credentials::model::PasswordHistoryEntry {
                    password: "forged".into(),
                    changed_at: "2000-01-01T00:00:00Z".into(),
                }],
                ..Default::default()
            },
        )
        .unwrap();

    let history = vault.item_secret(&id).unwrap().password_history;
    assert_eq!(history.len(), 1, "only the genuine prior password is kept");
    assert_eq!(history[0].password, "real-pw");
    assert!(!history.iter().any(|h| h.password == "forged"));
}

#[test]
fn password_history_is_capped() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    let id = vault.add_item(NewItem::login("Site", "u", "pw-0")).unwrap();

    for i in 1..=30 {
        vault
            .update_item_secret(
                &id,
                &ItemSecret {
                    password: Some(format!("pw-{i}")),
                    ..Default::default()
                },
            )
            .unwrap();
    }

    let history = vault.item_secret(&id).unwrap().password_history;
    assert_eq!(history.len(), 20, "history is bounded");
    // Newest superseded password first.
    assert_eq!(history[0].password, "pw-29");
}

// ---- health report -----------------------------------------------------------

#[test]
fn a_healthy_vault_scores_100() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    // Distinct, strong passwords.
    vault
        .add_item(NewItem::login("A", "u", "Kx9#mP2$vL8@qR4!nW6z"))
        .unwrap();
    vault
        .add_item(NewItem::login("B", "u", "Zt7&yH3^bN1*cM5!dW8x"))
        .unwrap();

    let report = vault.health_report().unwrap();
    assert_eq!(report.total_with_passwords, 2);
    assert_eq!(report.score, 100);
    assert!(report.weak.is_empty());
    assert!(report.reused.is_empty());
}

#[test]
fn weak_passwords_are_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault
        .add_item(NewItem::login("Bank", "u", "password1"))
        .unwrap();
    vault
        .add_item(NewItem::login("Strong", "u", "Kx9#mP2$vL8@qR4!nW6z"))
        .unwrap();

    let report = vault.health_report().unwrap();
    assert_eq!(report.weak.len(), 1);
    assert_eq!(report.weak[0].name, "Bank");
    // The score reflects one of two items being unhealthy.
    assert_eq!(report.score, 50);
    // A finding never carries the password itself.
    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains("password1"));
}

#[test]
fn reused_passwords_are_flagged_across_items() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    // Same strong password in three places — strong, but reuse is the risk.
    let shared = "Kx9#mP2$vL8@qR4!nW6z";
    vault
        .add_item(NewItem::login("Email", "u", shared))
        .unwrap();
    vault.add_item(NewItem::login("Bank", "u", shared)).unwrap();
    vault
        .add_item(NewItem::login("Cloud", "u", shared))
        .unwrap();

    let report = vault.health_report().unwrap();
    assert_eq!(
        report.reused.len(),
        3,
        "all three sharing the password are flagged"
    );
    assert!(report.reused[0].detail.contains("Reused across 3"));
    assert!(
        report.weak.is_empty(),
        "a strong password is not weak, only reused"
    );
    assert_eq!(report.score, 0, "every item is unhealthy");
}

#[test]
fn secure_notes_without_passwords_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault
        .add_item(NewItem {
            kind: consistent_world_credentials::model::ItemKind::SecureNote,
            name: "Note".to_string(),
            scope: Scope::Global,
            folder: None,
            uris: vec![],
            favorite: false,
            secret: ItemSecret {
                notes: Some("some note".to_string()),
                ..Default::default()
            },
        })
        .unwrap();
    vault
        .add_item(NewItem::login("Login", "u", "Kx9#mP2$vL8@qR4!nW6z"))
        .unwrap();

    let report = vault.health_report().unwrap();
    // Only the login counts; the note has no password to judge.
    assert_eq!(report.total_with_passwords, 1);
    assert_eq!(report.score, 100);
}

#[test]
fn the_health_report_needs_an_open_vault() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    vault.lock();
    assert!(vault.health_report().is_err());
}

// ---- item metadata -----------------------------------------------------------

#[test]
fn item_metadata_can_be_edited_without_touching_the_secret() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    let id = vault
        .add_item(NewItem::login("Githbu", "ahmad", "hunter2"))
        .unwrap();

    vault
        .update_item_metadata(
            &id,
            Some("GitHub".to_string()),
            Some(Some("Work".to_string())),
            Some(vec!["https://github.com".to_string()]),
            Some(true),
        )
        .unwrap();

    let item = &vault.list_items(None).unwrap()[0];
    assert_eq!(item.name, "GitHub");
    assert_eq!(item.folder.as_deref(), Some("Work"));
    assert_eq!(item.uris, vec!["https://github.com"]);
    assert!(item.favorite);
    // The secret half is untouched.
    assert_eq!(
        vault.item_secret(&id).unwrap().password.as_deref(),
        Some("hunter2")
    );
}

#[test]
fn an_item_cannot_be_renamed_to_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    let id = vault.add_item(NewItem::login("GitHub", "u", "p")).unwrap();

    assert!(
        vault
            .update_item_metadata(&id, Some("   ".to_string()), None, None, None)
            .is_err()
    );
    assert_eq!(vault.list_items(None).unwrap()[0].name, "GitHub");
}

#[test]
fn a_partial_metadata_edit_leaves_the_other_fields_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, _) = new_vault(&dir, "v", "master");
    let id = vault
        .add_item(NewItem {
            kind: consistent_world_credentials::model::ItemKind::Login,
            name: "GitHub".to_string(),
            scope: Scope::Global,
            folder: Some("Work".to_string()),
            uris: vec!["https://github.com".to_string()],
            favorite: true,
            secret: ItemSecret {
                password: Some("p".to_string()),
                ..Default::default()
            },
        })
        .unwrap();

    // Only the favourite flag changes.
    vault
        .update_item_metadata(&id, None, None, None, Some(false))
        .unwrap();

    let item = &vault.list_items(None).unwrap()[0];
    assert_eq!(item.name, "GitHub");
    assert_eq!(item.folder.as_deref(), Some("Work"));
    assert_eq!(item.uris, vec!["https://github.com"]);
    assert!(!item.favorite);
}
