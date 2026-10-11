# Plan C — Open source, local-first, nothing depends on the author

Goal: a stranger downloads one installer (or runs one container), creates a
username and password, picks a model lane that is already on their machine,
pairs their phone by standing next to the computer, and then runs the product
for years with no server, account, key, or domain owned by the author. The
author uses the same build as everyone else. No payments in this plan.

Status: **phases 1–4 and 6 implemented in the working tree** (2026-10-11,
uncommitted on `feat/ceo-platform`; see Progress at the end, which records
where the build departed from this plan and why). Phase 5 and the repository
flip need the owner. Sibling plan:
[`phone-desktop-control.md`](phone-desktop-control.md) (Plan B) built the
remote path this plan hardens; read its "What shipped" table first.

Who executes this: a coding agent in this repo, phase by phase, one PR per
phase. Each phase has an exit test that a human can run. Do not start a phase
until the previous one's exit passes, except where "parallel" is marked.

---

## 0. Ground rules for the executing agent

Read `CLAUDE.md`, then `ARCHITECTURE.md` and `STATUS.md`, before touching
anything. The rules that bite in this plan:

- **Never rename identifiers.** `consistent-world-*`, `~/.consistent-world/`,
  bundle ids and DB namespaces stay. Only user-facing copy says "Consistent CEO".
- **Never run `cargo fmt --all`**, and never hand rustfmt a file that declares
  modules (`main.rs`, `lib.rs`, `control_api.rs`). Format leaf files only.
- A new `crates/gateway/src/*.rs` must be declared with `mod` in **both**
  `lib.rs` and `main.rs` or CI fails.
- New HTTP routes: handler in `control_api/<domain>_api.rs` (or `auth_api.rs`
  for auth), registered in `control_routes()` / `auth_routes()`.
- Gates before every PR: `cargo check --workspace --all-targets`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, `scripts/repo-health.sh`. If linking tests runs
  out of memory, set `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`.
- Commits: `type(scope): lowercase imperative subject`, body says why.
- After each phase, update `STATUS.md` (what is real vs. stubbed) and the
  table at the end of this file. If a doc disagrees with the code, the code
  wins and the doc gets fixed in the same PR.
- Other sessions may be editing this tree. A compile error in a file you did
  not touch is probably in-flight work, not your bug.
- **Decisions already made by the owner** (do not reopen): open source under
  MIT; no payment or billing work; no Bluetooth pairing; passkeys stay an
  optional dashboard sign-in, not the phone binding; Tailscale is the
  recommended "from anywhere" transport, the relay is opt-in and
  self-hostable.

---

## 1. Cut the cords — nothing phones home, nothing needs the author

Everything hosted is already off by default. This phase makes that a
guarantee with a test, and removes the one config that only ever ran on the
author's infrastructure.

### 1.1 Inventory (verified 2026-10-11)

| Hosted-only knob | Where | Default | Action |
|---|---|---|---|
| `CONSISTENT_WORLD_INVITE_CODE` (gate on `/api/auth/register`) | `auth_api.rs` `register()` | unset = open | keep, document as "hosted only" |
| Trial credits on first sign-in (`CONSISTENT_WORLD_TRIAL_CENTS`, 500) | `server.rs` ~L638 | seeds a ledger row | keep, but **skip seeding when `enforce == false`** so a self-host DB never carries a credit table it does not use |
| Billing middleware (402) | `billing.rs` `require_credit`, `enforce` decided at startup | off on loopback binds and when `CONSISTENT_WORLD_BILLING_ENFORCE=0` | keep; add the inverse guarantee: enforcement is **also** off unless `CONSISTENT_WORLD_BILLING_ENFORCE=1` is set explicitly. Loopback alone is not enough once docker binds `0.0.0.0` |
| `[gateway.auth.external]` issuer (`id.consistent.ceo`) | `config.rs` ~L53, JWT path in auth | commented out | keep, commented, documented as "bring your own identity provider" |
| Relay (`CONSISTENT_WORLD_RELAY_URL/_GATEWAY_ID/_SECRET/_API_KEY`) | `relay_client.rs`, `crates/relay` | unset = no dial-out | keep; README of `crates/relay` already says "yours to host" |
| `CONSISTENT_WORLD_GITHUB_CLIENT_ID` | grep to find | unset | document or remove if dead |
| Railway config | `docker/consistent-world.railway.toml`, `Dockerfile` L69-70, `startup.rs` L167/L184 (`ws://surrealdb.railway.internal:8000`) | only when env points at it | **remove** the railway TOML and the Dockerfile COPY; replace the two `startup.rs` defaults with a neutral `ws://surrealdb:8000` (the compose service name). Railway was the author's deploy, not a product feature |
| Marketing links (`consistent.ceo/download`, `install.sh` one-liner, `dl.consistent.ceo`) | `README.md`, `install.sh`, `sveltfront/static/install.sh`, `sveltfront/src/lib/download.ts`, `mobile/src/lib/links.ts` | dead (ROADMAP item 5) | point every download at **GitHub Releases** of the public repo. Privacy and support links may stay on the domain; they are documents, not services |

