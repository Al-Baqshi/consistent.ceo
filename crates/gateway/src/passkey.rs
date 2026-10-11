//! Passkey (WebAuthn / FIDO2) support via `webauthn-rs`.
//!
//! Registration is authenticated (you add a passkey to your existing account);
//! login is public (username → challenge → assertion → session). Short-lived
//! ceremony state (`PasskeyRegistration` / `PasskeyAuthentication`) is kept in
//! memory keyed by an opaque state id; long-lived credentials (`Passkey`) are
//! persisted per user by the auth store as JSON.
//!
//! DEPLOYMENT: the RP id must be the domain the apps are associated with
//! (`PASSKEY_RP_ID`, default `localhost`) and the origin must match
//! (`PASSKEY_RP_ORIGIN`, default `http://localhost:18800`). Mobile passkeys also
//! need apple-app-site-association (iOS) / assetlinks.json (Android) served at
//! that domain over HTTPS, with the app's associated-domains entitlement.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use dashmap::DashMap;
use webauthn_rs::prelude::*;

/// Ceremony state older than this (seconds) is swept.
const STATE_TTL_SECS: i64 = 300;

/// Holds the `webauthn-rs` relying-party config plus the short-lived ceremony
/// state for in-flight registrations and logins. Long-lived credentials are
/// not kept here — those live in `AuthStore`.
pub struct PasskeyService {
    webauthn: Webauthn,
    reg_states: DashMap<String, (PasskeyRegistration, i64)>,
    auth_states: DashMap<String, AuthCeremony>,
}

/// An in-flight login ceremony.
///
/// `user_id` is the whole point: the challenge was issued for one specific
/// account, and the finish step must take the user from *here* rather than from
/// anything the client sends alongside the assertion. Before this, the assertion
/// was verified against one user's credentials and the session minted for
/// whatever username the request body named.
struct AuthCeremony {
    state: PasskeyAuthentication,
    user_id: String,
    started: i64,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Ceremony state that has outlived [`STATE_TTL_SECS`]. Checked at finish, not
/// only in the sweep: `sweep()` runs from `start_*`, so on a quiet gateway a
/// challenge could sit valid indefinitely simply because nobody started a new one.
fn expired(started: i64) -> bool {
    now() - started > STATE_TTL_SECS
}

impl PasskeyService {
    pub fn new(rp_id: &str, rp_origin: &str) -> Result<Arc<Self>> {
        let origin =
            Url::parse(rp_origin).map_err(|e| anyhow!("invalid PASSKEY_RP_ORIGIN: {e}"))?;
        let webauthn = WebauthnBuilder::new(rp_id, &origin)
            .map_err(|e| anyhow!("webauthn builder: {e}"))?
            .rp_name("Consistent")
            .build()
            .map_err(|e| anyhow!("webauthn build: {e}"))?;
        Ok(Arc::new(Self {
            webauthn,
            reg_states: DashMap::new(),
            auth_states: DashMap::new(),
        }))
    }

    // Drop ceremony state older than the TTL. Called from every `start_*`, so
    // the maps stay bounded without a background task.
    fn sweep(&self) {
        let cutoff = now() - STATE_TTL_SECS;
        self.reg_states.retain(|_, (_, t)| *t >= cutoff);
        self.auth_states.retain(|_, c| c.started >= cutoff);
    }

    /// Begin adding a passkey to `user_id`'s account. `existing` are the user's
    /// current passkeys (excluded so the same authenticator isn't re-enrolled).
    pub fn start_register(
        &self,
        user_id: &str,
        username: &str,
        existing: &[Passkey],
    ) -> Result<(CreationChallengeResponse, String)> {
        self.sweep();
        let uuid = Uuid::parse_str(user_id).unwrap_or_else(|_| Uuid::new_v4());
        let exclude: Vec<CredentialID> = existing.iter().map(|p| p.cred_id().clone()).collect();
        let exclude = if exclude.is_empty() {
            None
        } else {
            Some(exclude)
        };
        let (ccr, state) = self
            .webauthn
            .start_passkey_registration(uuid, username, username, exclude)
            .map_err(|e| anyhow!("start registration: {e}"))?;
        let state_id = Uuid::new_v4().simple().to_string();
        self.reg_states.insert(state_id.clone(), (state, now()));
        Ok((ccr, state_id))
    }

