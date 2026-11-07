//! Terminal client for a running gateway: `chat`, `status`, `tasks`,
//! `approvals`, `approve`, `reject`, `entities`.
//!
//! These commands are thin: they call the same HTTP API the dashboard and the
//! phone use, on this computer's loopback port, with the machine token that
//! `gateway_auth_token` holds next to the config. Nothing here touches the
//! database or the agents directly, so a command can never disagree with what
//! the dashboard shows. When no gateway is running they say how to start one
//! rather than booting a second process against the same database.
//!
//! Output convention: the answer (chat text, tables, JSON) goes to stdout so it
//! pipes cleanly; progress, actions taken and errors go to stderr.

use std::io::{BufRead, IsTerminal, Read, Write};
use std::time::Duration;

use futures::StreamExt;
use serde_json::{Value, json};

use crate::config::AppConfig;

/// A connection to the local gateway.
pub struct Client {
    base: String,
    token: String,
    http: reqwest::Client,
}

/// Why a call did not produce a value, in words a terminal user can act on.
fn not_running(base: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "Consistent CEO is not running on this computer ({base}).\n\
         Start it with `consistent-world start`, or open the desktop app, then try again."
    )
}

impl Client {
    pub fn connect(cfg: &AppConfig, config_path: &str) -> anyhow::Result<Self> {
        let base = format!("http://127.0.0.1:{}", cfg.gateway_port);
        let Some(token) = crate::pair_cli::machine_token(cfg, config_path) else {
            anyhow::bail!(
                "No machine token found next to {config_path}. Start Consistent CEO once \
                 (`consistent-world start`) and run this again."
            );
        };
        Ok(Self {
            base,
            token,
            // No overall timeout: a chat turn streams for as long as the agent works.
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .build()?,
        })
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> anyhow::Result<reqwest::Response> {
        let resp = match req.bearer_auth(&self.token).send().await {
            Ok(r) => r,
            Err(e) if e.is_connect() || e.is_timeout() => return Err(not_running(&self.base)),
            Err(e) => return Err(e.into()),
        };
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            anyhow::bail!(
                "The gateway rejected the machine token (401). If you changed \
                 `gateway_auth_token`, restart the gateway so both sides agree."
            );
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let detail = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or(body);
            anyhow::bail!("The gateway answered {status}{}", fmt_detail(&detail));
        }
        Ok(resp)
    }

    async fn get_json(&self, path: &str, query: &[(&str, String)]) -> anyhow::Result<Value> {
        Ok(self
            .send(self.http.get(format!("{}{path}", self.base)).query(query))
            .await?
            .json()
            .await?)
    }

    async fn items(&self, path: &str, query: &[(&str, String)]) -> anyhow::Result<Vec<Value>> {
        let v = self.get_json(path, query).await?;
        Ok(v["items"].as_array().cloned().unwrap_or_default())
    }
}

fn fmt_detail(detail: &str) -> String {
    let d = detail.trim();
    if d.is_empty() {
        String::new()
    } else {
        format!(": {d}")
    }
}

/// Pretty JSON for `--json`.
fn print_json(v: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
    );
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}

/// First `n` characters, so ids and titles fit a terminal row.
fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

// ── status ────────────────────────────────────────────────────────────────

pub async fn status(client: &Client, as_json: bool) -> anyhow::Result<()> {
    // `/health` is public, so it answers even if the token is wrong.
    let health: Value = match client
        .http
        .get(format!("{}/health", client.base))
        .send()
        .await
    {
        Ok(r) => r.json().await?,
        Err(_) => return Err(not_running(&client.base)),
    };
    let tasks = client
        .items("/api/tasks", &[("limit", "1000".into())])
        .await?;
    let approvals = client
        .items("/api/approvals", &[("status", "pending".into())])
        .await
        .unwrap_or_default();

    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    for t in &tasks {
        *counts.entry(str_of(t, "status").to_string()).or_default() += 1;
    }

    if as_json {
        print_json(&json!({
            "health": health,
            "tasks": counts,
            "pendingApprovals": approvals.len(),
        }));
        return Ok(());
    }
    println!(
        "gateway   {} (v{}, {})  {}",
        str_of(&health, "status"),
        str_of(&health, "version"),
        str_of(&health, "memory_backend"),
        client.base
    );
    if counts.is_empty() {
        println!("tasks     none");
    } else {
        let line: Vec<String> = counts.iter().map(|(k, n)| format!("{n} {k}")).collect();
        println!("tasks     {}", line.join(", "));
    }
    println!(
        "approvals {} waiting{}",
        approvals.len(),
        if approvals.is_empty() {
            ""
        } else {
            "  (consistent-world approvals)"
        }
    );
    Ok(())
}

