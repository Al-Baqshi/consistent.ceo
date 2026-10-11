//! Encrypted vault: many ways in, one key, secrets scoped per entity.
//!
//! # Key hierarchy
//!
//! ```text
//!   master password ──Argon2id──┐
//!   passkey (WebAuthn PRF) ─────┤
//!   device key (OS keychain) ───┼──> wraps ──> user_key
//!   recovery code ──────────────┘                 │
//!                    ┌──────────────────────────┬─┴────────────────┐
//!                    │                          │                  │
//!              wraps item_key          wraps X25519 private   encrypts payload
//!                    │                    (share identity)    (envs, items, audit)
//!              encrypts ItemSecret
//! ```
//!
//! Nothing in the hierarchy encrypts user data with a key derived directly from a
//! credential. Every unlock method is just another *wrapping* of the same random
//! `user_key`, which buys three things:
//!
//! 1. **Adding or removing a way in rewraps 32 bytes.** Enrolling a passkey does
//!    not re-encrypt the vault, and revoking a lost laptop's device key does not
//!    touch a single item ciphertext.
//! 2. **Changing the master password is equally cheap**, and leaves passkeys and
//!    recovery codes working.
//! 3. **No method is privileged.** A vault can be passkey-only, password-only, or
//!    both. The one invariant [`Vault::remove_unlock_method`] enforces is that the
//!    last remaining method can never be removed.
//!
//! # Why a recovery code always exists
//!
//! A passkey lives in one vendor's keychain. If that is the only way in, the vault
//! is one lost phone or one closed account away from being unreadable. Every vault
//! is therefore created with a recovery code (see [`crate::recovery`]) which is
//! shown once and never recoverable from the file.
//!
//! # Scoped secrets
//!
//! Environment variables live under a [`Scope`] — `global`, or one entity's own
//! space. An entity resolves its own value first and falls back to global exactly
//! one step, so two entities can hold different `STRIPE_SECRET_KEY` values without
//! either being able to read the other's. See [`crate::scope`].
//!
//! # Sharing
//!
//! Every item carries its own `item_key`. Sharing seals *that key* to the
//! recipient's X25519 public key ([`ShareEnvelope`]), so the item's ciphertext is
//! reused untouched and no unlock credential is ever involved. The envelope is
//! safe to persist server-side: the gateway stores and routes it without being
//! able to read it.
//!
//! # On-disk format
//!
//! Version 3 is JSON so the file stays inspectable. Older files are upgraded in
//! place on first unlock, without the caller doing anything:
//!
//! - **Version 1** (raw `salt | nonce | ciphertext`, Argon2 defaults, flat string
//!   map) is detected by its non-JSON first byte and rewritten by [`Vault::unlock`].
//! - **Version 2** shares this file layout; only the encrypted payload differs, and
//!   its flat `secrets` map is folded into `envs["global"]` by [`Payload::normalize`]
//!   on load. The version number is raised on the next write.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::crypto::{
    KdfParams, KeyPair, Sealed, SecretKey, open, open_from_private, seal, seal_to_public,
    unwrap_key, wrap_key,
};
use crate::model::{
    AuditEntry, ItemSecret, ItemSummary, NewItem, PlainItem, ShareEnvelope, VaultExport, VaultItem,
};
use crate::recovery::{self, RecoveryCode};
use crate::scope::{Scope, validate_env_key};

/// The on-disk format this build writes. See the module docs' "On-disk format"
/// section for what each version means.
pub const FORMAT_VERSION: u32 = 3;
/// Versions this build can open. Anything else is refused rather than guessed at.
const READABLE_VERSIONS: &[u32] = &[2, 3];

/// HKDF domain separators. Distinct contexts must never share a derived key.
const CTX_USER_KEY_WRAP: &[u8] = b"consistent-ceo/vault/user-key-wrap/v1";
const CTX_PAYLOAD: &[u8] = b"consistent-ceo/vault/payload/v1";
const CTX_EXPORT: &[u8] = b"consistent-ceo/vault/export/v1";

/// Audit entries kept before the oldest are dropped. Bounded because the log
/// lives inside the encrypted payload, which is rewritten on every change.
const AUDIT_MAX: usize = 500;

/// Previous passwords retained per item. Enough to undo a mistaken change and to
/// warn against re-using a recent one, without letting one item grow unbounded.
const PASSWORD_HISTORY_MAX: usize = 20;

/// Parameter key holding the WebAuthn PRF salt for a passkey method.
pub const PARAM_PRF_SALT: &str = "prfSaltB64";
/// Parameter key holding the WebAuthn credential id a passkey method belongs to.
pub const PARAM_CREDENTIAL_ID: &str = "credentialIdB64";

/// How a given holder can unwrap the `user_key`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnlockKind {
    /// A human-typed password, stretched with Argon2id.
    MasterPassword,
    /// A key derived from a WebAuthn PRF (`hmac-secret`) evaluation.
    Passkey,
    /// A high-entropy printed code. See [`crate::recovery`].
    RecoveryKey,
    /// A random key held in an OS keychain behind biometrics, one per device.
    DeviceKey,
}

impl UnlockKind {
    /// Whether this method's input is a human-typed string needing a slow KDF,
    /// as opposed to a uniform 32-byte key.
    pub fn needs_kdf(&self) -> bool {
        matches!(self, Self::MasterPassword)
    }
}

/// # On-disk spelling
///
/// Fields are snake_case, matching [`VaultFile`] and the version 2 files this
/// build still has to open. The camelCase aliases are belt-and-braces for any
/// file written by a build that got this wrong; they cost nothing to keep and
/// the alternative is a vault that will not open.
///
/// [`UnlockMethodInfo`] — which is an API shape, not a file shape — is camelCase.
/// Do not "tidy" these to match each other.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnlockMethod {
    pub id: String,
    pub kind: UnlockKind,
    pub label: String,
    /// The `user_key`, wrapped under whatever key this method produces.
    #[serde(alias = "wrappedUserKey")]
    pub wrapped_user_key: Sealed,
    /// KDF parameters for methods that stretch a low-entropy input. `None` for
    /// methods whose input is already a uniform 32-byte key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kdf: Option<KdfParams>,
    /// Public, non-secret parameters a client needs to reproduce this method's
    /// key — a passkey's PRF salt and credential id, for instance.
    ///
    /// An open map rather than per-kind fields on purpose: a future unlock method
    /// can carry whatever it needs without a format version bump, and an older
    /// build round-trips the values it does not understand instead of dropping
    /// them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
    #[serde(alias = "createdAt")]
    pub created_at: String,
    #[serde(default, alias = "lastUsedAt", skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
}

/// An unlock method as shown to a client: everything except the wrapped key.
///
/// Readable while the vault is still locked, which is the point — a client has to
/// know a passkey is enrolled, and with which PRF salt, *before* it can unlock.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnlockMethodInfo {
    pub id: String,
    pub kind: UnlockKind,
    pub label: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
}

