# consistent-whisper — cross-platform on-device dictation sidecar

A small Rust CLI that runs an **OpenAI Whisper** model fully on-device (via
[whisper.cpp](https://github.com/ggerganov/whisper.cpp), through the `whisper-rs`
bindings) and streams a live transcript. It is the portable counterpart to
[`consistent-dictate`](../dictation/README.md), which uses Apple's
SpeechAnalyzer and only runs on **Apple Silicon + macOS 26**.

Between the two, dictation works everywhere:

| Platform | Engine | Why |
|---|---|---|
| Apple Silicon + macOS 26 | `consistent-dictate` (SpeechAnalyzer) | Neural-Engine model, tightest OS integration |
| Intel Mac, older macOS, Windows, Linux | `consistent-whisper` (this) | whisper.cpp on the CPU — runs anywhere |

The Tauri shell picks one at runtime in
[`dictation.rs`](../src-tauri/src/dictation.rs); both speak the **same NDJSON
protocol**, so nothing downstream cares which ran. Force one with
`CONSISTENT_DICTATION_ENGINE=apple|whisper`.

## Wire protocol (identical to `consistent-dictate`)

**stdin — control:** `{"cmd":"start"}` · `{"cmd":"stop"}` · `{"cmd":"quit"}`

**stdout — events:**

```json
{"type":"ready"}
{"type":"model","status":"downloading"}   // one-time model fetch
{"type":"model","status":"ready"}
{"type":"state","value":"listening"}
{"type":"volatile","text":"live best guess"}   // rewrite-in-place hypothesis
{"type":"final","text":"committed segment "}    // append + inject at the cursor
{"type":"state","value":"stopped"}
{"type":"error","message":"…"}
```

## How it works

whisper.cpp is not a streaming recogniser — it transcribes a whole buffer at
once — so this sidecar segments speech itself:

1. **Capture + resample.** Mic audio (cpal) is downmixed to mono and resampled
   to 16 kHz, reusing `consistent-world-audio` (the Gemini Live voice path's
   code).
2. **Energy VAD.** A simple RMS gate marks speech vs. silence and bounds each
   *utterance* (speech between pauses).
3. **Transcribe.** While you speak, the growing buffer is transcribed on a
   throttle and emitted as `volatile`; on a pause it is transcribed once more and
   emitted as `final` — the committed text the shell types at the cursor.

Default model is `base.en` (~148 MB, cached in `~/.consistent-world/models/`).
Override with `CONSISTENT_WHISPER_MODEL` (e.g. `ggml-tiny.en.bin` for speed) or
`CONSISTENT_WHISPER_MODEL_URL`.

## Build

```bash
# From desktop/ — builds release, (macOS) ad-hoc signs, stages the sidecar.
npm run build:whisper

# Or directly:
cd whisper && cargo build --release
```

Requires **cmake + a C/C++ compiler** on the *build* machine (whisper.cpp is
compiled from source). The shipped binary is self-contained — end users need
nothing. On macOS, `build.rs` embeds `Info.plist` so TCC honours the microphone
usage string; the build script ad-hoc signs so that section binds.

## Self-test (offline, no mic)

```bash
# Transcribe a WAV and print the text — proves the model runs on this host.
cargo run --release -- --file path/to/16k-mono.wav

# Make a test clip on macOS:
say -o /tmp/s.aiff "the quick brown fox jumps over the lazy dog"
afconvert -f WAVE -d LEI16@16000 -c 1 /tmp/s.aiff /tmp/s.wav
./target/release/consistent-whisper --file /tmp/s.wav
```

## Status

- ✅ Compiles and links whisper.cpp (via whisper-rs) with cmake + clang.
- ✅ Verified on an **Intel** Mac: transcribes a generated clip exactly on
      CPU + BLAS (`use gpu = 0`).
- ✅ Same NDJSON protocol as `consistent-dictate`; runtime engine selection.
- ⏳ Live mic path needs an interactive session to grant the microphone TCC
      prompt and speak; the `--file` path exercises everything except capture.
