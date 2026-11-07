//! Remote-access doctor: how does a phone reach this gateway from outside the
//! LAN?
//!
//! The gateway deliberately keeps binding loopback (see the guards in
//! `main.rs`); remote reachability comes from something proxying *to*
//! loopback. Today that something is Tailscale (`tailscale serve` gives an
//! HTTPS URL on the user's tailnet with zero gateway changes); a first-party
//! relay is the planned successor. This module detects what is available,
//! can switch `tailscale serve` on, and builds the pairing payload the mobile
//! app scans as a QR code.
//!
//! Everything here shells out to the `tailscale` CLI with short commands and
//! never stores state of its own — `tailscale serve --bg` persists on the
//! tailscale side across reboots.

use serde::Serialize;

/// Where the tailscale CLI hides per platform. The macOS app bundles it
/// outside PATH; the Windows installer puts it under Program Files, which a
/// process started before the install does not yet have on its PATH.
fn tailscale_candidates() -> Vec<String> {
    let mut candidates = vec!["tailscale".to_string()];
    if cfg!(windows) {
        if let Some(program_files) = std::env::var_os("ProgramFiles") {
            candidates.push(
                std::path::Path::new(&program_files)
                    .join("Tailscale")
                    .join("tailscale.exe")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    } else {
        candidates.extend(
            [
                "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
                "/usr/local/bin/tailscale",
                "/opt/homebrew/bin/tailscale",
            ]
            .map(String::from),
        );
    }
    candidates
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteStatus {
    /// Public base URL when a relay link is configured. Preferred over
    /// everything else: it reaches the gateway from any network.
    pub relay_url: Option<String>,
    pub tailscale_installed: bool,
    pub tailscale_running: bool,
    /// MagicDNS name of this machine, e.g. `mac-studio.tail1234.ts.net`.
    pub tailscale_dns_name: Option<String>,
    /// `https://…` URL when `tailscale serve` is already proxying our port.
    pub serve_url: Option<String>,
    /// Same-network fallback, e.g. `http://192.168.1.20:18800`.
    pub lan_url: Option<String>,
    /// Best reachable base URL for a phone, in preference order:
    /// relay → serve (both work anywhere) → LAN (same Wi-Fi only).
    pub recommended_url: Option<String>,
}

pub fn find_tailscale() -> Option<String> {
    for candidate in tailscale_candidates() {
        let ok = consistent_world_common::platform::std_command(&candidate)
            .arg("version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Some(candidate);
        }
    }
    None
}

fn tailscale_status_json(bin: &str) -> Option<serde_json::Value> {
    let out = consistent_world_common::platform::std_command(bin)
        .args(["status", "--json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn dns_name(status: &serde_json::Value) -> Option<String> {
    status
        .get("Self")
        .and_then(|s| s.get("DNSName"))
        .and_then(|v| v.as_str())
        .map(|v| v.trim_end_matches('.').to_string())
        .filter(|v| !v.is_empty())
}

fn backend_running(status: &serde_json::Value) -> bool {
    status
        .get("BackendState")
        .and_then(|v| v.as_str())
        .map(|v| v.eq_ignore_ascii_case("Running"))
        .unwrap_or(false)
}

/// Is `tailscale serve` already proxying to our loopback port? The serve
/// status output is human text, but the loopback target string appears in it
/// verbatim in every CLI version so far.
fn serve_proxies_port(bin: &str, port: u16) -> bool {
    let Ok(out) = consistent_world_common::platform::std_command(bin)
        .args(["serve", "status"])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.contains(&format!("127.0.0.1:{port}")) || text.contains(&format!("localhost:{port}"))
}

/// Turn `tailscale serve` on for our port, in the background (persists across
/// reboots on the tailscale side). Tries the modern argument form first, then
/// the older path-based form.
pub fn enable_serve(bin: &str, port: u16) -> anyhow::Result<()> {
    let modern = consistent_world_common::platform::std_command(bin)
        .args(["serve", "--bg", &port.to_string()])
        .output()?;
    if modern.status.success() {
        return Ok(());
    }
    let legacy = consistent_world_common::platform::std_command(bin)
        .args([
            "serve",
            "--bg",
            "https",
            "/",
            &format!("http://127.0.0.1:{port}"),
        ])
        .output()?;
    if legacy.status.success() {
        return Ok(());
    }
    anyhow::bail!(
        "tailscale serve failed: {}",
        String::from_utf8_lossy(&legacy.stderr).trim()
    )
}

/// The LAN address a phone on the same wifi can reach. The UDP "connect"
/// never sends a packet; it just asks the OS which interface would route out.
pub fn lan_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    if ip.is_loopback() {
        return None;
    }
    Some(ip.to_string())
}

/// The same-Wi-Fi URL, when something actually answers on it.
///
/// A gateway bound beyond loopback (docker, a server) answers on every
/// address, so its LAN address works. The default desktop install binds
/// 127.0.0.1: a LAN address pointing at that would send the phone to a port
/// nothing answers on, so it is offered only while the Wi-Fi listener
/// (`wifi_access`) is actually listening — and then at the address it bound.
pub fn lan_url_for(
    port: u16,
    bind: &str,
    wifi_listener: Option<std::net::SocketAddr>,
    lan_ip: impl FnOnce() -> Option<String>,
) -> Option<String> {
    if crate::billing::is_loopback_bind(bind) {
        wifi_listener.map(|addr| format!("http://{addr}"))
    } else {
        lan_ip().map(|ip| format!("http://{ip}:{port}"))
    }
}

/// `bind` is the address the gateway listens on; see [`lan_url_for`] for when
/// a same-Wi-Fi URL is offered.
pub fn remote_status(port: u16, bind: &str) -> RemoteStatus {
    let bin = find_tailscale();
    let (running, dns) = match bin.as_deref().and_then(tailscale_status_json) {
        Some(status) => (backend_running(&status), dns_name(&status)),
        None => (false, None),
    };
    let serve_url = match (&bin, running, &dns) {
        (Some(bin), true, Some(dns)) if serve_proxies_port(bin, port) => {
            Some(format!("https://{dns}"))
        }
        _ => None,
    };
    let lan_url = lan_url_for(port, bind, crate::wifi_access::listening_addr(), lan_ip);
    let relay_url = relay_public_url();
    // Preference order is reachability: a relay works from any network, a
    // tailnet from any network the operator has joined, LAN only at home.
    let recommended_url = relay_url
        .clone()
        .or_else(|| serve_url.clone())
        .or_else(|| lan_url.clone());
    RemoteStatus {
        relay_url,
        tailscale_installed: bin.is_some(),
        tailscale_running: running,
        tailscale_dns_name: dns,
        serve_url,
        lan_url,
        recommended_url,
    }
}

/// The public URL of this gateway through its relay, when one is configured.
/// Mirrors `relay_client::RelayClientConfig::from_env` so the two can never
/// disagree about whether a relay is in play.
pub fn relay_public_url() -> Option<String> {
    let base = std::env::var("CONSISTENT_WORLD_RELAY_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())?;
    let gateway_id = std::env::var("CONSISTENT_WORLD_RELAY_GATEWAY_ID")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())?;
    // The phone speaks HTTP(S) to the relay even though the gateway link is a
    // WebSocket, so normalise a ws:// URL back to http(s)://.
    let base = base
        .strip_prefix("wss://")
        .map(|rest| format!("https://{rest}"))
        .or_else(|| {
            base.strip_prefix("ws://")
                .map(|rest| format!("http://{rest}"))
        })
        .unwrap_or(base);
    let base = base.strip_suffix("/link").unwrap_or(&base).to_string();
    Some(format!("{base}/g/{gateway_id}"))
}

/// The JSON the mobile app scans out of the QR code. Never carries a secret:
/// pairing ends in the normal device-approval login, the QR only says where.
pub fn pair_payload(base_url: &str, workspace_id: &str) -> serde_json::Value {
    serde_json::json!({
        "url": base_url,
        "workspace": workspace_id,
        "v": 1,
    })
}

/// One-shot `remote setup`: make this gateway reachable from anywhere the
/// user's tailnet reaches, and say exactly what happened.
pub fn run_setup(port: u16, bind: &str) -> anyhow::Result<RemoteStatus> {
    let Some(bin) = find_tailscale() else {
        anyhow::bail!(
            "Tailscale is not installed. Install it from https://tailscale.com/download, \
             sign in, then re-run `consistent-world remote setup`."
        );
    };
    let status = tailscale_status_json(&bin);
    let running = status.as_ref().map(backend_running).unwrap_or(false);
    if !running {
        anyhow::bail!(
            "Tailscale is installed but not connected. Open Tailscale and sign in, \
             then re-run `consistent-world remote setup`."
        );
    }
    if !serve_proxies_port(&bin, port) {
        enable_serve(&bin, port)?;
    }
    Ok(remote_status(port, bind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_loopback_gateway_offers_wifi_only_while_the_wifi_listener_listens() {
        // Nothing on the network can reach 127.0.0.1, so without the Wi-Fi
        // listener a LAN URL would send the phone to a port nothing answers on.
        let any_ip = || Some("192.168.1.20".to_string());
        assert_eq!(lan_url_for(18800, "127.0.0.1", None, any_ip), None);
        assert_eq!(lan_url_for(18800, "localhost", None, any_ip), None);
        // Tests never start the listener, so the live status agrees.
        assert_eq!(remote_status(18800, "127.0.0.1").lan_url, None);

        // Listening: the URL is exactly where the listener bound.
        let addr: std::net::SocketAddr = "192.168.1.20:18800".parse().unwrap();
        assert_eq!(
            lan_url_for(18800, "127.0.0.1", Some(addr), || None).as_deref(),
            Some("http://192.168.1.20:18800")
        );
    }

    #[test]
    fn a_network_bound_gateway_offers_its_lan_address() {
        assert_eq!(
            lan_url_for(18800, "0.0.0.0", None, || Some("10.0.0.5".into())).as_deref(),
            Some("http://10.0.0.5:18800")
        );
        assert_eq!(lan_url_for(18800, "0.0.0.0", None, || None), None);
    }

    #[test]
    fn pair_payload_shape_is_stable() {
        let p = pair_payload("https://mac.tail1234.ts.net", "default");
        assert_eq!(p["url"], "https://mac.tail1234.ts.net");
        assert_eq!(p["workspace"], "default");
        assert_eq!(p["v"], 1);
    }

    #[test]
    fn a_relay_url_is_derived_from_the_link_settings() {
        unsafe {
            std::env::set_var("CONSISTENT_WORLD_RELAY_URL", "wss://relay.example.com/link");
            std::env::set_var("CONSISTENT_WORLD_RELAY_GATEWAY_ID", "gw-7");
        }
        assert_eq!(
            relay_public_url().as_deref(),
            Some("https://relay.example.com/g/gw-7")
        );
        // Without a gateway id there is no addressable URL.
        unsafe {
            std::env::remove_var("CONSISTENT_WORLD_RELAY_GATEWAY_ID");
        }
        assert!(relay_public_url().is_none());
        unsafe {
            std::env::remove_var("CONSISTENT_WORLD_RELAY_URL");
        }
    }

    #[test]
    fn dns_name_strips_trailing_dot() {
        let status = serde_json::json!({ "Self": { "DNSName": "mac.tail1234.ts.net." } });
        assert_eq!(dns_name(&status).as_deref(), Some("mac.tail1234.ts.net"));
    }

    #[test]
    fn backend_state_gates_running() {
        assert!(backend_running(
            &serde_json::json!({ "BackendState": "Running" })
        ));
        assert!(!backend_running(
            &serde_json::json!({ "BackendState": "Stopped" })
        ));
        assert!(!backend_running(&serde_json::json!({})));
    }
}
