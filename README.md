# Consistent CEO

A self-hosted AI operator for the things you run: businesses, projects, goals.
Give it a mission in plain words; it plans the work, does what it safely can on
your own computer, asks before anything leaves it, and shows evidence for every
"done" (a git diff, a passing test), not just a claim.

It runs entirely on your machine. There is no account with us, no server of
ours in the loop, and nothing to pay. Your data, your keys, your models.

- **One install.** A desktop app (macOS, Windows) or a single binary or
  container. The database and vector search are built in; there is nothing
  else to run.
- **Any brain.** Use the Claude or ChatGPT subscription you already have
  through their own CLIs, a local model through [Ollama](https://ollama.com)
  (free, works offline), or an API key.
- **Your phone as a remote.** Pair it by scanning a code on your computer's
  screen. No password, no cloud: the phone keeps its own key and talks to your
  computer directly.
- **You stay in charge.** File and code work runs inside a project folder you
  choose. Anything outward-facing, like sending a message or touching the
  network, waits for your approval.

MIT licensed. Built in Rust, with a SvelteKit dashboard, a Tauri desktop shell
and an Expo phone app.

## Install

### Desktop app (easiest)

Download the installer for your computer from the
[latest release](https://github.com/Al-Baqshi/consistent.ceo/releases/latest):

| Computer | File |
|---|---|
| Mac with Apple Silicon (M1 and later) | `Consistent-CEO-macOS-AppleSilicon.dmg` |
| Mac with Intel | `Consistent-CEO-macOS-Intel.dmg` |
| Windows 10 or 11 | `Consistent-CEO-Windows-Setup.exe` |

Open it and follow the setup. It starts with your computer and lives in the
menu bar, so missions and reminders keep running when the window is closed.
Quit from the menu-bar icon to stop it. Data lives in `~/.consistent-world/`.

### Command line (macOS, Linux)

```bash
curl -fsSL https://raw.githubusercontent.com/Al-Baqshi/consistent.ceo/main/install.sh | sh
```

This installs `consistent-world` into `~/.local/bin` and starts it in the
background (a LaunchAgent on macOS, a systemd user service on Linux). Open
<http://127.0.0.1:18800>. Set `CONSISTENT_WORLD_NO_SERVICE=1` to skip the
background service.

### Docker

```bash
docker run -d --name consistent -p 127.0.0.1:18800:18800 \
  -v consistent:/var/lib/consistent-world ghcr.io/al-baqshi/consistent.ceo
```

One container, database included, everything kept in the `consistent` volume.
Ollama on the host is reached at `host.docker.internal`. Keep the port on
`127.0.0.1` as shown; see the [Dockerfile](Dockerfile) header before exposing
it to a network. `docker compose up` runs the same image against a separate
SurrealDB server instead.

### From source

Needs Rust 1.97 and Node 20.

```bash
git clone https://github.com/Al-Baqshi/consistent.ceo.git
cd consistent.ceo
cd sveltfront && npm ci && npm run build && cd ..
cargo run --release -p consistent-world-gateway --features dashboard -- start --open
```

The desktop app builds from `desktop/` ([desktop/README.md](desktop/README.md)),
the phone app from `mobile/` ([mobile/README.md](mobile/README.md)).

## First run

1. **Make your account.** It exists only on this computer. Save the backup
   codes it shows you; they are the only way back in if you forget the
   password.
2. **Pick a brain** in Settings → Connect your AI. Each card sends a real test
   message before it says "ready".
   - *Claude Code* or *Codex*: uses your existing subscription through the
     official CLI. The desktop app signs you in from the card.
   - *Ollama (local)*: free and offline. If Ollama has no model yet, the card
     downloads one for you.
   - *An API key*: OpenAI, Anthropic, Google, or any OpenAI-compatible server.
3. **State a goal** in chat. It becomes a mission with a definition of done.

Semantic memory uses a small local embedding model through Ollama when Ollama
is installed, and is downloaded automatically. Without Ollama it uses an
OpenAI or Gemini key if you added one, and otherwise falls back to recent
history.

## Your phone

The phone app is a remote control for *your* computer. Nothing passes through
a service of ours.

1. Make your computer reachable from your phone. The simplest way is
   [Tailscale](https://tailscale.com/download), free for personal use: install
   it on both, sign in to the same account, then run
   `consistent-world remote setup` once. Prefer to host it yourself? Run the
   small [relay](crates/relay/README.md) anywhere instead.
2. On the computer: Settings → Your phone → **Pair a phone** (or run
   `consistent-world pair` in a terminal).
3. In the app: Settings → **Scan to connect**. Both screens show the same six
   digits. Check they match, then click **Allow** on the computer.

The code on screen works once, for 90 seconds. The phone makes its own
signing key, which never leaves it, and uses it to sign back in from anywhere.
Remove the phone from Settings → Your phone and it is signed out at once.

## What it does

- **Missions that finish themselves.** Give a mission a definition of done
  ("done when importing a sample CSV creates rows and the tests pass"). It
  loops develop → verify → develop, judging each round by what changed on disk
  and what the tests say. A run that claims success but changed nothing is
  flagged for review. Hard limits: 5 iterations by default (25 at most), an
  optional spend cap, and outward actions still wait for approval.
- **Directors and agents.** Each entity (a business, a project, you) gets
  directors with their own lessons and heartbeats, running on a schedule.
- **Work on your computer, safely.** Code and file work runs unattended only
  inside that entity's project folder. Browser work uses a separate browser
  profile, never your own.
- **A vault.** Encrypted credentials and notes, unlocked with your password, a
  passkey, or your device's biometrics. See [docs/VAULT.md](docs/VAULT.md).
- **Integrations.** Scoped API keys for other systems to call in, and
  webhooks out. See [docs/integrations.md](docs/integrations.md).

## Configuration

The first start writes `~/.consistent-world/consistent_world.toml`. The
annotated sample is [consistent-world.toml](consistent-world.toml). Useful
settings:

| Setting | What it does |
|---|---|
| `[gateway] port`, `bind` | Where it listens. Default `127.0.0.1:18800`. |
| `[memory] surreal_url` | Embedded database by default; point at a SurrealDB server to share one. |
| `[memory] embedding_provider` | `auto`, `ollama`, `openai`, `gemini` or `off`. |
| `[llm]`, `[llm.providers.*]` | Default brain and the providers you have. |
| `consistent-world start --offline` | Start without any outbound network call. |

`consistent-world --onboard` walks through the basics in a terminal.

## How it fits together

One process, `consistent-world`, serves the API and the dashboard and runs
every background loop. It stores everything in an embedded SurrealDB, which
also provides the vector index. The desktop app wraps that process; the phone
app talks to it over your own network path.

| Crate | Role |
|---|---|
| `gateway` | The binary: HTTP API, dashboard, missions, directors, approvals, pairing |
| `agents` | Agent loop and sub-agents |
| `llm` | Providers, including the keyless Claude Code and Codex CLI lanes |
| `memory` | Sessions, memories, embeddings and semantic recall |
| `tools` | Shell, filesystem, browser and HTTP tools |
| `plugins` | Sandboxed WASM plugins |
| `credentials` | OS keychain and the encrypted vault |
| `architect` | Shadow-mode verification of the gateway's answers |
| `relay` | Optional self-hosted relay for reaching your computer from anywhere |

[ARCHITECTURE.md](ARCHITECTURE.md) is the technical guide, and
[STATUS.md](STATUS.md) is the honest record of what is finished and what is
not.

## Contributing

Issues and pull requests are welcome. Start with
[CONTRIBUTING.md](CONTRIBUTING.md). Report security problems privately as
described in [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE). The name "Consistent CEO" and its logo identify this project;
forks are welcome under a different name.
