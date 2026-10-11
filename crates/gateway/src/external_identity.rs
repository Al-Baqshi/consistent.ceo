//! Optional third way to authenticate: an EdDSA-signed JWT from an external
//! issuer, verified against its JWKS and mapped onto a local account.
//!
//! # This is additive, and off
//!
//! Local username + password is the product. It works offline, it works when a
//! stranger self-hosts this on a box in their cupboard, and nothing here may
//! change that. This module is inert unless `[gateway.auth.external] issuer` is
//! configured, and a user who never touches a cloud account is unaffected by
//! every line of it.
//!
//! The gateway held exactly this shape once, when `ClerkVerifier` existed; the
//! difference is that the identity is now *mapped onto* a local `auth_users`
//! row rather than replacing one. Everything downstream — entities, missions,
//! the vault — keys off a local user id, and this is the seam that produces one.
//!
//! # What is actually checked
//!
//! `iss`, `aud`, `exp`, and a `kid` that resolves to a key in the issuer's JWKS.
//! The signing algorithm comes from the **JWK**, never from the token's own
//! header: a header saying `alg` is an attacker-controlled field, and trusting
//! it is how "verify this RS256 token with the HMAC of the public key" attacks
//! work. A key the JWKS does not vouch for verifies nothing.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use tokio::sync::RwLock;

/// How long a fetched JWKS is trusted before a scheduled refresh.
const JWKS_TTL: Duration = Duration::from_secs(30 * 60);
/// Floor between refetches triggered by an unknown `kid`, so a spray of tokens
/// with junk key ids cannot turn into a request flood against the issuer.
const UNKNOWN_KID_COOLDOWN: Duration = Duration::from_secs(60);
/// Clock skew allowed on `exp`. Enough for two NTP-synced machines to disagree,
/// short enough that an expired token is actually expired.
const LEEWAY_SECS: u64 = 30;

/// What identifies the trusted issuer and how to reach its keys. Presence of
/// this (vs. `None`) is what turns external-identity login on for a deployment.
#[derive(Debug, Clone)]
pub struct ExternalIdentityConfig {
    /// The `iss` claim to require, e.g. `https://id.consistent.ceo`.
    pub issuer: String,
    /// The `aud` claim to require. Strongly recommended: without it, a token
    /// this issuer minted for a different service is accepted here.
    pub audience: Option<String>,
    /// Where the signing keys live. Defaults to the issuer's
    /// `/.well-known/jwks.json`.
    pub jwks_url: String,
}

impl ExternalIdentityConfig {
    pub fn new(issuer: String, audience: Option<String>, jwks_url: Option<String>) -> Self {
        let jwks_url = jwks_url
            .unwrap_or_else(|| format!("{}/.well-known/jwks.json", issuer.trim_end_matches('/')));
        Self {
            issuer,
            audience,
            jwks_url,
        }
    }
}

/// A verified external identity, before it is mapped to a local account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalIdentity {
    /// The issuer's own id for this person. Stable, opaque, and the only field
    /// the local mapping keys off.
    pub subject: String,
    pub issuer: String,
    /// A display hint, if the issuer sent one. Never used for identity.
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
    iss: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    preferred_username: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct Jwks {
    keys: Vec<Jwk>,
}

/// The JWK fields this accepts. `OKP`/Ed25519 is what the issuer signs with;
/// RSA is here because a JWKS in the wild usually carries both and refusing to
/// parse the rest of the document would be brittle.
#[derive(Debug, Deserialize, Clone)]
struct Jwk {
    kid: String,
    kty: String,
    #[serde(default)]
    alg: Option<String>,
    #[serde(default)]
    crv: Option<String>,
    /// Ed25519 public key (OKP).
    #[serde(default)]
    x: Option<String>,
    /// RSA modulus / exponent.
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
}

/// A usable key: the material, plus the algorithm *the issuer says it is for*.
struct VerifyingKey {
    key: DecodingKey,
    algorithm: Algorithm,
}

impl Jwk {
    fn to_verifying_key(&self) -> Option<VerifyingKey> {
        match self.kty.as_str() {
            "OKP" if self.crv.as_deref() == Some("Ed25519") => {
                let x = self.x.as_deref()?;
                Some(VerifyingKey {
                    key: DecodingKey::from_ed_components(x).ok()?,
                    algorithm: Algorithm::EdDSA,
                })
            }
            "RSA" => {
                let (n, e) = (self.n.as_deref()?, self.e.as_deref()?);
                let algorithm = match self.alg.as_deref() {
                    Some("RS384") => Algorithm::RS384,
                    Some("RS512") => Algorithm::RS512,
                    // `alg` is optional on a JWK; RS256 is the near-universal
                    // default for an RSA signing key.
                    _ => Algorithm::RS256,
                };
                Some(VerifyingKey {
                    key: DecodingKey::from_rsa_components(n, e).ok()?,
                    algorithm,
                })
            }
            _ => None,
        }
    }
}

