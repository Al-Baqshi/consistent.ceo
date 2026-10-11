//! Long-running Claude Code sessions as a first-class execution engine.
//!
//! The original path ([`consistent_world_tools::coding_agent::CodingAgentTool`])
//! spawns the CLI, waits for it to exit, and hands back one buffered blob. That
//! is fine for a two-minute errand and useless for the thing this product
//! promises — "keep working on this for days". It has four specific problems:
//!
//! 1. **No visibility.** Output arrives only at the end, and a timeout throws
//!    away everything the agent produced.
//! 2. **No continuity.** Each invocation starts cold; there is no session id to
//!    resume, so a multi-day job can't build on itself.
//! 3. **No control.** The child handle is dropped, so "cancel" only marks a row
//!    — the process keeps running and keeps editing files.
//! 4. **No durability.** A gateway restart loses the work in flight.
//!
//! This module fixes all four by driving `claude -p --output-format stream-json`
//! and reading its NDJSON event stream as it happens:
//!
//! - assistant text is appended to a tail-capped buffer and flushed to the run
//!   row every couple of seconds, so the existing `GET .../agent-runs/{id}`
//!   poll becomes a live feed with no new frontend plumbing;
//! - the `session_id` from the `init` event is persisted, which is what makes
//!   `--resume` (and therefore multi-day work) possible;
//! - a cancel channel is registered per run so stopping actually kills the
//!   child;
//! - an interrupted run is marked resumable rather than failed, and swept back
//!   up on the next boot.
//!
//! The parser is deliberately tolerant: unknown event types are ignored and
//! unparseable lines are kept as raw text. The CLI's stream schema is not a
//! stable contract, and a schema drift should degrade the transcript, not kill
//! the run.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use consistent_world_common::platform;
use consistent_world_tools::coding_agent::locate_program;

use crate::state::ControlState;

/// Keep the in-memory transcript bounded; the full text is mirrored to the
/// entity's vault folder by the caller.
const OUTPUT_TAIL_CAP: usize = 64 * 1024;
/// How often partial output is flushed to the run row.
const FLUSH_INTERVAL: Duration = Duration::from_secs(2);
/// No event for this long means the CLI is wedged. Generous, because a single
/// turn can legitimately think for a long time.
pub const DEFAULT_STALL_TIMEOUT_SECS: u64 = 1800;

/// How much freedom a headless run has inside its working directory.
///
/// Note the jail is the *directory*, not this setting: every run is confined to
/// the entity's resolved cwd by `resolve_entity_agent_run_cwd` before it starts.
/// This controls how much the agent has to stop and ask within that jail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Full autonomy inside the cwd jail. The default: unattended multi-day
    /// work can't pause for a permission prompt nobody is there to answer.
    BypassPermissions,
    /// File edits are auto-approved; riskier tools still prompt (and, headless,
    /// are refused). Safer, but long runs can stall on a denial.
    AcceptEdits,
    /// The CLI's own default prompting behaviour.
    Default,
}

impl PermissionMode {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim).unwrap_or_default() {
            "acceptEdits" | "accept-edits" => Self::AcceptEdits,
            "default" => Self::Default,
            _ => Self::BypassPermissions,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BypassPermissions => "bypassPermissions",
            Self::AcceptEdits => "acceptEdits",
            Self::Default => "default",
        }
    }
}

/// One headless Claude Code invocation: what to run, where, and how much
/// autonomy it has inside its working directory.
#[derive(Debug, Clone)]
pub struct SessionRequest {
    pub run_id: String,
    pub prompt: String,
    pub cwd: String,
    pub permission_mode: PermissionMode,
    pub max_turns: Option<u32>,
    /// Continue a prior Claude Code session instead of starting cold.
    pub resume_session_id: Option<String>,
    pub stall_timeout_secs: u64,
    /// Path to an `--mcp-config` file granting this run extra tools (today:
    /// the browser). Absent for every ordinary run — a task only gets the
    /// browser when it asked for it and the policy allowed it.
    pub mcp_config_path: Option<String>,
}

/// Result of a finished (or interrupted) Claude Code session.
#[derive(Debug, Clone, Default)]
pub struct SessionOutcome {
    /// `completed` | `failed` | `cancelled` | `interrupted`.
    pub status: String,
    pub output: String,
    pub error: Option<String>,
    pub session_id: Option<String>,
    pub num_turns: Option<u64>,
    /// What the run *would* have cost on API pricing. On a Max subscription
    /// nothing is billed per-run, so treat this as a usage proxy, not spend.
    pub cost_usd: Option<f64>,
    /// The run stopped because the subscription window was spent.
    pub rate_limited: bool,
    /// Unix seconds when work can resume. Pairs with `rate_limited`.
    pub resets_at: Option<i64>,
}