impl From<&UnlockMethod> for UnlockMethodInfo {
    fn from(m: &UnlockMethod) -> Self {
        Self {
            id: m.id.clone(),
            kind: m.kind,
            label: m.label.clone(),
            params: m.params.clone(),
            created_at: m.created_at.clone(),
            last_used_at: m.last_used_at.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VaultFile {
    version: u32,
    unlock_methods: Vec<UnlockMethod>,
    /// X25519 public key; other users seal shares to it. Not secret.
    public_key_b64: String,
    /// The matching private key, wrapped under `user_key`.
    wrapped_private_key: Sealed,
    /// [`Payload`] as JSON, encrypted under a subkey of `user_key`.
    payload: Sealed,
    /// Anything a newer build wrote that this one does not understand.
    ///
    /// Without this, serde drops unknown fields on read and they vanish on the
    /// next write. Someone running a new version on their laptop and an older
    /// one on their server would silently destroy whatever the newer build
    /// added — the kind of loss nobody notices until they need the thing that
    /// is gone. Round-tripping them costs nothing and makes downgrades safe.
    #[serde(flatten, default)]
    extra: serde_json::Map<String, serde_json::Value>,
}

/// Everything the vault holds, encrypted as one blob.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Payload {
    /// Version 2's flat secret map. Read on load, folded into `envs["global"]` by
    /// [`Self::normalize`], and never written again.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    secrets: HashMap<String, String>,
    /// Environment variables per scope.
    #[serde(default)]
    envs: BTreeMap<Scope, BTreeMap<String, String>>,
    /// Human-facing password-manager entries.
    #[serde(default)]
    items: Vec<VaultItem>,
    #[serde(default)]
    folders: Vec<String>,
    /// Append-only activity log, oldest first.
    #[serde(default)]
    audit: Vec<AuditEntry>,
    /// Unknown fields from a newer build, preserved across writes. Same reasoning
    /// as [`VaultFile::extra`], and the same consequence if it were absent.
    #[serde(flatten, default)]
    extra: serde_json::Map<String, serde_json::Value>,
}

impl Payload {
    /// Fold a version 2 flat secret map into the scoped store.
    ///
    /// A value already present in `envs["global"]` wins: if both exist the file
    /// has been written by a newer build, and the scoped copy is the current one.
    fn normalize(&mut self) {
        if self.secrets.is_empty() {
            return;
        }
        let global = self.envs.entry(Scope::Global).or_default();
        for (key, value) in self.secrets.drain() {
            global.entry(key).or_insert(value);
        }
    }

    // Append an audit entry and trim the oldest ones past `AUDIT_MAX`.
    fn record(&mut self, action: &str, detail: Option<String>) {
        self.audit.push(AuditEntry {
            at: Utc::now().to_rfc3339(),
            action: action.to_string(),
            detail,
        });
        if self.audit.len() > AUDIT_MAX {
            let excess = self.audit.len() - AUDIT_MAX;
            self.audit.drain(0..excess);
        }
    }
}

/// In-memory state; present only while unlocked.
struct Unlocked {
    user_key: SecretKey,
    payload: Payload,
}

/// One encrypted vault backed by a single file on disk. Locked by default;
/// `state` holds the decrypted key and payload only between an `unlock*` call
/// and [`Self::lock`].
pub struct Vault {
    vault_path: PathBuf,
    state: RwLock<Option<Unlocked>>,
}

/// Pre-existing name kept so current call sites (`credentials_resolver`) compile
/// unchanged.
pub type EncryptedVault = Vault;

/// What [`Vault::init`] hands back: the vault is open, and this is the one and
/// only time the recovery code can be read.
pub struct InitOutcome {
    pub recovery_code: RecoveryCode,
}

impl Vault {
    pub fn new(vault_path: PathBuf) -> Self {
        Self {
            vault_path,
            state: RwLock::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.vault_path
    }

    pub fn exists(&self) -> bool {
        self.vault_path.exists()
    }

    pub fn is_unlocked(&self) -> bool {
        self.state.read().unwrap().is_some()
    }

    /// Drop all key material and decrypted content from memory.
    ///
    /// [`SecretKey`] zeroizes on drop, so the derived keys are wiped here. Note
    /// that plaintext secret *strings* inside the payload are ordinary `String`s
    /// and are merely deallocated, not scrubbed.
    pub fn lock(&self) {
        *self.state.write().unwrap() = None;
    }

    // ---- lifecycle ----------------------------------------------------------

    /// Create a new vault, returning the recovery code to show the user once.
    ///
    /// Fails if the file already exists.
    pub fn init(&self, master_password: &str) -> Result<InitOutcome> {
        if self.vault_path.exists() {
            bail!("Vault file already exists at {:?}", self.vault_path);
        }
        if master_password.is_empty() {
            bail!("master password must not be empty");
        }

        let kdf = KdfParams::recommended();
        let master_key = kdf.derive(master_password)?;
        let user_key = SecretKey::generate();
        let keypair = KeyPair::generate();
        let now = Utc::now().to_rfc3339();

        // Born with two ways in. A vault whose only credential is the one the
        // user just invented is a vault they can lock themselves out of today.
        let recovery_code = RecoveryCode::generate();
        let recovery_key = recovery_code.key()?;

        let mut payload = Payload::default();
        payload.record("vault.created", None);

        let file = VaultFile {
            version: FORMAT_VERSION,
            unlock_methods: vec![
                UnlockMethod {
                    id: "master-password".to_string(),
                    kind: UnlockKind::MasterPassword,
                    label: "Master password".to_string(),
                    wrapped_user_key: wrap_key(
                        &master_key.derive_subkey(CTX_USER_KEY_WRAP),
                        &user_key,
                    )?,
                    kdf: Some(kdf),
                    params: BTreeMap::new(),
                    created_at: now.clone(),
                    last_used_at: None,
                },
                UnlockMethod {
                    id: uuid::Uuid::new_v4().to_string(),
                    kind: UnlockKind::RecoveryKey,
                    label: "Recovery code".to_string(),
                    wrapped_user_key: wrap_key(
                        &recovery_key.derive_subkey(CTX_USER_KEY_WRAP),
                        &user_key,
                    )?,
                    kdf: None,
                    params: BTreeMap::new(),
                    created_at: now.clone(),
                    last_used_at: None,
                },
            ],
            public_key_b64: keypair.public_b64(),
            wrapped_private_key: seal(&user_key, keypair.secret.as_bytes())?,
            payload: seal_payload(&user_key, &payload)?,
            extra: serde_json::Map::new(),
        };

        write_atomic(&self.vault_path, &serde_json::to_vec_pretty(&file)?)?;
        *self.state.write().unwrap() = Some(Unlocked { user_key, payload });
        Ok(InitOutcome { recovery_code })
    }

    /// Unlock with the master password.
    ///
    /// A version 1 file is transparently migrated here, preserving its secrets and
    /// the caller's existing master password.
    pub fn unlock(&self, master_password: &str) -> Result<()> {
        let bytes = std::fs::read(&self.vault_path)
            .with_context(|| format!("read vault file {:?}", self.vault_path))?;

        // Distinguish a v2/v3 vault (JSON) from a v1 one (raw
        // `salt | nonce | ciphertext` binary) by *trying to parse it*, not by
        // sniffing the first byte. A v1 file begins with 16 random salt bytes,
        // so a first-byte-is-`{` heuristic misclassifies it as JSON roughly once
        // every few hundred vaults — a genuine intermittent failure. Binary v1
        // data essentially never parses as our JSON schema, so a failed parse is
        // a reliable "this is v1" signal.
        let file = match serde_json::from_slice::<VaultFile>(&bytes) {
            Ok(file) => file,
            Err(_) => return self.migrate_v1(&bytes, master_password),
        };
        if !READABLE_VERSIONS.contains(&file.version) {
            bail!(
                "unsupported vault format version {} (this build understands {:?})",
                file.version,
                READABLE_VERSIONS
            );
        }

        let method = file
            .unlock_methods
            .iter()
            .find(|m| m.kind == UnlockKind::MasterPassword)
            .ok_or_else(|| {
                anyhow!("this vault has no master password; use a passkey or recovery code")
            })?;
        let kdf = method.kdf.as_ref().ok_or_else(|| {
            anyhow!("master-password unlock method is missing its KDF parameters")
        })?;

        let master_key = kdf.derive(master_password)?;
        let method_id = method.id.clone();
        self.finish_unlock(
            &file,
            &method_id,
            &master_key.derive_subkey(CTX_USER_KEY_WRAP),
        )
    }

    /// Unlock with a uniform 32-byte key — a WebAuthn PRF output, or a device key
    /// held in an OS keychain.
    ///
    /// `method_id` says which wrapping to try. Naming the method rather than
    /// trying them all keeps this a single AES-GCM open, so a caller cannot use
    /// it to probe how many methods a vault has.
    pub fn unlock_with_key(&self, method_id: &str, key: &SecretKey) -> Result<()> {
        let file = self.read_file()?;
        let method = file
            .unlock_methods
            .iter()
            .find(|m| m.id == method_id)
            .ok_or_else(|| anyhow!("unknown unlock method"))?;
        if method.kind.needs_kdf() {
            bail!("this unlock method expects a password, not a key");
        }
        self.finish_unlock(&file, method_id, &key.derive_subkey(CTX_USER_KEY_WRAP))
    }

    /// Unlock with a printed recovery code, in any formatting.
    ///
    /// Unlike [`Self::unlock_with_key`] this does try every recovery method, since
    /// the user typing a code off a card has no idea which one they hold.
    pub fn unlock_with_recovery_code(&self, typed_code: &str) -> Result<()> {
        let key = recovery::derive_key(typed_code)?.derive_subkey(CTX_USER_KEY_WRAP);
        let file = self.read_file()?;

        let matched = file
            .unlock_methods
            .iter()
            .filter(|m| m.kind == UnlockKind::RecoveryKey)
            .find(|m| unwrap_key(&key, &m.wrapped_user_key).is_ok())
            .map(|m| m.id.clone())
            .ok_or_else(|| anyhow!("that recovery code does not open this vault"))?;

        self.finish_unlock(&file, &matched, &key)
    }

    /// Shared tail of every unlock path: unwrap, decrypt, publish state.
    fn finish_unlock(&self, file: &VaultFile, method_id: &str, wrapping: &SecretKey) -> Result<()> {
        let method = file
            .unlock_methods
            .iter()
            .find(|m| m.id == method_id)
            .ok_or_else(|| anyhow!("unknown unlock method"))?;

        let user_key = unwrap_key(wrapping, &method.wrapped_user_key)?;
        let mut payload = open_payload(&user_key, &file.payload)?;
        payload.normalize();

        *self.state.write().unwrap() = Some(Unlocked { user_key, payload });

        // Best-effort: a vault on read-only media still opens, it just cannot
        // record that it did. Failing the unlock over a bookkeeping write would
        // turn a recoverable annoyance into a lockout.
        let label = method.label.clone();
        if let Err(e) = self.note_unlock(method_id, &label) {
            tracing::debug!(error = %e, "could not record vault unlock");
        }
        Ok(())
    }

    /// Stamp `last_used_at` and append an audit entry for a successful unlock.
    fn note_unlock(&self, method_id: &str, label: &str) -> Result<()> {
        self.mutate(|payload| {
            payload.record("vault.unlocked", Some(label.to_string()));
            Ok(())
        })?;
        let mut file = self.read_file()?;
        if let Some(m) = file.unlock_methods.iter_mut().find(|m| m.id == method_id) {
            m.last_used_at = Some(Utc::now().to_rfc3339());
        }
        self.write_file(&file)
    }

    /// Re-derive the master key under a fresh salt and rewrap the `user_key`.
    ///
    /// Item ciphertexts, passkeys and recovery codes are all untouched.
    pub fn change_master_password(&self, current: &str, new: &str) -> Result<()> {
        if new.is_empty() {
            bail!("master password must not be empty");
        }
        // Verify the current password by unlocking rather than trusting session state.
        self.unlock(current)
            .context("current master password is incorrect")?;

        let mut file = self.read_file()?;
        let user_key = self.user_key()?;

        let existing_kdf = file
            .unlock_methods
            .iter()
            .find(|m| m.kind == UnlockKind::MasterPassword)
            .and_then(|m| m.kdf.clone())
            .unwrap_or_else(KdfParams::recommended);
        let kdf = existing_kdf.with_fresh_salt();
        let master_key = kdf.derive(new)?;
        let wrapped = wrap_key(&master_key.derive_subkey(CTX_USER_KEY_WRAP), &user_key)?;

        match file
            .unlock_methods
            .iter_mut()
            .find(|m| m.kind == UnlockKind::MasterPassword)
        {
            Some(method) => {
                method.wrapped_user_key = wrapped;
                method.kdf = Some(kdf);
            }
            None => bail!("vault has no master-password unlock method"),
        }

        self.write_file(&file)?;
        self.mutate(|payload| {
            payload.record("vault.password-changed", None);
            Ok(())
        })
    }

    // ---- unlock methods ------------------------------------------------------

    /// Every way into this vault, readable while still locked.
    ///
    /// Returns metadata only — never a wrapped key — so this is safe to serve to
    /// an unauthenticated-but-addressed client deciding which button to show.
    pub fn unlock_methods(&self) -> Result<Vec<UnlockMethodInfo>> {
        Ok(self
            .read_file()?
            .unlock_methods
            .iter()
            .map(UnlockMethodInfo::from)
            .collect())
    }

    /// Enrol another way in, wrapping the same `user_key` under `key`.
    ///
    /// Requires the vault to be unlocked: you must already be able to open it to
    /// add a new door. Returns the new method's id, which the client stores and
    /// passes back to [`Self::unlock_with_key`].
    pub fn add_key_unlock_method(
        &self,
        kind: UnlockKind,
        label: &str,
        key: &SecretKey,
        params: BTreeMap<String, String>,
    ) -> Result<String> {
        if kind.needs_kdf() {
            bail!("{kind:?} is a password method; it cannot be added from a raw key");
        }
        if label.trim().is_empty() {
            bail!("unlock method label must not be empty");
        }

        let user_key = self.user_key()?;
        let id = uuid::Uuid::new_v4().to_string();
        let mut file = self.read_file()?;

        file.unlock_methods.push(UnlockMethod {
            id: id.clone(),
            kind,
            label: label.to_string(),
            wrapped_user_key: wrap_key(&key.derive_subkey(CTX_USER_KEY_WRAP), &user_key)?,
            kdf: None,
            params,
            created_at: Utc::now().to_rfc3339(),
            last_used_at: None,
        });
        self.write_file(&file)?;

        let label = label.to_string();
        self.mutate(move |payload| {
            payload.record("unlock-method.added", Some(label));
            Ok(())
        })?;
        Ok(id)
    }

    /// Mint a fresh recovery code and enrol it. The code is returned once.
    pub fn add_recovery_code(&self) -> Result<RecoveryCode> {
        let code = RecoveryCode::generate();
        self.add_key_unlock_method(
            UnlockKind::RecoveryKey,
            "Recovery code",
            &code.key()?,
            BTreeMap::new(),
        )?;
        Ok(code)
    }

    /// Revoke a way in — a lost phone's device key, a rotated passkey.
    ///
    /// Refuses to remove the last remaining method. That guard is the difference
    /// between "I revoked my old laptop" and "I have permanently destroyed every
    /// credential in here", and no confirmation dialog is a reliable substitute
    /// for making it impossible.
    pub fn remove_unlock_method(&self, method_id: &str) -> Result<()> {
        // Unlocked-only: revoking access is exactly the operation an attacker
        // holding a stolen session would reach for.
        if !self.is_unlocked() {
            bail!("vault is locked");
        }

        let mut file = self.read_file()?;
        if file.unlock_methods.len() <= 1 {
            bail!("this is the only way into the vault; add another before removing it");
        }
        let Some(index) = file.unlock_methods.iter().position(|m| m.id == method_id) else {
            bail!("unlock method not found: {method_id}");
        };

        let removed = file.unlock_methods.remove(index);
        self.write_file(&file)?;
        self.mutate(move |payload| {
            payload.record("unlock-method.removed", Some(removed.label.clone()));
            Ok(())
        })
    }

    // ---- environment variables ----------------------------------------------

    /// Set a variable in one scope.
    pub fn set_scoped(&self, scope: &Scope, key: &str, value: &str) -> Result<()> {
        validate_env_key(key)?;
        let scope = scope.clone();
        let (key, value) = (key.to_string(), value.to_string());
        self.mutate(move |payload| {
            payload
                .envs
                .entry(scope.clone())
                .or_default()
                .insert(key.clone(), value);
            payload.record("secret.set", Some(format!("{scope}/{key}")));
            Ok(())
        })
    }

    /// Read a variable from exactly one scope, with no fallback.
    pub fn get_scoped(&self, scope: &Scope, key: &str) -> Result<String> {
        let guard = self.state.read().unwrap();
        let state = guard.as_ref().ok_or_else(locked_err)?;
        state
            .payload
            .envs
            .get(scope)
            .and_then(|m| m.get(key))
            .cloned()
            .ok_or_else(|| anyhow!("Key not found: {key}"))
    }

    /// Read a variable the way a running entity sees it: its own scope first,
    /// then global.
    pub fn resolve(&self, scope: &Scope, key: &str) -> Result<String> {
        let guard = self.state.read().unwrap();
        let state = guard.as_ref().ok_or_else(locked_err)?;
        scope
            .resolution_order()
            .iter()
            .find_map(|s| state.payload.envs.get(s).and_then(|m| m.get(key)).cloned())
            .ok_or_else(|| anyhow!("Key not found: {key}"))
    }

    /// The full environment an entity runs with: global defaults, overlaid with
    /// the entity's own values.
    ///
    /// This is what the agent runtime injects, and it is why shadowing has to be
    /// last-write-wins in scope order rather than a merge.
    pub fn env_for(&self, scope: &Scope) -> Result<BTreeMap<String, String>> {
        let guard = self.state.read().unwrap();
        let state = guard.as_ref().ok_or_else(locked_err)?;

        let mut out = BTreeMap::new();
        // Reverse resolution order: least specific first, so the most specific
        // scope overwrites it.
        for s in scope.resolution_order().into_iter().rev() {
            if let Some(values) = state.payload.envs.get(&s) {
                out.extend(values.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
        }
        Ok(out)
    }

    pub fn delete_scoped(&self, scope: &Scope, key: &str) -> Result<()> {
        let scope = scope.clone();
        let key = key.to_string();
        self.mutate(move |payload| {
            if let Some(values) = payload.envs.get_mut(&scope) {
                values.remove(&key);
                // Do not leave an empty scope behind; it would show up as a
                // populated entity in any UI that lists scopes.
                if values.is_empty() {
                    payload.envs.remove(&scope);
                }
            }
            payload.record("secret.deleted", Some(format!("{scope}/{key}")));
            Ok(())
        })
    }

    /// Key names in one scope, without fallback. Values stay in the vault.
    pub fn list_scoped_keys(&self, scope: &Scope) -> Result<Vec<String>> {
        let guard = self.state.read().unwrap();
        let state = guard.as_ref().ok_or_else(locked_err)?;
        Ok(state
            .payload
            .envs
            .get(scope)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default())
    }

    /// Every scope that currently holds at least one variable.
    pub fn list_scopes(&self) -> Result<Vec<Scope>> {
        let guard = self.state.read().unwrap();
        let state = guard.as_ref().ok_or_else(locked_err)?;
        Ok(state.payload.envs.keys().cloned().collect())
    }

    // ---- flat secrets (global scope; pre-existing API) -----------------------

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.set_scoped(&Scope::Global, key, value)
    }

    pub fn get(&self, key: &str) -> Result<String> {
        self.get_scoped(&Scope::Global, key)
    }

    /// Idempotent: removing an absent key is not an error.
    pub fn delete(&self, key: &str) -> Result<()> {
        self.delete_scoped(&Scope::Global, key)
    }

    pub fn list_keys(&self) -> Result<Vec<String>> {
        self.list_scoped_keys(&Scope::Global)
    }

    // ---- items (password manager) ------------------------------------------

    /// Store a new item under its own freshly generated key. Returns the item id.
    pub fn add_item(&self, new_item: NewItem) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let item_key = SecretKey::generate();
        let secret_json = serde_json::to_vec(&new_item.secret).context("serialize item secret")?;

        let user_key = self.user_key()?;
        let name = new_item.name.clone();
        let item = VaultItem {
            id: id.clone(),
            kind: new_item.kind,
            name: new_item.name,
            scope: new_item.scope,
            folder: new_item.folder,
            uris: new_item.uris,
            favorite: new_item.favorite,
            shared_from: None,
            wrapped_item_key: wrap_key(&user_key, &item_key)?,
            secret: seal(&item_key, &secret_json)?,
            created_at: now.clone(),
            updated_at: now,
        };

        self.mutate(move |payload| {
            payload.items.push(item);
            payload.record("item.created", Some(name));
            Ok(())
        })?;
        Ok(id)
    }

    /// Decrypt one item's secret half.
    pub fn item_secret(&self, item_id: &str) -> Result<ItemSecret> {
        let guard = self.state.read().unwrap();
        let state = guard.as_ref().ok_or_else(locked_err)?;
        let item = find_item(&state.payload, item_id)?;
        let item_key = unwrap_key(&state.user_key, &item.wrapped_item_key)?;
        let plaintext = open(&item_key, &item.secret)?;
        serde_json::from_slice(&plaintext).context("parse item secret")
    }

    /// Metadata for items, without decrypting any secret.
    ///
    /// `scope` filters to one entity's set; `None` returns every item across all
    /// entities (each summary carries its own scope, so a UI can still group). No
    /// fallback — unlike environment variables, an entity's passwords are its own
    /// and do not inherit the global set.
    pub fn list_items(&self, scope: Option<&Scope>) -> Result<Vec<ItemSummary>> {
        let guard = self.state.read().unwrap();
        let state = guard.as_ref().ok_or_else(locked_err)?;
        Ok(state
            .payload
            .items
            .iter()
            .filter(|item| scope.is_none_or(|s| &item.scope == s))
            .map(ItemSummary::from)
            .collect())
    }

    /// Every entity that currently holds at least one item, plus how many, so a
    /// UI can render an entity switcher for the password sets.
    pub fn list_item_scopes(&self) -> Result<Vec<(Scope, usize)>> {
        let guard = self.state.read().unwrap();
        let state = guard.as_ref().ok_or_else(locked_err)?;
        let mut counts: std::collections::BTreeMap<Scope, usize> =
            std::collections::BTreeMap::new();
        for item in &state.payload.items {
            *counts.entry(item.scope.clone()).or_insert(0) += 1;
        }
        Ok(counts.into_iter().collect())
    }

    /// Move an item to a different entity's set. The item keeps its id, key and
    /// secret — only its filing changes — so shares and TOTP survive the move.
    pub fn set_item_scope(&self, item_id: &str, scope: Scope) -> Result<()> {
        let item_id = item_id.to_string();
        self.mutate(move |payload| {
            let item = payload
                .items
                .iter_mut()
                .find(|i| i.id == item_id)
                .ok_or_else(|| anyhow!("item not found: {item_id}"))?;
            item.scope = scope.clone();
            item.updated_at = Utc::now().to_rfc3339();
            let name = item.name.clone();
            payload.record("item.moved", Some(format!("{name} → {scope}")));
            Ok(())
        })
    }

    /// Replace an item's secret, keeping its id and its existing item key. The
    /// key is retained deliberately: rotating it would invalidate shares that
    /// recipients have already accepted.
    pub fn update_item_secret(&self, item_id: &str, secret: &ItemSecret) -> Result<()> {
        let user_key = self.user_key()?;
        let mut secret = secret.clone();
        let item_id = item_id.to_string();
        self.mutate(move |payload| {
            let item = payload
                .items
                .iter_mut()
                .find(|i| i.id == item_id)
                .ok_or_else(|| anyhow!("item not found: {item_id}"))?;
            let item_key = unwrap_key(&user_key, &item.wrapped_item_key)?;

            // Read the stored secret so the vault — not the caller — owns the
            // password history. Trusting the caller's `password_history` would
            // let it be forged or dropped; recomputing it here keeps it honest.
            let previous: ItemSecret = serde_json::from_slice(&open(&item_key, &item.secret)?)
                .context("parse existing item secret")?;
            let mut history = previous.password_history.clone();

            // Record the old password only when it actually changed to a new,
            // non-empty one — re-saving an item without touching its password
            // should not litter the history with duplicates.
            if let Some(old) = previous.password.as_deref()
                && !old.is_empty()
                && secret.password.as_deref() != Some(old)
            {
                history.insert(
                    0,
                    crate::model::PasswordHistoryEntry {
                        password: old.to_string(),
                        changed_at: Utc::now().to_rfc3339(),
                    },
                );
                history.truncate(PASSWORD_HISTORY_MAX);
            }
            secret.password_history = history;

            let secret_json = serde_json::to_vec(&secret).context("serialize item secret")?;
            item.secret = seal(&item_key, &secret_json)?;
            item.updated_at = Utc::now().to_rfc3339();
            let name = item.name.clone();
            payload.record("item.updated", Some(name));
            Ok(())
        })
    }

    /// Update the non-secret half of an item — its name, folder, URIs, favourite
    /// flag. Leaves the ciphertext and the item key untouched.
    pub fn update_item_metadata(
        &self,
        item_id: &str,
        name: Option<String>,
        folder: Option<Option<String>>,
        uris: Option<Vec<String>>,
        favorite: Option<bool>,
    ) -> Result<()> {
        let item_id = item_id.to_string();
        self.mutate(move |payload| {
            let item = payload
                .items
                .iter_mut()
                .find(|i| i.id == item_id)
                .ok_or_else(|| anyhow!("item not found: {item_id}"))?;
            if let Some(name) = name {
                if name.trim().is_empty() {
                    bail!("item name must not be empty");
                }
                item.name = name;
            }
            if let Some(folder) = folder {
                item.folder = folder;
            }
            if let Some(uris) = uris {
                item.uris = uris;
            }
            if let Some(favorite) = favorite {
                item.favorite = favorite;
            }
            item.updated_at = Utc::now().to_rfc3339();
            let label = item.name.clone();
            payload.record("item.updated", Some(label));
            Ok(())
        })
    }

    pub fn delete_item(&self, item_id: &str) -> Result<()> {
        let item_id = item_id.to_string();
        self.mutate(move |payload| {
            let Some(index) = payload.items.iter().position(|i| i.id == item_id) else {
                bail!("item not found: {item_id}");
            };
            let removed = payload.items.remove(index);
            payload.record("item.deleted", Some(removed.name));
            Ok(())
        })
    }

    // ---- health report -------------------------------------------------------

    /// Analyse every password in the vault for weakness, reuse and staleness.
    ///
    /// This decrypts each item to judge its password, so it is the one read that
    /// touches every secret — but it returns only ids, names and reasons, never a
    /// value. Empty passwords and items without one (secure notes) are ignored;
    /// they are not password hygiene problems.
    pub fn health_report(&self) -> Result<crate::model::HealthReport> {
        use crate::model::{HealthReport, ItemFlag};

        let guard = self.state.read().unwrap();
        let state = guard.as_ref().ok_or_else(locked_err)?;

        // Decrypt once; hold the plaintext only for the duration of this scan.
        struct Scanned {
            id: String,
            name: String,
            scope: String,
            password: String,
            updated_at: String,
        }
        let mut scanned = Vec::new();
        for item in &state.payload.items {
            let item_key = unwrap_key(&state.user_key, &item.wrapped_item_key)?;
            let secret: ItemSecret = serde_json::from_slice(&open(&item_key, &item.secret)?)
                .context("parse item secret for health report")?;
            let Some(password) = secret.password.filter(|p| !p.is_empty()) else {
                continue;
            };
            scanned.push(Scanned {
                id: item.id.clone(),
                name: item.name.clone(),
                scope: item.scope.to_string(),
                password,
                updated_at: item.updated_at.clone(),
            });
        }

        let total_with_passwords = scanned.len();

        // Reuse: count how many items share each exact password.
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for s in &scanned {
            *counts.entry(s.password.as_str()).or_insert(0) += 1;
        }

        let now = Utc::now();
        let stale_cutoff = now - chrono::Duration::days(365);

        let mut weak = Vec::new();
        let mut reused = Vec::new();
        let mut stale = Vec::new();
        // An item is "unhealthy" if it trips any check; that drives the score.
        let mut unhealthy = 0usize;

        for s in &scanned {
            let mut flagged = false;
            let flag = |detail: String| ItemFlag {
                id: s.id.clone(),
                name: s.name.clone(),
                scope: s.scope.clone(),
                detail,
            };

            let strength = crate::strength::estimate(&s.password, &[]);
            if strength.score <= 2 {
                weak.push(flag(format!("{} password", strength.label)));
                flagged = true;
            }

            let shared = counts.get(s.password.as_str()).copied().unwrap_or(1);
            if shared > 1 {
                reused.push(flag(format!("Reused across {shared} items")));
                flagged = true;
            }

            // Staleness is only worth flagging on a password that is otherwise
            // fine — nagging about the age of a password you are already telling
            // the user to change would just be noise.
            if !flagged
                && let Ok(updated) = chrono::DateTime::parse_from_rfc3339(&s.updated_at)
                && updated.with_timezone(&Utc) < stale_cutoff
            {
                stale.push(flag("Not changed in over a year".to_string()));
                flagged = true;
            }

            if flagged {
                unhealthy += 1;
            }
        }

        let score = if total_with_passwords == 0 {
            100
        } else {
            let healthy = total_with_passwords - unhealthy;
            ((healthy as f64 / total_with_passwords as f64) * 100.0).round() as u8
        };

        Ok(HealthReport {
            total_with_passwords,
            weak,
            reused,
            stale,
            score,
        })
    }

    // ---- breach check --------------------------------------------------------

    /// Reduce every password-bearing item to the SHA-1 prefix/suffix a Have I
    /// Been Pwned check needs.
    ///
    /// The password is hashed *inside the vault* and never returned; only the
    /// hash split leaves, and only the 5-char prefix of it is ever sent onward to
    /// HIBP by the gateway. See [`crate::breach`] for the k-anonymity rationale.
    pub fn breach_scan_targets(&self) -> Result<Vec<crate::model::BreachTarget>> {
        let guard = self.state.read().unwrap();
        let state = guard.as_ref().ok_or_else(locked_err)?;

        let mut out = Vec::new();
        for item in &state.payload.items {
            let item_key = unwrap_key(&state.user_key, &item.wrapped_item_key)?;
            let secret: ItemSecret = serde_json::from_slice(&open(&item_key, &item.secret)?)
                .context("parse item secret for breach scan")?;
            if let Some(password) = secret.password.filter(|p| !p.is_empty()) {
                let (prefix, suffix) = crate::breach::prefix_suffix(&password);
                out.push(crate::model::BreachTarget {
                    id: item.id.clone(),
                    name: item.name.clone(),
                    scope: item.scope.to_string(),
                    prefix,
                    suffix,
                });
            }
        }
        Ok(out)
    }

    // ---- audit ---------------------------------------------------------------

    /// Recent activity, newest first. Bounded to [`AUDIT_MAX`] entries.
    pub fn audit_log(&self) -> Result<Vec<AuditEntry>> {
        let guard = self.state.read().unwrap();
        let state = guard.as_ref().ok_or_else(locked_err)?;
        Ok(state.payload.audit.iter().rev().cloned().collect())
    }

    // ---- portable backup ------------------------------------------------------

    /// Encrypt everything under a passphrase of the caller's choosing.
    ///
    /// The export is deliberately *not* tied to this vault's key hierarchy: it
    /// carries decrypted items re-encrypted under a fresh Argon2id key. Restoring
    /// it needs nothing but the passphrase — no passkey, no device, no copy of the
    /// original file. That independence is the whole point, and it is also why the
    /// passphrase is the only thing standing between a stolen backup and every
    /// secret in it.
    pub fn export(&self, passphrase: &str) -> Result<VaultExport> {
        if passphrase.len() < 8 {
            bail!("export passphrase must be at least 8 characters");
        }

        let plain = {
            let guard = self.state.read().unwrap();
            let state = guard.as_ref().ok_or_else(locked_err)?;

            let mut items = Vec::with_capacity(state.payload.items.len());
            for item in &state.payload.items {
                let item_key = unwrap_key(&state.user_key, &item.wrapped_item_key)?;
                let secret: ItemSecret = serde_json::from_slice(&open(&item_key, &item.secret)?)
                    .context("parse item secret for export")?;
                items.push(PlainItem {
                    kind: item.kind,
                    name: item.name.clone(),
                    scope: item.scope.clone(),
                    folder: item.folder.clone(),
                    uris: item.uris.clone(),
                    favorite: item.favorite,
                    secret,
                    created_at: item.created_at.clone(),
                    updated_at: item.updated_at.clone(),
                });
            }

            PlainExport {
                envs: state.payload.envs.clone(),
                items,
                folders: state.payload.folders.clone(),
            }
        };

        let kdf = KdfParams::recommended();
        let key = kdf.derive(passphrase)?.derive_subkey(CTX_EXPORT);
        let sealed = seal(
            &key,
            &serde_json::to_vec(&plain).context("serialize export")?,
        )?;

        self.mutate(|payload| {
            payload.record("vault.exported", None);
            Ok(())
        })?;

        Ok(VaultExport {
            format: VaultExport::FORMAT.to_string(),
            version: VaultExport::VERSION,
            created_at: Utc::now().to_rfc3339(),
            item_count: plain.items.len(),
            kdf,
            payload: sealed,
        })
    }

    /// Merge an export into this vault. Returns `(items added, variables added)`.
    ///
    /// Additive by design: items are appended with fresh ids and fresh item keys,
    /// and a variable that already exists in a scope is left alone. An import can
    /// therefore never destroy something the current vault holds, which is the
    /// behaviour you want at 2am when you are not certain which copy is newer.
    pub fn import(&self, export: &VaultExport, passphrase: &str) -> Result<(usize, usize)> {
        if export.format != VaultExport::FORMAT {
            bail!("not a Consistent vault export");
        }
        if export.version > VaultExport::VERSION {
            bail!(
                "this backup was written by a newer version ({}); upgrade before restoring",
                export.version
            );
        }

        let key = export.kdf.derive(passphrase)?.derive_subkey(CTX_EXPORT);
        let plain: PlainExport = serde_json::from_slice(
            &open(&key, &export.payload)
                .context("could not decrypt the backup with that passphrase")?,
        )
        .context("parse backup contents")?;

        let mut added_items = 0usize;
        for item in &plain.items {
            self.add_item(NewItem {
                kind: item.kind,
                name: item.name.clone(),
                scope: item.scope.clone(),
                folder: item.folder.clone(),
                uris: item.uris.clone(),
                favorite: item.favorite,
                secret: item.secret.clone(),
            })?;
            added_items += 1;
        }

        let mut added_vars = 0usize;
        self.mutate(|payload| {
            for (scope, values) in &plain.envs {
                let target = payload.envs.entry(scope.clone()).or_default();
                for (k, v) in values {
                    if !target.contains_key(k) {
                        target.insert(k.clone(), v.clone());
                        added_vars += 1;
                    }
                }
            }
            for folder in &plain.folders {
                if !payload.folders.contains(folder) {
                    payload.folders.push(folder.clone());
                }
            }
            payload.record(
                "vault.imported",
                Some(format!("{added_items} items, {added_vars} variables")),
            );
            Ok(())
        })?;

        Ok((added_items, added_vars))
    }

    // ---- sharing ------------------------------------------------------------

    /// This vault's X25519 public key. Publish it so others can share to you.
    pub fn public_key_b64(&self) -> Result<String> {
        Ok(self.read_file()?.public_key_b64)
    }

    /// Seal one item to a recipient's public key.
    ///
    /// The returned envelope is safe to hand to the gateway for delivery: it is
    /// readable only by the holder of the matching private key.
    pub fn create_share(
        &self,
        item_id: &str,
        recipient_public_key_b64: &str,
        from_user: &str,
        scenario: Option<String>,
        expires_at: Option<String>,
    ) -> Result<ShareEnvelope> {
        let envelope = {
            let guard = self.state.read().unwrap();
            let state = guard.as_ref().ok_or_else(locked_err)?;
            let item = find_item(&state.payload, item_id)?;
            let item_key = unwrap_key(&state.user_key, &item.wrapped_item_key)?;

            ShareEnvelope {
                share_id: uuid::Uuid::new_v4().to_string(),
                scenario,
                from_user: from_user.to_string(),
                to_public_key_b64: recipient_public_key_b64.to_string(),
                kind: item.kind,
                name: item.name.clone(),
                uris: item.uris.clone(),
                sealed_item_key: seal_to_public(recipient_public_key_b64, item_key.expose())?,
                // Reused verbatim — the recipient gets the key, so the existing
                // ciphertext opens for them as-is.
                secret: item.secret.clone(),
                created_at: Utc::now().to_rfc3339(),
                expires_at,
            }
        };

        let name = envelope.name.clone();
        self.mutate(move |payload| {
            payload.record("share.created", Some(name));
            Ok(())
        })?;
        Ok(envelope)
    }

    /// Accept an envelope addressed to this vault, storing it as a local item.
    /// Returns the new item id.
    pub fn accept_share(&self, envelope: &ShareEnvelope) -> Result<String> {
        if let Some(expiry) = &envelope.expires_at {
            let deadline = chrono::DateTime::parse_from_rfc3339(expiry)
                .context("parse share expiry")?
                .with_timezone(&Utc);
            if Utc::now() > deadline {
                bail!("this share expired at {expiry}");
            }
        }

        let file = self.read_file()?;
        if file.public_key_b64 != envelope.to_public_key_b64 {
            bail!("this share is addressed to a different vault");
        }

        let user_key = self.user_key()?;
        let keypair = self.keypair(&user_key, &file)?;
        let item_key_bytes = open_from_private(&keypair, &envelope.sealed_item_key)
            .context("unseal shared item key")?;
        let item_key = SecretKey::from_bytes(
            item_key_bytes
                .as_slice()
                .try_into()
                .map_err(|_| anyhow!("shared item key has wrong length"))?,
        );

        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let name = envelope.name.clone();
        let from = envelope.from_user.clone();
        let item = VaultItem {
            id: id.clone(),
            kind: envelope.kind,
            name: envelope.name.clone(),
            // A share lands in the recipient's own set; they can refile it after.
            scope: Scope::Global,
            folder: None,
            uris: envelope.uris.clone(),
            favorite: false,
            shared_from: Some(envelope.from_user.clone()),
            // Rewrapped under *our* user key; the sender's wrapping is discarded.
            wrapped_item_key: wrap_key(&user_key, &item_key)?,
            secret: envelope.secret.clone(),
            created_at: now.clone(),
            updated_at: now,
        };

        self.mutate(move |payload| {
            payload.items.push(item);
            payload.record("share.accepted", Some(format!("{name} from {from}")));
            Ok(())
        })?;
        Ok(id)
    }

    // ---- internals ----------------------------------------------------------

    fn user_key(&self) -> Result<SecretKey> {
        let guard = self.state.read().unwrap();
        Ok(guard.as_ref().ok_or_else(locked_err)?.user_key.clone())
    }

    fn keypair(&self, user_key: &SecretKey, file: &VaultFile) -> Result<KeyPair> {
        let bytes =
            open(user_key, &file.wrapped_private_key).context("unwrap vault private key")?;
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("vault private key has wrong length"))?;
        Ok(KeyPair::from_secret_bytes(arr))
    }

    fn parse_file(&self, bytes: &[u8]) -> Result<VaultFile> {
        let file: VaultFile = serde_json::from_slice(bytes).context("parse vault file")?;
        if !READABLE_VERSIONS.contains(&file.version) {
            bail!(
                "unsupported vault format version {} (this build understands {:?})",
                file.version,
                READABLE_VERSIONS
            );
        }
        Ok(file)
    }

    fn read_file(&self) -> Result<VaultFile> {
        let bytes = std::fs::read(&self.vault_path)
            .with_context(|| format!("read vault file {:?}", self.vault_path))?;
        self.parse_file(&bytes)
    }

    /// Persist a file struct, stamping it at the current format version.
    ///
    /// This is where a version 2 file quietly becomes a version 3 one: it is only
    /// ever called after a successful unlock, so the upgrade cannot strand a file
    /// the caller could not already read.
    fn write_file(&self, file: &VaultFile) -> Result<()> {
        self.back_up_before_upgrade(file.version);
        let mut file = file.clone();
        file.version = FORMAT_VERSION;
        write_atomic(&self.vault_path, &serde_json::to_vec_pretty(&file)?)
    }

    /// Keep a copy of the old file the first time a vault is written at a new
    /// format version.
    ///
    /// The upgrade itself is safe — it only happens after a successful unlock,
    /// and `write_atomic` cannot leave a torn file. What this guards is the case
    /// that safety argument does not cover: a *bug* in the new version's writer.
    /// By the time anyone notices, the only copy has been rewritten several times
    /// and the original is gone.
    ///
    /// Best effort throughout. A vault that cannot be backed up should still be
    /// usable, so every failure here is logged and swallowed. Written once — if
    /// the backup already exists it is left alone, so later writes cannot
    /// overwrite the pristine copy with an already-upgraded one.
    fn back_up_before_upgrade(&self, current_version: u32) {
        if current_version >= FORMAT_VERSION {
            return;
        }
        let backup = self
            .vault_path
            .with_extension(format!("v{current_version}.bak"));
        if backup.exists() {
            return;
        }
        match std::fs::copy(&self.vault_path, &backup) {
            Ok(_) => tracing::info!(
                from = current_version,
                to = FORMAT_VERSION,
                backup = ?backup,
                "upgraded vault format; kept a copy of the previous file"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                backup = ?backup,
                "could not back up the vault before upgrading its format"
            ),
        }
    }

    /// Apply a change to the decrypted payload and persist it.
    ///
    /// The in-memory payload is only updated after the write succeeds, so a
    /// failed write leaves memory and disk consistent with each other.
    fn mutate<F>(&self, f: F) -> Result<()>
    where
        F: FnOnce(&mut Payload) -> Result<()>,
    {
        let mut guard = self.state.write().unwrap();
        let state = guard.as_mut().ok_or_else(locked_err)?;

        let mut next = state.payload.clone();
        f(&mut next)?;

        let mut file = {
            let bytes = std::fs::read(&self.vault_path)
                .with_context(|| format!("read vault file {:?}", self.vault_path))?;
            serde_json::from_slice::<VaultFile>(&bytes).context("parse vault file")?
        };
        self.back_up_before_upgrade(file.version);
        file.version = FORMAT_VERSION;
        file.payload = seal_payload(&state.user_key, &next)?;
        write_atomic(&self.vault_path, &serde_json::to_vec_pretty(&file)?)?;

        state.payload = next;
        Ok(())
    }

    /// Read a version 1 file (`salt | nonce | ciphertext`, Argon2 defaults, flat
    /// string map) and rewrite it in the current format under the same password.
    fn migrate_v1(&self, bytes: &[u8], master_password: &str) -> Result<()> {
        const V1_SALT_LEN: usize = 16;
        const V1_NONCE_LEN: usize = 12;
        const V1_TAG_LEN: usize = 16;

        if bytes.len() < V1_SALT_LEN + V1_NONCE_LEN + V1_TAG_LEN {
            bail!("Vault file too short or corrupted");
        }
        let (salt, rest) = bytes.split_at(V1_SALT_LEN);
        let (nonce, ct_and_tag) = rest.split_at(V1_NONCE_LEN);

        let mut legacy_key = [0u8; 32];
        argon2::Argon2::default()
            .hash_password_into(master_password.as_bytes(), salt, &mut legacy_key)
            .map_err(|e| anyhow!("argon2: {e}"))?;

        let secrets: HashMap<String, String> = {
            use aes_gcm::aead::Aead;
            use aes_gcm::{Aes256Gcm, KeyInit};
            let cipher =
                Aes256Gcm::new_from_slice(&legacy_key).map_err(|_| anyhow!("aes key length"))?;
            let plaintext = cipher
                .decrypt(aes_gcm::Nonce::from_slice(nonce), ct_and_tag)
                .map_err(|_| {
                    anyhow!("decryption failed (wrong password, or the data was modified)")
                })?;
            serde_json::from_slice(&plaintext).context("parse legacy vault contents")?
        };

        // Keep the old file until the new one is safely written.
        let backup = self.vault_path.with_extension("v1.bak");
        std::fs::copy(&self.vault_path, &backup)
            .with_context(|| format!("back up legacy vault to {backup:?}"))?;

        let kdf = KdfParams::recommended();
        let master_key = kdf.derive(master_password)?;
        let user_key = SecretKey::generate();
        let keypair = KeyPair::generate();
        let now = Utc::now().to_rfc3339();

        let mut envs = BTreeMap::new();
        envs.insert(Scope::Global, secrets.into_iter().collect());
        let mut payload = Payload {
            envs,
            ..Default::default()
        };
        payload.record("vault.migrated", Some("v1 -> v3".to_string()));

        let file = VaultFile {
            version: FORMAT_VERSION,
            unlock_methods: vec![UnlockMethod {
                id: "master-password".to_string(),
                kind: UnlockKind::MasterPassword,
                label: "Master password".to_string(),
                wrapped_user_key: wrap_key(
                    &master_key.derive_subkey(CTX_USER_KEY_WRAP),
                    &user_key,
                )?,
                kdf: Some(kdf),
                params: BTreeMap::new(),
                created_at: now.clone(),
                last_used_at: None,
            }],
            public_key_b64: keypair.public_b64(),
            wrapped_private_key: seal(&user_key, keypair.secret.as_bytes())?,
            payload: seal_payload(&user_key, &payload)?,
            extra: serde_json::Map::new(),
        };

        write_atomic(&self.vault_path, &serde_json::to_vec_pretty(&file)?)?;
        tracing::info!(
            backup = ?backup,
            "migrated credentials vault from v1 (stronger KDF, per-item keys, scoped secrets)"
        );
        *self.state.write().unwrap() = Some(Unlocked { user_key, payload });
        Ok(())
    }
}

/// The decrypted shape written into a backup.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlainExport {
    #[serde(default)]
    envs: BTreeMap<Scope, BTreeMap<String, String>>,
    #[serde(default)]
    items: Vec<PlainItem>,
    #[serde(default)]
    folders: Vec<String>,
}

fn locked_err() -> anyhow::Error {
    anyhow!("Vault is locked; call unlock() first")
}

fn find_item<'a>(payload: &'a Payload, item_id: &str) -> Result<&'a VaultItem> {
    payload
        .items
        .iter()
        .find(|i| i.id == item_id)
        .ok_or_else(|| anyhow!("item not found: {item_id}"))
}

