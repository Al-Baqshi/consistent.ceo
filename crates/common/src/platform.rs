//! Where the operating system shows through.
//!
//! The workspace grew up on macOS, and three assumptions leaked into every
//! crate that names a path or spawns a process: `$HOME` is set, a CLI is a file
//! called exactly `claude` in a Unix-shaped directory, and `sh` exists. None of
//! them hold on Windows, where the home directory is `%USERPROFILE%`, an npm
//! global is `codex.cmd`, and there is no POSIX shell at all. Each copy of the
//! assumption failed quietly — a lane that "isn't installed", a data directory
//! literally named `~` — so the answers live here, once.
//!
//! A fourth limit is new rather than leaked: Windows caps a command line at
//! 32,767 characters, and a `.cmd` shim runs through `cmd.exe`, which caps it at
//! 8,191 and cannot carry a newline at all. A prompt is exactly the argument
//! that breaks both, so [`output_with_stdin`] exists to take it off argv.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use tokio::io::AsyncWriteExt;

fn non_empty_env(key: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(key).filter(|v| !v.is_empty())
}

/// The user's home directory.
///
/// Unix reads `$HOME`. Windows prefers `%USERPROFILE%`, because that is what
/// Node's `os.homedir()` returns and so where the Claude and Codex CLIs keep
/// their state; `%HOME%` is only a fallback, since Git for Windows and MSYS set
/// it to directories the CLIs never look in.
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(profile) = non_empty_env("USERPROFILE") {
            return Some(PathBuf::from(profile));
        }
        if let (Some(drive), Some(path)) = (non_empty_env("HOMEDRIVE"), non_empty_env("HOMEPATH")) {
            let mut joined = drive;
            joined.push(path);
            return Some(PathBuf::from(joined));
        }
    }
    non_empty_env("HOME").map(PathBuf::from)
}

/// Expand a leading `~` to the home directory; any other path is returned
/// unchanged, as is everything when no home directory can be found.
///
/// On Unix the result is byte-for-byte what the gateway's old per-module
/// copies produced (`{home}/{rest}`). On Windows `~\` is accepted too and the
/// remainder's `/` separators become `\`, so a config default such as
/// `~/.consistent-world/db` comes out as one consistent native path.
pub fn expand_tilde(path: &str) -> String {
    let rest = if path == "~" {
        ""
    } else if let Some(rest) = path.strip_prefix("~/") {
        rest
    } else if let Some(rest) = path.strip_prefix("~\\").filter(|_| cfg!(windows)) {
        rest
    } else {
        return path.to_string();
    };
    let Some(home) = home_dir() else {
        return path.to_string();
    };
    let home = home.to_string_lossy();
    if cfg!(windows) {
        let home = home.trim_end_matches(['\\', '/']);
        if rest.is_empty() {
            return home.to_string();
        }
        format!("{home}\\{}", rest.replace('/', "\\"))
    } else {
        format!("{}/{rest}", home.trim_end_matches('/'))
    }
}

/// Directories CLIs commonly install into but a GUI-launched process often
/// does not inherit on its `PATH`.
///
/// macOS hands an app launched from Finder launchd's minimal
/// `/usr/bin:/bin:/usr/sbin:/sbin`, which misses Homebrew and the native Claude
/// installer. Windows apps do get the user's `PATH`, but a CLI installed after
/// the app started is not on it until the next login, so the usual install
/// locations are listed there too.
pub fn standard_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let home = home_dir();
    #[cfg(windows)]
    {
        // The native Claude installer (`install.ps1`) lands here, as on Unix.
        if let Some(home) = &home {
            dirs.push(home.join(".local").join("bin"));
        }
        // `npm install -g` — where `codex.cmd` and `claude.cmd` shims live.
        if let Some(appdata) = non_empty_env("APPDATA") {
            dirs.push(PathBuf::from(appdata).join("npm"));
        }
        if let Some(local) = non_empty_env("LOCALAPPDATA") {
            dirs.push(
                PathBuf::from(local)
                    .join("Microsoft")
                    .join("WinGet")
                    .join("Links"),
            );
        }
        if let Some(home) = &home {
            dirs.push(home.join("scoop").join("shims"));
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(home) = &home {
            dirs.push(home.join(".local").join("bin"));
        }
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
    }
    dirs
}

