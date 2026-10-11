//! Vault crypto primitives.
//!
//! Deliberately small and boring. Everything the vault does reduces to four
//! operations defined here:
//!
//! - [`KdfParams::derive`] — master password + salt -> 32-byte master key (Argon2id).
//! - [`seal`] / [`open`] — AES-256-GCM with a random 96-bit nonce, used for every
//!   symmetric wrap in the vault (user key, item keys, item payloads).
//! - [`seal_to_public`] / [`open_from_private`] — anonymous X25519 sealed box, used
//!   only when handing an item key to *another* user during a share.
//! - [`SecretKey`] — a 32-byte key that zeroizes on drop.
//!
//! Design notes that matter for review:
//!
//! * The master key never encrypts user data. It only wraps a random `user_key`
//!   (see [`crate::vault`]). Changing the master password therefore rewraps 32
//!   bytes rather than re-encrypting the vault, and additional unlock methods
//!   (passkey, hardware token) become extra wrappings of the same `user_key`.
//! * Nonces are random per seal. AES-GCM tolerates this comfortably at vault
//!   write volumes; keys are also rotated per item rather than shared globally.
//! * `hkdf` is used for domain separation so that two different uses of the same
//!   root secret can never produce the same subkey.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use hkdf::Hkdf;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Width of every symmetric key in this crate (AES-256-GCM, X25519).
pub const KEY_LEN: usize = 32;
/// Width of an Argon2id salt.
pub const SALT_LEN: usize = 16;
/// Width of an AES-GCM nonce (96 bits, as the construction requires).
pub const NONCE_LEN: usize = 12;

/// A 32-byte symmetric key that is wiped from memory when dropped.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SecretKey([u8; KEY_LEN]);

impl SecretKey {
    /// Wrap already-derived key bytes (e.g. after unwrapping one from storage).
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// Fresh key from the OS CSPRNG.
    pub fn generate() -> Self {
        let mut bytes = [0u8; KEY_LEN];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    /// Borrow the raw bytes. Named loudly so call sites make clear they are
    /// handling key material, not an opaque token.
    pub fn expose(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    /// Derive a domain-separated subkey. Two different `info` strings can never
    /// yield the same subkey, which is what lets one root secret safely serve
    /// several purposes.
    pub fn derive_subkey(&self, info: &[u8]) -> SecretKey {
        let hk = Hkdf::<Sha256>::new(None, &self.0);
        let mut out = [0u8; KEY_LEN];
        hk.expand(info, &mut out)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        SecretKey(out)
    }
}

impl std::fmt::Debug for SecretKey {
    /// Never print key material, even by accident.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretKey(<redacted>)")
    }
}

/// Argon2id parameters, stored alongside the vault so they can be raised later
/// without locking anyone out of an existing file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KdfParams {
    /// Always `"argon2id"`; checked by [`Self::derive`] before use.
    pub algorithm: String,
    /// Memory cost in KiB.
    pub m_cost: u32,
    /// Iterations.
    pub t_cost: u32,
    /// Parallelism (lanes).
    pub p_cost: u32,
    /// Base64-encoded random salt, unique per set of parameters.
    pub salt_b64: String,
}

impl KdfParams {
    /// OWASP's recommended Argon2id baseline (64 MiB, 3 iterations, 4 lanes),
    /// which is a substantial step up from `Argon2::default()`'s 19 MiB / t=2.
    pub fn recommended() -> Self {
        let mut salt = [0u8; SALT_LEN];
        rand::rngs::OsRng.fill_bytes(&mut salt);
        Self {
            algorithm: "argon2id".to_string(),
            m_cost: 65_536,
            t_cost: 3,
            p_cost: 4,
            salt_b64: B64.encode(salt),
        }
    }

    /// Same parameters, fresh salt. Used when changing the master password.
    pub fn with_fresh_salt(&self) -> Self {
        let mut salt = [0u8; SALT_LEN];
        rand::rngs::OsRng.fill_bytes(&mut salt);
        Self {
            salt_b64: B64.encode(salt),
            ..self.clone()
        }
    }

    /// Stretch a low-entropy password into a full-width key via Argon2id.
    pub fn derive(&self, master_password: &str) -> Result<SecretKey> {
        if self.algorithm != "argon2id" {
            bail!("unsupported vault KDF algorithm: {}", self.algorithm);
        }
        let salt = B64
            .decode(&self.salt_b64)
            .context("decode vault KDF salt")?;
        let params = argon2::Params::new(self.m_cost, self.t_cost, self.p_cost, Some(KEY_LEN))
            .map_err(|e| anyhow!("invalid argon2 params: {e}"))?;
        let argon =
            argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        let mut out = [0u8; KEY_LEN];
        argon
            .hash_password_into(master_password.as_bytes(), &salt, &mut out)
            .map_err(|e| anyhow!("argon2: {e}"))?;
        Ok(SecretKey(out))
    }
}