/// Build the CLI invocation.
///
/// `--verbose` is not optional: the CLI refuses `--output-format stream-json`
/// with `-p` without it.
pub fn build_args(req: &SessionRequest) -> Vec<String> {
    let mut args = vec![
        "-p".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        // Always name the model. The CLI's own default follows whatever the
        // operator last picked with `/model` in an interactive Claude Code
        // session — which can be a model the installed CLI cannot serve, and
        // then every session here dies in seconds with an API error. Same fix
        // as the completion adapter (see `claude_code::DEFAULT_MODEL_ALIAS`).
        "--model".to_string(),
        consistent_world_llm::claude_code::DEFAULT_MODEL_ALIAS.to_string(),
    ];
    match req.permission_mode {
        PermissionMode::BypassPermissions => {
            args.push("--dangerously-skip-permissions".to_string());
        }
        PermissionMode::AcceptEdits => {
            args.push("--permission-mode".to_string());
            args.push("acceptEdits".to_string());
        }
        PermissionMode::Default => {}
    }
    if let Some(turns) = req.max_turns {
        args.push("--max-turns".to_string());
        args.push(turns.to_string());
    }
    if let Some(sid) = req
        .resume_session_id
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        args.push("--resume".to_string());
        args.push(sid.to_string());
    }
    if let Some(path) = req
        .mcp_config_path
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        args.push("--mcp-config".to_string());
        args.push(path.to_string());
    }
    // No prompt here: it is written to stdin (see `run_claude_session`). As a
    // trailing positional it could be read as one more value for a list flag
    // (`--mcp-config` takes several), and on Windows a task prompt outgrows
    // the command line — `claude.cmd` shims cannot even carry its newlines.
    args
}

/// Folds the CLI's NDJSON event stream into the state we care about.
///
/// Pure and synchronous so the stream contract can be tested without spawning
/// anything.
#[derive(Debug, Default)]
pub struct StreamAccumulator {
    pub session_id: Option<String>,
    /// Assistant text as it streams, tail-capped.
    pub text: String,
    /// The `result` event's final answer, which supersedes `text` when present.
    pub final_result: Option<String>,
    pub num_turns: Option<u64>,
    pub cost_usd: Option<f64>,
    pub is_error: bool,
    pub error_message: Option<String>,
    /// Latest `rate_limit_event.status` seen (`"allowed"`, `"rejected"`, …).
    pub rate_limit_status: Option<String>,
    /// Unix seconds when the subscription window reopens.
    ///
    /// This is the field that lets the operator stop babysitting: on a Claude
    /// Max subscription the CLI stops when the five-hour window is spent, and
    /// until now a human had to come back and say "keep going". The CLI tells
    /// us exactly when it can continue — so we can just wait and resume.
    pub rate_limit_resets_at: Option<i64>,
}

impl StreamAccumulator {
    // Appends text to the tail-capped buffer, trimming from the front (and
    // re-aligning to a char boundary) once it grows past the cap.
    fn push_text(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            self.text.push('\n');
        }
        self.text.push_str(chunk);
        if self.text.len() > OUTPUT_TAIL_CAP {
            // Trim from the front, then re-align to a char boundary so the
            // buffer stays valid UTF-8.
            let cut = self.text.len() - OUTPUT_TAIL_CAP;
            let mut idx = cut;
            while idx < self.text.len() && !self.text.is_char_boundary(idx) {
                idx += 1;
            }
            self.text = self.text[idx..].to_string();
        }
    }

    /// Ingest one line of the stream. Never fails: a line we can't parse is
    /// still evidence of what the agent did, so it is kept as raw text.
    pub fn ingest(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            self.push_text(line);
            return;
        };

        // `session_id` shows up on most events; the earliest one wins nothing —
        // we take the latest, because `--resume` can hand back a new id.
        if let Some(sid) = value.get("session_id").and_then(|v| v.as_str())
            && !sid.is_empty()
        {
            self.session_id = Some(sid.to_string());
        }

        match value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
        {
            "assistant" => {
                if let Some(content) = value
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                {
                    for block in content {
                        if block.get("type").and_then(|v| v.as_str()) == Some("text")
                            && let Some(t) = block.get("text").and_then(|v| v.as_str())
                        {
                            self.push_text(t);
                        }
                    }
                }
            }
            "result" => {
                if let Some(r) = value.get("result").and_then(|v| v.as_str()) {
                    self.final_result = Some(r.to_string());
                }
                if let Some(n) = value.get("num_turns").and_then(|v| v.as_u64()) {
                    self.num_turns = Some(n);
                }
                if let Some(c) = value.get("total_cost_usd").and_then(|v| v.as_f64()) {
                    self.cost_usd = Some(c);
                }
                if value.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
                    self.is_error = true;
                    // The CLI puts the actual failure (an API error, an auth
                    // problem) in `result` and still labels the event
                    // `subtype: "success"` — so the subtype is only a
                    // fallback, and never when it just says "success".
                    let from_result = value
                        .get("result")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|r| !r.is_empty())
                        .map(|r| r.chars().take(600).collect::<String>());
                    let from_subtype = value
                        .get("subtype")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty() && *s != "success")
                        .map(str::to_string);
                    self.error_message = from_result
                        .or(from_subtype)
                        .or_else(|| Some("the agent reported an error".to_string()));
                }
            }
            "rate_limit_event" => {
                if let Some(info) = value.get("rate_limit_info") {
                    if let Some(s) = info.get("status").and_then(|v| v.as_str()) {
                        self.rate_limit_status = Some(s.to_string());
                    }
                    // Accept both spellings; the CLI uses camelCase today.
                    if let Some(r) = info
                        .get("resetsAt")
                        .or_else(|| info.get("resets_at"))
                        .and_then(|v| v.as_i64())
                    {
                        self.rate_limit_resets_at = Some(r);
                    }
                }
            }
            // `system`/`user` events carry init metadata and tool results; the
            // session id above is all we need from them today.
            _ => {}
        }
    }

    /// Did this run stop because the subscription window was spent?
    ///
    /// Two signals, because the CLI can surface the same condition either way:
    /// an explicit `rate_limit_event` whose status is no longer `allowed`, or
    /// a terminal error that talks about limits. Being generous here is safe —
    /// the only consequence is waiting for a reset that had already happened,
    /// and the scheduler floors that to "retry shortly".
    pub fn hit_rate_limit(&self) -> bool {
        // The CLI's statuses are `allowed`, `allowed_warning` (approaching the
        // limit — the run continues) and `rejected`. Only a status that is
        // not some flavour of "allowed" means the window is actually spent;
        // reading a warning as a rejection turned finished runs into
        // failures.
        if self.rate_limit_status.as_deref().is_some_and(|s| {
            let s = s.trim().to_ascii_lowercase();
            !s.is_empty() && !s.starts_with("allowed")
        }) {
            return true;
        }
        let haystack = format!(
            "{} {}",
            self.error_message.as_deref().unwrap_or_default(),
            self.final_result.as_deref().unwrap_or_default()
        )
        .to_ascii_lowercase();
        haystack.contains("rate limit")
            || haystack.contains("rate_limit")
            || haystack.contains("usage limit")
    }

    /// The transcript to persist: the final answer when the run produced one,
    /// otherwise everything streamed so far (which is what makes a killed or
    /// stalled run still useful).
    pub fn output(&self) -> String {
        match self.final_result.as_deref() {
            Some(r) if !r.trim().is_empty() => r.to_string(),
            _ => self.text.clone(),
        }
    }
}