/// Find an executable: `PATH` first, then [`standard_bin_dirs`].
///
/// `None` means genuinely not installed; callers that report availability
/// depend on that distinction.
pub fn find_executable(name: &str) -> Option<PathBuf> {
    find_executable_with(name, &[])
}

/// [`find_executable`], also searching `extra_dirs` (after the standard ones)
/// for programs with install locations of their own, such as Git for Windows.
///
/// A name containing a path separator is checked directly instead of searched.
/// On Windows a bare name is tried with each `%PATHEXT%` extension and never
/// without one: npm drops an extensionless POSIX shim beside every `.cmd`, and
/// that file is a script for Git Bash that `CreateProcess` cannot run.
pub fn find_executable_with(name: &str, extra_dirs: &[PathBuf]) -> Option<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    if name.contains('/') || (cfg!(windows) && name.contains('\\')) {
        return executable_variants(Path::new(name))
            .into_iter()
            .find(|p| p.is_file());
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect())
        .unwrap_or_default();
    dirs.extend(standard_bin_dirs());
    dirs.extend(extra_dirs.iter().cloned());
    dirs.iter()
        .filter(|dir| !dir.as_os_str().is_empty())
        .flat_map(|dir| executable_variants(&dir.join(name)))
        .find(|p| p.is_file())
}

#[cfg(windows)]
fn executable_variants(base: &Path) -> Vec<PathBuf> {
    let pathext = std::env::var("PATHEXT")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
    let extensions: Vec<&str> = pathext
        .split(';')
        .map(str::trim)
        .filter(|e| e.starts_with('.') && e.len() > 1)
        .collect();
    let already_executable = base.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        extensions
            .iter()
            .any(|known| known[1..].eq_ignore_ascii_case(e))
    });
    if already_executable {
        return vec![base.to_path_buf()];
    }
    // Appended, not `with_extension`: that would turn `python3.12` into
    // `python3.exe`.
    extensions
        .iter()
        .map(|ext| {
            let mut s = base.as_os_str().to_owned();
            s.push(ext.to_ascii_lowercase());
            PathBuf::from(s)
        })
        .collect()
}

#[cfg(not(windows))]
fn executable_variants(base: &Path) -> Vec<PathBuf> {
    vec![base.to_path_buf()]
}

/// Windows' `CREATE_NO_WINDOW` process-creation flag.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A [`tokio::process::Command`] for a background child process — use it
/// instead of `Command::new` for anything the gateway runs on its own.
///
/// On Windows the gateway is the desktop app's sidecar, started without a
/// console, and a console program launched from a console-less parent gets a
/// console window of its own. Every `git` read, PowerShell command, CLI
/// completion and `taskkill` would flash a black window over whatever the
/// operator is doing. This sets `CREATE_NO_WINDOW`; elsewhere it is exactly
/// `Command::new`.
pub fn command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cmd = tokio::process::Command::new(program);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// [`command`] for [`std::process::Command`].
pub fn std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cmd = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// A path suitable for handing to a child process as its working directory.
///
/// `canonicalize` on Windows returns verbatim paths (`\\?\C:\work`). They are
/// right for comparing — both sides of a jail check are canonical — and wrong
/// for spawning: `cmd.exe`, and therefore every `.cmd` shim, refuses a UNC
/// working directory and silently runs in `C:\Windows` instead. This strips the
/// verbatim prefix wherever an ordinary path means the same thing. Unix paths
/// pass through untouched.
pub fn process_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            let bytes = rest.as_bytes();
            if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
                return PathBuf::from(rest);
            }
        }
    }
    path.to_path_buf()
}

/// Program and arguments that run `script` through the platform's shell.
///
/// Unix: `sh -c`, or `sh -lc` when `login` asks for the user's profile (the
/// agent shell tool wants their PATH; a verification command does not).
///
/// Windows: PowerShell — `pwsh` when installed, else the `powershell.exe` that
/// ships with every Windows. The script travels as `-EncodedCommand` (base64
/// UTF-16LE), because PowerShell 5.1 re-parses a plain `-Command` argument and
/// mangles embedded quotes; an encoded command reaches it verbatim. A prelude
/// makes output UTF-8 and silences progress bars (which 5.1 otherwise serialises
/// to stderr as CLIXML), and an epilogue passes a failing native command's exit
/// code through instead of PowerShell's generic `1`. `login` has no meaning
/// there: a Windows process already inherits the user's PATH.
pub fn shell_command(script: &str, login: bool) -> (PathBuf, Vec<String>) {
    #[cfg(windows)]
    {
        let _ = login;
        let program = find_executable("pwsh").unwrap_or_else(|| PathBuf::from("powershell.exe"));
        (program, powershell_args(script))
    }
    #[cfg(not(windows))]
    {
        let flag = if login { "-lc" } else { "-c" };
        (
            PathBuf::from("sh"),
            vec![flag.to_string(), script.to_string()],
        )
    }
}