### 1.2 Startup must succeed with no network

Today startup probes embedding candidates with 10 s timeouts and the LLM
probes are best-effort, but nobody has proven the whole boot path offline.

- Add an integration test `crates/gateway/tests/offline_boot_test.rs` that
  starts the gateway with `memory` Surreal, no provider keys, no CLIs on
  `PATH`, and asserts `/health` answers within 5 s. Run it in CI.
- Grep every `reqwest`/`hyper` call made during boot (`main.rs`, `startup.rs`,
  `llm_provider_probe.rs`, embedding selection ~L1318) and make each one
  either lazy or bounded by a short timeout. None may be fatal.
- Add a `--offline` flag (or `CONSISTENT_WORLD_OFFLINE=1`) that skips all
  outbound probes, so a user on a plane gets a fast boot. Default stays
  "probe with timeouts".

### 1.3 Exit

- `grep -rn 'railway' --exclude-dir=target --exclude-dir=node_modules .`
  returns only `crates/relay/README.md`'s "anywhere that can hold a WebSocket"
  sentence.
- Offline boot test green in CI.
- A fresh `docker compose up` with the host's network disabled reaches a
  healthy gateway (Ollama optional).

---

## 2. Fully local brain — semantic memory without an API key

The HNSW indexes in `crates/memory/src/store.rs` (`EMBEDDING_DIMENSION =
1536`) and `director_lessons_store.rs` (`LESSON_EMBEDDING_DIMENSION = 1536`)
are hard-wired to OpenAI's `text-embedding-3-small`. The "local" provider in
`crates/memory/src/embedding.rs` is a stub. So an install with only Ollama
silently gets no semantic recall. This is the one place the product still
quietly prefers a hosted key.

### 2.1 Work

1. **Make the dimension a stored fact, not a constant.** Add a `meta` row
   (`embedding_model`, `embedding_dim`) to each store's `init_schema()`.
   `DEFINE INDEX ... DIMENSION {dim}` is built from it. If the configured
   provider's dimension differs from the stored one, log a warning, drop and
   rebuild the index, and clear the `embedding` field on existing rows so the
   existing backfill sweep (`lessons::spawn_embedding_backfill_sweep`) refills
   them. Never mix dimensions in one table.
2. **Add an Ollama embedding candidate**, first in the candidate list in
   `main.rs` ~L1318. Ollama serves an OpenAI-compatible
   `/v1/embeddings`, so `ApiEmbedding::new` with an empty key and the Ollama
   base URL may already work; otherwise add a thin `OllamaEmbedding`. Default
   model `nomic-embed-text` (768 dims), configurable under
   `[memory] embedding_model` / `embedding_provider` in `config.rs` (wire it
   through `config.rs` like every other section: struct, defaults, `warn_missing`).
3. **Pull the model on demand.** When Ollama is reachable but the embedding
   model is absent, call `POST /api/pull` once (streaming progress to the
   log and to a feed event) rather than failing. Same for the default chat
   model in Phase 3.