// Payload encryption uses a domain-separated subkey of `user_key`, never
// `user_key` itself — see the module docs' key-hierarchy diagram.
fn seal_payload(user_key: &SecretKey, payload: &Payload) -> Result<Sealed> {
    let json = serde_json::to_vec(payload).context("serialize vault payload")?;
    seal(&user_key.derive_subkey(CTX_PAYLOAD), &json)
}

fn open_payload(user_key: &SecretKey, sealed: &Sealed) -> Result<Payload> {
    let json = open(&user_key.derive_subkey(CTX_PAYLOAD), sealed)?;
    serde_json::from_slice(&json).context("parse vault payload")
}

/// Write via a temporary file in the same directory, fsync it, then rename.
///
/// `rename` within a directory is atomic, so a crash mid-write leaves either the
/// old vault or the new one — never a truncated file. Permissions are tightened
/// to `0600` before the rename so the contents are never briefly world-readable.
///
/// The `sync_all` matters and is easy to leave out: atomicity is about ordering,
/// not persistence. Without it the rename can reach disk while the data it points
/// at is still in the page cache, and a power cut at the wrong moment leaves a
/// vault file that is intact, current, and full of zeroes. Paying one fsync per
/// write is the correct trade for a file whose loss means losing every credential
/// the user owns.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("vault path has no parent directory"))?;
    std::fs::create_dir_all(parent).context("create vault directory")?;

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("vault path has no file name"))?;
    let tmp = parent.join(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));

    let write_and_sync = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()
    };
    if let Err(e) = write_and_sync() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("write temporary vault {tmp:?}"));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).context("restrict vault file permissions to 0600");
        }
    }

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("replace vault file {path:?}"));
    }

    // Also sync the directory, so the rename itself survives a crash. Best
    // effort: some platforms and filesystems refuse to open a directory for
    // this, and failing the write over it would be worse than the risk.
    #[cfg(unix)]
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }

    Ok(())
}

