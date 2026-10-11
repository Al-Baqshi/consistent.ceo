//! Time-based one-time passwords (RFC 6238), so the vault replaces the
//! authenticator app rather than sitting beside it.
//!
//! An item's `totp_secret` is the shared seed a service hands you when you turn on
//! 2FA — usually as a QR code, sometimes as raw base32. This module turns that
//! seed into the six digits you type, and parses the `otpauth://` URI a QR code
//! actually encodes so a user can paste the whole thing.
//!
//! # Why this belongs in the vault
//!
//! A password manager that stores your 2FA seeds but cannot generate the codes
//! has stored the hard part and skipped the useful part. Keeping the seed and the
//! code in the same place, behind the same unlock, is the entire "you only need
//! this one" promise — and the seed is exactly as sensitive as the password, so
//! it belongs behind the same encryption, not in a separate app with weaker
//! protection.
//!
//! # Scope
//!
//! RFC 6238 with the SHA-1/SHA-256/SHA-512 hashes real services use, 6–8 digits,
//! and a configurable period. Codes are *generated*, never validated here — the
//! vault produces them for the user to type elsewhere, so there is no verification
//! window to reason about.

use anyhow::{Context, Result, anyhow, bail};
use hmac::{Hmac, Mac};

/// The hash a TOTP secret was issued for. Almost everything is SHA-1; a few
/// services (notably some banks) use the stronger variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TotpAlgorithm {
    Sha1,
    Sha256,
    Sha512,
}

impl TotpAlgorithm {
    fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_uppercase().as_str() {
            "SHA1" => Ok(Self::Sha1),
            "SHA256" => Ok(Self::Sha256),
            "SHA512" => Ok(Self::Sha512),
            other => bail!("unsupported TOTP algorithm: {other}"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Sha1 => "SHA1",
            Self::Sha256 => "SHA256",
            Self::Sha512 => "SHA512",
        }
    }
}

/// A fully specified TOTP configuration.
#[derive(Debug, Clone)]
pub struct Totp {
    /// The decoded shared secret.
    secret: Vec<u8>,
    pub algorithm: TotpAlgorithm,
    /// Number of digits in the code, 6–8.
    pub digits: u32,
    /// Time step in seconds. Almost always 30.
    pub period: u64,
    /// Optional issuer/account labels, carried through from an `otpauth://` URI
    /// so a UI can show which account a code is for.
    pub issuer: Option<String>,
    pub account: Option<String>,
}

/// A generated code and how long it remains valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TotpCode {
    /// Zero-padded to the configured digit count.
    pub code: String,
    /// Seconds until this code rolls over to the next one.
    pub seconds_remaining: u64,
    /// Length of the full step, so a UI can render a countdown ring.
    pub period: u64,
}

