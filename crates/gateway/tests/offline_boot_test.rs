//! The real binary boots with no network, no keys, no model CLI, and no
//! Ollama, and answers `/health` inside the desktop shell's 15 s budget.
//!
//! This is the promise an open-source, self-hosted install makes: nothing at
//! startup depends on a service someone else runs. Each step that used to
//! block the port on the network (the brain check runs a real completion; the
//! embedding probes call hosted APIs) has to stay off this path, and this
//! test is what notices when one creeps back.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Kills the gateway even when an assertion fails.
struct Gateway(Child);

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

fn health(port: u16) -> Option<String> {
    let mut s = TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().ok()?,
        Duration::from_millis(300),
    )
    .ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    s.write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    Some(out)
}

#[test]
fn boots_offline_with_nothing_installed() {
    let home = tempfile::tempdir().expect("tempdir");
    let config = home.path().join("consistent_world.toml");
    // In-memory database; Ollama pointed at a port nothing listens on, so a
    // developer machine's real Ollama is never asked to download a model.
    std::fs::write(
        &config,
        r#"
[memory]
surreal_url = "memory"

[llm.providers.ollama]
base_url = "http://127.0.0.1:9"
api_key_env = "OLLAMA_API_KEY"
"#,
    )
    .expect("write config");

    let port = free_port();
    let started = Instant::now();
    let child = Command::new(env!("CARGO_BIN_EXE_consistent-world"))
        .args(["--config", config.to_str().unwrap(), "start", "--offline"])
        .env_clear()
        .env("HOME", home.path())
        // No claude / codex / ollama binaries reachable.
        .env("PATH", "/usr/bin:/bin")
        .env("CONSISTENT_WORLD_PORT", port.to_string())
        .env("GATEWAY_AUTH_TOKEN", "offline-boot-test")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn gateway");
    let mut gateway = Gateway(child);

    let budget = Duration::from_secs(15);
    loop {
        if let Some(resp) = health(port)
            && resp.starts_with("HTTP/1.1 200")
        {
            break;
        }
        if let Ok(Some(status)) = gateway.0.try_wait() {
            panic!("gateway exited during boot: {status}");
        }
        assert!(
            started.elapsed() < budget,
            "gateway did not answer /health within {budget:?} offline"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("offline boot reached /health in {:?}", started.elapsed());
}