/// Format-internal tests.
///
/// These live in the crate rather than `tests/` because they hand-build old
/// vault files, which needs the private `Payload` and the domain-separation
/// constants. Upgrading someone's only copy of their credentials is the one
/// operation that has to be right, so it is tested against a real v2 file rather
/// than a mock of one.
#[cfg(test)]
mod tests {
    use super::*;

    /// Write a genuine version 2 file.
    ///
    /// The JSON is spelled out by hand rather than serialized from [`VaultFile`],
    /// and that is the whole point: a test that round-trips through the current
    /// structs proves only that this build agrees with itself. It would happily
    /// pass while a serde rename quietly made every real v2 file on disk
    /// unreadable. These are the bytes version 2 actually wrote — flat `secrets`
    /// map, snake_case throughout, no `params`, no `last_used_at`.
    fn write_v2_vault(path: &Path, password: &str, secrets: &[(&str, &str)]) {
        let kdf = KdfParams::recommended();
        let master_key = kdf.derive(password).unwrap();
        let user_key = SecretKey::generate();
        let keypair = KeyPair::generate();

        let payload = Payload {
            secrets: secrets
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        };

        let wrapped_user_key =
            wrap_key(&master_key.derive_subkey(CTX_USER_KEY_WRAP), &user_key).unwrap();

        let file = serde_json::json!({
            "version": 2,
            "unlock_methods": [{
                "id": "master-password",
                "kind": "master-password",
                "label": "Master password",
                "wrapped_user_key": wrapped_user_key,
                "kdf": kdf,
                "created_at": Utc::now().to_rfc3339(),
            }],
            "public_key_b64": keypair.public_b64(),
            "wrapped_private_key": seal(&user_key, keypair.secret.as_bytes()).unwrap(),
            "payload": seal_payload(&user_key, &payload).unwrap(),
        });
        write_atomic(path, &serde_json::to_vec_pretty(&file).unwrap()).unwrap();
    }