impl Totp {
    /// Build from a stored secret, which may be raw base32 *or* a full
    /// `otpauth://` URI. Most services give one or the other, and users paste
    /// whichever they were given, so both are accepted.
    pub fn from_stored_secret(value: &str) -> Result<Self> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            bail!("no TOTP secret");
        }
        if trimmed.starts_with("otpauth://") {
            Self::from_uri(trimmed)
        } else {
            Self::from_base32(trimmed, TotpAlgorithm::Sha1, 6, 30)
        }
    }

    /// Build from a bare base32 secret and explicit parameters.
    pub fn from_base32(
        secret_b32: &str,
        algorithm: TotpAlgorithm,
        digits: u32,
        period: u64,
    ) -> Result<Self> {
        if !(6..=8).contains(&digits) {
            bail!("TOTP digit count must be between 6 and 8");
        }
        if period == 0 {
            bail!("TOTP period must be non-zero");
        }
        Ok(Self {
            secret: decode_base32(secret_b32)?,
            algorithm,
            digits,
            period,
            issuer: None,
            account: None,
        })
    }

    /// Parse an `otpauth://totp/...` URI, the thing a 2FA QR code actually
    /// encodes.
    ///
    /// Hand-parsed rather than pulling in a URL crate: the format is fixed and
    /// small, and the query values here are base32 and short integers, none of
    /// which need percent-decoding beyond the label. Keeping it dependency-free
    /// keeps the trusted surface of a security-critical crate smaller.
    pub fn from_uri(uri: &str) -> Result<Self> {
        let rest = uri
            .strip_prefix("otpauth://totp/")
            .ok_or_else(|| anyhow!("not an otpauth TOTP URI"))?;

        let (label, query) = match rest.split_once('?') {
            Some((label, query)) => (label, query),
            None => (rest, ""),
        };

        let mut secret_b32 = None;
        let mut algorithm = TotpAlgorithm::Sha1;
        let mut digits = 6u32;
        let mut period = 30u64;
        let mut issuer = None;

        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| anyhow!("malformed otpauth parameter: {pair}"))?;
            match key.to_ascii_lowercase().as_str() {
                "secret" => secret_b32 = Some(value.to_string()),
                "algorithm" => algorithm = TotpAlgorithm::parse(value)?,
                "digits" => {
                    digits = value.parse().context("otpauth digits")?;
                }
                "period" => {
                    period = value.parse().context("otpauth period")?;
                }
                "issuer" => issuer = Some(percent_decode(value)),
                // Unknown parameters are ignored, not rejected: the spec allows
                // vendor extensions and a stricter reader would break on them.
                _ => {}
            }
        }

        let secret_b32 = secret_b32.ok_or_else(|| anyhow!("otpauth URI has no secret"))?;

        // The label is `Issuer:Account` or just `Account`. The issuer query
        // parameter wins when both are present, matching the spec's guidance.
        let label = percent_decode(label);
        let (label_issuer, account) = match label.split_once(':') {
            Some((iss, acct)) => (Some(iss.trim().to_string()), Some(acct.trim().to_string())),
            None if !label.is_empty() => (None, Some(label)),
            None => (None, None),
        };

        let mut totp = Self::from_base32(&secret_b32, algorithm, digits, period)?;
        totp.issuer = issuer.or(label_issuer);
        totp.account = account;
        Ok(totp)
    }

    /// The code valid at a given Unix time. Split out from [`Self::now`] so tests
    /// can pin the clock to the RFC's published vectors.
    pub fn code_at(&self, unix_seconds: u64) -> TotpCode {
        let counter = unix_seconds / self.period;
        let code = self.hotp(counter);
        let seconds_remaining = self.period - (unix_seconds % self.period);
        TotpCode {
            code,
            seconds_remaining,
            period: self.period,
        }
    }

    /// The code valid right now.
    pub fn now(&self) -> TotpCode {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is before 1970")
            .as_secs();
        self.code_at(now)
    }

    /// HOTP (RFC 4226): HMAC the counter, then dynamic-truncate to N digits.
    fn hotp(&self, counter: u64) -> String {
        let msg = counter.to_be_bytes();
        let digest = match self.algorithm {
            TotpAlgorithm::Sha1 => hmac_digest::<Hmac<sha1::Sha1>>(&self.secret, &msg),
            TotpAlgorithm::Sha256 => hmac_digest::<Hmac<sha2::Sha256>>(&self.secret, &msg),
            TotpAlgorithm::Sha512 => hmac_digest::<Hmac<sha2::Sha512>>(&self.secret, &msg),
        };

        // Dynamic truncation, straight from RFC 4226 §5.3.
        let offset = (digest[digest.len() - 1] & 0x0f) as usize;
        let binary = ((u32::from(digest[offset]) & 0x7f) << 24)
            | (u32::from(digest[offset + 1]) << 16)
            | (u32::from(digest[offset + 2]) << 8)
            | u32::from(digest[offset + 3]);

        let modulo = 10u32.pow(self.digits);
        format!("{:0width$}", binary % modulo, width = self.digits as usize)
    }

    /// Re-serialize to an `otpauth://` URI, for exporting a stored TOTP back out
    /// (a QR code the user can move to another device).
    pub fn to_uri(&self) -> String {
        let label = match (&self.issuer, &self.account) {
            (Some(iss), Some(acct)) => format!("{iss}:{acct}"),
            (None, Some(acct)) => acct.clone(),
            _ => "Consistent".to_string(),
        };
        let secret = data_encoding::BASE32_NOPAD.encode(&self.secret);
        let mut uri = format!(
            "otpauth://totp/{}?secret={secret}&algorithm={}&digits={}&period={}",
            percent_encode(&label),
            self.algorithm.label(),
            self.digits,
            self.period,
        );
        if let Some(issuer) = &self.issuer {
            uri.push_str(&format!("&issuer={}", percent_encode(issuer)));
        }
        uri
    }
}

