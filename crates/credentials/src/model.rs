//! The shapes stored in a vault.
//!
//! Split into two layers on purpose:
//!
//! - [`ItemSecret`] is the sensitive half (password, TOTP seed, private notes,
//!   arbitrary custom fields). It is encrypted under a per-item key and never
//!   written in the clear.
//! - [`VaultItem`] is the envelope: id, kind, name, folder, timestamps, and the
//!   wrapped key. Names and folders are themselves inside the vault's encrypted
//!   payload, so this split is not about hiding metadata from an attacker — it is
//!   about giving each item an independently shareable key.

use serde::{Deserialize, Serialize};

use crate::crypto::{Sealed, SealedBox};
use crate::scope::Scope;

/// What kind of thing an item stores. Drives which fields a UI shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ItemKind {
    /// Site or app login.
    Login,
    /// Free-form secret note.
    SecureNote,
    /// API key, token, or other machine credential.
    ApiCredential,
    /// Payment card.
    Card,
}

/// The encrypted-at-rest half of an item.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ItemSecret {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// TOTP shared secret (base32), for generating 2FA codes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Extra user-defined fields, e.g. security questions or account numbers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_fields: Vec<CustomField>,
    /// Previous passwords, newest first. Maintained by the vault on every
    /// password change (see [`crate::vault::Vault::update_item_secret`]) — a
    /// caller cannot set or grow it, so it is a trustworthy record. Lets a user
    /// recover a password they changed and immediately regretted, and gives the
    /// breach check something to warn against reusing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub password_history: Vec<PasswordHistoryEntry>,
}

/// One superseded password and when it was replaced.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PasswordHistoryEntry {
    pub password: String,
    pub changed_at: String,
}

/// One user-defined key/value pair attached to an item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CustomField {
    pub name: String,
    pub value: String,
    /// Hidden fields are masked in any UI that renders the item.
    #[serde(default)]
    pub hidden: bool,
}

/// An item as stored in the vault payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultItem {
    pub id: String,
    pub kind: ItemKind,
    pub name: String,
    /// Which entity this item belongs to. `global` is the CEO's own set; an
    /// `entity:{id}` scope is that entity's own password set. Defaults to
    /// `global` so items written before scoping stay where they were.
    #[serde(default)]
    pub scope: Scope,
    /// A category *within* a scope — "Banking", "Social", "Infra". Free text so
    /// the user names their own filing; the UI groups by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// URIs this login should be offered for (autofill matching).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uris: Vec<String>,
    #[serde(default)]
    pub favorite: bool,
    /// Set when this item arrived via [`crate::vault::Vault::accept_share`],
    /// recording who sent it. `None` means the item was created locally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_from: Option<String>,
    /// This item's content key, wrapped under the owner's `user_key`.
    pub wrapped_item_key: Sealed,
    /// [`ItemSecret`] as JSON, encrypted under the item key.
    pub secret: Sealed,
    pub created_at: String,
    pub updated_at: String,
}

/// A summary safe to hand to a UI or API without unlocking the item's secret.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ItemSummary {
    pub id: String,
    pub kind: ItemKind,
    pub name: String,
    /// The entity this item belongs to, as its string form (`global`,
    /// `entity:acme`). Sent so a UI can group items by entity without a second
    /// call.
    pub scope: String,
    pub folder: Option<String>,
    pub uris: Vec<String>,
    pub favorite: bool,
    pub shared_from: Option<String>,
    pub updated_at: String,
}

impl From<&VaultItem> for ItemSummary {
    fn from(item: &VaultItem) -> Self {
        Self {
            id: item.id.clone(),
            kind: item.kind,
            name: item.name.clone(),
            scope: item.scope.to_string(),
            folder: item.folder.clone(),
            uris: item.uris.clone(),
            favorite: item.favorite,
            shared_from: item.shared_from.clone(),
            updated_at: item.updated_at.clone(),
        }
    }
}

/// What the caller supplies to create or replace an item.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewItem {
    pub kind: ItemKind,
    pub name: String,
    /// Which entity to file this under. Defaults to `global` (the CEO's own set).
    #[serde(default)]
    pub scope: Scope,
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default)]
    pub uris: Vec<String>,
    #[serde(default)]
    pub favorite: bool,
    #[serde(default)]
    pub secret: ItemSecret,
}

impl NewItem {
    /// Build a login item in the global scope from the fields it always has.
    pub fn login(
        name: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            kind: ItemKind::Login,
            name: name.into(),
            scope: Scope::Global,
            folder: None,
            uris: Vec::new(),
            favorite: false,
            secret: ItemSecret {
                username: Some(username.into()),
                password: Some(password.into()),
                ..Default::default()
            },
        }
    }

    /// The same login, filed under a given entity.
    pub fn in_scope(mut self, scope: Scope) -> Self {
        self.scope = scope;
        self
    }
}