4. **Keep the probe-and-fallback shape**: candidate order Ollama → default
   LLM base → OpenAI entry → Gemini. A user with no model at all still boots;
   recall is keyword-only and the dashboard says so (Phase 3.3).
5. Tests: dimension migration (768 → 1536 → 768) in a `memory` Surreal; the
   `director_lessons_store` knn comment at L171 describes a short-circuit
   trap, keep that test passing at the new dimension.

### 2.2 Exit

- Fresh install + Ollama with `nomic-embed-text` only: a lesson written in
  one chat turn is recalled semantically in the next, with no key anywhere.
- `STATUS.md` row for semantic memory no longer says "needs OpenAI".

---

## 3. Desktop first run — a person who has never seen a terminal

Today: config auto-creates, the gateway sidecar boots, the loopback login
flow (`desktop/src-tauri/src/login.rs`, RFC 8628 style, all `localhost`) is
already the right design. What is missing is the screen that leads from "the
window opened" to "a model is answering".

### 3.1 Onboarding route in the dashboard (`sveltfront/`)

New route `src/routes/welcome/` (or a gate in `+layout.svelte`): shown while
`GET /api/setup/status` says any of `account`, `lane`, or `workspace` is
incomplete. Steps, each one screen, each skippable later from Settings:

1. **Create your account.** Username + password, backup codes shown once with
   a "I saved these" checkbox. Calls the existing `/api/auth/register`. Copy
   says plainly: this account lives only on this computer.
2. **Pick a brain.** One card per lane the gateway can see, using
   `llm_provider_probe.rs` and the CLI detection the wizard already does
   (`wizard.rs` L117-L123): "Claude Code CLI (signed in)", "Codex CLI",
   "Ollama (local, free, works offline)", "Bring an API key". Each card has
   one button. For Ollama: if not installed, open the download page and poll
   until it appears; if installed but no model, pull a default
   (`consistent-beat` from `local-models/` if present, else a small
   general model) with a progress bar. For the CLIs: open the sign-in
   command in the embedded terminal (`desktop/src-tauri/src/pty.rs` already
   exists for this).
3. **Prove it.** Run one real completion through the chosen lane (the
   Connect card already does this, ROADMAP item 3) and show the answer.
   Nothing advances on a probe alone.
4. **Your first space.** The Personal Space entity is already created on
   first sign-in (`server.rs` ~L652); land the user in it with one suggested
   mission, not an empty workspace.

Gateway side: `GET /api/setup/status` (new, in `control_api/setup_api.rs`),
and **fix the known gap** that providers registered from the settings UI
"still need a config entry or resolution silently falls back to Ollama"
(CLAUDE.md). The onboarding must write the `[llm.providers.*]` and
`[llm.models.*]` entries through the same path the settings UI uses, and
`config::resolve_llm*` must read them without a restart.

### 3.2 Runs by itself

- **Launch at login**: add `tauri-plugin-autostart`, default on, toggle in
  Settings. The tray already exists (`build_tray`); closing the window keeps
  the gateway alive, quitting from the tray stops it.
- **Headless install** (`install.sh` already renders a LaunchAgent, Plan B
  phase 2): keep it working, point its download at GitHub Releases, add a
  systemd unit for Linux. Windows users get the installer only.
- **Updates**: add `tauri-plugin-updater` reading a `latest.json` published
  by `release-desktop.yml` to GitHub Releases, signed with a Tauri updater
  key held in CI secrets. Opt-in prompt ("A new version is ready"), never
  silent. Note for the owner: this is the one thing that depends on someone
  publishing releases; a fork changes one URL in `tauri.conf.json` to take
  it over.

### 3.3 Honest surfaces

Every screen that depends on something optional (semantic recall, voice,
browser tool, phone push) shows what it is missing and the one action that
fixes it, instead of an empty state. Reuse the `NeedsComputer` / `DemoNotice`
pattern from the mobile app.

