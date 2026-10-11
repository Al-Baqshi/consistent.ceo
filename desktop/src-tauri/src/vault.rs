//! Opening the vault with this machine, via the OS keychain.
//!
//! # Why not passkeys here
//!
//! The desktop shell loads the same web UI as the browser, and that UI unlocks
//! the vault with the WebAuthn PRF extension. Inside an embedded webview that is
//! not dependable: WKWebView needs associated-domain entitlements before it will
//! surface platform passkeys, WebView2 behaves differently again, and WebKitGTK
//! largely does not implement PRF at all. Shipping a Face-ID button that works on
//! one of three platforms is worse than not shipping one.
//!
//! So the desktop uses the same shape as mobile instead: a random 32-byte *device
//! key*, held in the macOS Keychain, Windows Credential Manager or the Linux
//! Secret Service, and enrolled as an ordinary `device-key` unlock method. The
//! vault cannot tell the difference between this and a phone — both are 32 uniform
//! bytes wrapping the same `user_key` — which is exactly why enrolling this Mac
//! does not disturb a passkey enrolled in a browser.
//!
//! # What this is worth
//!
//! The key sits behind whatever the OS keychain enforces, which on a locked
//! machine means the login keychain is locked too. It is not a biometric prompt
//! per unlock; it is "this machine, while its owner is logged in". That is a real
//! step up from typing a master password into a window on every launch, and it is
//! honest about being a lesser guarantee than the phone's `requireAuthentication`.
//!
//! Revocation is server-side: removing the unlock method makes this key open
//! nothing, whether or not it is still sitting in the keychain.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::Manager;

use crate::agent::GatewayAuth;

/// Keychain service. Internal identifiers stay `consistent-world` even though the
/// product is presented as Consistent.
const SERVICE: &str = "consistent-world-vault";
const KEY_ENTRY: &str = "device-key";
const METHOD_ENTRY: &str = "device-method-id";

/// Whether this machine is enrolled, remembered for the life of the process.
///
/// Every keychain read is a potential macOS authorization dialog: the ACL is
/// bound to the app's code signature, so any build whose signature the keychain
/// does not already trust prompts for the login password *per item, per read*.
/// The vault page polls its status every 20s, so an uncached
/// [`vault_device_status`] turned that into a password box every twenty seconds
/// until the user gave up.
///
/// Enrolment only changes through [`vault_enroll_device`] and
/// [`vault_forget_device`], both of which run in this process, so a plain cache
/// is exact rather than merely fresh — there is no third party to invalidate it.
/// `None` means "not looked yet"; `Some(v)` is what the keychain said.
static METHOD_CACHE: Mutex<Option<Option<String>>> = Mutex::new(None);

/// The enrolled method id, from cache when we have already asked.
fn cached_method_id() -> Option<String> {
    // A poisoned lock here means a previous caller panicked mid-read; the value
    // is still structurally sound, so recover rather than propagate.
    let mut guard = METHOD_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cached) = guard.as_ref() {
        return cached.clone();
    }
    let fresh = read(METHOD_ENTRY);
    *guard = Some(fresh.clone());
    fresh
}

/// Forget what we remembered, so the next status call asks the keychain again.
fn invalidate_method_cache() {
    let mut guard = METHOD_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

fn entry(name: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, name).map_err(|e| format!("keychain unavailable: {e}"))
}

fn read(name: &str) -> Option<String> {
    entry(name).ok().and_then(|e| e.get_password().ok())
}

fn write(name: &str, value: &str) -> Result<(), String> {
    entry(name)?
        .set_password(value)
        .map_err(|e| format!("could not write to the keychain: {e}"))
}

fn clear(name: &str) {
    if let Ok(e) = entry(name) {
        // Idempotent: a missing entry is not an error worth surfacing.
        let _ = e.delete_credential();
    }
}

/// Whether this machine can unlock the vault, and with which method.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStatus {
    /// Always true on desktop; the field exists so the web UI can branch on one
    /// shape across desktop, mobile and browser.
    supported: bool,
    enrolled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    method_id: Option<String>,
}

#[derive(Deserialize)]
struct RandomKeyResponse {
    #[serde(rename = "keyB64")]
    key_b64: String,
}

#[derive(Deserialize)]
struct AddMethodResponse {
    id: String,
}

/// Build a request to the local gateway, carrying the shell's bearer token.
async fn gateway(
    app: &tauri::AppHandle,
    method: reqwest::Method,
    path: &str,
) -> Result<reqwest::RequestBuilder, String> {
    let (base_url, token) = {
        let auth = app.state::<GatewayAuth>();
        (auth.base_url.clone(), auth.token())
    };
    let mut req = reqwest::Client::new().request(method, format!("{base_url}{path}"));
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    Ok(req)
}