/// PowerShell arguments for `script`; see [`shell_command`]. Built on every
/// platform so the encoding is tested where the tests actually run.
pub fn powershell_args(script: &str) -> Vec<String> {
    use base64::Engine as _;
    let wrapped = format!(
        "$ProgressPreference = 'SilentlyContinue'\n\
         [Console]::OutputEncoding = [System.Text.Encoding]::UTF8\n\
         $OutputEncoding = [System.Text.Encoding]::UTF8\n\
         {script}\n\
         if (-not $?) {{ if ($LASTEXITCODE) {{ exit $LASTEXITCODE }} else {{ exit 1 }} }}\n"
    );
    let utf16: Vec<u8> = wrapped.encode_utf16().flat_map(u16::to_le_bytes).collect();
    vec![
        "-NoLogo".to_string(),
        "-NoProfile".to_string(),
        "-NonInteractive".to_string(),
        "-ExecutionPolicy".to_string(),
        "Bypass".to_string(),
        "-EncodedCommand".to_string(),
        base64::engine::general_purpose::STANDARD.encode(utf16),
    ]
}

/// Run `cmd` to completion with `input` written to its stdin, capturing stdout
/// and stderr — [`tokio::process::Command::output`] for a child that reads its
/// prompt from stdin.
///
/// The write and the wait run concurrently, so a child that prints before it
/// has read everything cannot deadlock against a full pipe. A child that exits
/// without reading all of its input breaks the pipe; its own output explains
/// why better than `BrokenPipe` would, so that one error is not reported.
/// Timeouts and `kill_on_drop` stay the caller's, set on `cmd` as usual.
pub async fn output_with_stdin(
    mut cmd: tokio::process::Command,
    input: &[u8],
) -> io::Result<Output> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("child stdin was not captured"))?;
    let write = async move {
        let result = stdin.write_all(input).await;
        drop(stdin);
        result
    };
    let (written, output) = tokio::join!(write, child.wait_with_output());
    let output = output?;
    match written {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(e),
        _ => Ok(output),
    }
}