### 3.4 Exit

Hand a laptop with nothing installed to a person who has not seen the app.
They reach a completed first mission without being told what to click.
Record where they stalled; fix that before calling the phase done.

---

## 4. Secure pairing — prove presence once, control from anywhere

Today (`remote_doctor::pair_payload`, `mobile/app/pair.tsx`): the QR carries
only `{url, workspace, v}`; the phone then logs in with the password or waits
for device approval. Binding is "knows the password", not "was at the
computer". Over Tailscale that is fine; over a relay or an exposed port it is
not. Plan B left "Devices screen and passkey client" open; this phase
replaces the passkey idea for phones with a device key, because WebAuthn
needs an HTTPS relying-party domain, which a self-hoster does not have.

### 4.1 Design

```
desktop (Settings → Pair a phone, or `consistent-world pair`)
  │  mints nonce N (random 32 bytes, 90 s TTL, single use), shows QR {url, workspace, v:2, nonce:N}
  ▼
phone scans → generates Ed25519 keypair, private key in expo-secure-store
  │  POST /api/pair/enroll {nonce, public_key, device_label, platform, install_id}
  │     gateway: nonce valid and unused? signature valid? → create pending approval
  ▼
desktop shows "iPhone (Moe) wants to pair — 4-digit code 7312 — Allow / Deny"
phone shows the same 4 digits (derived from nonce+pubkey, so both sides agree)
  │  user taps Allow on the desktop
  ▼
gateway stores public_key on the `devices` row (device_store.rs), returns a device session
  │
later, from anywhere:
  POST /api/auth/device-login {device_id} → challenge
  POST /api/auth/device-login/verify {device_id, signature(challenge)} → session token
```

### 4.2 Work

Gateway:

1. `pair_store` (or extend `device_store.rs`): nonces table with TTL and
   `used` flag; `public_key` + `enrolled_at` + `enrolled_from` on `Device`
   (remember the `device_field_completeness_guard` test that pins the
   projection; extend it).
2. Routes in `control_api/devices_api.rs` and `auth_api.rs`:
   `POST /api/pair/nonce` (authenticated; the desktop asks for it),
   `GET /api/pair/payload` (existing, now includes the nonce, `v: 2`),
   `POST /api/pair/enroll` (public, every call signed), `POST /api/auth/device-login` +
   `/verify` (public, challenge 60 s, Ed25519 via `ed25519-dalek`).
3. ~~LAN-only enrolment~~ **dropped during implementation.** The gateway
   binds loopback, so a phone never reaches it directly: every request
   arrives through Tailscale Serve or the relay, both of which connect from
   loopback. A LAN-only rule would refuse every pairing on a default install.
   Presence is proven by the screen-only code plus the owner's Allow with
   matching digits; the relay cannot forge a step because each one is signed
   by the phone's key.
4. Approval reuses the existing device-approval request/approve routes
   (`auth_api.rs` L131, L178) so the desktop UI and the "approve from an
   already-trusted phone" path both work.
5. Session issued to a device is tagged with `device_id`; revoking the device
   (`DELETE /api/auth/devices/{id}`, exists) revokes its sessions.
6. `consistent-world pair` CLI: mint the nonce, print the QR, then wait and
   print the 4 digits + Allow prompt in the terminal. Stop hard-coding
   workspace `"default"` (main.rs L725): read it from the signed-in context.

Desktop/dashboard:

7. Settings → Devices: show QR (the dashboard has no pairing UI today; the
   only QR is the CLI), list devices with last seen, Revoke button, and the
   pending-approval card with the 4 digits.

Mobile (`mobile/`):

8. `pair.tsx`: accept `v: 2`, generate the keypair (`@noble/ed25519` or
   `expo-crypto` if it has grown Ed25519 by then), store with
   `expo-secure-store` (`WHEN_UNLOCKED_THIS_DEVICE_ONLY`), call enroll, show
   the 4 digits while waiting, then store the session. Keep the `v: 1`
   path for one release so an older desktop still pairs with the password.
