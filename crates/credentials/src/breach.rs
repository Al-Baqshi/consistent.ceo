//! Checking passwords against Have I Been Pwned, without telling them anything.
//!
//! HIBP's Pwned Passwords set holds ~1 billion passwords that have appeared in
//! real breaches. Checking against it is the single most useful safety signal a
//! password manager can offer — a strong-*looking* password that is already in a
//! breach corpus is worthless.
//!
//! # k-anonymity: the query leaks nothing
//!
//! The naive version — "send us your password's hash, we'll tell you if it's
//! breached" — hands a third party a hash of every password you own. HIBP's range
//! API avoids this: you SHA-1 the password, send only the **first five hex
//! characters** of the hash, and get back every breached suffix sharing that
//! prefix (typically a few hundred to a thousand). You match your own suffix
//! locally. HIBP never learns which password you asked about, only that you asked
//! about *one of* ~500 possibilities.
//!
//! This module is the pure, offline half — hashing and response parsing, both
//! deterministic and unit-testable. The HTTP call itself lives in the gateway
//! (`vault_api`), which already has an HTTP client and is the only layer allowed
//! to reach the network.

use sha1::{Digest, Sha1};

/// The prefix length HIBP's range API expects.
pub const PREFIX_LEN: usize = 5;

/// SHA-1 a password and split the uppercase hex digest into the 5-char prefix
/// sent to HIBP and the 35-char suffix matched locally.
///
/// SHA-1 is not a security choice here and its weaknesses do not matter: it is
/// the fixed protocol HIBP speaks, and the whole point is that the full hash is
/// never transmitted.
pub fn prefix_suffix(password: &str) -> (String, String) {
    let mut hasher = Sha1::new();
    hasher.update(password.as_bytes());
    let digest = hasher.finalize();

    let hex: String = digest.iter().map(|b| format!("{b:02X}")).collect();
    let (prefix, suffix) = hex.split_at(PREFIX_LEN);
    (prefix.to_string(), suffix.to_string())
}

/// Find how many times a password's suffix appears in a HIBP range response.
///
/// The body is newline-separated `SUFFIX:COUNT`. Returns `0` when the suffix is
/// absent — i.e. the password is not in the breach set. Matching is
/// case-insensitive because HIBP returns uppercase hex but robustness costs
/// nothing here.
pub fn count_in_range(range_body: &str, suffix: &str) -> u64 {
    let suffix = suffix.trim();
    for line in range_body.lines() {
        let line = line.trim();
        let Some((hash_suffix, count)) = line.split_once(':') else {
            continue;
        };
        if hash_suffix.eq_ignore_ascii_case(suffix) {
            return count.trim().parse().unwrap_or(0);
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_the_canonical_example() {
        // SHA-1("password") is a fixed, well-known value; HIBP documents this
        // exact split in its API guide.
        let (prefix, suffix) = prefix_suffix("password");
        assert_eq!(prefix, "5BAA6");
        assert_eq!(suffix, "1E4C9B93F3F0682250B6CF8331B7EE68FD8");
    }

    #[test]
    fn prefix_and_suffix_reassemble_to_a_full_sha1() {
        let (prefix, suffix) = prefix_suffix("correct horse battery staple");
        assert_eq!(prefix.len(), PREFIX_LEN);
        assert_eq!(prefix.len() + suffix.len(), 40, "SHA-1 is 40 hex chars");
        assert!(prefix.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn finds_a_breached_suffix_and_its_count() {
        let (_, suffix) = prefix_suffix("password");
        // A stand-in range response: the real one has hundreds of lines.
        let body = format!("00000000000000000000000000000000000:3\n{suffix}:9876543\nOTHER:5");
        assert_eq!(count_in_range(&body, &suffix), 9_876_543);
    }

    #[test]
    fn a_suffix_not_in_the_range_counts_as_zero() {
        let (_, suffix) = prefix_suffix("a-password-not-in-this-mock-range-xyz");
        let body = "00000000000000000000000000000000000:3\nABCDEF0123456789ABCDEF0123456789ABC:9";
        assert_eq!(count_in_range(body, &suffix), 0);
    }

    #[test]
    fn matching_ignores_case() {
        let body = "aaaabbbbccccddddeeeeffff0000111122223:42";
        assert_eq!(
            count_in_range(body, "AAAABBBBCCCCDDDDEEEEFFFF0000111122223"),
            42
        );
    }

    #[test]
    fn a_malformed_line_is_skipped_not_fatal() {
        let body = "garbage-no-colon\nABC:notanumber\nAAAA:7";
        // The bad count parses to 0; a genuine match still works.
        assert_eq!(count_in_range(body, "ABC"), 0);
        assert_eq!(count_in_range(body, "AAAA"), 7);
        assert_eq!(count_in_range("", "AAAA"), 0);
    }
}