/// Kill `pid` and every process it started.
///
/// On Windows, killing a `.cmd` shim ends `cmd.exe` and orphans the Node
/// process doing the actual work, which then runs on invisibly — so a cancel
/// asks `taskkill /T` for the whole tree. Unix callers keep their ordinary
/// kill; there this is a no-op.
pub fn kill_process_tree(pid: u32) {
    #[cfg(windows)]
    {
        let _ = std_command("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expansion_leaves_other_paths_alone() {
        assert_eq!(expand_tilde("/etc/hosts"), "/etc/hosts");
        assert_eq!(expand_tilde("relative/~/x"), "relative/~/x");
        assert_eq!(expand_tilde("~user/x"), "~user/x");
    }

    #[test]
    fn tilde_expands_into_the_home_directory() {
        let home = home_dir().expect("tests run with a home directory");
        let expanded = expand_tilde("~/.consistent-world/db");
        assert!(expanded.starts_with(home.to_string_lossy().trim_end_matches(['/', '\\'])));
        assert!(expanded.ends_with("db"));
        assert!(!expanded.contains('~'));
        if cfg!(windows) {
            assert!(!expanded.contains('/'), "mixed separators: {expanded}");
        } else {
            assert_eq!(
                expanded,
                format!(
                    "{}/.consistent-world/db",
                    home.to_string_lossy().trim_end_matches('/')
                )
            );
        }
    }

    #[test]
    fn a_missing_program_is_none_not_a_guess() {
        assert!(find_executable("definitely-not-a-real-cli-4f1c9e").is_none());
        assert!(find_executable("").is_none());
    }

    #[test]
    fn the_platform_shell_is_found() {
        // `sh` on Unix; on Windows `cmd` is always present, and exercises the
        // PATHEXT expansion a bare name needs there.
        let name = if cfg!(windows) { "cmd" } else { "sh" };
        let found = find_executable(name).expect("platform shell on PATH");
        assert!(found.is_file());
    }

    #[test]
    fn a_path_is_checked_directly_rather_than_searched() {
        let dir = tempfile::tempdir().unwrap();
        let file_name = if cfg!(windows) { "tool.exe" } else { "tool" };
        let exe = dir.path().join(file_name);
        std::fs::write(&exe, b"").unwrap();
        assert_eq!(
            find_executable(exe.to_str().unwrap()).as_deref(),
            Some(exe.as_path())
        );
    }

    #[test]
    fn powershell_receives_the_script_verbatim() {
        use base64::Engine as _;
        let script = "Write-Output \"it's $env:USERNAME\"\nnpm test";
        let args = powershell_args(script);
        assert_eq!(args[args.len() - 2], "-EncodedCommand");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(args.last().unwrap())
            .unwrap();
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let decoded = String::from_utf16(&units).unwrap();
        assert!(decoded.contains(script), "{decoded}");
        assert!(decoded.contains("exit $LASTEXITCODE"));
    }

    /// Runs the real PowerShell with the real wrapper, wherever `pwsh` exists
    /// (Windows CI, or a Mac with PowerShell installed). Skipped otherwise.
    #[tokio::test]
    async fn the_powershell_wrapper_behaves_like_a_shell() {
        let Some(pwsh) = find_executable("pwsh") else {
            eprintln!("skipping: pwsh not installed");
            return;
        };
        let run = |script: &'static str| {
            let pwsh = pwsh.clone();
            async move {
                command(pwsh)
                    .args(powershell_args(script))
                    .output()
                    .await
                    .expect("pwsh runs")
            }
        };

        let out = run("Write-Output \"it's \"\"quoted\"\" – héllo ✓\"").await;
        assert!(out.status.success(), "{out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "it's \"quoted\" – héllo ✓"
        );

        let out = run("exit 3").await;
        assert_eq!(out.status.code(), Some(3), "explicit exit code");

        // A failing native command's own exit code comes through, not `1`.
        let out = run("& (Get-Process -Id $PID).Path -NoProfile -Command 'exit 7'").await;
        assert_eq!(out.status.code(), Some(7), "native exit code");

        // A failing cmdlet is a failure.
        let out = run("Get-Item -LiteralPath 'definitely-missing-4f1c9e'").await;
        assert!(!out.status.success(), "cmdlet failure must not exit 0");
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_shell_command_keeps_its_old_shape() {
        assert_eq!(
            shell_command("echo hi", true),
            (PathBuf::from("sh"), vec!["-lc".into(), "echo hi".into()])
        );
        assert_eq!(shell_command("echo hi", false).1[0], "-c");
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_paths_become_spawnable() {
        assert_eq!(
            process_path(Path::new(r"\\?\C:\work\repo")),
            PathBuf::from(r"C:\work\repo")
        );
        assert_eq!(
            process_path(Path::new(r"\\?\UNC\server\share\x")),
            PathBuf::from(r"\\server\share\x")
        );
        assert_eq!(
            process_path(Path::new(r"C:\plain")),
            PathBuf::from(r"C:\plain")
        );
    }

    #[tokio::test]
    async fn stdin_reaches_the_child_and_output_comes_back() {
        let (program, args) = if cfg!(windows) {
            shell_command("[Console]::In.ReadToEnd()", false)
        } else {
            shell_command("cat", false)
        };
        let mut cmd = command(program);
        cmd.args(args);
        let input = "line one\nline two with \"quotes\" and a % sign\n".repeat(2000);
        let out = output_with_stdin(cmd, input.as_bytes()).await.unwrap();
        assert!(out.status.success());
        let echoed = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
        assert_eq!(echoed.trim_end(), input.trim_end());
    }

    #[tokio::test]
    async fn a_child_that_ignores_stdin_is_not_an_error() {
        let (program, args) = shell_command("exit 3", false);
        let mut cmd = command(program);
        cmd.args(args);
        let big = vec![b'x'; 4 * 1024 * 1024];
        let out = output_with_stdin(cmd, &big).await.unwrap();
        assert_eq!(out.status.code(), Some(3));
    }
}
