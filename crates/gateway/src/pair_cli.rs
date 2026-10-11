//! `consistent-world pair`: pair a phone from the terminal.
//!
//! Asks the running gateway for a one-time code (`POST /api/pair/offer`),
//! prints it as a QR, then waits for the phone to scan and asks in the
//! terminal whether to allow it, showing the six digits the phone shows. See
//! `pairing.rs` for the protocol. When no gateway is running there is no one
//! to mint a code, so it says how to start one instead of printing a QR that
//! could only lead to a password prompt.

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::AppConfig;

/// How long to wait for a scan after printing the code (the code itself
/// lives 90 s; the request it turns into lives three minutes).
const WAIT: Duration = Duration::from_secs(240);

fn machine_token(cfg: &AppConfig, config_path: &str) -> Option<String> {
    if let Ok(t) = std::env::var("GATEWAY_AUTH_TOKEN")
        && !t.trim().is_empty()
    {
        return Some(t.trim().to_string());
    }
    let file = Path::new(config_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("gateway_auth_token");
    if let Ok(t) = std::fs::read_to_string(file)
        && !t.trim().is_empty()
    {
        return Some(t.trim().to_string());
    }
    crate::credentials_resolver::resolve_gateway_auth_token(cfg)
        .ok()
        .flatten()
}

fn spaced(code: &str) -> String {
    if code.len() == 6 {
        format!("{} {}", &code[..3], &code[3..])
    } else {
        code.to_string()
    }
}

pub async fn run(cfg: &AppConfig, config_path: &str) -> anyhow::Result<()> {
    let base = format!("http://127.0.0.1:{}", cfg.gateway_port);
    let Some(token) = machine_token(cfg, config_path) else {
        anyhow::bail!(
            "No machine token found next to {config_path}. Start Consistent CEO once \
             (`consistent-world start`, or open the desktop app) and run this again."
        );
    };
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;

    let resp = match http
        .post(format!("{base}/api/pair/offer"))
        .bearer_auth(&token)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => anyhow::bail!(
            "Consistent CEO is not running on this computer ({base}). Start it \
             (`consistent-world start`, or open the desktop app), then run `consistent-world pair` again."
        ),
    };
    if !resp.status().is_success() {
        anyhow::bail!(
            "The gateway refused to make a pairing code ({}). Pairing from the terminal \
             works on a gateway bound to this computer only; sign in to the dashboard and \
             use Settings → Devices instead.",
            resp.status()
        );
    }
    let offer: Value = resp.json().await?;
    let Some(url) = offer["url"].as_str() else {
        anyhow::bail!(
            "Your phone has no way to reach this computer yet. Install Tailscale on both \
             (https://tailscale.com/download), sign in, run `consistent-world remote setup`, \
             then run this again."
        );
    };

    let qr = qrcode::QrCode::new(offer["payload"].to_string().as_bytes())?;
    println!(
        "{}\n",
        qr.render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build()
    );
    println!("Scan this from the Consistent CEO app: Settings → Scan to connect.");
    println!("The code works once, for 90 seconds. Address: {url}");
    if offer["via"] == "lan" {
        println!(
            "(Same Wi-Fi only. Run `consistent-world remote setup` to reach it from anywhere.)"
        );
    }
    println!("\nWaiting for your phone…");

    let started = Instant::now();
    let mut asked: HashSet<String> = HashSet::new();
    while started.elapsed() < WAIT {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let Ok(r) = http
            .get(format!("{base}/api/pair/requests"))
            .bearer_auth(&token)
            .send()
            .await
        else {
            continue;
        };
        let Ok(pending) = r.json::<Vec<Value>>().await else {
            continue;
        };
        for req in pending {
            let Some(id) = req["requestId"].as_str().map(str::to_string) else {
                continue;
            };
            if !asked.insert(id.clone()) {
                continue;
            }
            let label = req["label"].as_str().unwrap_or("A phone");
            let code = spaced(req["code"].as_str().unwrap_or(""));
            println!("\n{label} wants to pair. Your phone should show: {code}");
            let allow = tokio::task::spawn_blocking(|| {
                dialoguer::Confirm::new()
                    .with_prompt("Is that your phone, and do the numbers match? Allow it")
                    .default(false)
                    .interact()
                    .unwrap_or(false)
            })
            .await
            .unwrap_or(false);
            let verb = if allow { "approve" } else { "deny" };
            let res = http
                .post(format!("{base}/api/pair/requests/{id}/{verb}"))
                .bearer_auth(&token)
                .send()
                .await?;
            if allow && res.status().is_success() {
                println!(
                    "Paired. The phone signs itself back in from anywhere with its own key. \
                     Remove it any time from Settings → Devices."
                );
                return Ok(());
            } else if allow {
                println!(
                    "Could not allow it ({}). Run `consistent-world pair` again.",
                    res.status()
                );
            } else {
                println!("Refused. The phone was told no.");
            }
        }
    }
    anyhow::bail!("No phone was allowed in time. Run `consistent-world pair` again.")
}