9. `client.ts`: on 401, try device-login before showing the login screen.
10. Add to Settings: "Forget this computer" which deletes the key and asks
    the gateway to revoke.

Relay:

11. No change to protocol, but document in `crates/relay/README.md` that the
    relay forwards an authenticated session and can therefore replay bytes
    but never originate them, and that enrolment never crosses it.

### 4.3 Exit

- After pairing, the phone controls the desktop over Tailscale from
  cellular with no password typed.
- Revoke on the desktop logs the phone out within one request.
- Tests: nonce single-use and expiry; private-range check table; signature
  verify rejects a wrong key; `device_field_completeness_guard` updated.

Parallel with Phase 3: yes, different files.

---

## 5. Ship the phone app — and let anyone build it

A self-hoster needs the phone app from a store (or a build they can make
themselves) or the pairing work is theoretical.

1. **App Store**: regenerate the screenshots from the simulator build (the
   earlier set was never committed) and submit with the maintainer's App
   Store Connect key, which lives outside this repository.
2. **Google Play**: same skill, `play-fit.sh` for sizes, 12 testers / 14 days
   gate for a personal account.
3. **Community builds**: document `npx expo run:ios` / `run:android` with a
   personal Apple ID (free provisioning) so a user can sideload without EAS
   or a paid account. EAS stays the author's convenience, not a requirement.
4. Commit the screenshots this time (`mobile/screenshots/`, small
   PNGs are fine to version) so the next release does not regenerate them.

Exit: the app is live in both stores, and a stranger can also build it from
the public repo in under an hour with the README alone.

---

## 6. Open-source release mechanics

Order matters: secrets sweep first, public flip last.

1. **Secrets sweep** of the whole history: run `gitleaks detect --source .`
   (or `trufflehog git file://.`). Known items to confirm are not secrets:
   `mobile/eas.json` holds an ASC key *id*, issuer id and app id (not
   secret, but rotate the key anyway since the `.p8` left the machine);
   `docker/*.toml` must hold no tokens. If anything real is found, rewrite history
   with `git filter-repo` **before** the flip and rotate the credential.