/// Live child processes, keyed by run id. Runtime-only — deliberately not part
/// of [`crate::state::ControlStateSnapshot`], because a process handle means
/// nothing after a restart.
#[derive(Default)]
pub struct AgentProcessRegistry {
    inner: tokio::sync::RwLock<HashMap<String, mpsc::UnboundedSender<()>>>,
}

impl AgentProcessRegistry {
    async fn register(&self, run_id: &str) -> mpsc::UnboundedReceiver<()> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.inner.write().await.insert(run_id.to_string(), tx);
        rx
    }

    async fn deregister(&self, run_id: &str) {
        self.inner.write().await.remove(run_id);
    }

    /// Ask a running session to stop. `false` = no live process for that id
    /// (already finished, or owned by a previous life of this gateway).
    pub async fn cancel(&self, run_id: &str) -> bool {
        let sender = self.inner.read().await.get(run_id).cloned();
        match sender {
            Some(tx) => tx.send(()).is_ok(),
            None => false,
        }
    }

    pub async fn is_running(&self, run_id: &str) -> bool {
        self.inner.read().await.contains_key(run_id)
    }
}

/// Flush partial progress to the run row so pollers see a live transcript.
async fn persist_progress(control: &ControlState, run_id: &str, acc: &StreamAccumulator) {
    let mut runs = control.external_agent_runs.write().await;
    if let Some(run) = runs.iter_mut().find(|r| r.id == run_id) {
        run.output = acc.output();
        if let Some(sid) = acc.session_id.as_deref() {
            if !run.metadata.is_object() {
                run.metadata = serde_json::json!({});
            }
            if let Some(obj) = run.metadata.as_object_mut() {
                obj.insert(
                    "sessionId".to_string(),
                    serde_json::Value::String(sid.to_string()),
                );
            }
        }
    }
}

/// Stop a session's process — and, on Windows, everything it started: an npm
/// `claude.cmd` shim is `cmd.exe` in front of Node, and killing only the shim
/// would leave the agent running on unseen.
fn kill_session(child: &mut tokio::process::Child, pid: Option<u32>) {
    if cfg!(windows)
        && let Some(pid) = pid
    {
        platform::kill_process_tree(pid);
    }
    let _ = child.start_kill();
}