// Generic over the HMAC hash so `hotp` can pick SHA-1/256/512 at the call site.
fn hmac_digest<M: Mac + hmac::digest::KeyInit>(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac = <M as hmac::digest::KeyInit>::new_from_slice(key)
        .expect("HMAC accepts a key of any length");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

/// Decode a base32 secret the forgiving way real ones arrive: any case, spaces
/// stripped, padding optional.
fn decode_base32(secret: &str) -> Result<Vec<u8>> {
    let cleaned: String = secret
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect::<String>()
        .to_ascii_uppercase();
    if cleaned.is_empty() {
        bail!("empty TOTP secret");
    }
    // Services vary on whether they pad; accept both by trying no-pad first and
    // then trimming to a valid length.
    data_encoding::BASE32_NOPAD
        .decode(cleaned.trim_end_matches('=').as_bytes())
        .map_err(|e| anyhow!("TOTP secret is not valid base32: {e}"))
}

/// Minimal percent-decoding for URI labels and issuers. Full URL parsing is not
/// worth a dependency here; `%XX` and `+` are the only escapes these fields use.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// Inverse of `percent_decode`, for round-tripping labels back into a URI.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The RFC 6238 Appendix B seed: the ASCII string "12345678901234567890".
    const RFC_SECRET_B32: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

    fn rfc_totp() -> Totp {
        // The published vectors are 8-digit SHA-1.
        Totp::from_base32(RFC_SECRET_B32, TotpAlgorithm::Sha1, 8, 30).unwrap()
    }

    #[test]
    fn matches_the_rfc_6238_test_vectors() {
        // Appendix B, the SHA-1 column. If any of these drift, real 2FA breaks.
        let totp = rfc_totp();
        for (time, expected) in [
            (59u64, "94287082"),
            (1111111109, "07081804"),
            (1111111111, "14050471"),
            (1234567890, "89005924"),
            (2000000000, "69279037"),
            (20000000000, "65353130"),
        ] {
            assert_eq!(totp.code_at(time).code, expected, "at t={time}");
        }
    }

    #[test]
    fn seconds_remaining_counts_down_within_the_step() {
        let totp = rfc_totp();
        // 30-second step: at t=59 we are 29s in, so 1s remains.
        assert_eq!(totp.code_at(59).seconds_remaining, 1);
        // At the top of a step, the full period remains.
        assert_eq!(totp.code_at(60).seconds_remaining, 30);
        assert_eq!(totp.code_at(75).seconds_remaining, 15);
    }

    #[test]
    fn a_bare_base32_secret_defaults_to_the_common_parameters() {
        // What a service that just prints a secret gives you: 6-digit SHA-1/30s.
        let totp = Totp::from_stored_secret(RFC_SECRET_B32).unwrap();
        assert_eq!(totp.digits, 6);
        assert_eq!(totp.period, 30);
        assert_eq!(totp.algorithm, TotpAlgorithm::Sha1);
        assert_eq!(totp.now().code.len(), 6);
    }

    #[test]
    fn base32_is_read_forgivingly() {
        // Lowercase, spaced, and unpadded must all decode to the same code as the
        // canonical form — this is how users actually paste secrets.
        let canonical = rfc_totp().code_at(59).code;
        for variant in [
            "gezd gnbv gy3t qojq gezd gnbv gy3t qojq",
            "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ",
            "gezdgnbvgy3tqojqgezdgnbvgy3tqojq",
        ] {
            let totp = Totp::from_base32(variant, TotpAlgorithm::Sha1, 8, 30).unwrap();
            assert_eq!(totp.code_at(59).code, canonical, "variant {variant:?}");
        }
    }

    #[test]
    fn parses_a_full_otpauth_uri() {
        let uri = "otpauth://totp/GitHub:ahmad?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ\
                   &issuer=GitHub&algorithm=SHA1&digits=6&period=30";
        let totp = Totp::from_uri(uri).unwrap();
        assert_eq!(totp.issuer.as_deref(), Some("GitHub"));
        assert_eq!(totp.account.as_deref(), Some("ahmad"));
        assert_eq!(totp.digits, 6);
        assert_eq!(totp.now().code.len(), 6);
    }

    #[test]
    fn from_stored_secret_accepts_a_uri_too() {
        // A user might paste either form into the same field.
        let uri =
            "otpauth://totp/Example:alice?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&issuer=Example";
        let totp = Totp::from_stored_secret(uri).unwrap();
        assert_eq!(totp.issuer.as_deref(), Some("Example"));
    }

    #[test]
    fn a_uri_round_trips_through_to_uri() {
        let uri =
            "otpauth://totp/Example:alice?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&issuer=Example";
        let totp = Totp::from_uri(uri).unwrap();
        let reparsed = Totp::from_uri(&totp.to_uri()).unwrap();
        // The generated code is what actually has to survive the round trip.
        assert_eq!(reparsed.code_at(59).code, totp.code_at(59).code);
        assert_eq!(reparsed.issuer, totp.issuer);
        assert_eq!(reparsed.account, totp.account);
    }

    #[test]
    fn nonsense_secrets_are_refused_rather_than_producing_a_bogus_code() {
        assert!(Totp::from_stored_secret("").is_err());
        assert!(Totp::from_stored_secret("   ").is_err());
        assert!(Totp::from_base32("not!valid!base32!", TotpAlgorithm::Sha1, 6, 30).is_err());
        assert!(Totp::from_base32(RFC_SECRET_B32, TotpAlgorithm::Sha1, 9, 30).is_err());
        assert!(Totp::from_base32(RFC_SECRET_B32, TotpAlgorithm::Sha1, 6, 0).is_err());
    }
}
