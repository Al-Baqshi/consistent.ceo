//! Recovery codes: the escape hatch when every device is gone.
//!
//! A passkey lives in one platform's keychain. Phones are lost, laptops die, and
//! an Apple or Google account can be closed out from under you. If the passkey is
//! the only way into the vault, the vault is one bad afternoon away from being
//! landfill. So every vault also gets a recovery code, printed once, that unwraps
//! the same `user_key`.
//!
//! # Format
//!
//! 160 bits of CSPRNG output in Crockford base32 — `0123456789ABCDEFGHJKMNPQRSTVWXYZ`,
//! which drops `I`, `L`, `O` and `U`. Thirty-two characters, shown in eight
//! dash-separated groups of four:
//!
//! ```text
//! K3M9-7PQR-2WXY-5TVB-8NHJ-4CDF-6GKM-9RST
//! ```
//!
//! 160 bits is far beyond brute force and short enough to write on paper without
//! transcription errors — the point of the whole exercise.
//!
//! [`decode`] is deliberately forgiving in exactly the ways a human transcribing
//! from paper gets it wrong, and strict everywhere else. Case is ignored; dashes,
//! spaces and tabs are ignored; and the four excluded letters are folded to the
//! digits they are mistaken for (`O`→`0`, `I`/`L`→`1`). `U` has no digit it is
//! confused with, so it is simply rejected rather than guessed at.

use anyhow::{Context, Result, anyhow, bail};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;

use crate::crypto::{KEY_LEN, SecretKey};

/// Crockford base32. No `I`, `L`, `O`, or `U`.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// 160 bits. Exactly 32 base32 characters, with no padding.
pub const RECOVERY_BYTES: usize = 20;
pub const RECOVERY_CHARS: usize = 32;
const GROUP: usize = 4;

/// HKDF domain separator. The typed code is high-entropy already, so it needs no
/// memory-hard stretching — but it must never be used as a key directly, and it
/// must land in a different place than any other use of the same bytes.
const CTX_RECOVERY: &[u8] = b"consistent-ceo/vault/recovery-code/v1";

/// A freshly generated recovery code.
///
/// Holds the display form only. The caller shows [`Self::formatted`] to the user
/// exactly once, wraps the `user_key` under [`Self::key`], and then drops this —
/// there is no way to recover the string from the vault file afterwards, which is
/// the entire security property.
pub struct RecoveryCode {
    formatted: String,
}

impl RecoveryCode {
    /// 160 fresh bits from the OS CSPRNG.
    pub fn generate() -> Self {
        let mut bytes = [0u8; RECOVERY_BYTES];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Self {
            formatted: format_groups(&encode(&bytes)),
        }
    }

    /// The dash-grouped form to show the user. Show it once.
    pub fn formatted(&self) -> &str {
        &self.formatted
    }

    /// The 32-byte key this code unwraps with.
    pub fn key(&self) -> Result<SecretKey> {
        derive_key(&self.formatted)
    }
}

impl std::fmt::Debug for RecoveryCode {
    /// A recovery code is a password. It never lands in a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RecoveryCode(<redacted>)")
    }
}

/// Turn a user-typed recovery code into the key that unwraps the `user_key`.
///
/// Accepts any formatting a person might produce from a printed card, so
/// `k3m9 7pqr...`, `K3M97PQR...` and the canonical dashed form are all the same
/// code.
/// The code carries 160 bits; a `SecretKey` is 256. HKDF-SHA256 expands the
/// transcribed bytes to key width, binding them to [`CTX_RECOVERY`] so the same
/// code can never collide with any other use of the same input.
///
/// This is expansion, not stretching — and it does not need to be. A memory-hard
/// KDF exists to make guessing a human-chosen password expensive; 160 bits of
/// CSPRNG output is not guessable at any cost, so Argon2 here would buy nothing
/// but a slow recovery path on the one day it matters most.
pub fn derive_key(typed: &str) -> Result<SecretKey> {
    let bytes = decode(typed)?;
    let hk = Hkdf::<Sha256>::new(None, &bytes);
    let mut out = [0u8; KEY_LEN];
    hk.expand(CTX_RECOVERY, &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    Ok(SecretKey::from_bytes(out))
}

/// Bytes to base32 characters, five bits at a time, most significant first.
fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 8 / 5 + 1);
    let mut acc: u16 = 0;
    let mut bits: u8 = 0;

    for &byte in bytes {
        acc = (acc << 8) | byte as u16;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let index = ((acc >> bits) & 0b1_1111) as usize;
            out.push(ALPHABET[index] as char);
        }
    }
    // 160 bits divides evenly into 5-bit groups, so nothing is left over. Handle
    // the remainder anyway so this stays correct if RECOVERY_BYTES ever changes.
    if bits > 0 {
        let index = ((acc << (5 - bits)) & 0b1_1111) as usize;
        out.push(ALPHABET[index] as char);
    }
    out
}

/// Insert a dash every [`GROUP`] characters.
fn format_groups(raw: &str) -> String {
    raw.as_bytes()
        .chunks(GROUP)
        .map(|c| std::str::from_utf8(c).expect("base32 output is ASCII"))
        .collect::<Vec<_>>()
        .join("-")
}

