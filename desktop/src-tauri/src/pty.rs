//! Embedded terminal — a real pseudo-terminal behind the workspace's xterm.js pane.
//!
//! The reason this exists is sign-in. `codex login` and `claude` both run an
//! interactive flow that prints a URL, opens a browser and then waits on a TTY
//! for the round trip to finish. Driving that through a pipe is a losing
//! battle — the CLIs notice there is no terminal and change behaviour — so the
//! app gives them the real thing: a PTY whose master side is streamed to the
//! webview as raw bytes and fed keystrokes back from it. The same machinery
//! doubles as an ordinary shell for operators who want one inside the app.
//!
//! # Why every spawn goes through the login shell
//!
//! A Finder-launched macOS app inherits launchd's environment, whose PATH is
//! `/usr/bin:/bin:/usr/sbin:/sbin` — nothing under `/usr/local/bin` or
//! `/opt/homebrew/bin`, which is exactly where `codex` and `claude` live.
//! Terminal.app sees the fuller PATH only because it runs a *login* shell that
//! sources `/etc/zprofile` (path_helper) and the user's own profile. So every
//! session here is `<shell> -l ...`: an interactive login shell for the
//! free-form case, or a login shell that runs the profiles and then `exec`s
//! the requested program with its argv handed over verbatim, so no user
//! argument ever passes through a layer of shell quoting.
//!
//! # Layout
//!
//! The PTY mechanics (`spawn_pty`, `SessionHandle`) know nothing about Tauri.
//! They take two callbacks — one per output chunk, one on exit — and drive
//! them from plain `std::thread`s, which is what lets `cargo test` exercise a
//! real shell with no app running. The `#[tauri::command]`s at the bottom are
//! thin wrappers that turn those callbacks into `pty-output` / `pty-exit`
//! events and keep the session table the webview addresses by id.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use portable_pty::{native_pty_system, Child, CommandBuilder, ExitStatus, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

/// Upper bound on live terminals. Each one is a process, two threads and a
/// pair of descriptors; a webview that leaks sessions should hit this wall,
/// not the operator's ulimit.
const MAX_SESSIONS: usize = 8;

/// Master-side read size. Large enough that a screenful of output travels as
/// one event, small enough that no chunk waits long behind serialisation.
const READ_CHUNK: usize = 8 * 1024;

/// How often the exit watcher polls `try_wait`. See `spawn_pty` for why it
/// polls instead of parking in `wait`.
const EXIT_POLL: Duration = Duration::from_millis(50);

/// Once the child is gone, how long to give the reader to deliver the last
/// bytes before the exit is announced regardless. The reader normally finishes
/// within a millisecond of the child; the cap only matters when a grandchild
/// kept the slave side open (a daemon the user backgrounded, say).
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// What the webview sends to open a terminal. Mirrors the frontend contract
/// exactly; anything not expressible from JavaScript lives on `PtySpec`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtySpawnArgs {
    /// Program plus arguments, run through the login shell and then `exec`ed
    /// with the argv preserved exactly. Wins over `script` if both are given.
    pub command: Option<Vec<String>>,
    /// A shell one-liner (`codex logout; codex login`), for compound commands.
    pub script: Option<String>,
    /// Working directory. Falls back to the home directory when absent or gone.
    pub cwd: Option<String>,
    pub cols: u16,
    pub rows: u16,
}

/// Everything `spawn_pty` needs, Tauri-free. Built from `PtySpawnArgs` by the
/// command and directly by tests.
pub struct PtySpec {
    pub command: Option<Vec<String>>,
    pub script: Option<String>,
    pub cwd: Option<PathBuf>,
    pub cols: u16,
    pub rows: u16,
    /// Applied on top of the inherited environment, before the shell runs.
    /// The app sets none; tests use it to fake a Finder-launched PATH and
    /// prove the login shell repairs it.
    pub env: Vec<(String, String)>,
}

impl From<PtySpawnArgs> for PtySpec {
    fn from(args: PtySpawnArgs) -> Self {
        PtySpec {
            command: args.command,
            script: args.script,
            cwd: args.cwd.map(PathBuf::from),
            cols: args.cols,
            rows: args.rows,
            env: Vec::new(),
        }
    }
}