/// One line in the vault's activity log.
///
/// Lives inside the encrypted payload, so the log is only readable by someone who
/// can already open the vault. `detail` holds an item or variable *name* and never
/// a value — the log is there to answer "what changed and when", and a log that
/// quietly accumulated plaintext secrets would be a second copy of the vault with
/// none of its protections.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntry {
    /// RFC3339.
    pub at: String,
    /// Dotted action name, e.g. `item.created`, `vault.unlocked`.
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// An item in its decrypted form, as written into a backup.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlainItem {
    pub kind: ItemKind,
    pub name: String,
    /// Preserved across a backup so an item comes back to the same entity.
    #[serde(default)]
    pub scope: Scope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uris: Vec<String>,
    #[serde(default)]
    pub favorite: bool,
    pub secret: ItemSecret,
    pub created_at: String,
    pub updated_at: String,
}

/// A portable, passphrase-encrypted backup of an entire vault.
///
/// Deliberately standalone: it does not reference the vault's key hierarchy, its
/// unlock methods, or the machine it came from. Ten years and three laptops later,
/// the passphrase and this file are together sufficient — which is the only
/// property that actually survives that long.
///
/// `kdf` is stored in the clear because it must be: the reader needs the salt and
/// cost parameters to derive the key at all. It reveals nothing without the
/// passphrase, and storing it means a backup written today still opens after the
/// recommended Argon2 parameters have been raised twice.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultExport {
    /// Always [`VaultExport::FORMAT`]; identifies the file to a future reader.
    pub format: String,
    pub version: u32,
    pub created_at: String,
    /// How many items are inside, so a restore can be confirmed before it runs.
    #[serde(default)]
    pub item_count: usize,
    pub kdf: crate::crypto::KdfParams,
    /// The decrypted vault contents, sealed under a key derived from the
    /// passphrase.
    pub payload: Sealed,
}

impl VaultExport {
    pub const FORMAT: &'static str = "consistent-ceo/vault-export";
    pub const VERSION: u32 = 1;
}

/// One flagged item in a [`HealthReport`]. Carries the item's id and name so a
/// UI can link straight to it — never the password itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ItemFlag {
    pub id: String,
    pub name: String,
    /// Which entity the item is in, so the UI can jump to the right set.
    pub scope: String,
    /// A short human explanation ("Weak — a top-1000 password", "Reused across
    /// 3 logins"). Never contains the secret.
    pub detail: String,
}

/// A password-hygiene report over the whole vault, in the spirit of Bitwarden's
/// Vault Health and 1Password's Watchtower.
///
/// Computed server-side because it must decrypt each item to judge it — but the
/// output is deliberately value-free: only ids, names and reasons cross the wire,
/// so the report is safe to render in a list without pulling every password.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HealthReport {
    /// Items that carry a password at all (the denominator for the score).
    pub total_with_passwords: usize,
    /// Passwords a cracker would reach quickly (zxcvbn score ≤ 2).
    pub weak: Vec<ItemFlag>,
    /// Passwords shared across more than one item — one breach, many accounts.
    pub reused: Vec<ItemFlag>,
    /// Passwords not changed in over a year.
    pub stale: Vec<ItemFlag>,
    /// 0–100. The share of password-bearing items with no issue at all.
    pub score: u8,
}

/// One item's password, reduced to the SHA-1 prefix/suffix a breach check needs.
///
/// The password itself never leaves the vault crate — only this hash split does,
/// and only the 5-char `prefix` is ever sent to an external service (see
/// [`crate::breach`]). Handed to the gateway, which does the HTTP.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BreachTarget {
    pub id: String,
    pub name: String,
    pub scope: String,
    /// First 5 hex chars of the SHA-1 — the only part sent to HIBP.
    pub prefix: String,
    /// Remaining 35 hex chars — matched locally against HIBP's response.
    pub suffix: String,
}

/// A single item handed to one recipient.
///
/// The item key is inside `sealed_item_key`, encrypted to the recipient's X25519
/// public key; the payload is encrypted under that item key. Neither the gateway
/// nor anyone holding the envelope can read it without the recipient's private
/// key, so this is safe to persist server-side and deliver asynchronously.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareEnvelope {
    pub share_id: String,
    /// Free-text label for why this was shared, e.g. "on-call handover".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenario: Option<String>,
    pub from_user: String,
    pub to_public_key_b64: String,
    pub kind: ItemKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uris: Vec<String>,
    /// The item key, sealed to the recipient's public key.
    pub sealed_item_key: SealedBox,
    /// [`ItemSecret`] as JSON, encrypted under the item key.
    pub secret: Sealed,
    pub created_at: String,
    /// RFC3339 instant after which a recipient must refuse this envelope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}