/// Turn a gateway response into either its JSON body or a useful message.
async fn json_or_error<T: serde::de::DeserializeOwned>(
    res: reqwest::Response,
) -> Result<T, String> {
    let status = res.status();
    if status.is_success() {
        return res
            .json::<T>()
            .await
            .map_err(|e| format!("unexpected response from the gateway: {e}"));
    }

    let body = res.text().await.unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_else(|| format!("gateway returned {status}"));
    Err(message)
}

/// Whether this machine is enrolled, and under which unlock method.
///
/// The caller still has to check the id against the vault's own method list —
/// a machine revoked from another device keeps its keychain entry, and offering
/// an unlock button that can only fail is worse than offering none.
#[tauri::command]
pub fn vault_device_status() -> DeviceStatus {
    // Deliberately does not read `KEY_ENTRY`. Confirming the secret exists would
    // double the number of keychain items touched — and so the number of
    // authorization prompts — to answer a question the id alone already
    // answers. The two entries are written and cleared together, and
    // `vault_unlock_with_device` already handles the key having gone missing
    // with a message that tells the user how to recover.
    let method_id = cached_method_id();
    DeviceStatus {
        supported: true,
        enrolled: method_id.is_some(),
        method_id,
    }
}

/// Enrol this machine. Requires the vault to be unlocked already — the gateway
/// rejects it otherwise, which is what stops a stolen session adding a way in.
#[tauri::command]
pub async fn vault_enroll_device(app: tauri::AppHandle, label: String) -> Result<String, String> {
    let label = if label.trim().is_empty() {
        "This computer".to_string()
    } else {
        label
    };

    // The gateway generates the bytes. It sees them at enrolment either way —
    // what matters is that they come to rest in the OS keychain.
    let res = gateway(&app, reqwest::Method::GET, "/api/vault/random-key")
        .await?
        .send()
        .await
        .map_err(|e| format!("could not reach the gateway: {e}"))?;
    let RandomKeyResponse { key_b64 } = json_or_error(res).await?;

    write(KEY_ENTRY, &key_b64)?;

    let res = gateway(&app, reqwest::Method::POST, "/api/vault/unlock-methods")
        .await?
        .json(&serde_json::json!({
            "kind": "device-key",
            "label": label,
            "key": key_b64,
        }))
        .send()
        .await
        .map_err(|e| format!("could not reach the gateway: {e}"))?;

    let added: AddMethodResponse = match json_or_error(res).await {
        Ok(v) => v,
        Err(e) => {
            // Never leave a key in the keychain that opens nothing.
            clear(KEY_ENTRY);
            return Err(e);
        }
    };

    write(METHOD_ENTRY, &added.id)?;
    invalidate_method_cache();
    Ok(added.id)
}

/// Unlock the vault with this machine's key. Returns the gateway's vault status.
#[tauri::command]
pub async fn vault_unlock_with_device(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let method_id = cached_method_id().ok_or("This computer is not enrolled.")?;
    let key = read(KEY_ENTRY).ok_or_else(|| {
        clear(METHOD_ENTRY);
        "The stored key is gone. Unlock with your master password and enrol again.".to_string()
    })?;

    let res = gateway(&app, reqwest::Method::POST, "/api/vault/unlock/key")
        .await?
        .json(&serde_json::json!({ "methodId": method_id, "key": key }))
        .send()
        .await
        .map_err(|e| format!("could not reach the gateway: {e}"))?;

    if res.status() == reqwest::StatusCode::UNAUTHORIZED {
        // The wrapping is gone: revoked from somewhere else.
        vault_forget_device();
        return Err("This computer was removed from your vault. Unlock with your password.".into());
    }

    json_or_error(res).await
}

/// Drop this machine's local key. Does not revoke it server-side.
#[tauri::command]
pub fn vault_forget_device() {
    clear(KEY_ENTRY);
    clear(METHOD_ENTRY);
    invalidate_method_cache();
}

/// Revoke this machine everywhere, then forget it locally.
#[tauri::command]
pub async fn vault_revoke_device(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(method_id) = cached_method_id() {
        let res = gateway(
            &app,
            reqwest::Method::DELETE,
            &format!("/api/vault/unlock-methods/{method_id}"),
        )
        .await?
        .send()
        .await
        .map_err(|e| format!("could not reach the gateway: {e}"))?;

        if !res.status().is_success() && res.status() != reqwest::StatusCode::NOT_FOUND {
            return Err(json_or_error::<serde_json::Value>(res)
                .await
                .unwrap_err_or_default());
        }
    }
    vault_forget_device();
    Ok(())
}

/// Small helper so the error branch above reads cleanly.
trait UnwrapErrOrDefault {
    fn unwrap_err_or_default(self) -> String;
}

impl UnwrapErrOrDefault for Result<serde_json::Value, String> {
    fn unwrap_err_or_default(self) -> String {
        self.err().unwrap_or_else(|| "unknown error".to_string())
    }
}