/// `pty-output` payload: bytes exactly as they left the master side. Not a
/// string on purpose — a chunk boundary can split a multi-byte character, and
/// xterm.js already reassembles those; decoding here would only corrupt them.
#[derive(Serialize, Clone)]
struct PtyOutput {
    id: String,
    data: Vec<u8>,
}

/// `pty-exit` payload, sent exactly once per session. `code` is `None` when
/// the process died from a signal (including our own kill).
#[derive(Serialize, Clone)]
struct PtyExit {
    id: String,
    code: Option<i32>,
}

/// Lock that survives a poisoned mutex. A panic in one terminal's thread must
/// not wedge every other terminal, and nothing guarded here is left in a
/// half-written state by a panic.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The child behind its lock, plus the one fact `kill` needs that the child
/// itself no longer knows once it has been reaped.
struct ChildSlot {
    child: Box<dyn Child + Send + Sync>,
    /// Set by the exit watcher, under this same lock, the moment `try_wait`
    /// reports the child gone. `kill` checks it first: portable-pty signals by
    /// raw pid, and a pid that has been reaped may already belong to some
    /// unrelated process.
    exited: bool,
}

/// The master's write end, shareable so a write never needs the session
/// table's lock. A PTY write blocks when the child stops reading its input
/// queue; with this only that terminal stalls, not every terminal command.
#[derive(Clone)]
pub struct PtyWriter(Arc<Mutex<Box<dyn Write + Send>>>);

impl PtyWriter {
    pub fn write(&self, data: &[u8]) -> Result<(), String> {
        let mut w = lock(&self.0);
        w.write_all(data)
            .and_then(|_| w.flush())
            .map_err(|e| format!("could not write to the terminal: {e}"))
    }
}

/// One live terminal: the master (for resize), its writer, and the child (for
/// kill). Dropping it closes the master side; the reader thread keeps its own
/// descriptor until it sees EOF, so a drop never races the last output.
pub struct SessionHandle {
    master: Box<dyn MasterPty + Send>,
    writer: PtyWriter,
    child: Arc<Mutex<ChildSlot>>,
}

/// What the Tauri session table stores. Same thing; the name says which side
/// of the seam you are on.
pub type PtySession = SessionHandle;

impl SessionHandle {
    /// The write end, detached so the caller can drop whatever lock it found
    /// this session under before the (possibly blocking) write.
    pub fn writer(&self) -> PtyWriter {
        self.writer.clone()
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        self.master
            .resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("could not resize the terminal: {e}"))
    }

    /// End the process. On unix portable-pty sends SIGHUP — what closing a
    /// terminal window does — waits briefly for a graceful exit, then SIGKILLs.
    /// Safe to call after the child has already gone; the exit watcher will
    /// still report exactly once.
    pub fn kill(&self) {
        let mut slot = lock(&self.child);
        if slot.exited {
            return;
        }
        let _ = slot.child.kill();
    }
}

/// Map portable-pty's status onto the wire shape: a code for a normal exit,
/// nothing for a signal death (portable-pty reports those as code 1, which
/// would be indistinguishable from a program that returned 1).
fn exit_code(status: &ExitStatus) -> Option<i32> {
    if status.signal().is_some() {
        None
    } else {
        Some(status.exit_code() as i32)
    }
}

/// The shell to run everything under: whatever the user chose, else the
/// platform's stock one.
fn default_shell() -> String {
    // Not on Windows: a `SHELL` there is Git Bash's or MSYS's own POSIX path
    // (`/usr/bin/bash`), which `CreateProcess` cannot resolve.
    #[cfg(unix)]
    if let Some(shell) = std::env::var_os("SHELL") {
        if !shell.is_empty() {
            return shell.to_string_lossy().into_owned();
        }
    }
    platform_shell().to_string()
}

#[cfg(target_os = "macos")]
fn platform_shell() -> &'static str {
    "/bin/zsh"
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_shell() -> &'static str {
    "/bin/bash"
}

/// PowerShell 7 when it is installed — it accepts `&&`, which pasted commands
/// and install instructions tend to use — else the 5.1 every Windows ships.
#[cfg(windows)]
fn platform_shell() -> &'static str {
    let has_pwsh = std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| dir.join("pwsh.exe").is_file())
    });
    if has_pwsh {
        "pwsh.exe"
    } else {
        "powershell.exe"
    }
}