/// Run a Claude Code session to completion, streaming as it goes.
///
/// Returns rather than panics on every failure path: the caller owns the run
/// row and needs an outcome to record no matter what happened.
pub async fn run_claude_session(control: Arc<ControlState>, req: SessionRequest) -> SessionOutcome {
    let Some(program) = locate_program("claude") else {
        return SessionOutcome {
            status: "failed".to_string(),
            error: Some(
                "The `claude` CLI was not found on PATH. Install it, or check Settings → Coding agents."
                    .to_string(),
            ),
            ..Default::default()
        };
    };

    let args = build_args(&req);
    let child = platform::command(&program)
        .args(&args)
        .current_dir(platform::process_path(std::path::Path::new(&req.cwd)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::piped())
        .kill_on_drop(true)
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return SessionOutcome {
                status: "failed".to_string(),
                error: Some(format!("Failed to start `claude`: {e}")),
                ..Default::default()
            };
        }
    };

    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill().await;
        return SessionOutcome {
            status: "failed".to_string(),
            error: Some("Could not capture the agent's output stream.".to_string()),
            ..Default::default()
        };
    };
    let stderr = child.stderr.take();

    // Hand over the prompt and close stdin so the CLI starts. Written from its
    // own task: the CLI may begin streaming before it has read everything, and
    // a full stdout pipe must not wait on a full stdin pipe.
    if let Some(mut stdin) = child.stdin.take() {
        let prompt = req.prompt.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(prompt.as_bytes()).await;
        });
    }
    let pid = child.id();

    let mut cancel_rx = control.agent_processes.register(&req.run_id).await;
    let mut lines = BufReader::new(stdout).lines();
    let mut acc = StreamAccumulator::default();
    let stall = Duration::from_secs(req.stall_timeout_secs.max(60));
    let mut last_flush = Instant::now();
    let mut cancelled = false;
    let mut stalled = false;

    // Drain stdout line-by-line, periodically flushing progress, until the
    // stream ends, a cancel arrives, or nothing arrives for `stall`.
    loop {
        let next = tokio::time::timeout(stall, lines.next_line());
        tokio::select! {
            // Check for a cancel request before reading the next line, so a
            // cancel is never starved by a chatty stream.
            biased;

            _ = cancel_rx.recv() => {
                cancelled = true;
                kill_session(&mut child, pid);
                break;
            }
            read = next => {
                match read {
                    // Stream ended: the CLI is done talking.
                    Ok(Ok(None)) => break,
                    Ok(Ok(Some(line))) => {
                        acc.ingest(&line);
                        if last_flush.elapsed() >= FLUSH_INTERVAL {
                            persist_progress(control.as_ref(), &req.run_id, &acc).await;
                            last_flush = Instant::now();
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(run_id = %req.run_id, error = %e, "agent session: stdout read failed");
                        break;
                    }
                    Err(_) => {
                        stalled = true;
                        kill_session(&mut child, pid);
                        break;
                    }
                }
            }
        }
    }

    control.agent_processes.deregister(&req.run_id).await;

    let exit = child.wait().await;
    // Drain stderr only after the process is done — it is the sole place the
    // CLI explains an auth failure or a bad flag.
    let stderr_text = match stderr {
        Some(handle) => {
            let mut buf = String::new();
            let mut reader = BufReader::new(handle);
            let _ = tokio::io::AsyncReadExt::read_to_string(&mut reader, &mut buf).await;
            buf.trim().to_string()
        }
        None => String::new(),
    };

    persist_progress(control.as_ref(), &req.run_id, &acc).await;

    let output = acc.output();
    let rate_limited = acc.hit_rate_limit();
    let base = SessionOutcome {
        status: String::new(),
        output: output.clone(),
        error: None,
        session_id: acc.session_id.clone(),
        num_turns: acc.num_turns,
        cost_usd: acc.cost_usd,
        rate_limited,
        resets_at: acc.rate_limit_resets_at,
    };

    if cancelled {
        return SessionOutcome {
            status: "cancelled".to_string(),
            error: Some("Stopped by the operator.".to_string()),
            ..base
        };
    }
    if stalled {
        return SessionOutcome {
            status: "failed".to_string(),
            error: Some(format!(
                "No activity for {}s; the run was stopped. Partial output kept.",
                stall.as_secs()
            )),
            ..base
        };
    }
    // A spent window is not a failed run: the work is fine, it just can't
    // continue yet. Calling it `failed` would make the engine give up on a
    // mission that is merely waiting.
    if rate_limited {
        return SessionOutcome {
            status: "rate_limited".to_string(),
            error: Some(match acc.rate_limit_resets_at {
                Some(_) => {
                    "Claude usage window is spent; work resumes automatically when it resets."
                        .to_string()
                }
                None => "Claude usage window is spent; will retry shortly.".to_string(),
            }),
            ..base
        };
    }
    if acc.is_error {
        return SessionOutcome {
            status: "failed".to_string(),
            error: acc.error_message.clone(),
            ..base
        };
    }
    match exit {
        Ok(code) if code.success() => SessionOutcome {
            status: "completed".to_string(),
            ..base
        },
        Ok(code) => SessionOutcome {
            status: "failed".to_string(),
            error: Some(if stderr_text.is_empty() {
                format!("`claude` exited with {code}.")
            } else {
                stderr_text
            }),
            ..base
        },
        Err(e) => SessionOutcome {
            status: "failed".to_string(),
            error: Some(format!("Could not wait on `claude`: {e}")),
            ..base
        },
    }
}

/// The continuation prompt for a session the gateway restarted out from under.
///
/// It asks for orientation rather than blind continuation: the agent may have
/// finished the work before the restart, and telling us so is a better outcome
/// than redoing it.
pub fn resume_prompt(original: &str) -> String {
    format!(
        "You were interrupted mid-task because the host process restarted. \
         Check the current state of the working directory first (what is already done?), \
         then continue from there. Do not redo completed work.\n\n\
         The original task was:\n{original}"
    )
}