2. **`LICENSE`** at the root (MIT, owner's name, 2025-2026). The workspace
   manifest already says MIT; `mobile/`, `desktop/`, `sveltfront/`
   `package.json` get `"license": "MIT"`.
3. **Docs for humans**, in the root: `README.md` rewritten for a user, not
   for Claude (what it is, screenshots, Install: desktop / docker / from
   source, Pair your phone, Privacy: nothing leaves your machine,
   Contributing, License). `CONTRIBUTING.md` (the quality gates, the
   formatting rule, the mod-list rule, commit style). `SECURITY.md` (how to
   report, what the threat model is, link `docs/VAULT.md`).
   `CODE_OF_CONDUCT.md`. `.github/ISSUE_TEMPLATE/` bug + feature.
   Keep `CLAUDE.md`, `AGENTS.md`, `ARCHITECTURE.md` as they are; they are
   part of the value for people building on it.
4. **CI on a public repo**: Actions minutes become free, which unblocks the
   red `main` that `STATUS.md` records as a billing block. Keep the five
   toolchain pins in `ci.yml`. Add `release-desktop.yml` outputs
   (`Consistent-CEO-macOS-AppleSilicon.dmg`, `...-Intel.dmg`,
   `...-Windows-Setup.exe`) as **GitHub Release assets** and drop the
   S3 mirror step and its two secrets. Add a Linux `.AppImage`/`.deb` job if
   Tauri builds cleanly on `ubuntu-latest`; otherwise document "from source"
   for Linux.
5. **Docker for the "I just run it" user**: a single-container image using
   the embedded `surrealkv://` store (no SurrealDB service) with a volume at
   `/data`, published to GHCR by the release workflow. Keep the two-service
   compose as the "server" variant. Both must come up with no env vars set.
6. **Signing**: macOS notarization (`docs/macos-signing.md`) and Windows
   signing stay in CI secrets. Document in `CONTRIBUTING.md` that forks
   build unsigned installers and how a user opens one.
7. **Flip the repo public**, create the GitHub org or keep the personal
   account, fix the dead `github.com/consistent-ceo` link on the marketing
   site, tag `v1.0.0`. The marketing site (`Consistent/`, separate repo)
   gets a Download page that links to GitHub Releases, nothing else to host.
8. **Community without the author**: enable Discussions, add a
   `MAINTAINERS.md` stating the owner's availability honestly ("I use this
   daily and merge what I can; forks are welcome"), and a release checklist
   so anyone with write access can cut one.

Exit: `scripts/repo-health.sh --full` returns 0 on a public `main`; CI green;
a clean macOS, Windows, and Linux machine each reach a running gateway from
the README alone.

---

## 7. What is deliberately not in this plan

- Payments, credits, Apple IAP, Stripe, tokens. The ledger code stays dormant.
- A hosted cloud offering. The same binary already runs on a VPS or in
  docker; nothing more is promised.
- Bluetooth proximity, continuous proximity, passkeys for the phone.
- Renaming `consistent-world` anything.
- Rewriting the mobile app's auth for multi-user; one person, many devices.

---

## Sequencing and effort

| Phase | Depends on | Rough size | Parallel |
|---|---|---|---|
| 1 Cut the cords | — | 1-2 days | — |
| 2 Local brain | 1 | 2-3 days | with 3, 4 |
| 3 Desktop first run | 1 | 4-6 days | with 2, 4 |
| 4 Secure pairing | 1 | 3-5 days | with 2, 3 |
| 5 Ship the phone app | 4 (for the pairing screens), owner's ASC key | 2 days + review waits | with 6 |
| 6 Open-source release | 1, 2, 3 | 2-3 days | with 5 |

Recommended order for a single agent: 1 → 4 → 2 → 3 → 6 → 5. Pairing before
the first-run screen because the first-run screen's "Pair your phone" step
needs the new QR.

## Progress

| Phase | State | Date | Where / notes |
|---|---|---|---|
| 1 | done | 2026-10-11 | Billing opt-in (`billing::billing_enforced`), trial credits only when enforced, Railway config removed, `--offline` / `CONSISTENT_WORLD_OFFLINE`, brain check off the startup path, `tests/offline_boot_test.rs` boots the real binary with nothing installed. Downloads point at GitHub Releases; the S3 mirror steps are gone. |
| 2 | done | 2026-10-11 | `embedding_setup.rs` + `OllamaEmbedding`. Chose zero-padding into the existing 1536-wide indexes over a dimension migration: it preserves cosine exactly and changes no schema. Identity stamp in `system_meta:embedding`; sticky, never switches on a blip. Live-tested against Ollama. |
| 3 | mostly done | 2026-10-11 | The Connect-your-AI cards already existed. Added: Ollama "no model" told apart from "not running", one-click model download (`/api/ollama/pull`), embedding models filtered from chat choices, start at login hidden in the tray, close hides instead of killing the gateway, systemd user unit in `install.sh`. **Not done:** the auto-updater (needs a signing key the owner holds) and the stranger test. |
| 4 | done | 2026-10-11 | `pairing.rs`, `pair_cli.rs`, auth-store device keys, dashboard `PhonePanel.svelte`, phone `pairingKey.ts` + `pair.tsx` + key re-sign-in. Six digits, not four. LAN-only enrolment dropped (see §4). Also fixed: the remote doctor advertised a Wi-Fi URL for a loopback-bound gateway. Phone screen not yet run on a device. |
| 5 | blocked on owner | | App Store Connect key missing locally; screenshots need regenerating. |
| 6 | done except the flip | 2026-10-11 | LICENSE, README, CONTRIBUTING, SECURITY, CODE_OF_CONDUCT, MAINTAINERS, issue templates, GHCR image job, single-container Docker (self-provisioned token, `CONSISTENT_WORLD_PROVISION_TOKEN=1`). Published as a fresh repository, without the private history. |