/// The one-liner a login shell runs to hand over to the real program. POSIX
/// shells expose the trailing arguments as `$0` and `$@`; fish spells the same
/// thing `$argv`. Either way the arguments arrive exactly as given — there is
/// no quoting layer for a stray space or `$` to slip through.
#[cfg(unix)]
fn exec_trampoline(shell: &str) -> &'static str {
    let is_fish = std::path::Path::new(shell)
        .file_name()
        .map_or(false, |name| name == "fish");
    if is_fish {
        "exec $argv"
    } else {
        "exec \"$0\" \"$@\""
    }
}

/// The argv to spawn for a spec, always starting with a login shell so the
/// child sees Terminal.app's PATH rather than launchd's.
///
/// `-l` alone is not enough: it sources `.zprofile` but not `.zshrc`, and the
/// CLIs this exists to run (`codex`, `claude`) are npm globals — under nvm they
/// only reach PATH through the rc file. So the `-c` forms add `-i` too, the
/// same `-ilc` VS Code uses to resolve the user's real environment. The cost is
/// that a chatty rc file prints before the command; in a visible terminal that
/// is fine, and far better than "codex: command not found".
#[cfg(unix)]
fn shell_argv(shell: &str, spec: &PtySpec) -> Result<Vec<String>, String> {
    let mut argv = vec![shell.to_string(), "-l".to_string()];
    match (&spec.command, &spec.script) {
        (Some(command), _) => {
            if command.is_empty() {
                return Err("command must name a program to run".to_string());
            }
            argv.push("-i".to_string());
            argv.push("-c".to_string());
            argv.push(exec_trampoline(shell).to_string());
            argv.extend(command.iter().cloned());
        }
        (None, Some(script)) => {
            argv.push("-i".to_string());
            argv.push("-c".to_string());
            argv.push(script.clone());
        }
        // Bare login shell. A PTY on stdin makes it interactive by itself; no
        // `-i` needed, and adding one makes bash re-read rc files twice.
        (None, None) => {}
    }
    Ok(argv)
}

/// Windows has no login-shell PATH problem (Explorer-launched apps get the
/// user's PATH), so a command runs directly — portable-pty resolves a bare
/// `codex` to `codex.cmd` through `PATHEXT` — while scripts and the bare case
/// go through PowerShell. The sign-in scripts (`codex logout; codex login`)
/// only use `;`, which every PowerShell accepts.
#[cfg(windows)]
fn shell_argv(shell: &str, spec: &PtySpec) -> Result<Vec<String>, String> {
    match (&spec.command, &spec.script) {
        (Some(command), _) => {
            if command.is_empty() {
                return Err("command must name a program to run".to_string());
            }
            Ok(command.clone())
        }
        (None, Some(script)) => Ok(vec![
            shell.to_string(),
            "-NoLogo".to_string(),
            "-Command".to_string(),
            script.clone(),
        ]),
        (None, None) => Ok(vec![shell.to_string(), "-NoLogo".to_string()]),
    }
}

/// Everything about the process except the terminal it runs in.
fn build_command(spec: &PtySpec) -> Result<CommandBuilder, String> {
    let shell = default_shell();
    let argv = shell_argv(&shell, spec)?;
    let mut cmd = CommandBuilder::new(&argv[0]);
    cmd.args(&argv[1..]);

    // A directory that vanished between the webview asking and us spawning
    // would make the spawn fail outright; home is always a sensible answer.
    let cwd = spec
        .cwd
        .clone()
        .filter(|dir| dir.is_dir())
        .or_else(dirs::home_dir);
    if let Some(dir) = cwd {
        cmd.cwd(dir);
    }

    // The environment is inherited; these are added on top. TERM/COLORTERM
    // tell CLIs what xterm.js can render, CONSISTENT_DESKTOP lets a profile
    // or tool know it is inside the app, and LANG guards against a launchd
    // environment with no locale at all, which makes some CLIs print `?`
    // for every non-ASCII character.
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("CONSISTENT_DESKTOP", "1");
    if std::env::var_os("LANG").map_or(true, |lang| lang.is_empty()) {
        cmd.env("LANG", "en_US.UTF-8");
    }
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }
    Ok(cmd)
}