/// Pick up sessions that were mid-flight when the gateway went down.
///
/// [`ControlState::mark_orphaned_external_agent_runs_stale`] has already marked
/// these `interrupted` and kept their `sessionId`; this respawns each one with
/// `--resume` so a multi-day job survives a restart instead of dying at it.
/// Release executor tasks whose run can never be resumed.
///
/// [`resume_interrupted`] can only resume a run that got far enough for the CLI
/// to report a `sessionId`. A gateway that dies inside that window leaves an
/// `interrupted` run with no session — unresumable — and its task pinned
/// `InProgress` with the claim marker set. Nothing else rescues it: the
/// executor only scans `Queued`, the orchestrator defers to the executor for
/// `executable` work, and the stuck-task watchdog is deliberately disarmed for
/// these long runs. So the task sits there forever, silently.
///
/// This closes that hole at startup: the run is marked failed and the task is
/// handed back — retryable if it has attempts left, on the operator's desk if
/// not. Returns how many tasks were released.
pub async fn release_unresumable_runs(control: Arc<ControlState>) -> usize {
    let orphaned: Vec<(String, Option<String>)> = {
        let runs = control.external_agent_runs.read().await;
        runs.iter()
            .filter(|r| r.status == "interrupted" && r.provider == "claude")
            // No session id: `--resume` has nothing to resume.
            .filter(|r| {
                r.metadata
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .is_none()
            })
            .filter(|r| r.metadata.get("source").and_then(|v| v.as_str()) == Some("task-executor"))
            .map(|r| (r.id.clone(), r.task_id.clone()))
            .collect()
    };
    if orphaned.is_empty() {
        return 0;
    }

    let mut released = 0usize;
    for (run_id, task_id) in orphaned {
        {
            let mut runs = control.external_agent_runs.write().await;
            if let Some(run) = runs.iter_mut().find(|r| r.id == run_id) {
                run.status = "failed".to_string();
                run.error = Some(
                    "the gateway restarted before this run reported a session; it cannot be resumed"
                        .to_string(),
                );
                run.completed_at = Some(chrono::Utc::now().to_rfc3339());
            }
        }
        let Some(task_id) = task_id else { continue };
        // Treated as a crash, so the existing retry policy decides: one more
        // attempt if it has one, otherwise the operator's desk. Never a silent
        // hang.
        let outcome = SessionOutcome {
            status: "failed".to_string(),
            error: Some("interrupted by a gateway restart".to_string()),
            ..SessionOutcome::default()
        };
        crate::task_executor::apply_run_outcome_to_task(&control, &task_id, &outcome, None).await;
        released += 1;
        tracing::info!(run_id = %run_id, task_id = %task_id, "released a task whose run could not be resumed");
    }
    released
}

pub async fn resume_interrupted(control: Arc<ControlState>) -> usize {
    struct PendingResume {
        run_id: String,
        session_id: String,
        prompt: String,
        cwd: String,
        /// Set when the run was started by the task executor: the resumed
        /// outcome must close that task row too, or it stays `InProgress`
        /// forever (the executor writes no `startedAt`, so the stuck-task
        /// watchdog never rescues it).
        executor_task_id: Option<String>,
    }

    let pending: Vec<PendingResume> = {
        let runs = control.external_agent_runs.read().await;
        runs.iter()
            .filter(|r| r.status == "interrupted" && r.provider == "claude")
            .filter_map(|r| {
                let sid = r.metadata.get("sessionId").and_then(|v| v.as_str())?;
                let executor_task_id = (r.metadata.get("source").and_then(|v| v.as_str())
                    == Some("task-executor"))
                .then(|| r.task_id.clone())
                .flatten();
                Some(PendingResume {
                    run_id: r.id.clone(),
                    session_id: sid.to_string(),
                    prompt: r.prompt.clone(),
                    cwd: r.cwd.clone(),
                    executor_task_id,
                })
            })
            .collect()
    };
    if pending.is_empty() {
        return 0;
    }
    let count = pending.len();

    for resume in pending {
        {
            let mut runs = control.external_agent_runs.write().await;
            if let Some(run) = runs.iter_mut().find(|r| r.id == resume.run_id) {
                run.status = "running".to_string();
                run.error = None;
            }
        }
        let control_for_run = control.clone();
        let req = SessionRequest {
            run_id: resume.run_id.clone(),
            prompt: resume_prompt(&resume.prompt),
            cwd: resume.cwd.clone(),
            // Resumed work is unattended by definition — the operator isn't
            // sitting there to answer a prompt.
            permission_mode: PermissionMode::BypassPermissions,
            max_turns: None,
            resume_session_id: Some(resume.session_id.clone()),
            stall_timeout_secs: DEFAULT_STALL_TIMEOUT_SECS,
            mcp_config_path: None,
        };
        tracing::info!(run_id = %resume.run_id, "resuming interrupted claude session");
        tokio::spawn(async move {
            let outcome = run_claude_session(control_for_run.clone(), req).await;
            {
                let mut runs = control_for_run.external_agent_runs.write().await;
                if let Some(run) = runs.iter_mut().find(|r| r.id == resume.run_id) {
                    run.status = outcome.status.clone();
                    run.output = outcome.output.clone();
                    run.error = outcome.error.clone();
                    run.completed_at = Some(chrono::Utc::now().to_rfc3339());
                }
            }
            if let Some(task_id) = resume.executor_task_id.as_deref() {
                // No pre-run baseline exists for a resumed session, so no
                // evidence pack: the trailer verdict stands on its own here.
                crate::task_executor::apply_run_outcome_to_task(
                    &control_for_run,
                    task_id,
                    &outcome,
                    None,
                )
                .await;
            }
        });
    }
    count
}

#[cfg(test)]
mod release_tests {
    use super::*;
    use crate::state::{ExternalAgentRun, Task, TaskStatus};