/// Map one typed character to its 5-bit value, forgiving the confusions a person
/// actually makes when reading a printed code.
fn value_of(c: char) -> Result<u8> {
    let upper = c.to_ascii_uppercase();
    let normalized = match upper {
        // Not in the alphabet; only ever a misread of the digit.
        'O' => '0',
        'I' | 'L' => '1',
        other => other,
    };
    ALPHABET
        .iter()
        .position(|&a| a as char == normalized)
        .map(|p| p as u8)
        .ok_or_else(|| anyhow!("'{c}' is not a valid recovery code character"))
}

/// Parse a typed code back to its 20 bytes.
pub fn decode(typed: &str) -> Result<[u8; RECOVERY_BYTES]> {
    let cleaned: Vec<char> = typed
        .chars()
        .filter(|c| !matches!(c, '-' | ' ' | '\t' | '\n' | '\r' | '_'))
        .collect();

    if cleaned.len() != RECOVERY_CHARS {
        bail!(
            "a recovery code is {RECOVERY_CHARS} characters; this one has {}",
            cleaned.len()
        );
    }

    let mut out = [0u8; RECOVERY_BYTES];
    let mut acc: u16 = 0;
    let mut bits: u8 = 0;
    let mut written = 0usize;

    for c in cleaned {
        acc = (acc << 5) | value_of(c).context("recovery code")? as u16;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out[written] = ((acc >> bits) & 0xFF) as u8;
            written += 1;
        }
    }

    debug_assert_eq!(written, RECOVERY_BYTES);
    Ok(out)
}

/// A 32-byte key from the OS CSPRNG, for unlock methods whose key material is
/// generated rather than typed — a device key held in the OS keychain, or the
/// output of a WebAuthn PRF evaluation.
pub fn random_key() -> SecretKey {
    let mut bytes = [0u8; KEY_LEN];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    SecretKey::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_code_has_the_documented_shape() {
        let code = RecoveryCode::generate();
        let text = code.formatted();

        assert_eq!(text.len(), RECOVERY_CHARS + 7, "32 chars plus 7 dashes");
        assert_eq!(text.matches('-').count(), 7);
        for group in text.split('-') {
            assert_eq!(group.len(), GROUP);
            for c in group.chars() {
                assert!(ALPHABET.contains(&(c as u8)), "{c} is outside the alphabet");
            }
        }
    }

    #[test]
    fn encoding_round_trips() {
        for _ in 0..100 {
            let mut bytes = [0u8; RECOVERY_BYTES];
            rand::rngs::OsRng.fill_bytes(&mut bytes);
            assert_eq!(decode(&encode(&bytes)).unwrap(), bytes);
        }
    }

    #[test]
    fn formatting_does_not_change_the_key() {
        let code = RecoveryCode::generate();
        let canonical = code.formatted().to_string();
        let expected = code.key().unwrap();

        // Every one of these is the same code as far as a human is concerned.
        for variant in [
            canonical.replace('-', ""),
            canonical.replace('-', " "),
            canonical.to_lowercase(),
            format!("  {canonical}  "),
            canonical.replace('-', "_"),
        ] {
            assert_eq!(
                derive_key(&variant).unwrap().expose(),
                expected.expose(),
                "{variant:?} should be the same code"
            );
        }
    }

    #[test]
    fn misread_letters_fold_to_their_digits() {
        // A code containing 0 and 1 must still decode when someone writes down
        // the letters they look like.
        let bytes = [0u8; RECOVERY_BYTES];
        let canonical = encode(&bytes); // all zeros -> "000...0"
        let misread: String = canonical.replace('0', "O");
        assert_eq!(decode(&misread).unwrap(), bytes);

        let ones = decode(&"1".repeat(RECOVERY_CHARS)).unwrap();
        assert_eq!(decode(&"I".repeat(RECOVERY_CHARS)).unwrap(), ones);
        assert_eq!(decode(&"l".repeat(RECOVERY_CHARS)).unwrap(), ones);
    }

    #[test]
    fn a_wrong_length_code_is_refused() {
        let code = RecoveryCode::generate();
        let text = code.formatted().replace('-', "");

        assert!(decode(&text[..RECOVERY_CHARS - 1]).is_err());
        assert!(decode(&format!("{text}A")).is_err());
        assert!(decode("").is_err());
    }

    #[test]
    fn characters_outside_the_alphabet_are_refused() {
        // `U` is excluded from Crockford base32 and is not a misread of anything,
        // so it is rejected rather than guessed at.
        assert!(decode(&"U".repeat(RECOVERY_CHARS)).is_err());
        assert!(decode(&"$".repeat(RECOVERY_CHARS)).is_err());
    }

    #[test]
    fn two_codes_are_never_the_same() {
        let a = RecoveryCode::generate();
        let b = RecoveryCode::generate();
        assert_ne!(a.formatted(), b.formatted());
        assert_ne!(a.key().unwrap().expose(), b.key().unwrap().expose());
    }

    #[test]
    fn the_derived_key_is_not_the_raw_code_bytes() {
        // The stored wrapping key must be a derivation, never the transcribed
        // bytes themselves.
        let code = RecoveryCode::generate();
        let raw = decode(code.formatted()).unwrap();
        let derived = code.key().unwrap();
        assert_ne!(&derived.expose()[..RECOVERY_BYTES], &raw[..]);
    }

    #[test]
    fn random_keys_are_distinct() {
        assert_ne!(random_key().expose(), random_key().expose());
    }
}