/// AES-256-GCM ciphertext with its nonce. Serialized as base64 so the vault file
/// stays inspectable JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sealed {
    pub nonce_b64: String,
    pub ciphertext_b64: String,
}

/// Encrypt `plaintext` under `key` with AES-256-GCM and a fresh random nonce.
pub fn seal(key: &SecretKey, plaintext: &[u8]) -> Result<Sealed> {
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let cipher = Aes256Gcm::new_from_slice(key.expose())
        .map_err(|_| anyhow!("aes-256-gcm rejected a 32-byte key"))?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
        .map_err(|_| anyhow!("vault encryption failed"))?;
    Ok(Sealed {
        nonce_b64: B64.encode(nonce_bytes),
        ciphertext_b64: B64.encode(ciphertext),
    })
}

/// Decrypt a [`Sealed`] value. Fails closed on either a wrong key or a
/// tampered ciphertext — AES-GCM's authentication tag makes the two
/// indistinguishable, which is the correct failure mode here.
pub fn open(key: &SecretKey, sealed: &Sealed) -> Result<Vec<u8>> {
    let nonce = B64.decode(&sealed.nonce_b64).context("decode nonce")?;
    if nonce.len() != NONCE_LEN {
        bail!("vault nonce has wrong length");
    }
    let ciphertext = B64
        .decode(&sealed.ciphertext_b64)
        .context("decode ciphertext")?;
    let cipher = Aes256Gcm::new_from_slice(key.expose())
        .map_err(|_| anyhow!("aes-256-gcm rejected a 32-byte key"))?;
    cipher
        .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
        // Deliberately opaque: the caller cannot distinguish a wrong password
        // from a tampered file, which is the correct behaviour for an AEAD.
        .map_err(|_| anyhow!("decryption failed (wrong password, or the data was modified)"))
}

/// Convenience wrappers for the common "seal a key under another key" case.
pub fn wrap_key(wrapping: &SecretKey, target: &SecretKey) -> Result<Sealed> {
    seal(wrapping, target.expose())
}

/// Inverse of [`wrap_key`]: open a sealed key and rebuild it as a [`SecretKey`].
pub fn unwrap_key(wrapping: &SecretKey, sealed: &Sealed) -> Result<SecretKey> {
    let bytes = open(wrapping, sealed)?;
    let arr: [u8; KEY_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("unwrapped key has wrong length"))?;
    Ok(SecretKey::from_bytes(arr))
}

/// An X25519 keypair. The public half identifies a user as a share recipient;
/// the secret half is itself stored wrapped under that user's `user_key`.
pub struct KeyPair {
    pub secret: StaticSecret,
    pub public: PublicKey,
}

impl KeyPair {
    /// Fresh X25519 keypair from the OS CSPRNG.
    pub fn generate() -> Self {
        let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    /// Rebuild a keypair from a previously stored secret half.
    pub fn from_secret_bytes(bytes: [u8; KEY_LEN]) -> Self {
        let secret = StaticSecret::from(bytes);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    /// The public half, base64-encoded for storage or publishing.
    pub fn public_b64(&self) -> String {
        B64.encode(self.public.as_bytes())
    }
}

/// An anonymous sealed box: an ephemeral X25519 keypair encrypts to a recipient's
/// public key, so only the holder of the matching private key can open it. The
/// sender is not authenticated, which is fine here — the gateway authenticates
/// *who shared what* separately, and the envelope only ever carries an item key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SealedBox {
    pub ephemeral_public_b64: String,
    pub nonce_b64: String,
    pub ciphertext_b64: String,
}

/// HKDF `info` binding the derived key to both public keys, so an envelope can't
/// be replayed against a different recipient.
fn sealed_box_context(ephemeral: &PublicKey, recipient: &PublicKey) -> Vec<u8> {
    let mut info = b"consistent-ceo/vault/sealed-box/v1".to_vec();
    info.extend_from_slice(ephemeral.as_bytes());
    info.extend_from_slice(recipient.as_bytes());
    info
}

/// Encrypt `plaintext` to a recipient's public key: ephemeral X25519 keypair,
/// Diffie-Hellman with the recipient, HKDF to a symmetric key, then AES-GCM.
/// Only the holder of the matching private key can open the result.
pub fn seal_to_public(recipient_public_b64: &str, plaintext: &[u8]) -> Result<SealedBox> {
    let raw = B64
        .decode(recipient_public_b64)
        .context("decode recipient public key")?;
    let arr: [u8; KEY_LEN] = raw
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("recipient public key must be 32 bytes"))?;
    let recipient = PublicKey::from(arr);