    fn interrupted_run(id: &str, task_id: &str, session: Option<&str>) -> ExternalAgentRun {
        let mut metadata = serde_json::json!({ "source": "task-executor", "taskId": task_id });
        if let Some(s) = session {
            metadata["sessionId"] = serde_json::json!(s);
        }
        ExternalAgentRun {
            id: id.to_string(),
            workspace_id: "default".to_string(),
            entity_id: "e1".to_string(),
            provider: "claude".to_string(),
            prompt: "do it".to_string(),
            cwd: "/tmp".to_string(),
            status: "interrupted".to_string(),
            task_id: Some(task_id.to_string()),
            output: String::new(),
            error: None,
            started_at: "2026-08-01T00:00:00Z".to_string(),
            completed_at: None,
            metadata,
            review_policy: None,
            review_status: None,
            review_verdict: None,
            review_run_id: None,
            reviewed_run_id: None,
        }
    }

    #[tokio::test]
    async fn a_run_that_never_reported_a_session_hands_its_task_back() {
        let control = Arc::new(ControlState::new("openai", "https://x", "m"));
        let task = Task {
            id: "t1".to_string(),
            title: "wedged".to_string(),
            status: TaskStatus::InProgress,
            metadata: Some(serde_json::json!({
                "executable": true,
                "activeRunId": "run-1",
                "autoRunAttempted": "2026-08-01T00:00:00Z",
                "autoRunAttempts": "1",
            })),
            ..Task::default()
        };
        control.tasks.write().await.push(task);
        control
            .external_agent_runs
            .write()
            .await
            .push(interrupted_run("run-1", "t1", None));

        let released = release_unresumable_runs(control.clone()).await;
        assert_eq!(released, 1);

        let tasks = control.tasks.read().await;
        let after = tasks.iter().find(|t| t.id == "t1").unwrap();
        assert_ne!(
            after.status,
            TaskStatus::InProgress,
            "the task must not stay wedged in progress"
        );
    }

