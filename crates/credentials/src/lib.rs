//! Credential storage for Consistent CEO.
//!
//! Two backends, chosen by deployment:
//!
//! - [`keyring`] — the OS keychain (macOS Keychain, Windows Credential Manager,
//!   Secret Service on Linux desktops). Preferred where it exists.
//! - [`vault`] — a single-file encrypted vault unlocked by one master password,
//!   for headless servers and containers with no keychain. This is also the
//!   password-manager surface: typed items, a generator, and end-to-end
//!   encrypted sharing between users.
//!
//! See [`vault`] for the key hierarchy and the seam for adding passkeys.

/// Have I Been Pwned checks (k-anonymity hashing only; no network calls here).
pub mod breach;
/// The AEAD/KDF/sealed-box primitives every other module builds on.
pub mod crypto;
/// Password and passphrase generation.
pub mod generator;
/// Reading CSV exports from other password managers.
pub mod import;
pub mod keyring;
/// The item and vault-payload shapes stored on disk.
pub mod model;
/// Printed recovery codes, the escape hatch when every device is gone.
pub mod recovery;
/// Where a secret belongs: global vs. per-entity vs. per-project.
pub mod scope;
/// Master-password strength estimation.
pub mod strength;
/// RFC 6238 time-based one-time passwords.
pub mod totp;
pub mod vault;