    let ephemeral_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
    let ephemeral_public = PublicKey::from(&ephemeral_secret);
    let mut shared = ephemeral_secret.diffie_hellman(&recipient).to_bytes();

    let hk = Hkdf::<Sha256>::new(None, &shared);
    let mut key_bytes = [0u8; KEY_LEN];
    hk.expand(
        &sealed_box_context(&ephemeral_public, &recipient),
        &mut key_bytes,
    )
    .expect("32 bytes is a valid HKDF-SHA256 output length");
    shared.zeroize();

    let key = SecretKey::from_bytes(key_bytes);
    let sealed = seal(&key, plaintext)?;
    Ok(SealedBox {
        ephemeral_public_b64: B64.encode(ephemeral_public.as_bytes()),
        nonce_b64: sealed.nonce_b64,
        ciphertext_b64: sealed.ciphertext_b64,
    })
}

/// Inverse of [`seal_to_public`]: redo the Diffie-Hellman with the recipient's
/// own private key to recover the symmetric key, then decrypt.
pub fn open_from_private(keypair: &KeyPair, boxed: &SealedBox) -> Result<Vec<u8>> {
    let raw = B64
        .decode(&boxed.ephemeral_public_b64)
        .context("decode ephemeral public key")?;
    let arr: [u8; KEY_LEN] = raw
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("ephemeral public key must be 32 bytes"))?;
    let ephemeral_public = PublicKey::from(arr);

    let mut shared = keypair.secret.diffie_hellman(&ephemeral_public).to_bytes();
    let hk = Hkdf::<Sha256>::new(None, &shared);
    let mut key_bytes = [0u8; KEY_LEN];
    hk.expand(
        &sealed_box_context(&ephemeral_public, &keypair.public),
        &mut key_bytes,
    )
    .expect("32 bytes is a valid HKDF-SHA256 output length");
    shared.zeroize();

    let key = SecretKey::from_bytes(key_bytes);
    open(
        &key,
        &Sealed {
            nonce_b64: boxed.nonce_b64.clone(),
            ciphertext_b64: boxed.ciphertext_b64.clone(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_round_trip() {
        let key = SecretKey::generate();
        let sealed = seal(&key, b"hunter2").unwrap();
        assert_eq!(open(&key, &sealed).unwrap(), b"hunter2");
    }

    #[test]
    fn open_with_wrong_key_fails() {
        let sealed = seal(&SecretKey::generate(), b"hunter2").unwrap();
        assert!(open(&SecretKey::generate(), &sealed).is_err());
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let key = SecretKey::generate();
        let mut sealed = seal(&key, b"hunter2").unwrap();
        let mut raw = B64.decode(&sealed.ciphertext_b64).unwrap();
        raw[0] ^= 0xff;
        sealed.ciphertext_b64 = B64.encode(raw);
        assert!(open(&key, &sealed).is_err());
    }

    #[test]
    fn subkeys_are_domain_separated() {
        let root = SecretKey::generate();
        assert_ne!(
            root.derive_subkey(b"a").expose(),
            root.derive_subkey(b"b").expose()
        );
        assert_eq!(
            root.derive_subkey(b"a").expose(),
            root.derive_subkey(b"a").expose()
        );
    }

    #[test]
    fn sealed_box_round_trip() {
        let recipient = KeyPair::generate();
        let boxed = seal_to_public(&recipient.public_b64(), b"item-key-bytes").unwrap();
        assert_eq!(
            open_from_private(&recipient, &boxed).unwrap(),
            b"item-key-bytes"
        );
    }

    #[test]
    fn sealed_box_cannot_be_opened_by_a_third_party() {
        let recipient = KeyPair::generate();
        let interloper = KeyPair::generate();
        let boxed = seal_to_public(&recipient.public_b64(), b"item-key-bytes").unwrap();
        assert!(open_from_private(&interloper, &boxed).is_err());
    }

    #[test]
    fn kdf_is_deterministic_for_a_given_salt() {
        let params = KdfParams::recommended();
        let a = params.derive("correct horse").unwrap();
        let b = params.derive("correct horse").unwrap();
        assert_eq!(a.expose(), b.expose());
        assert_ne!(params.derive("wrong horse").unwrap().expose(), a.expose());
    }

    #[test]
    fn fresh_salt_changes_the_derived_key() {
        let params = KdfParams::recommended();
        let rotated = params.with_fresh_salt();
        assert_ne!(params.salt_b64, rotated.salt_b64);
        assert_ne!(
            params.derive("same password").unwrap().expose(),
            rotated.derive("same password").unwrap().expose()
        );
    }
}