    #[test]
    fn a_v2_vault_opens_and_its_secrets_land_in_global_scope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v2.json");
        write_v2_vault(&path, "master", &[("OPENAI_API_KEY", "sk-old"), ("B", "2")]);

        let vault = Vault::new(path.clone());
        vault.unlock("master").unwrap();

        // Reachable through both the legacy flat API and the scoped one.
        assert_eq!(vault.get("OPENAI_API_KEY").unwrap(), "sk-old");
        assert_eq!(
            vault.get_scoped(&Scope::Global, "OPENAI_API_KEY").unwrap(),
            "sk-old"
        );
        assert_eq!(vault.list_scopes().unwrap(), vec![Scope::Global]);
    }

    #[test]
    fn opening_a_v2_vault_upgrades_the_file_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v2.json");
        write_v2_vault(&path, "master", &[("K", "V")]);

        let vault = Vault::new(path.clone());
        vault.unlock("master").unwrap();
        // Any write upgrades the stamped version.
        vault.set("NEW", "value").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"version\": 3"), "file should be upgraded");
        // The legacy key is gone from the file, not just shadowed.
        assert!(
            !raw.contains("\"secrets\""),
            "legacy map should not persist"
        );

        // And it still opens with the original password afterwards.
        let reopened = Vault::new(path);
        reopened.unlock("master").unwrap();
        assert_eq!(reopened.get("K").unwrap(), "V");
        assert_eq!(reopened.get("NEW").unwrap(), "value");
    }

    #[test]
    fn a_scoped_value_wins_over_a_legacy_one_of_the_same_name() {
        // Both halves populated means a newer build already wrote the scoped
        // copy; folding must not clobber it with the stale flat value.
        let mut payload = Payload {
            secrets: [("K".to_string(), "legacy".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        payload
            .envs
            .entry(Scope::Global)
            .or_default()
            .insert("K".to_string(), "current".to_string());

        payload.normalize();

        assert_eq!(payload.envs[&Scope::Global]["K"], "current");
        assert!(payload.secrets.is_empty(), "legacy map should be drained");
    }

    #[test]
    fn normalize_is_idempotent() {
        let mut payload = Payload {
            secrets: [("A".to_string(), "1".to_string())].into_iter().collect(),
            ..Default::default()
        };
        payload.normalize();
        let once = payload.envs.clone();
        payload.normalize();
        assert_eq!(payload.envs, once);
    }

    #[test]
    fn the_audit_log_is_bounded() {
        let mut payload = Payload::default();
        for i in 0..(AUDIT_MAX + 250) {
            payload.record("test.event", Some(i.to_string()));
        }
        assert_eq!(payload.audit.len(), AUDIT_MAX);
        // The oldest entries are the ones dropped.
        assert_eq!(payload.audit[0].detail.as_deref(), Some("250"));
    }

    #[test]
    fn upgrading_a_v2_file_keeps_a_copy_of_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v2.json");
        write_v2_vault(&path, "master", &[("K", "V")]);
        let original = std::fs::read(&path).unwrap();

        let vault = Vault::new(path.clone());
        vault.unlock("master").unwrap();
        vault.set("NEW", "value").unwrap();

        let backup = path.with_extension("v2.bak");
        assert!(backup.exists(), "the pre-upgrade file should be kept");
        assert_eq!(
            std::fs::read(&backup).unwrap(),
            original,
            "the backup must be the original bytes, not an upgraded copy"
        );

        // Later writes must not overwrite the pristine copy.
        vault.set("ANOTHER", "value").unwrap();
        assert_eq!(std::fs::read(&backup).unwrap(), original);
    }

    #[test]
    fn a_vault_already_at_the_current_version_is_not_backed_up() {
        // Otherwise every vault would accumulate a redundant .bak beside it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.json");
        let vault = Vault::new(path.clone());
        vault.init("master").unwrap();
        vault.set("K", "V").unwrap();

        assert!(!path.with_extension("v3.bak").exists());
    }

    #[test]
    fn fields_from_a_newer_build_survive_being_rewritten() {
        // The downgrade scenario: a newer version writes a field this build has
        // never heard of, then this build opens the vault and saves. Dropping
        // the field would silently destroy whatever it held.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.json");
        let vault = Vault::new(path.clone());
        vault.init("master").unwrap();
        vault.lock();

        // Splice in fields from an imagined future version.
        let mut raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        raw["quantum_resistant_wrap"] = serde_json::json!({ "alg": "ml-kem-768" });
        raw["unlock_methods"][0]["future_hint"] = serde_json::json!("keep me");
        std::fs::write(&path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();

        // Open it and cause several writes.
        let vault = Vault::new(path.clone());
        vault.unlock("master").unwrap();
        vault.set("K", "V").unwrap();
        vault.add_item(NewItem::login("Site", "u", "p")).unwrap();

        let after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            after["quantum_resistant_wrap"]["alg"], "ml-kem-768",
            "an unknown top-level field must survive"
        );
        // And the vault still works.
        assert_eq!(vault.get("K").unwrap(), "V");
    }

    #[test]
    fn a_v2_file_with_camel_cased_methods_still_opens() {
        // Defensive: an intermediate build spelled UnlockMethod in camelCase.
        // Any file it wrote must still open rather than reading as corrupt.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("camel.json");
        write_v2_vault(&path, "master", &[("K", "V")]);

        let mut raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let method = &mut raw["unlock_methods"][0];
        method["wrappedUserKey"] = method["wrapped_user_key"].take();
        method["createdAt"] = method["created_at"].take();
        let obj = method.as_object_mut().unwrap();
        obj.remove("wrapped_user_key");
        obj.remove("created_at");
        std::fs::write(&path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();

        let vault = Vault::new(path);
        vault.unlock("master").unwrap();
        assert_eq!(vault.get("K").unwrap(), "V");
    }

    #[test]
    fn unknown_method_params_survive_a_round_trip() {
        // A future build may add an unlock method carrying parameters this one
        // has never heard of. Rewriting the file must not silently drop them, or
        // upgrading and then downgrading would destroy a working credential.
        let mut params = BTreeMap::new();
        params.insert("somethingFromTheFuture".to_string(), "keep-me".to_string());

        let method = UnlockMethod {
            id: "m".to_string(),
            kind: UnlockKind::Passkey,
            label: "Future".to_string(),
            wrapped_user_key: Sealed {
                nonce_b64: "AAAA".to_string(),
                ciphertext_b64: "BBBB".to_string(),
            },
            kdf: None,
            params,
            created_at: Utc::now().to_rfc3339(),
            last_used_at: None,
        };

        let json = serde_json::to_string(&method).unwrap();
        let back: UnlockMethod = serde_json::from_str(&json).unwrap();
        assert_eq!(back.params["somethingFromTheFuture"], "keep-me");
    }
}
