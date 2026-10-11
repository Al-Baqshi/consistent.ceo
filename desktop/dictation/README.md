# consistent-dictate — on-device dictation sidecar

A tiny Swift CLI that wraps Apple's **SpeechAnalyzer / SpeechTranscriber**
(macOS 26+, WWDC25) and streams a live transcript. It's the "ears" of the
Wispr-Flow-style dictation feature: on-device, zero cost-per-call, no network.

The Rust/Tauri desktop shell spawns it exactly like the gateway sidecar and
forwards its output to the workspace UI. See
[`desktop/src-tauri/src/dictation.rs`](../src-tauri/src/dictation.rs).

> **Not on Apple Silicon + macOS 26?** This engine won't run. The shell falls
> back to [`consistent-whisper`](../whisper/README.md) — an on-device Whisper
> (whisper.cpp) engine that runs on Intel Macs, older macOS, Windows, and Linux
> and speaks this same protocol. Selection is automatic; override with
> `CONSISTENT_DICTATION_ENGINE=apple|whisper`.

## Wire protocol (NDJSON, one JSON object per line)

**stdin — control (client → sidecar):**

```json
{"cmd":"start"}   // begin capturing + transcribing
{"cmd":"stop"}    // stop; flush a final result
{"cmd":"quit"}    // stop and exit the process
```

**stdout — events (sidecar → client):**

```json
{"type":"ready"}                              // process is up, awaiting commands
{"type":"model","status":"downloading"}       // one-time on-device model fetch
{"type":"model","status":"ready"}
{"type":"state","value":"listening"}
{"type":"volatile","text":"live best guess"}  // rewrite-in-place hypothesis
{"type":"final","text":"committed segment "}  // append + inject at the cursor
{"type":"state","value":"stopped"}
{"type":"error","message":"…"}
```

`volatile` is the constantly-revised in-progress guess; `final` segments are
committed text — those are what a consumer appends and (Phase 2) injects at the
OS cursor. stderr carries freeform debug logs and is never parsed.

## Build

```bash
# From desktop/ — builds release, ad-hoc signs, stages the sidecar binary.
npm run build:dictation

# Or directly:
cd dictation && swift build -c release
```

Ad-hoc signing (`codesign --force --sign -`) is **required**: the binary embeds
its `Info.plist` (mic + speech usage strings) as a `__TEXT,__info_plist`
section, and TCC only honors those strings once the signature binds the section.
`build:dictation` does this for you.

## Manual smoke test

```bash
cd dictation && swift build && codesign --force --sign - .build/debug/consistent-dictate
.build/debug/consistent-dictate --once      # start immediately, Ctrl-C to stop
```

## Requirements & known constraints

| Requirement | Why |
|---|---|
| **macOS 26 (Tahoe)+** | `SpeechAnalyzer` / `SpeechTranscriber` / `AssetInventory` ship in macOS 26. The package deployment target is pinned to `26.0`. |
| **Apple Silicon** | The on-device neural model runs on the Neural Engine. On Intel, model acquisition fails with `… is not subscribed to transcription.en` — the API compiles and runs, but the model won't download. |
| **Mic + Speech usage strings** | A flat CLI touching the mic/speech APIs SIGABRTs under TCC without `NSMicrophoneUsageDescription` / `NSSpeechRecognitionUsageDescription`. Provided via the embedded `Info.plist` (sidecar) and the app's [`src-tauri/Info.plist`](../src-tauri/Info.plist) (host app). |
| **Interactive first run** | First use shows the macOS mic/speech permission prompt and downloads the model — must happen on a real machine with a logged-in user, not headless CI. |

The desktop app's minimum system version stays at 10.15 — dictation is a
**runtime-detected** feature, not a hard app requirement. The frontend must
feature-gate it (fall back to Gemini Live voice on older systems) rather than
forcing everyone onto macOS 26.

## Modes & hotkeys

Each hotkey starts a dictation session; what happens to the committed text
depends on which one you pressed. Two keys rather than inferring intent from
what you said — acting on your workspace should be a choice you made.

| Default hotkey | Mode | What `final` segments do |
|---|---|---|
| `Cmd/Ctrl+Shift+D` | **Dictate** | Typed at the OS cursor in whatever app is focused. |
| `Cmd/Ctrl+Shift+A` | **Command** | Sent to the gateway's tool-calling agent (`POST /api/capture/chat`) to act on — "remind me to ship the invoice" creates a reminder. |

Those are only the defaults: bindings are user-configurable from Settings →
Shortcuts in the desktop app, persisted to `<app-data>/shortcuts.json`, and
each dictation binding can be **hands-free** (press toggles start/stop) or
**push-to-talk** (listen while held, stop on release) — see
[`shortcuts.rs`](../src-tauri/src/shortcuts.rs).

Everything is driven entirely from Rust, so it works with no frontend
involvement. The UI can still observe everything: `dictation-event`
(transcript), `dictation-hotkey` (mode), `agent-reply` (what the agent said
and did).

Implementation: [`inject.rs`](../src-tauri/src/inject.rs) (typing),
[`agent.rs`](../src-tauri/src/agent.rs) (agent bridge),
[`dictation.rs`](../src-tauri/src/dictation.rs) (mode dispatch),
[`shortcuts.rs`](../src-tauri/src/shortcuts.rs) (configurable bindings).

## Permissions

| Permission | Needed for | Granted to |
|---|---|---|
| Microphone / Speech Recognition | Capturing + transcribing | The sidecar (embedded `Info.plist`) and the app. |
| **Accessibility** | Typing at the cursor | **Consistent.app** — check with `injection_trusted`, prompt via `open_accessibility_settings`. |

Injection lives in the main app binary on purpose: Accessibility is granted per
code signature, so the user grants it once to Consistent.app. An ad-hoc-signed
sidecar would lose the grant on every rebuild.

## Status

- ✅ Sidecar compiles & links against the real SpeechAnalyzer API (Swift 6.3 / SDK 26.5).
- ✅ Runs, speaks the NDJSON protocol, mic-permission path is clean.
- ✅ Text injection at the cursor via `CGEvent` Unicode keystrokes — chunked to
      respect `CGEventKeyboardSetUnicodeString` limits without splitting
      surrogate pairs (unit-tested).
- ✅ Command mode bridges a transcript to the gateway's tool-calling agent.
- ✅ Whole Tauri crate compiles clean, 6/6 tests pass, zero warnings.
- ⏳ **Unverified end-to-end.** Needs Apple Silicon + macOS 26 + an interactive
      session to grant mic/Accessibility and download the model. This dev box is
      Intel, so the model won't load and synthetic events can't be granted.

## Next

1. **Verify on Apple Silicon.** The one thing blocking a real demo: grant the
   permissions, confirm the model downloads, speak into a text field.
2. **Dictation formatting.** Punctuation/filler cleanup, "scratch that" / edit
   commands, personal vocabulary.
3. **Paste-mode injection.** For long segments, clipboard + synthetic `Cmd+V` is
   faster than per-chunk typing — needs save/restore of the user's clipboard.
4. **Command-mode confirmations.** Today a spoken command executes as soon as the
   agent decides to call a tool. The available tools are additive
   (`create_note`, `create_task`, `set_reminder`), but if destructive tools are
   ever added, this needs a confirmation step to match the product's
   explicit-oversight stance.