    #[tokio::test]
    async fn a_resumable_run_is_left_for_the_resume_path() {
        let control = Arc::new(ControlState::new("openai", "https://x", "m"));
        control.tasks.write().await.push(Task {
            id: "t2".to_string(),
            status: TaskStatus::InProgress,
            ..Task::default()
        });
        control
            .external_agent_runs
            .write()
            .await
            .push(interrupted_run("run-2", "t2", Some("sess-abc")));

        assert_eq!(release_unresumable_runs(control.clone()).await, 0);
        let tasks = control.tasks.read().await;
        assert_eq!(
            tasks.iter().find(|t| t.id == "t2").unwrap().status,
            TaskStatus::InProgress,
            "a resumable run keeps its claim so --resume can continue it"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> SessionRequest {
        SessionRequest {
            run_id: "run-1".to_string(),
            prompt: "do the thing".to_string(),
            cwd: "/tmp".to_string(),
            permission_mode: PermissionMode::BypassPermissions,
            max_turns: None,
            resume_session_id: None,
            stall_timeout_secs: DEFAULT_STALL_TIMEOUT_SECS,
            mcp_config_path: None,
        }
    }

    #[test]
    fn stream_json_always_carries_verbose() {
        // The CLI hard-errors on stream-json without --verbose.
        let args = build_args(&req());
        assert!(args.contains(&"--verbose".to_string()));
        assert!(args.contains(&"stream-json".to_string()));
    }

    #[test]
    fn the_prompt_travels_on_stdin_not_argv() {
        let mut r = req();
        r.max_turns = Some(3);
        r.resume_session_id = Some("sess-9".to_string());
        r.mcp_config_path = Some("/tmp/mcp.json".to_string());
        let args = build_args(&r);
        assert!(!args.contains(&"do the thing".to_string()), "{args:?}");
    }

    #[test]
    fn resume_is_only_passed_when_a_session_id_exists() {
        assert!(!build_args(&req()).contains(&"--resume".to_string()));
        let mut r = req();
        r.resume_session_id = Some("  ".to_string());
        assert!(!build_args(&r).contains(&"--resume".to_string()));
        r.resume_session_id = Some("sess-9".to_string());
        let args = build_args(&r);
        assert!(args.contains(&"--resume".to_string()));
        assert!(args.contains(&"sess-9".to_string()));
    }

    #[test]
    fn permission_modes_map_to_distinct_flags() {
        let mut r = req();
        assert!(build_args(&r).contains(&"--dangerously-skip-permissions".to_string()));

        r.permission_mode = PermissionMode::AcceptEdits;
        let args = build_args(&r);
        assert!(args.contains(&"--permission-mode".to_string()));
        assert!(args.contains(&"acceptEdits".to_string()));
        assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));

        r.permission_mode = PermissionMode::Default;
        let args = build_args(&r);
        assert!(!args.contains(&"--permission-mode".to_string()));
        assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn permission_mode_defaults_to_full_autonomy() {
        assert_eq!(
            PermissionMode::parse(None),
            PermissionMode::BypassPermissions
        );
        assert_eq!(
            PermissionMode::parse(Some("nonsense")),
            PermissionMode::BypassPermissions
        );
        assert_eq!(
            PermissionMode::parse(Some("acceptEdits")),
            PermissionMode::AcceptEdits
        );
        assert_eq!(
            PermissionMode::parse(Some("default")),
            PermissionMode::Default
        );
    }

    #[test]
    fn init_event_yields_the_session_id() {
        let mut acc = StreamAccumulator::default();
        acc.ingest(r#"{"type":"system","subtype":"init","session_id":"abc-123","model":"claude"}"#);
        assert_eq!(acc.session_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn assistant_text_blocks_accumulate_in_order() {
        let mut acc = StreamAccumulator::default();
        acc.ingest(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"first"}]}}"#,
        );
        acc.ingest(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"second"}]}}"#,
        );
        assert_eq!(acc.output(), "first\nsecond");
    }

    #[test]
    fn tool_use_blocks_are_not_treated_as_text() {
        let mut acc = StreamAccumulator::default();
        acc.ingest(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{}}]}}"#,
        );
        assert!(acc.output().is_empty());
    }

    #[test]
    fn the_result_event_supersedes_streamed_text() {
        let mut acc = StreamAccumulator::default();
        acc.ingest(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"thinking"}]}}"#,
        );
        acc.ingest(
            r#"{"type":"result","subtype":"success","result":"done","num_turns":4,"total_cost_usd":0.012}"#,
        );
        assert_eq!(acc.output(), "done");
        assert_eq!(acc.num_turns, Some(4));
        assert_eq!(acc.cost_usd, Some(0.012));
        assert!(!acc.is_error);
    }

    #[test]
    fn an_error_result_is_flagged_but_keeps_the_transcript() {
        let mut acc = StreamAccumulator::default();
        acc.ingest(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"partial"}]}}"#,
        );
        acc.ingest(r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#);
        assert!(acc.is_error);
        assert_eq!(acc.error_message.as_deref(), Some("error_max_turns"));
        assert_eq!(acc.output(), "partial");
    }

    #[test]
    fn unknown_events_are_ignored_not_fatal() {
        let mut acc = StreamAccumulator::default();
        acc.ingest(r#"{"type":"something_new_in_a_future_version","payload":{"a":1}}"#);
        assert!(acc.output().is_empty());
        assert!(!acc.is_error);
    }

    #[test]
    fn unparseable_lines_are_kept_as_raw_text() {
        let mut acc = StreamAccumulator::default();
        acc.ingest("not json at all");
        assert_eq!(acc.output(), "not json at all");
    }

    #[test]
    fn blank_lines_are_skipped() {
        let mut acc = StreamAccumulator::default();
        acc.ingest("");
        acc.ingest("   ");
        assert!(acc.output().is_empty());
    }

    #[test]
    fn a_resumed_session_adopts_the_new_session_id() {
        let mut acc = StreamAccumulator::default();
        acc.ingest(r#"{"type":"system","subtype":"init","session_id":"old"}"#);
        acc.ingest(r#"{"type":"result","session_id":"new","result":"ok"}"#);
        assert_eq!(acc.session_id.as_deref(), Some("new"));
    }

    #[test]
    fn the_transcript_tail_is_capped_and_stays_valid_utf8() {
        let mut acc = StreamAccumulator::default();
        // Multi-byte chars make a naive byte-slice panic.
        for _ in 0..40 {
            acc.ingest(&format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{}"}}]}}}}"#,
                "é".repeat(1000)
            ));
        }
        let out = acc.output();
        assert!(out.len() <= OUTPUT_TAIL_CAP + 16);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    /// Real output captured from `claude -p --output-format stream-json
    /// --verbose --max-turns 1` (CLI v2.1.206), trimmed of noise fields.
    ///
    /// Recorded rather than hand-written because the stream schema is not a
    /// documented contract: this is the only test that would notice the CLI
    /// changing its event shapes out from under us. Note the `hook_started`,
    /// `hook_response` and `rate_limit_event` rows — event types we never
    /// designed for, which is precisely why unknown types must be inert.
    const REAL_STREAM: &str = r#"{"type":"system","subtype":"hook_started","hook_name":"SessionStart:startup","session_id":"b6277ee6-7b92-41f3-8c8f-b8f0814c5c57"}
{"type":"system","subtype":"hook_response","hook_name":"SessionStart:startup","exit_code":0,"outcome":"success","session_id":"b6277ee6-7b92-41f3-8c8f-b8f0814c5c57"}
{"type":"system","subtype":"init","cwd":"/tmp/scratch","session_id":"b6277ee6-7b92-41f3-8c8f-b8f0814c5c57","model":"claude-opus-4-8","permissionMode":"default","claude_code_version":"2.1.206"}
{"type":"assistant","message":{"model":"claude-opus-4-8","role":"assistant","content":[{"type":"text","text":"OK"}],"usage":{"input_tokens":3346,"output_tokens":1}},"session_id":"b6277ee6-7b92-41f3-8c8f-b8f0814c5c57"}
{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour"},"session_id":"b6277ee6-7b92-41f3-8c8f-b8f0814c5c57"}
{"type":"result","subtype":"success","is_error":false,"duration_ms":1901,"num_turns":1,"result":"OK","stop_reason":"end_turn","session_id":"b6277ee6-7b92-41f3-8c8f-b8f0814c5c57","total_cost_usd":0.0520755,"terminal_reason":"completed"}"#;

    #[test]
    fn the_real_cli_stream_parses_end_to_end() {
        let mut acc = StreamAccumulator::default();
        for line in REAL_STREAM.lines() {
            acc.ingest(line);
        }
        assert_eq!(
            acc.session_id.as_deref(),
            Some("b6277ee6-7b92-41f3-8c8f-b8f0814c5c57"),
            "session id is what makes --resume possible"
        );
        assert_eq!(acc.output(), "OK");
        assert_eq!(acc.num_turns, Some(1));
        assert_eq!(acc.cost_usd, Some(0.0520755));
        assert!(!acc.is_error);
        // Hook and rate-limit events must not leak into the transcript.
        assert!(!acc.text.contains("hook"));
        assert!(!acc.text.contains("rate_limit"));
    }

    #[test]
    fn a_rate_limit_event_is_captured_with_its_reset_time() {
        let mut acc = StreamAccumulator::default();
        // Exact shape emitted by CLI v2.1.206.
        acc.ingest(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1785168600,"rateLimitType":"five_hour"}}"#,
        );
        assert_eq!(acc.rate_limit_status.as_deref(), Some("allowed"));
        assert_eq!(acc.rate_limit_resets_at, Some(1785168600));
        // "allowed" is the healthy case — it must not look like a limit.
        assert!(!acc.hit_rate_limit());
    }

    #[test]
    fn an_allowed_warning_is_not_a_spent_window() {
        let mut acc = StreamAccumulator::default();
        acc.ingest(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1785168600,"rateLimitType":"five_hour"}}"#,
        );
        acc.ingest(r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"name\":\"ok\"}"}"#);
        assert_eq!(acc.rate_limit_status.as_deref(), Some("allowed_warning"));
        assert!(
            !acc.hit_rate_limit(),
            "a warning means the run kept going; it must not be read as rejected"
        );
        assert!(!acc.is_error);
    }

    #[test]
    fn a_rejected_window_reads_as_rate_limited() {
        let mut acc = StreamAccumulator::default();
        acc.ingest(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1785168600,"rateLimitType":"five_hour"}}"#,
        );
        assert!(acc.hit_rate_limit());
        assert_eq!(acc.rate_limit_resets_at, Some(1785168600));
    }

    #[test]
    fn a_usage_limit_stated_only_in_prose_is_still_caught() {
        // Belt and braces: the CLI can report the same condition as text.
        let mut acc = StreamAccumulator::default();
        acc.ingest(r#"{"type":"result","is_error":true,"subtype":"error","result":"Claude usage limit reached"}"#);
        assert!(acc.hit_rate_limit());
    }

    #[test]
    fn an_ordinary_error_is_not_mistaken_for_a_rate_limit() {
        let mut acc = StreamAccumulator::default();
        acc.ingest(r#"{"type":"result","is_error":true,"subtype":"error_max_turns"}"#);
        assert!(!acc.hit_rate_limit());
    }

    #[test]
    fn args_always_name_a_model() {
        let args = build_args(&req());
        let at = args
            .iter()
            .position(|a| a == "--model")
            .expect("--model is always passed");
        assert_eq!(
            args[at + 1],
            consistent_world_llm::claude_code::DEFAULT_MODEL_ALIAS,
            "the CLI's own default can be a model it cannot serve"
        );
        assert!(
            !args.contains(&"do the thing".to_string()),
            "the prompt goes on stdin, not argv"
        );
    }

    #[test]
    fn error_result_reports_the_real_failure_not_the_subtype() {
        let mut acc = StreamAccumulator::default();
        acc.ingest(
            r#"{"type":"result","subtype":"success","is_error":true,"result":"API Error: 400 this model is not supported by your CLI version"}"#,
        );
        assert!(acc.is_error);
        assert_eq!(
            acc.error_message.as_deref(),
            Some("API Error: 400 this model is not supported by your CLI version")
        );

        let mut bare = StreamAccumulator::default();
        bare.ingest(r#"{"type":"result","subtype":"success","is_error":true}"#);
        assert_eq!(
            bare.error_message.as_deref(),
            Some("the agent reported an error"),
            "a bare success subtype is never shown as the error"
        );

        let mut typed = StreamAccumulator::default();
        typed.ingest(r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#);
        assert_eq!(typed.error_message.as_deref(), Some("error_max_turns"));
    }

    #[test]
    fn the_resume_prompt_orients_before_continuing() {
        let p = resume_prompt("Refactor the auth module");
        assert!(
            p.contains("Refactor the auth module"),
            "keeps the original task"
        );
        assert!(p.contains("Do not redo completed work"));
    }

    #[tokio::test]
    async fn cancelling_an_unknown_run_reports_no_live_process() {
        let registry = AgentProcessRegistry::default();
        assert!(!registry.cancel("nope").await);
        assert!(!registry.is_running("nope").await);
    }

    #[tokio::test]
    async fn a_registered_run_can_be_cancelled_once() {
        let registry = AgentProcessRegistry::default();
        let mut rx = registry.register("run-7").await;
        assert!(registry.is_running("run-7").await);
        assert!(registry.cancel("run-7").await);
        assert!(rx.recv().await.is_some());
        registry.deregister("run-7").await;
        assert!(!registry.cancel("run-7").await);
    }
}