/// Open a PTY, start the process in it, and stream its life to two callbacks.
///
/// `on_output` is called from a reader thread with each chunk as it arrives.
/// `on_exit` is called exactly once, from a second thread, after the child is
/// gone and the reader has had its chance to drain — so it always follows
/// the last `on_output`, barring a grandchild that outlives the child.
///
/// The exit watcher polls `try_wait` rather than parking in `wait` because
/// `kill` needs the same child: the real child's `kill` escalates SIGHUP to
/// SIGKILL, while the detachable `clone_killer` only ever sends SIGHUP and
/// would leave a process that ignores it running forever. Sharing the child
/// under one lock, with `ChildSlot::exited` set under that lock, gives both
/// the escalation and a guarantee that a reaped pid is never signalled.
pub fn spawn_pty(
    spec: &PtySpec,
    mut on_output: impl FnMut(Vec<u8>) + Send + 'static,
    on_exit: impl FnOnce(Option<i32>) + Send + 'static,
) -> Result<SessionHandle, String> {
    let cmd = build_command(spec)?;
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: spec.rows.max(1),
            cols: spec.cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("could not open a pseudo-terminal: {e}"))?;
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("could not start the terminal process: {e}"))?;
    // The child has its own copies of the slave descriptor. Ours must go now:
    // a reader on the master only sees EOF once *every* slave descriptor is
    // closed, and keeping this one is the classic way to hang after exit.
    drop(pair.slave);

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("could not attach to the terminal output: {e}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("could not attach to the terminal input: {e}"))?;
    let child = Arc::new(Mutex::new(ChildSlot {
        child,
        exited: false,
    }));

    // Dropped when the reader finishes; the watcher waits on it so an exit is
    // never announced ahead of output the child wrote just before dying.
    let (drained_tx, drained_rx) = mpsc::channel::<()>();

    let spawn_reader = thread::Builder::new().name("pty-reader".into()).spawn({
        let mut reader = reader;
        move || {
            let mut buf = vec![0u8; READ_CHUNK];
            loop {
                match reader.read(&mut buf) {
                    // 0 is EOF; portable-pty also folds the EIO Linux raises
                    // once the slave closes into it.
                    Ok(0) => break,
                    Ok(n) => on_output(buf[..n].to_vec()),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            drop(drained_tx);
        }
    });
    if let Err(e) = spawn_reader {
        let _ = lock(&child).child.kill();
        return Err(format!("could not start the terminal reader: {e}"));
    }

    let spawn_watcher = thread::Builder::new().name("pty-exit".into()).spawn({
        let watched = child.clone();
        move || {
            let code = loop {
                let mut slot = lock(&watched);
                match slot.child.try_wait() {
                    Ok(Some(status)) => {
                        slot.exited = true;
                        break exit_code(&status);
                    }
                    Ok(None) => {}
                    // ECHILD and friends: someone else reaped it. It is gone
                    // either way, and we must not signal its pid again.
                    Err(_) => {
                        slot.exited = true;
                        break None;
                    }
                }
                drop(slot);
                thread::sleep(EXIT_POLL);
            };
            let _ = drained_rx.recv_timeout(DRAIN_GRACE);
            on_exit(code);
        }
    });
    if let Err(e) = spawn_watcher {
        let _ = lock(&child).child.kill();
        return Err(format!("could not start the terminal exit watcher: {e}"));
    }

    Ok(SessionHandle {
        master: pair.master,
        writer: PtyWriter(Arc::new(Mutex::new(writer))),
        child,
    })
}

// ---------------------------------------------------------------------------
// Tauri surface
// ---------------------------------------------------------------------------

/// Live terminals by id. Managed by the app; also drained on window destroy
/// and tray quit so no shell outlives the window it was typed into.
#[derive(Default)]
pub struct PtySessions(pub Arc<Mutex<HashMap<String, PtySession>>>);

impl PtySessions {
    /// Kill every session and forget them. Each exit watcher still fires and
    /// finds its row already gone, which is fine — the event goes to a
    /// window that is closing anyway.
    pub fn kill_all(&self) {
        let sessions: Vec<PtySession> = lock(&self.0).drain().map(|(_, s)| s).collect();
        for session in &sessions {
            session.kill();
        }
    }
}

/// Open a terminal. Returns the id the other commands and both events use.
#[tauri::command]
pub async fn pty_spawn(
    app: AppHandle,
    state: State<'_, PtySessions>,
    args: PtySpawnArgs,
) -> Result<String, String> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let table = state.0.clone();

    // The table lock is held across spawn and insert on purpose. A command
    // that fails instantly can exit before the insert; its watcher then
    // blocks on this lock until the row exists and removes it, instead of
    // removing nothing and leaving a dead session counted against the cap.
    let mut sessions = lock(&table);
    if sessions.len() >= MAX_SESSIONS {
        return Err(format!(
            "too many terminals open ({MAX_SESSIONS}); close one before opening another"
        ));
    }

    let output_app = app.clone();
    let output_id = id.clone();
    let exit_table = table.clone();
    let exit_id = id.clone();
    let session = spawn_pty(
        &PtySpec::from(args),
        move |data| {
            let _ = output_app.emit(
                "pty-output",
                PtyOutput {
                    id: output_id.clone(),
                    data,
                },
            );
        },
        move |code| {
            // Forget the session before telling the webview, so a `pty_kill`
            // it sends in response finds nothing to signal.
            lock(&exit_table).remove(&exit_id);
            let _ = app.emit("pty-exit", PtyExit { id: exit_id, code });
        },
    )?;
    sessions.insert(id.clone(), session);
    Ok(id)
}

/// Keystrokes (or a paste) from xterm.js, as the UTF-8 the terminal typed.
#[tauri::command]
pub async fn pty_write(
    state: State<'_, PtySessions>,
    id: String,
    data: String,
) -> Result<(), String> {
    let writer = lock(&state.0)
        .get(&id)
        .map(SessionHandle::writer)
        .ok_or_else(|| format!("no terminal with id {id}"))?;
    writer.write(data.as_bytes())
}

/// The pane changed size; tell the kernel so full-screen programs redraw.
#[tauri::command]
pub async fn pty_resize(
    state: State<'_, PtySessions>,
    id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    lock(&state.0)
        .get(&id)
        .ok_or_else(|| format!("no terminal with id {id}"))?
        .resize(cols, rows)
}

/// Close a terminal. Idempotent: an id that is already gone is a success,
/// because the webview and the exit watcher race to forget the same session.
#[tauri::command]
pub async fn pty_kill(state: State<'_, PtySessions>, id: String) -> Result<(), String> {
    let Some(session) = lock(&state.0).remove(&id) else {
        return Ok(());
    };
    // The kill sleeps up to a quarter second while it escalates; keep that
    // off the async runtime's threads.
    tauri::async_runtime::spawn_blocking(move || session.kill())
        .await
        .map_err(|e| format!("terminal kill did not complete: {e}"))
}

/// Hosts a sign-in flow may send the user to. Matched as the host itself or
/// any subdomain of it — never as a substring, so `notclaude.ai` and
/// `claude.ai.evil.com` both fail.
const SIGNIN_HOSTS: &[&str] = &[
    "auth.openai.com",
    "chatgpt.com",
    "platform.openai.com",
    "claude.ai",
    "console.anthropic.com",
    "anthropic.com",
];

/// Is this a URL the app is willing to hand to the system browser on the
/// terminal's behalf? Only `https://`, only the vendor sign-in hosts, and
/// nothing that could smuggle a second host or a second line past `open`.
fn check_signin_url(url: &str) -> Result<(), String> {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("refusing a sign-in URL containing whitespace or control characters".into());
    }
    let (scheme, rest) = url.split_at(url.len().min("https://".len()));
    if !scheme.eq_ignore_ascii_case("https://") {
        return Err("only https:// sign-in links can be opened".to_string());
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // `user@host`, an explicit port, an IPv6 literal, or a backslash that a
    // browser might read as a slash: none of these belong in a sign-in link.
    if authority.is_empty() || authority.contains(['@', ':', '\\']) {
        return Err("refusing a sign-in URL with an unexpected host".to_string());
    }
    let host = authority.trim_end_matches('.').to_ascii_lowercase();
    let allowed = SIGNIN_HOSTS
        .iter()
        .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")));
    if allowed {
        Ok(())
    } else {
        Err(format!("refusing to open {host}: not a known sign-in host"))
    }
}

/// Open the URL a sign-in flow printed, in the real browser — where the
/// user's existing OpenAI / Anthropic session is, and where a link the
/// webview navigated itself would on macOS become a download instead.
#[tauri::command]
pub fn open_signin_url(url: String) -> Result<(), String> {
    check_signin_url(&url)?;
    crate::login::launch_system_browser(&url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signin_allowlist_accepts_vendor_hosts_and_subdomains() {
        for url in [
            "https://auth.openai.com/authorize?client_id=x&state=y",
            "https://chatgpt.com/",
            "https://platform.openai.com/apps",
            "https://claude.ai/login",
            "https://console.anthropic.com/settings/keys",
            "https://anthropic.com",
            "https://www.anthropic.com/pricing#top",
            "https://login.claude.ai/oauth",
            "HTTPS://CLAUDE.AI/login",
        ] {
            assert!(check_signin_url(url).is_ok(), "should allow {url}");
        }
    }

    #[test]
    fn signin_allowlist_rejects_everything_else() {
        for url in [
            "http://claude.ai/login",
            "https://evil.com/claude.ai",
            "https://claude.ai@evil.com/",
            "https://notclaude.ai/",
            "https://claude.ai.evil.com/",
            "https://claude.ai:8443/",
            "https://claude.ai/\nopen -a Calculator",
            "https://claude.ai/\r\n",
            "https://claude.ai\\@evil.com/",
            "https://",
            "https:///claude.ai",
            "file:///etc/passwd",
            "",
        ] {
            assert!(check_signin_url(url).is_err(), "should reject {url:?}");
        }
    }

    fn spec(command: Option<&[&str]>, script: Option<&str>) -> PtySpec {
        PtySpec {
            command: command.map(|argv| argv.iter().map(|s| s.to_string()).collect()),
            script: script.map(str::to_string),
            cwd: None,
            cols: 80,
            rows: 24,
            env: Vec::new(),
        }
    }

    #[cfg(windows)]
    mod windows {
        use super::*;

        #[test]
        fn a_command_runs_directly_without_a_shell() {
            assert_eq!(
                shell_argv("powershell.exe", &spec(Some(&["codex", "login"]), None)).unwrap(),
                ["codex", "login"]
            );
        }

        #[test]
        fn a_script_goes_through_powershell() {
            assert_eq!(
                shell_argv("pwsh.exe", &spec(None, Some("codex logout; codex login"))).unwrap(),
                [
                    "pwsh.exe",
                    "-NoLogo",
                    "-Command",
                    "codex logout; codex login"
                ]
            );
        }

        #[test]
        fn the_shell_is_powershell_whatever_shell_says() {
            assert!(default_shell().ends_with(".exe"), "{}", default_shell());
        }
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::path::Path;
        use std::time::Instant;

        #[test]
        fn argv_bare_is_a_login_shell() {
            assert_eq!(
                shell_argv("/bin/zsh", &spec(None, None)).unwrap(),
                ["/bin/zsh", "-l"]
            );
        }

        #[test]
        fn argv_command_execs_through_login_shell_unquoted() {
            let argv =
                shell_argv("/bin/zsh", &spec(Some(&["codex", "login", "a b$c"]), None)).unwrap();
            assert_eq!(
                argv,
                [
                    "/bin/zsh",
                    "-l",
                    "-i",
                    "-c",
                    "exec \"$0\" \"$@\"",
                    "codex",
                    "login",
                    "a b$c"
                ]
            );
        }

        #[test]
        fn argv_script_runs_through_login_shell() {
            let argv =
                shell_argv("/bin/bash", &spec(None, Some("codex logout; codex login"))).unwrap();
            assert_eq!(
                argv,
                ["/bin/bash", "-l", "-i", "-c", "codex logout; codex login"]
            );
        }

        #[test]
        fn argv_command_wins_over_script() {
            let argv = shell_argv("/bin/zsh", &spec(Some(&["claude"]), Some("ignored"))).unwrap();
            assert_eq!(
                argv,
                ["/bin/zsh", "-l", "-i", "-c", "exec \"$0\" \"$@\"", "claude"]
            );
        }

        #[test]
        fn argv_empty_command_is_an_error() {
            assert!(shell_argv("/bin/zsh", &spec(Some(&[]), None)).is_err());
        }

        #[test]
        fn argv_fish_uses_its_own_trampoline() {
            let argv = shell_argv(
                "/opt/homebrew/bin/fish",
                &spec(Some(&["codex", "login"]), None),
            )
            .unwrap();
            assert_eq!(
                argv,
                [
                    "/opt/homebrew/bin/fish",
                    "-l",
                    "-i",
                    "-c",
                    "exec $argv",
                    "codex",
                    "login"
                ]
            );
        }

        #[derive(Debug)]
        enum Ev {
            Out(Vec<u8>),
            Exit(Option<i32>),
        }

        fn spawn_collect(spec: &PtySpec) -> (SessionHandle, mpsc::Receiver<Ev>) {
            let (tx, rx) = mpsc::channel();
            let out_tx = tx.clone();
            let handle = spawn_pty(
                spec,
                move |bytes| {
                    let _ = out_tx.send(Ev::Out(bytes));
                },
                move |code| {
                    let _ = tx.send(Ev::Exit(code));
                },
            )
            .expect("spawn_pty");
            (handle, rx)
        }

        /// Collect output until the exit callback fires or `timeout` passes.
        /// The second element is `None` when the deadline hit first.
        fn drain(rx: &mpsc::Receiver<Ev>, timeout: Duration) -> (String, Option<Option<i32>>) {
            let deadline = Instant::now() + timeout;
            let mut out = Vec::new();
            loop {
                let now = Instant::now();
                if now >= deadline {
                    return (String::from_utf8_lossy(&out).into_owned(), None);
                }
                match rx.recv_timeout(deadline - now) {
                    Ok(Ev::Out(bytes)) => out.extend(bytes),
                    Ok(Ev::Exit(code)) => {
                        return (String::from_utf8_lossy(&out).into_owned(), Some(code));
                    }
                    Err(_) => return (String::from_utf8_lossy(&out).into_owned(), None),
                }
            }
        }

        fn run(spec: &PtySpec) -> (String, Option<Option<i32>>) {
            let (handle, rx) = spawn_collect(spec);
            let result = drain(&rx, Duration::from_secs(10));
            if result.1.is_none() {
                handle.kill();
            }
            result
        }

        fn shell_available() -> bool {
            let shell = default_shell();
            if Path::new(&shell).exists() {
                true
            } else {
                eprintln!("skipping: shell {shell} does not exist on this machine");
                false
            }
        }

        #[test]
        fn command_output_and_exit_code() {
            if !shell_available() {
                return;
            }
            let (out, exit) = run(&spec(Some(&["printf", "hello-pty"]), None));
            assert!(out.contains("hello-pty"), "output was {out:?}");
            assert_eq!(exit, Some(Some(0)), "exit should fire with code 0");
        }

        #[test]
        fn command_exit_code_is_reported() {
            if !shell_available() {
                return;
            }
            let (_, exit) = run(&spec(Some(&["sh", "-c", "exit 7"]), None));
            assert_eq!(exit, Some(Some(7)));
        }

        #[test]
        fn script_runs_compound_commands() {
            if !shell_available() {
                return;
            }
            let (out, exit) = run(&spec(None, Some("printf one; printf -- '-two'")));
            assert!(out.contains("one-two"), "output was {out:?}");
            assert_eq!(exit, Some(Some(0)));
        }

        #[test]
        fn interactive_login_shell_round_trip() {
            if !shell_available() {
                return;
            }
            let (handle, rx) = spawn_collect(&spec(None, None));
            // Wait for the prompt so the shell is unquestionably reading the
            // tty before we type; a type-ahead would also work (the line
            // discipline buffers it) but this keeps the test honest.
            let mut out = Vec::new();
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ev::Out(bytes)) => out.extend(bytes),
                other => {
                    handle.kill();
                    panic!("no prompt from the login shell: {other:?}");
                }
            }
            // `PTY_O''K` so the tty's echo of what we typed cannot satisfy
            // the assertion; only the shell's own output reads `PTY_OK`.
            let writer = handle.writer();
            writer.write(b"echo PTY_O''K\n").unwrap();
            writer.write(b"exit\n").unwrap();
            let (rest, exit) = drain(&rx, Duration::from_secs(10));
            if exit.is_none() {
                handle.kill();
            }
            let all = String::from_utf8_lossy(&out).into_owned() + &rest;
            assert!(all.contains("PTY_OK"), "output was {all:?}");
            assert_eq!(exit, Some(Some(0)), "shell should exit cleanly");
        }

        #[test]
        fn command_sees_login_shell_path() {
            if !shell_available() {
                return;
            }
            let present: Vec<&str> = ["/usr/local/bin", "/opt/homebrew/bin"]
                .into_iter()
                .filter(|dir| Path::new(dir).is_dir())
                .collect();
            if present.is_empty() {
                eprintln!(
                    "skipping: neither /usr/local/bin nor /opt/homebrew/bin exists on this machine"
                );
                return;
            }
            // Start from launchd's PATH, the one a Finder-launched app has,
            // and prove the login shell puts the tool directories back.
            let mut spec = spec(Some(&["sh", "-c", "echo $PATH"]), None);
            spec.env
                .push(("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()));
            let (out, exit) = run(&spec);
            assert_eq!(exit, Some(Some(0)));
            assert!(
                present.iter().any(|dir| out.contains(dir)),
                "login-shell PATH lacks {present:?}: {out:?}"
            );
        }

        /// `-l` reads the profile; `-i` is what reads the rc file, and the rc
        /// file is where nvm and friends put npm globals like `codex` on PATH.
        /// Point the shell at a scratch startup file that exports a marker and
        /// make sure the exec'd program inherits it. (bash's login+interactive
        /// mode reads `.bash_profile`, not `.bashrc`, hence the per-shell file.)
        #[test]
        fn command_sees_rc_file_environment() {
            if !shell_available() {
                return;
            }
            let shell = default_shell();
            let name = Path::new(&shell)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let (rc_env, rc_file) = match name.as_str() {
                "zsh" => ("ZDOTDIR", ".zshrc"),
                "bash" => ("HOME", ".bash_profile"),
                other => {
                    eprintln!("skipping: no startup-file convention known for shell {other:?}");
                    return;
                }
            };
            let dir =
                std::env::temp_dir().join(format!("consistent-pty-rc-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("scratch rc dir");
            std::fs::write(dir.join(rc_file), "export PTY_RC_MARK=sourced\n").expect("write rc");
            let mut spec = spec(Some(&["sh", "-c", "echo rc=$PTY_RC_MARK"]), None);
            spec.env
                .push((rc_env.into(), dir.to_string_lossy().into_owned()));
            let (out, exit) = run(&spec);
            let _ = std::fs::remove_dir_all(&dir);
            assert_eq!(exit, Some(Some(0)));
            assert!(
                out.contains("rc=sourced"),
                "startup file was not sourced: {out:?}"
            );
        }

        #[test]
        fn env_marks_the_desktop_terminal() {
            if !shell_available() {
                return;
            }
            let (out, _) = run(&spec(
                Some(&["sh", "-c", "echo \"$CONSISTENT_DESKTOP|$TERM|$COLORTERM\""]),
                None,
            ));
            assert!(
                out.contains("1|xterm-256color|truecolor"),
                "output was {out:?}"
            );
        }

        #[test]
        fn kill_ends_the_session_and_reports_exit_once() {
            if !shell_available() {
                return;
            }
            let (handle, rx) = spawn_collect(&spec(Some(&["sleep", "30"]), None));
            handle.kill();
            let (_, exit) = drain(&rx, Duration::from_secs(10));
            assert!(exit.is_some(), "exit should fire after kill");
            assert_ne!(exit, Some(Some(0)), "a killed process is not a success");
            // Exactly once: nothing else arrives afterwards.
            assert!(
                rx.recv_timeout(Duration::from_millis(300)).is_err(),
                "no events after exit"
            );
            // And a second kill on a reaped child is a harmless no-op.
            handle.kill();
        }

        #[test]
        fn resize_is_accepted_while_running() {
            if !shell_available() {
                return;
            }
            let (handle, rx) = spawn_collect(&spec(Some(&["sleep", "30"]), None));
            handle.resize(120, 40).unwrap();
            handle.kill();
            let (_, exit) = drain(&rx, Duration::from_secs(10));
            assert!(exit.is_some());
        }
    }
}