// ── lists ─────────────────────────────────────────────────────────────────

pub async fn entities(client: &Client, as_json: bool) -> anyhow::Result<()> {
    let items = client.items("/api/entities", &[]).await?;
    if as_json {
        print_json(&Value::Array(items));
        return Ok(());
    }
    if items.is_empty() {
        println!("No entities yet. Create one in the dashboard or by chatting.");
    }
    for e in &items {
        println!(
            "{:<38} {:<28} {}",
            str_of(e, "id"),
            clip(str_of(e, "name"), 28),
            str_of(e, "status")
        );
    }
    Ok(())
}

pub async fn tasks(
    client: &Client,
    status: Option<String>,
    limit: usize,
    as_json: bool,
) -> anyhow::Result<()> {
    let mut q = vec![("limit", limit.to_string())];
    if let Some(s) = status {
        q.push(("status", s));
    }
    let items = client.items("/api/tasks", &q).await?;
    if as_json {
        print_json(&Value::Array(items));
        return Ok(());
    }
    if items.is_empty() {
        println!("No tasks.");
    }
    for t in &items {
        println!(
            "{:<38} {:<12} {:>3}%  {}",
            str_of(t, "id"),
            str_of(t, "status"),
            t["progress"].as_u64().unwrap_or(0),
            clip(str_of(t, "title"), 60)
        );
    }
    Ok(())
}

pub async fn approvals(client: &Client, all: bool, as_json: bool) -> anyhow::Result<()> {
    let q: Vec<(&str, String)> = if all {
        vec![]
    } else {
        vec![("status", "pending".into())]
    };
    let items = client.items("/api/approvals", &q).await?;
    if as_json {
        print_json(&Value::Array(items));
        return Ok(());
    }
    if items.is_empty() {
        println!("Nothing is waiting for you.");
    }
    for a in &items {
        println!(
            "{:<38} {:<9} {}",
            str_of(a, "id"),
            str_of(a, "status"),
            clip(str_of(a, "action"), 60)
        );
        let detail = str_of(a, "detail");
        if !detail.is_empty() {
            println!("    {}", clip(&detail.replace('\n', " "), 100));
        }
    }
    Ok(())
}

/// Approve or reject one approval. `id` may be a unique prefix of the full id.
pub async fn resolve_approval(client: &Client, id: &str, approve: bool) -> anyhow::Result<()> {
    let all = client.items("/api/approvals", &[]).await?;
    let matches: Vec<&Value> = all
        .iter()
        .filter(|a| str_of(a, "id").starts_with(id))
        .collect();
    let target = match matches.as_slice() {
        [one] => str_of(one, "id").to_string(),
        [] => anyhow::bail!("No approval starts with `{id}`. See `consistent-world approvals`."),
        many => anyhow::bail!(
            "`{id}` matches {} approvals; use more of the id.",
            many.len()
        ),
    };
    let status = if approve { "approved" } else { "rejected" };
    client
        .send(
            client
                .http
                .patch(format!("{}/api/approvals/{target}", client.base))
                .json(&json!({ "status": status })),
        )
        .await?;
    println!("{status} {target}");
    Ok(())
}

// ── chat ──────────────────────────────────────────────────────────────────

