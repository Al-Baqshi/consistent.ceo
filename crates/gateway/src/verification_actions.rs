//! First-class verification *actions*: run a shell test command or a browser URL
//! check as a task-verification step, capturing exit code, logs, and (as a clean
//! extension point) artifact paths.
//!
//! This is the executor layer — pure and deterministic given the command/URL.
//! The orchestration (record lifecycle, task flagging, follow-up creation) lives
//! in `control_api`, which owns task creation and entity visibility. Screenshot
//! capture is intentionally a stub extension point: the repo has a headless
//! browser tool, but wiring persisted screenshots is left as a hook so this slice
//! does not overbuild browser automation.

use consistent_world_common::platform;
use std::process::Stdio;
use std::time::Duration;

/// Hard ceiling for a single verification, matching the shell tool's cap.
const MAX_TIMEOUT_SECS: u64 = 300;
/// Max bytes of captured logs kept per outcome, to bound stored/returned size.
const LOG_CAP: usize = 16_000;

/// Which kind of verification step to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationKind {
    Shell,
    Browser,
}

impl VerificationKind {
    /// Parse a kind string, accepting a few common aliases (case-insensitive).
    pub fn parse(value: &str) -> Option<VerificationKind> {
        match value.trim().to_ascii_lowercase().as_str() {
            "shell" | "command" | "test" => Some(VerificationKind::Shell),
            "browser" | "url" => Some(VerificationKind::Browser),
            _ => None,
        }
    }

    /// Canonical string form, the inverse of [`VerificationKind::parse`].
    pub fn as_str(&self) -> &'static str {
        match self {
            VerificationKind::Shell => "shell",
            VerificationKind::Browser => "browser",
        }
    }
}

/// Result of a shell verification command.
#[derive(Debug, Clone)]
pub struct ShellOutcome {
    pub passed: bool,
    pub exit_code: Option<i64>,
    pub logs: String,
}

/// Run a shell test command (`sh -c "<command>"`, PowerShell on Windows) and
/// capture its exit code and combined stdout/stderr. `passed` is true only on
/// exit code 0.
pub async fn run_shell(command: &str, cwd: Option<&str>, timeout_secs: u64) -> ShellOutcome {
    let timeout = Duration::from_secs(timeout_secs.clamp(1, MAX_TIMEOUT_SECS));
    let (program, args) = platform::shell_command(command, false);
    let mut cmd = platform::command(program);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(dir) = cwd.filter(|d| !d.trim().is_empty()) {
        cmd.current_dir(platform::process_path(std::path::Path::new(dir)));
    }

    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(output)) => {
            let mut logs = String::new();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stdout.trim().is_empty() {
                logs.push_str(stdout.trim_end());
            }
            if !stderr.trim().is_empty() {
                if !logs.is_empty() {
                    logs.push_str("\n--- stderr ---\n");
                }
                logs.push_str(stderr.trim_end());
            }
            truncate_logs(&mut logs);
            ShellOutcome {
                passed: output.status.success(),
                exit_code: output.status.code().map(i64::from),
                logs,
            }
        }
        Ok(Err(err)) => ShellOutcome {
            passed: false,
            exit_code: None,
            logs: format!("failed to launch command: {err}"),
        },
        Err(_) => ShellOutcome {
            passed: false,
            exit_code: None,
            logs: format!("command timed out after {}s", timeout.as_secs()),
        },
    }
}

/// Result of a browser/URL reachability check.
#[derive(Debug, Clone)]
pub struct UrlOutcome {
    pub passed: bool,
    pub status_code: Option<u16>,
    pub logs: String,
}

/// Lightweight browser URL check: GET the URL and pass on a 2xx/3xx response.
/// This is a real reachability check without launching a headless browser;
/// screenshot capture via the browser tool is a separate extension point.
pub async fn run_url_check(url: &str, timeout_secs: u64) -> UrlOutcome {
    let timeout = Duration::from_secs(timeout_secs.clamp(1, MAX_TIMEOUT_SECS));
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(err) => {
            return UrlOutcome {
                passed: false,
                status_code: None,
                logs: format!("failed to build HTTP client: {err}"),
            };
        }
    };
    match client.get(url).send().await {
        Ok(resp) => {
            let status = resp.status();
            let passed = status.is_success() || status.is_redirection();
            UrlOutcome {
                passed,
                status_code: Some(status.as_u16()),
                logs: format!("GET {url} -> {status}"),
            }
        }
        Err(err) => UrlOutcome {
            passed: false,
            status_code: None,
            logs: format!("GET {url} failed: {err}"),
        },
    }
}

/// Whether an address is on the public internet: not loopback, private,
/// link-local (where cloud metadata lives, `169.254.169.254`), carrier-grade
/// NAT (`100.64.0.0/10`, which is also Tailscale), unique-local, documentation,
/// broadcast or unspecified.
pub fn is_public_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xC0) == 64))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80)
        }
    }
}