/// The issuer's JWKS, indexed by `kid`, plus enough bookkeeping to decide when
/// a refetch is due.
struct Cache {
    keys: HashMap<String, VerifyingKey>,
    fetched_at: Instant,
    /// Last refetch prompted by a `kid` we didn't have.
    last_unknown_kid_fetch: Option<Instant>,
}

/// Verifies bearer tokens against one configured external issuer. One
/// instance per deployment (there is exactly one `[gateway.auth.external]`
/// issuer, not a list).
pub struct ExternalVerifier {
    config: ExternalIdentityConfig,
    http: reqwest::Client,
    cache: RwLock<Option<Cache>>,
}

/// Cheap check for "this bearer is a JWT, not one of our opaque tokens".
///
/// Session tokens and API keys are 64 hex characters, so they can never look
/// like this. Getting the branch wrong is not a security problem — a token that
/// takes the wrong path simply fails to verify — but it keeps the JWT path from
/// doing work on every ordinary request.
pub fn looks_like_jwt(token: &str) -> bool {
    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(s), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    !h.is_empty() && !p.is_empty() && !s.is_empty()
}

impl ExternalVerifier {
    pub fn new(config: ExternalIdentityConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            cache: RwLock::new(None),
        }
    }

    /// Verify a bearer as an external JWT. `Err` means "not authenticated by
    /// this path" and callers fall through to the others.
    pub async fn verify(&self, token: &str) -> Result<ExternalIdentity> {
        let header = decode_header(token).map_err(|e| anyhow!("bad JWT header: {e}"))?;
        let kid = header
            .kid
            .ok_or_else(|| anyhow!("JWT has no kid; cannot choose a signing key"))?;

        self.ensure_keys(&kid).await?;

        let guard = self.cache.read().await;
        let cache = guard.as_ref().ok_or_else(|| anyhow!("no JWKS loaded"))?;
        let key = cache
            .keys
            .get(&kid)
            .ok_or_else(|| anyhow!("kid {kid} is not in the issuer's JWKS"))?;

        // The algorithm is the JWK's, not the token header's.
        let mut validation = Validation::new(key.algorithm);
        validation.set_issuer(&[&self.config.issuer]);
        match self.config.audience.as_deref() {
            Some(aud) => validation.set_audience(&[aud]),
            // `jsonwebtoken` skips the audience check when none is set, but
            // still rejects on `exp` — which is the claim that matters most.
            None => validation.validate_aud = false,
        }
        validation.validate_exp = true;
        // Explicit, not inherited. `jsonwebtoken` defaults to 60 seconds of
        // slack on `exp`, which is a real decision about how long a revoked or
        // expired token keeps working and should be visible in the code that
        // makes it. Thirty seconds covers ordinary clock drift between two
        // machines that both run NTP.
        validation.leeway = LEEWAY_SECS;

        let data = decode::<Claims>(token, &key.key, &validation)
            .map_err(|e| anyhow!("JWT rejected: {e}"))?;
        if data.claims.sub.trim().is_empty() {
            bail!("JWT has an empty subject");
        }
        Ok(ExternalIdentity {
            subject: data.claims.sub,
            issuer: data.claims.iss,
            name: data.claims.name.or(data.claims.preferred_username),
        })
    }

    /// Make sure `kid` is loaded, fetching or refreshing the JWKS if needed.
    async fn ensure_keys(&self, kid: &str) -> Result<()> {
        let now = Instant::now();
        {
            let guard = self.cache.read().await;
            if let Some(cache) = guard.as_ref() {
                let fresh = now.duration_since(cache.fetched_at) < JWKS_TTL;
                if fresh && cache.keys.contains_key(kid) {
                    return Ok(());
                }
                // An unknown kid usually means the issuer rotated. Refetching is
                // right, but only at a bounded rate.
                if fresh
                    && let Some(last) = cache.last_unknown_kid_fetch
                    && now.duration_since(last) < UNKNOWN_KID_COOLDOWN
                {
                    bail!("kid {kid} is unknown and the JWKS was refreshed recently");
                }
            }
        }
        self.refresh(kid).await
    }

    // Fetch the JWKS and replace the cache wholesale. `requested_kid` is only
    // used to record whether this refetch actually found the key that
    // triggered it, for `last_unknown_kid_fetch`'s cooldown logic.
    async fn refresh(&self, requested_kid: &str) -> Result<()> {
        let res = self
            .http
            .get(&self.config.jwks_url)
            .send()
            .await
            .map_err(|e| anyhow!("fetch JWKS: {e}"))?;
        if !res.status().is_success() {
            bail!("JWKS endpoint returned {}", res.status());
        }
        let jwks: Jwks = res.json().await.map_err(|e| anyhow!("parse JWKS: {e}"))?;
        let keys = Self::index(jwks);
        if keys.is_empty() {
            bail!("issuer's JWKS has no key this gateway can use");
        }
        tracing::info!(
            keys = keys.len(),
            url = %self.config.jwks_url,
            "loaded external identity signing keys"
        );
        let had_requested = keys.contains_key(requested_kid);
        let mut guard = self.cache.write().await;
        *guard = Some(Cache {
            keys,
            fetched_at: Instant::now(),
            last_unknown_kid_fetch: (!had_requested).then(Instant::now),
        });
        Ok(())
    }

    // Keep only the keys this verifier knows how to use; an unusable JWK
    // (unsupported `kty`, missing fields) is logged and dropped rather than
    // failing the whole JWKS.
    fn index(jwks: Jwks) -> HashMap<String, VerifyingKey> {
        jwks.keys
            .into_iter()
            .filter_map(|jwk| {
                let kid = jwk.kid.clone();
                match jwk.to_verifying_key() {
                    Some(k) => Some((kid, k)),
                    None => {
                        tracing::debug!(kid, kty = jwk.kty, "skipping unusable JWK");
                        None
                    }
                }
            })
            .collect()
    }

    /// Seed the key cache directly. Tests use this to exercise verification
    /// without an HTTP round trip.
    #[cfg(test)]
    async fn install_keys(&self, jwks_json: &str) {
        let jwks: Jwks = serde_json::from_str(jwks_json).expect("jwks");
        let mut guard = self.cache.write().await;
        *guard = Some(Cache {
            keys: Self::index(jwks),
            fetched_at: Instant::now(),
            last_unknown_kid_fetch: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
    use jsonwebtoken::{EncodingKey, Header};
    use ring::signature::KeyPair;
    use serde::Serialize;

    #[derive(Serialize)]
    struct TestClaims {
        sub: String,
        iss: String,
        aud: String,
        exp: i64,
    }

    struct Issuer {
        encoding: EncodingKey,
        jwks: String,
        kid: String,
    }

    /// A real Ed25519 keypair, its JWKS, and the means to sign with it.
    fn issuer(kid: &str) -> Issuer {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let public = B64.encode(pair.public_key().as_ref());
        Issuer {
            encoding: EncodingKey::from_ed_der(pkcs8.as_ref()),
            jwks: format!(
                r#"{{"keys":[{{"kid":"{kid}","kty":"OKP","crv":"Ed25519","alg":"EdDSA","x":"{public}"}}]}}"#
            ),
            kid: kid.to_string(),
        }
    }

    fn sign(issuer: &Issuer, claims: &TestClaims) -> String {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(issuer.kid.clone());
        jsonwebtoken::encode(&header, claims, &issuer.encoding).unwrap()
    }

    fn claims(iss: &str, aud: &str, ttl_secs: i64) -> TestClaims {
        TestClaims {
            sub: "user_abc123".to_string(),
            iss: iss.to_string(),
            aud: aud.to_string(),
            exp: chrono::Utc::now().timestamp() + ttl_secs,
        }
    }

    async fn verifier(iss: &str, aud: Option<&str>, jwks: &str) -> ExternalVerifier {
        let v = ExternalVerifier::new(ExternalIdentityConfig::new(
            iss.to_string(),
            aud.map(str::to_string),
            None,
        ));
        v.install_keys(jwks).await;
        v
    }

    #[tokio::test]
    async fn a_properly_signed_token_verifies() {
        let iss = issuer("key-1");
        let v = verifier("https://id.example", Some("consistent"), &iss.jwks).await;
        let token = sign(&iss, &claims("https://id.example", "consistent", 300));

        let identity = v.verify(&token).await.expect("verifies");
        assert_eq!(identity.subject, "user_abc123");
        assert_eq!(identity.issuer, "https://id.example");
    }

    #[tokio::test]
    async fn a_token_from_another_issuer_is_refused() {
        let iss = issuer("key-1");
        let v = verifier("https://id.example", Some("consistent"), &iss.jwks).await;
        // Correctly signed by this key, but claiming a different issuer.
        let token = sign(
            &iss,
            &claims("https://id.somewhere-else", "consistent", 300),
        );
        assert!(v.verify(&token).await.is_err());
    }

    #[tokio::test]
    async fn a_token_for_another_audience_is_refused() {
        let iss = issuer("key-1");
        let v = verifier("https://id.example", Some("consistent"), &iss.jwks).await;
        // The issuer minted this for a different service of its own.
        let token = sign(&iss, &claims("https://id.example", "some-other-app", 300));
        assert!(v.verify(&token).await.is_err());
    }

    #[tokio::test]
    async fn an_expired_token_is_refused() {
        let iss = issuer("key-1");
        let v = verifier("https://id.example", Some("consistent"), &iss.jwks).await;
        // Comfortably past the clock-skew allowance, which the test below pins.
        let token = sign(&iss, &claims("https://id.example", "consistent", -300));
        assert!(v.verify(&token).await.is_err());
    }

    #[tokio::test]
    async fn a_token_signed_by_a_key_the_issuer_does_not_vouch_for_is_refused() {
        let real = issuer("key-1");
        // Same kid, different keypair — an attacker who knows the key id.
        let forger = issuer("key-1");
        let v = verifier("https://id.example", Some("consistent"), &real.jwks).await;
        let token = sign(&forger, &claims("https://id.example", "consistent", 300));
        assert!(v.verify(&token).await.is_err());
    }

    #[tokio::test]
    async fn a_tampered_payload_is_refused() {
        let iss = issuer("key-1");
        let v = verifier("https://id.example", Some("consistent"), &iss.jwks).await;
        let token = sign(&iss, &claims("https://id.example", "consistent", 300));

        let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
        let mut payload: serde_json::Value =
            serde_json::from_slice(&B64.decode(&parts[1]).unwrap()).unwrap();
        payload["sub"] = serde_json::json!("somebody_else");
        parts[1] = B64.encode(serde_json::to_vec(&payload).unwrap());
        assert!(v.verify(&parts.join(".")).await.is_err());
    }

    /// A `kid` the JWKS never listed must not fall back to "try every key".
    #[tokio::test]
    async fn an_unknown_kid_is_refused_without_a_network_call() {
        let iss = issuer("key-1");
        let v = verifier("https://id.example", Some("consistent"), &iss.jwks).await;
        let other = issuer("key-999");
        let token = sign(&other, &claims("https://id.example", "consistent", 300));
        // The JWKS URL is unreachable in tests, so a refetch attempt errors —
        // either way this must not authenticate.
        assert!(v.verify(&token).await.is_err());
    }

    #[test]
    fn opaque_session_tokens_are_not_mistaken_for_jwts() {
        // What `create_session` mints: 64 hex characters.
        let opaque = "a".repeat(64);
        assert!(!looks_like_jwt(&opaque));
        assert!(!looks_like_jwt("secret.workspace.principal.extra"));
        assert!(!looks_like_jwt(""));
        assert!(!looks_like_jwt("a.b"));
        assert!(!looks_like_jwt("a..c"));
        // The scoped static bearer is three dot-separated parts too, which is
        // exactly why a wrong guess here must stay harmless: it fails to verify
        // and falls through to the static path.
        assert!(looks_like_jwt("secret.workspace.principal"));
    }

    #[test]
    fn jwks_url_defaults_to_the_well_known_path() {
        let c = ExternalIdentityConfig::new("https://id.example/".into(), None, None);
        assert_eq!(c.jwks_url, "https://id.example/.well-known/jwks.json");

        let explicit = ExternalIdentityConfig::new(
            "https://id.example".into(),
            None,
            Some("https://k".into()),
        );
        assert_eq!(explicit.jwks_url, "https://k");
    }

    #[test]
    fn an_rsa_jwk_without_alg_reads_as_rs256() {
        let jwk = Jwk {
            kid: "k".into(),
            kty: "RSA".into(),
            alg: None,
            crv: None,
            x: None,
            n: Some(B64.encode([1u8; 256])),
            e: Some(B64.encode([1, 0, 1])),
        };
        assert_eq!(
            jwk.to_verifying_key().map(|k| k.algorithm),
            Some(Algorithm::RS256)
        );
    }
}