/// Find an entity by exact id, else by case-insensitive name (or unique prefix).
async fn resolve_entity(client: &Client, wanted: &str) -> anyhow::Result<String> {
    let items = client.items("/api/entities", &[]).await?;
    if let Some(e) = items.iter().find(|e| str_of(e, "id") == wanted) {
        return Ok(str_of(e, "id").to_string());
    }
    let lower = wanted.to_lowercase();
    let exact: Vec<&Value> = items
        .iter()
        .filter(|e| str_of(e, "name").to_lowercase() == lower)
        .collect();
    let found = if exact.is_empty() {
        items
            .iter()
            .filter(|e| str_of(e, "name").to_lowercase().starts_with(&lower))
            .collect()
    } else {
        exact
    };
    match found.as_slice() {
        [one] => Ok(str_of(one, "id").to_string()),
        [] => anyhow::bail!("No entity matches `{wanted}`. See `consistent-world entities`."),
        many => anyhow::bail!(
            "`{wanted}` matches {} entities ({}); be more specific.",
            many.len(),
            many.iter()
                .map(|e| str_of(e, "name"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Options shared by one-shot and interactive chat.
pub struct ChatOpts {
    pub entity: Option<String>,
    pub as_json: bool,
}

/// One streamed turn. Returns the session id so the next turn continues it.
async fn chat_turn(
    client: &Client,
    message: &str,
    entity_id: Option<&str>,
    session_id: Option<&str>,
    as_json: bool,
) -> anyhow::Result<Option<String>> {
    let mut body = json!({
        "message": message,
        "timezoneOffsetMinutes": chrono::Local::now().offset().local_minus_utc() / 60,
    });
    if let Some(id) = entity_id {
        body["entityId"] = json!(id);
        body["scope"] = json!("entity");
    }
    if let Some(s) = session_id {
        body["sessionId"] = json!(s);
    }

    let resp = client
        .send(
            client
                .http
                .post(format!("{}/api/capture/chat/stream", client.base))
                .json(&body),
        )
        .await?;

    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut session = session_id.map(str::to_string);
    let mut printed_any = false;
    let mut out = std::io::stdout();

    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk?);
        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if as_json {
                println!("{line}");
                continue;
            }
            let Ok(ev) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            match ev["type"].as_str() {
                Some("meta") => {
                    if let Some(s) = ev["sessionId"].as_str() {
                        session = Some(s.to_string());
                    }
                }
                Some("token") => {
                    print!("{}", str_of(&ev, "text"));
                    let _ = out.flush();
                    printed_any = true;
                }
                Some("done") => {
                    let p = &ev["payload"];
                    // A turn that produced no tokens (a canned or non-streamed
                    // reply) still carries its text in the final payload.
                    if !printed_any && let Some(m) = p["message"].as_str() {
                        print!("{m}");
                    }
                    println!();
                    for a in p["actions"].as_array().into_iter().flatten() {
                        eprintln!(
                            "  · {} {} ({})",
                            str_of(a, "actionType"),
                            clip(str_of(a, "description"), 80),
                            str_of(a, "status")
                        );
                    }
                    if p["routing"]["outcome"] == "auto_filed"
                        && let Some(r) = p["routing"]["entityName"].as_str()
                    {
                        eprintln!("  → filed under {r}");
                    }
                    if let Some(s) = p["sessionId"].as_str() {
                        session = Some(s.to_string());
                    }
                }
                Some("error") => {
                    anyhow::bail!(
                        "{}",
                        ev["message"]
                            .as_str()
                            .or(ev["error"].as_str())
                            .unwrap_or("chat failed")
                    );
                }
                _ => {}
            }
        }
    }
    Ok(session)
}

/// `chat`: one turn when a message (or piped stdin) is given, else a REPL.
pub async fn chat(client: &Client, words: Vec<String>, opts: ChatOpts) -> anyhow::Result<()> {
    let entity_id = match opts.entity.as_deref() {
        Some(w) => Some(resolve_entity(client, w).await?),
        None => None,
    };

    let mut message = words.join(" ");
    if message.trim().is_empty() && !std::io::stdin().is_terminal() {
        let mut piped = String::new();
        std::io::stdin().read_to_string(&mut piped)?;
        message = piped;
    }
    if !message.trim().is_empty() {
        chat_turn(
            client,
            message.trim(),
            entity_id.as_deref(),
            None,
            opts.as_json,
        )
        .await?;
        return Ok(());
    }

    eprintln!("Consistent CEO — type a message, /quit to leave (Ctrl-D works too).");
    let stdin = std::io::stdin();
    let mut session: Option<String> = None;
    loop {
        eprint!("> ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            eprintln!();
            return Ok(());
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if matches!(line, "/quit" | "/exit" | "/q") {
            return Ok(());
        }
        match chat_turn(
            client,
            line,
            entity_id.as_deref(),
            session.as_deref(),
            opts.as_json,
        )
        .await
        {
            Ok(s) => session = s,
            // A refused or dropped turn should not end the conversation.
            Err(e) => eprintln!("{e}"),
        }
    }
}