    /// Verify the registration ceremony started by [`PasskeyService::start_register`]
    /// and return the new credential for the caller to persist.
    pub fn finish_register(
        &self,
        state_id: &str,
        cred: &RegisterPublicKeyCredential,
    ) -> Result<Passkey> {
        let (_, (state, started)) = self
            .reg_states
            .remove(state_id)
            .ok_or_else(|| anyhow!("unknown or expired registration state"))?;
        if expired(started) {
            return Err(anyhow!("registration challenge expired"));
        }
        self.webauthn
            .finish_passkey_registration(cred, &state)
            .map_err(|e| anyhow!("finish registration: {e}"))
    }

    /// Begin a login ceremony for one specific user. `passkeys` must be that
    /// user's credentials — the pair travels together into the ceremony state so
    /// the finish step cannot be pointed at a different account.
    pub fn start_auth(
        &self,
        user_id: &str,
        passkeys: &[Passkey],
    ) -> Result<(RequestChallengeResponse, String)> {
        self.sweep();
        let (rcr, state) = self
            .webauthn
            .start_passkey_authentication(passkeys)
            .map_err(|e| anyhow!("start authentication: {e}"))?;
        let state_id = Uuid::new_v4().simple().to_string();
        self.auth_states.insert(
            state_id.clone(),
            AuthCeremony {
                state,
                user_id: user_id.to_string(),
                started: now(),
            },
        );
        Ok((rcr, state_id))
    }

    /// Verify an assertion, returning the id of the user the challenge was
    /// issued for together with the result. Callers must use *this* user id and
    /// must check the returned credential id against that user's stored
    /// credentials — see `auth_api::passkey_login_finish`.
    pub fn finish_auth(
        &self,
        state_id: &str,
        cred: &PublicKeyCredential,
    ) -> Result<(String, AuthenticationResult)> {
        let (_, ceremony) = self
            .auth_states
            .remove(state_id)
            .ok_or_else(|| anyhow!("unknown or expired authentication state"))?;
        if expired(ceremony.started) {
            return Err(anyhow!("authentication challenge expired"));
        }
        let result = self
            .webauthn
            .finish_passkey_authentication(cred, &ceremony.state)
            .map_err(|e| anyhow!("finish authentication: {e}"))?;
        Ok((ceremony.user_id, result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc() -> Arc<PasskeyService> {
        PasskeyService::new("localhost", "http://localhost:18800").expect("build")
    }

    /// The property that finding 1 was about: a login ceremony belongs to one
    /// account, chosen when the challenge is issued.
    ///
    /// `finish_auth` reads the user out of this state, and its caller has no
    /// username to read instead — so an attacker signing their own challenge
    /// with their own passkey gets a session as themselves, not as whoever they
    /// name. The end-to-end version of this test needs a software authenticator
    /// to mint a real assertion; the binding it would exercise is the one
    /// asserted here.
    #[test]
    fn a_login_ceremony_is_bound_to_the_user_it_was_issued_for() {
        let pk = svc();
        let (_, alice_state) = pk.start_auth("alice-id", &[]).expect("start for alice");
        let (_, bob_state) = pk.start_auth("bob-id", &[]).expect("start for bob");

        assert_ne!(alice_state, bob_state);
        assert_eq!(
            pk.auth_states.get(&alice_state).unwrap().user_id,
            "alice-id"
        );
        assert_eq!(pk.auth_states.get(&bob_state).unwrap().user_id, "bob-id");
    }

    #[test]
    fn an_unknown_state_id_is_refused() {
        let pk = svc();
        assert!(pk.auth_states.get("no-such-ceremony").is_none());
    }

    /// The TTL was declared but never enforced at finish: `sweep()` only ran
    /// from `start_*`, so on a quiet gateway a challenge stayed valid for as
    /// long as nobody started another one.
    #[test]
    fn a_stale_challenge_is_refused_at_finish() {
        let pk = svc();
        let (_, state_id) = pk.start_auth("alice-id", &[]).expect("start");
        assert!(!expired(pk.auth_states.get(&state_id).unwrap().started));

        // Age it past the TTL without touching anything else.
        pk.auth_states.get_mut(&state_id).unwrap().started -= STATE_TTL_SECS + 1;
        let stale = pk.auth_states.get(&state_id).unwrap().started;
        assert!(expired(stale));

        // `sweep` runs from `start_*`, so a second ceremony evicts it too.
        let _ = pk.start_auth("someone-else", &[]);
        assert!(pk.auth_states.get(&state_id).is_none());
    }
}
