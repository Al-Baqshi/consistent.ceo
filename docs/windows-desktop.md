# The Windows desktop app

Consistent CEO's desktop app is Tauri, so the window itself was always
portable. What was not portable was everything the gateway does on the
operator's machine, which was written against a Mac. This page records how the
Windows build differs, how to build and sign it, and the smoke test that stands
between "it compiles" and "it works".

## What is different on Windows

| Concern | macOS | Windows |
|---|---|---|
| Home directory | `$HOME` | `%USERPROFILE%` (what the Claude and Codex CLIs use too). `~/.consistent-world` resolves to `C:\Users\<you>\.consistent-world` |
| Finding `claude` / `codex` / `git` | a file with that exact name on `PATH`, Homebrew, `~/.local/bin` | `%PATHEXT%` variants (`claude.exe`, npm's `codex.cmd`) on `PATH`, `~\.local\bin`, `%APPDATA%\npm`, WinGet links, Scoop shims, Git for Windows |
| Prompts to the CLIs | stdin | stdin. Windows caps a command line at 32,767 characters, and a `.cmd` shim runs through `cmd.exe`, which caps it at 8,191 and cannot carry a newline. The Claude system prompt goes in a temporary file (`--append-system-prompt-file`) for the same reason |
| Agent `shell` tool, verification commands | `sh -lc` / `sh -c` | PowerShell: `pwsh` when installed, else the built-in 5.1. The script is passed as `-EncodedCommand`, so quoting survives. The tool tells the model it is writing PowerShell |
| Risk patterns in the shell tool | `sudo`, `chmod 777`, `rm -rf /` | `-Verb RunAs`, `icacls`, `Set-ExecutionPolicy`, `Format-Volume`, `diskpart`, … matched case-insensitively |
| Cancelling an agent run | kill the process | `taskkill /T`: killing only a `.cmd` shim would leave Node running the agent |
| Background child processes | plain spawn | `CREATE_NO_WINDOW`. The desktop app starts the gateway without a console, and every console program it then started (git, PowerShell, the CLIs) would otherwise flash a console window of its own. New spawn sites use `platform::command` / `platform::std_command` |
| Keeping the machine awake during runs | `caffeinate -is` | `SetThreadExecutionState(ES_SYSTEM_REQUIRED)` on a dedicated thread |
| Typing dictation at the cursor | `CGEvent` Unicode keystrokes (needs Accessibility) | `SendInput` with `KEYEVENTF_UNICODE` (no permission prompt) |
| Dictation engine | Apple SpeechAnalyzer on Apple Silicon + macOS 26, otherwise Whisper | Whisper |
| Embedded terminal | login `zsh`/`bash` | ConPTY + PowerShell |
| Vault device key | Keychain | Credential Manager |

Everything above lives behind one module, `crates/common/src/platform.rs`, so a
new call site asks it instead of repeating a Unix assumption.

## Known limits

- **Elevated windows.** Windows drops synthetic input aimed at a window running
  "as administrator" when the app is not elevated. Dictation into such a window
  reports an error instead of typing.
- **PowerShell 5.1 has no `&&`.** Commands like `npm test && npm run build`
  need PowerShell 7 (`winget install Microsoft.PowerShell`), which the gateway
  prefers automatically once it is installed.
- **`opencode` and `cursor-agent`** still receive their prompt as an argument
  (their stdin behaviour is not verified), so a very long or multi-line prompt
  to either can fail on Windows. `claude` and `codex` are unaffected.
- **Vault file permissions.** On Unix the vault file is `chmod 600`. Windows has
  no equivalent call in the code; the file inherits the ACL of the user profile,
  which is private to that user by default.
- **Shell tool containment is by working directory only**, exactly as on macOS:
  the jail picks where a command starts, not what it can touch.

## Building

### On the Windows machine (no CI needed)

The quickest route, and the one that needs nothing from GitHub Actions. From a
PowerShell window in a clone of the repository:

```powershell
git clone https://github.com/Al-Baqshi/Consistent-world.git
cd Consistent-world
git checkout feat/windows-desktop
powershell -ExecutionPolicy Bypass -File desktop\scripts\build-windows.ps1 -InstallPrerequisites
```

`build-windows.ps1` checks for Visual Studio Build Tools (C++), Rust, Node.js,
Git, CMake, LLVM, Strawberry Perl and NASM, installs whatever is missing with
winget, then runs the Tauri build and prints the installer's path
(`desktop\src-tauri\target\release\bundle\nsis\…-setup.exe`). The first build
takes a long while: it compiles OpenSSL, whisper.cpp and the whole gateway.
Without `-InstallPrerequisites` it only reports what is missing.

If a tool installed but is still reported missing, open a new PowerShell window
(installers update PATH for new sessions) and run the script again.

### In CI

`ci.yml` has a `windows` job: `cargo clippy --workspace --all-targets -D
warnings` (a gate), `cargo test --workspace` (visible but not yet required,
because the test suite has never run on Windows), and a check + test of the
Tauri shell. Windows minutes are expensive on a private repository, so it runs
only on demand (`gh workflow run ci.yml --ref <branch>`) and on release tags.

Installers come from `release-desktop.yml`. A version tag builds every platform
and releases. To build **without** releasing, run it by hand:

```bash
gh workflow run release-desktop.yml --ref <branch> -f platforms=windows
gh run watch                                   # ~1–2h on a windows runner
gh run download <run-id> -n installer-windows-x64
```

The artifact is `Consistent-CEO-Windows-Setup.exe`, an NSIS installer.

> **Actions is blocked on billing** as of 2026-09-09 (see `STATUS.md`). No job
> starts until the payment issue under GitHub → Settings → Billing is cleared.

### Locally, from a Mac

A Mac cannot produce the installer, but it can type-check the Windows code, which
catches most mistakes before a slow CI round trip. It needs rustup with the
`x86_64-pc-windows-msvc` target, `cargo-xwin` (which fetches the MSVC CRT and
uses Homebrew LLVM's `clang-cl`), and one workaround: vendored OpenSSL cannot be
cross-built, but its headers are enough for `check`.

```bash
export XWIN_ACCEPT_LICENSE=1 PATH="/usr/local/opt/llvm/bin:$PATH"
export OPENSSL_NO_VENDOR=1 OPENSSL_DIR=/usr/local/opt/openssl@3 OPENSSL_STATIC=1
cargo xwin check --target x86_64-pc-windows-msvc --workspace --all-targets
```

Install rustup with `--no-modify-path` and its own `RUSTUP_HOME`/`CARGO_HOME`
if Homebrew's `rustc` must stay the default toolchain (it is what the
`repo-health.sh` pin check reads).

## Publishing the download

The public download lives where the macOS one does: in the marketing site's
repository (`Al-Baqshi/Consistent`, deployed by Vercel) under
`public/downloads/`, served from the site itself. The site's download page
turns the Windows card into a real button **as soon as**
`public/downloads/Consistent-CEO-Windows-Setup.exe` exists at build time, and
shows "coming soon" otherwise, so there is never a button to a missing file.

On the Windows machine, after `build-windows.ps1` and **after the smoke test
below has passed with that build**:

```powershell
powershell -ExecutionPolicy Bypass -File desktop\scripts\publish-windows.ps1
```

It finds the newest installer, asks you to confirm the smoke test passed,
clones the site repo into `%USERPROFILE%\consistent-site` (or uses the one there),
copies the installer in under the stable name, commits and pushes. Vercel
redeploys in a few minutes, and then:

- `https://www.consistent.ceo/downloads/Consistent-CEO-Windows-Setup.exe` downloads it, and
- `https://www.consistent.ceo/download` shows a Windows download button with
  SmartScreen guidance next to it.

`-NoPush` commits without publishing. Pushing needs your GitHub login on that
machine (Git for Windows opens a browser sign-in the first time). A new build
is published by running the script again; the stable file name means links
never change.

GitHub rejects files of 100 MB or more, which the script checks. If the
installer ever outgrows that, host it in a bucket and set
`NEXT_PUBLIC_DOWNLOAD_BASE` on the site instead (see the site's
`lib/downloads.ts`).

## Signing

An unsigned installer works, but every downloader first meets SmartScreen's
*"Windows protected your PC"* and has to click **More info → Run anyway**.
Signing removes the "unknown publisher" warning. Note that SmartScreen also
weighs download reputation, so a brand-new identity can still see a warning for
a while after signing.

The release workflow signs through **Azure Trusted Signing**: Microsoft holds
the certificate, so there is no key file to protect and nothing to renew by
hand. It switches on by itself once these are set, and until then builds
unsigned.

### One-time setup

1. In the Azure portal, create a **Trusted Signing account** (the Basic tier is
   enough) in a region near you. Note its endpoint, e.g.
   `https://eus.codesigning.azure.net`.
2. Complete **identity validation** in that account, for the organization or
   individual that will appear as the publisher. This is the slow step: Microsoft
   verifies the identity, which can take days.
3. Create a **certificate profile** of type *Public Trust* linked to that
   validation.
4. Create an **app registration** (service principal) in Microsoft Entra ID,
   add a client secret, and on the Trusted Signing account grant it the role
   **Trusted Signing Certificate Profile Signer**.
5. In the GitHub repo, Settings → Secrets and variables → Actions:

   | Kind | Name | Value |
   |---|---|---|
   | secret | `AZURE_CLIENT_ID` | the app registration's client ID |
   | secret | `AZURE_CLIENT_SECRET` | its client secret |
   | secret | `AZURE_TENANT_ID` | the directory (tenant) ID |
   | variable | `WINDOWS_SIGNING_ENDPOINT` | the account endpoint from step 1 |
   | variable | `WINDOWS_SIGNING_ACCOUNT` | the Trusted Signing account name |
   | variable | `WINDOWS_SIGNING_PROFILE` | the certificate profile name |

Neither the account name nor the profile name may contain a space: Tauri splits
its `signCommand` on spaces.

### How it works

When the secrets exist, the workflow installs `trusted-signing-cli` and writes
`desktop/src-tauri/tauri.windows.conf.json` with a `bundle.windows.signCommand`.
Tauri merges that file over `tauri.conf.json` on Windows and runs the command
for each binary it signs. A later step prints `Get-AuthenticodeSignature` for
the installer, so the log says whether signing actually happened.

**Verify on the first signed build** that the sidecars are signed too, not just
the installer: after installing, right-click
`C:\Program Files\Consistent CEO\consistent-world.exe` → Properties → Digital
Signatures.

## Smoke test on a real Windows machine

Unit tests and CI prove the pieces compile and behave in isolation. They do not
prove a first launch works. Run this on Windows 10 (22H2) or 11, x64, ideally a
machine that has never had the app. Note the step number of anything that fails,
with a screenshot or the exact error text.

### Before you start

- Install **Git for Windows** (`winget install Git.Git`). Claude Code on Windows
  needs its bash, so this is required for the Claude lane, not optional.
- Install **Claude Code**: in PowerShell,
  `irm https://claude.ai/install.ps1 | iex`, then run `claude` once and sign in.
- Optional but worth covering: **Node.js LTS** with
  `npm install -g @openai/codex` (then `codex login`; Codex's Windows support is
  newer than its macOS support), and **PowerShell 7**.
- If you built the installer on this same machine, that is fine for a first
  pass, but note it: a build machine has tools on PATH a user's machine will not.
- Have a small git repository somewhere under your user folder, e.g.
  `C:\Users\<you>\projects\demo`.

### 1. Install and first launch

1. Get the installer: the `…-setup.exe` that `build-windows.ps1` printed, or
   `Consistent-CEO-Windows-Setup.exe` from a manual workflow run. If SmartScreen
   appears, **More info → Run anyway**, and note
   whether the publisher says *Unknown* (unsigned) or a name (signed).
2. Run the installer. The app opens a window showing the dashboard, not a
   "Dashboard not embedded" page.
3. In a browser, open `http://127.0.0.1:18800/health`. It answers.
4. Check `C:\Users\<you>\.consistent-world\` exists and contains `db\`. There
   must be **no folder literally named `~`** anywhere: not in that user folder,
   not under `C:\Program Files\Consistent CEO\`, and not under
   `%APPDATA%\world.consistent.desktop\`. A `~` folder means home-directory
   resolution regressed.

### 2. The brain: Claude and Codex through the CLI

5. Settings → the Claude card shows it as installed, with a version. Use its
   verify/connect action: it runs a real completion and reports success.
6. If Codex is installed, same for the Codex card. The card must find it even
   though it is `codex.cmd`.
7. In chat, with Claude as the brain, send a message that spans several lines.
   A reply arrives.
8. Paste a long message (a few thousand words) and send it. A reply arrives.
   This is the command-line length limit.

### 3. Agent work in a real project

9. Create an entity whose project folder is your demo repository.
10. Ask the agent to list the files in the project. The shell tool runs in
    PowerShell, inside that folder, and returns the listing.
    **No black console window flashes up** while it works, here or in any later
    step. One that does means a spawn site skipped `platform::command`.
11. Give it a small code task ("add a README line saying hello"). The run
    starts, streams progress, and finishes with the change visible in
    `git diff`.
12. Start another task and **cancel** it while it runs. Then open Task Manager:
    no `claude.exe` or `node.exe` from that run is still alive.
13. While a task is running, open an **administrator** terminal and run
    `powercfg /requests`. `consistent-world.exe` appears under `SYSTEM`. Run it
    again a couple of minutes after all work finishes: it is gone.

### 4. The desktop shell

14. Settings → enable the embedded terminal. It opens a PowerShell prompt that
    accepts input and resizes with the window.
15. Use the guided **Sign in** on the Claude card. The sign-in flow runs inside
    the embedded terminal.
16. Open Notepad, click into it, and press the Dictate shortcut (default
    **Ctrl+Shift+D**; see Settings → Shortcuts). Speak a sentence. The text is
    typed into Notepad. The first use downloads the Whisper model into
    `.consistent-world\models\`, which can take a minute.
17. Minimize the app and cause an approval (a task that needs sign-off). A
    Windows notification appears.
18. Vault: create one, then use **Unlock with this computer**. Close and reopen
    the app, and unlock again the same way.

### 5. Leaving

19. Quit the app from its tray icon or window. In Task Manager,
    `consistent-world.exe` is gone within a few seconds.
20. Launch it again: your entity, chat history and settings are still there.