/// [`run_url_check`] for a caller who is not the machine's owner. On a hosted
/// gateway a URL check is a request the *server* makes, so it must only reach
/// the public internet — never the host's own network, admin ports, or the
/// cloud metadata endpoint. The host is resolved once and every address must
/// be public; the request is pinned to the checked address so a DNS answer
/// can't change between the check and the GET; redirects are not followed (a
/// 3xx already counts as reachable), so a public URL can't bounce inward.
pub async fn run_public_url_check(url: &str, timeout_secs: u64) -> UrlOutcome {
    let refused = |why: &str| UrlOutcome {
        passed: false,
        status_code: None,
        logs: format!("GET {url} refused: {why}"),
    };
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return refused("not a valid URL");
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return refused("only http and https URLs can be checked");
    }
    let Some(host) = parsed.host_str().map(|h| h.trim_matches(['[', ']']).to_string()) else {
        return refused("the URL has no host");
    };
    let port = parsed.port_or_known_default().unwrap_or(80);
    let addrs: Vec<std::net::SocketAddr> = match tokio::net::lookup_host((host.as_str(), port)).await
    {
        Ok(found) => found.collect(),
        Err(err) => return refused(&format!("could not resolve {host}: {err}")),
    };
    let Some(first) = addrs.first().copied() else {
        return refused(&format!("{host} has no addresses"));
    };
    if !addrs.iter().all(|a| is_public_ip(a.ip())) {
        return refused("it points at a private or local address");
    }
    let timeout = Duration::from_secs(timeout_secs.clamp(1, MAX_TIMEOUT_SECS));
    let client = match reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .resolve(&host, first)
        .build()
    {
        Ok(c) => c,
        Err(err) => return refused(&format!("failed to build HTTP client: {err}")),
    };
    match client.get(parsed).send().await {
        Ok(resp) => {
            let status = resp.status();
            UrlOutcome {
                passed: status.is_success() || status.is_redirection(),
                status_code: Some(status.as_u16()),
                logs: format!("GET {url} -> {status}"),
            }
        }
        Err(err) => UrlOutcome {
            passed: false,
            status_code: None,
            logs: format!("GET {url} failed: {err}"),
        },
    }
}

// Cap stored log size so a chatty command can't balloon a task record.
//
// `String::truncate` panics when the cut lands inside a multi-byte character,
// and command output is arbitrary bytes rendered lossily — one `é` or emoji
// straddling byte 16 000 took the whole verification request down. Walk back
// to the nearest char boundary instead.
fn truncate_logs(logs: &mut String) {
    if logs.len() > LOG_CAP {
        let mut cut = LOG_CAP;
        while !logs.is_char_boundary(cut) {
            cut -= 1;
        }
        logs.truncate(cut);
        logs.push_str("\n… (truncated)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kind() {
        assert_eq!(
            VerificationKind::parse("shell"),
            Some(VerificationKind::Shell)
        );
        assert_eq!(
            VerificationKind::parse("Test"),
            Some(VerificationKind::Shell)
        );
        assert_eq!(
            VerificationKind::parse("browser"),
            Some(VerificationKind::Browser)
        );
        assert_eq!(
            VerificationKind::parse("url"),
            Some(VerificationKind::Browser)
        );
        assert_eq!(VerificationKind::parse("nope"), None);
    }

    #[test]
    fn only_public_addresses_count_as_public() {
        use std::net::IpAddr;
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.10",
            "169.254.169.254",
            "100.100.1.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            let ip: IpAddr = private.parse().unwrap();
            assert!(!is_public_ip(ip), "{private} must not count as public");
        }
        for public in ["1.1.1.1", "93.184.216.34", "2606:4700:4700::1111"] {
            let ip: IpAddr = public.parse().unwrap();
            assert!(is_public_ip(ip), "{public} is public");
        }
    }

    #[tokio::test]
    async fn a_hosted_url_check_never_reaches_the_hosts_own_network() {
        for url in [
            "http://127.0.0.1:18800/health",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
            "file:///etc/passwd",
            "http://localhost/",
        ] {
            let out = run_public_url_check(url, 2).await;
            assert!(!out.passed, "{url} must be refused");
            assert!(out.status_code.is_none(), "{url} must not be fetched");
            assert!(out.logs.contains("refused"), "{url}: {}", out.logs);
        }
    }

    #[test]
    fn truncating_logs_never_splits_a_character() {
        // A 3-byte char straddling the cap used to panic `String::truncate`.
        let mut logs = "a".repeat(LOG_CAP - 1) + &"€".repeat(10);
        truncate_logs(&mut logs);
        assert!(logs.ends_with("(truncated)"));
        assert!(logs.starts_with(&"a".repeat(LOG_CAP - 1)));

        let mut short = "fine".to_string();
        truncate_logs(&mut short);
        assert_eq!(short, "fine");
    }

    #[tokio::test]
    async fn shell_passes_on_exit_zero() {
        let command = if cfg!(windows) {
            "Write-Output hello"
        } else {
            "printf hello"
        };
        let out = run_shell(command, None, 30).await;
        assert!(out.passed);
        assert_eq!(out.exit_code, Some(0));
        assert!(out.logs.contains("hello"));
    }

    #[tokio::test]
    async fn shell_fails_on_nonzero_exit_and_captures_stderr() {
        let command = if cfg!(windows) {
            "[Console]::Error.WriteLine('boom'); exit 3"
        } else {
            "echo boom 1>&2; exit 3"
        };
        let out = run_shell(command, None, 30).await;
        assert!(!out.passed);
        assert_eq!(out.exit_code, Some(3));
        assert!(out.logs.contains("boom"));
    }
}
