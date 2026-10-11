# Plan B — Drive the Mac from the phone, from anywhere

Goal: from anywhere (e.g. the car), chat or voice on the phone triggers real,
approval-gated actions on the desktop through the gateway — manage files, drive
a browser, and more — with results and approvals pushed back to the phone.

Status: **Phases 1-4 implemented** (2026-09-04); parts of Phase 5 remain.
Verified against `feat/ceo-assistant-phases`.

## What shipped

| Phase | State | Where |
|---|---|---|
| 1 — car scenario | done | `remote_doctor.rs` + `consistent-world remote setup\|status` (Tailscale detect/serve/URL), `consistent-world pair` (terminal QR), `GET /api/pair/payload` + `GET /api/remote/status`, `RunOnMyComputerTool` + `internal_create_task_queued` + linked approval, `patch_approval` now stamps `approvedAt` / cancels on reject / writes through, mobile QR scanner + honest-health banner + inline action card |
| 2 — always-on | done (+ task detail screen) | `install.sh` (+ `sveltfront/static/` mirror) renders and bootstraps a real LaunchAgent at the path it actually installs to; desktop probes `/health` before spawning its sidecar (the two-gateways-on-18800 landmine); new `power.rs` holds `caffeinate` only while runs/approvals/queued work exist |
| 3 — browser + voice | done | `BrowserTool` gains `CONSISTENT_WORLD_BROWSER_PROFILE_DIR` (dedicated profile, never the operator's Chrome), new `mcp_browser.rs` + `consistent-world mcp-browser` stdio MCP server, `SessionRequest.mcp_config_path` wired only for `actionKind == "browser"`, Gemini Live gained tool-calling and voice declares `run_on_computer` |
| 4 — relay | done (self-hostable; hosting is yours to choose) | new `crates/relay` (`protocol.rs` frames, `server.rs` link + proxy, binary, Dockerfile, README) and `crates/gateway/src/relay_client.rs` (outbound dial, streamed replay against loopback, reconnect backoff). Relay URL now leads pairing discovery in `remote_doctor.rs` |
| 5 — hardening | partial | per-principal rate limiting (`limit_authenticated`, 240 req/min, 20 streams/min, loopback exempt) and **session idle expiry** (`session_is_idle_expired`, 14 days unused; API keys exempt since revocation is their kill switch) both shipped. Devices screen and passkey client still open |

**The relay ships as an artifact, not a service.** The code is done and tested
end to end (a request reaching a loopback-only gateway, a streamed response
arriving in pieces, an offline gateway answering 503, a static token refused,
a wrong-secret link never becoming routable). What remains is *yours to
choose*: where to run it and on what domain. That domain decision also fixes
`PASSKEY_RP_ID`, which is why the passkey client is still open — passkeys are
origin-bound and there is no stable origin until you pick one.

Discovery order is now relay → tailnet → LAN, so adding a relay simply upgrades
what `consistent-world pair` hands out; nothing else changes.

**One bug worth remembering.** The forwarding client was first built with
`timeout(Duration::from_secs(0))`, meaning "no timeout" — reqwest reads that as
*expire immediately*, so every forwarded request failed the instant it was
sent. Frames encoded correctly, the link established correctly, and only a
request travelling the whole path revealed it. To have no overall timeout,
omit `.timeout()` entirely. Guarded now by
`crates/gateway/tests/relay_link_test.rs`, which was verified to fail with the
bug re-injected.

## Post-audit fixes (2026-09-05)

An adversarial multi-agent audit of this session's own work found real defects.
The ones fixed here, with what each would have cost:

| Severity | Defect | Fix |
|---|---|---|
| critical | **The relay 403'd every real session.** `looks_like_static_token` guessed that session tokens contain a `.`. They do not — `AuthStore::gen_token()` mints 64 hex characters with no separators, and the *auto-provisioned deploy token uses the same generator*, so no predicate over the string can tell them apart. Worse, the guess was inverted: the dotted form is the deploy token's `<secret>.<workspace>.<principal>` impersonation syntax. The relay was non-functional for its only client and would have admitted the one shape it should refuse. | Deleted the predicate. The relay now stamps an authoritative `x-consistent-relay` marker (stripping any client-supplied copy first, so it can only tighten), and the **gateway** — the only party that can resolve a bearer — refuses `PrincipalKind::Static` on marked requests. Verified live: real session via relay 200, deploy token via relay 403, deploy token on loopback still 200. |
| high | **Rate limiting was dead code.** `limit_authenticated` skipped every loopback bind — which is exactly the configuration the relay creates, so the limiter never ran in the one case it exists for. | Exempt local requests, not loopback *binds*: throttle when the relay marker is present. |
| high | **The model could rewrite its own browser policy.** MCP passed the model's raw arguments through, and `_policy.allowedDomains` *replaces* the allowlist while `_policy.screenshotDir` redirects file writes. | `_policy` is stripped from model-supplied arguments; policy comes only from the executor-set environment. |
| high | **A relay 503 read as "reachable".** The relay answers 503 for a disconnected gateway — literally "your Mac is offline" — and the phone recorded it as a successful reach, so the unreachable banner stayed hidden during the exact outage it exists to report. | 503 now records an outage. |
| medium | **An approved action could strand.** An `executable` task in `Backlog` is owned by no loop: the orchestrator defers to the executor, the executor takes only `Queued`. | Approving an executable task promotes it to `Queued`. |

Also fixed in the same pass:

| Severity | Defect | Fix |
|---|---|---|
| high | **A reconnecting gateway was evicted by its own dead link.** Cleanup removed the map entry by id, so a link unwinding after the gateway had already reconnected deleted the *live* replacement — the relay then reported offline while the gateway believed it was connected. | Cleanup removes the entry only if it is still that same connection (`Arc::ptr_eq`). |
| high | **A restart mid-run wedged the task permanently.** `resume_interrupted` can only resume a run that reported a `sessionId`; a gateway dying before that leaves an unresumable run and a task pinned `InProgress` — the executor scans only `Queued`, the orchestrator defers to the executor, and the stuck-task watchdog is deliberately disarmed for these long runs. | New `release_unresumable_runs` at startup treats it as a crash: retried if attempts remain, otherwise handed to the operator. |
| high | **Per-call filesystem root overrides were silently discarded** (pre-existing, not from this session). `check_policy` merged the `_policy` override then read roots from the *constructed* policy, so the jail in force was never the jail the caller specified. | Roots are read from the policy actually in force. |

**The lesson worth keeping:** the relay's end-to-end tests passed while the
feature was 100% broken, because they authenticated with a hand-invented
`cws_workspace.principal.random` token — a shape nothing in the product mints.
A test fixture that invents its own contract tests nothing. Those fixtures now
use a token shaped exactly like `gen_token()` output.

**A flaw the task screen exposed.** `metadata.activeRunId` is cleared the
moment a run finishes — which is exactly when that run's screenshots become
worth looking at, so artifacts were unreachable for every completed task. The
first workaround hunted the activity timeline for an `agent_run` row. The root
fix is one line in `apply_run_outcome_to_task`: carry the finished run forward
as `metadata.lastRunId` before blanking the active pointer. The client prefers
it and keeps the timeline scan only for tasks finished by an older gateway.

**Why idle expiry, not just absolute TTL.** A 30-day TTL was a weak bound once
the gateway became reachable from outside the LAN: a token lifted from a phone
nobody had opened in weeks would still authenticate for the remainder of its
life. `last_used_at` was already tracked for other reasons, so enforcing a
14-day idle window cost almost nothing and never nags anyone actually using the
app. A malformed timestamp deliberately does *not* expire a session — that is a
bug in our own bookkeeping, not evidence against the user.

**Not tunnelled yet: the voice WebSocket.** The relay forwards HTTP, including
streamed NDJSON (which covers chat). Voice uses a WebSocket upgrade, which
needs a second frame path through the tunnel. Voice therefore works on the
tailnet today, not through the relay.

## Key verified facts

- Mobile chat, streaming, voice (Gemini Live STT + device TTS), and approvals
  UI are all real. The blocker is connectivity: no relay/tunnel/hosted endpoint
  exists; the phone defaults to `http://127.0.0.1:18800` with a manual URL
  field; off-LAN the app silently degrades to mock fixtures.
- The approve-then-execute seam is half built: `patch_approval` on `Approved`
  clears `requires_human_approval` on the linked task and leaves it `Queued`
  for pickup. Only a creation path producing executor-visible tasks is missing
  (chat tasks land `Backlog` + `metadata: None`).
- Voice has **no** tool bridge today (`voice.rs` declares no functions).
- `resolve_entity_agent_run_cwd` falls back to the entity's local-vault folder,
  so phone actions can run without a configured repo path.
- Per-device revocation already exists server-side
  (`DELETE /api/auth/sessions/{id}`, `DELETE /api/auth/devices/{id}`).
- Landmine: the desktop Tauri shell unconditionally spawns the gateway sidecar
  (`desktop/src-tauri/src/main.rs`); installing a LaunchAgent means two
  gateways fighting over 18800 — the desktop must probe `/health` and attach.
- `install.sh` installs to `~/.local/bin` and never touches launchd, while
  `world.consistent.plist` hardcodes `/usr/local/*` paths.
- Executor runs are `claude` CLI jailed only by cwd; file/shell phone actions
  work through it as-is; BrowserTool is not reachable from CLI runs (no MCP
  config today).

## Design decisions

1. **Connectivity — two steps.**
   - *Stopgap (Phase 1):* automated Tailscale — `consistent-world remote setup`
     detects tailscale, runs `tailscale serve --bg https / http://127.0.0.1:18800`,
     prints/QRs the `https://<mac>.<tailnet>.ts.net` URL. Gateway bind, auth
     guards, token provisioning untouched; WS + NDJSON streaming work through it.
   - *Primary (Phase 4):* `crates/relay` — a thin self-hostable relay both the
     phone and gateway dial **out** to over WSS; the relay pairs them per
     account/device and shuttles frames; the gateway keeps binding loopback and
     every request is still end-to-end authenticated by own-auth session
     bearers. Hosted instance = the "managed relay" GTM line. The relay refuses
     static-principal bearers and detects a dropped gateway connection to push
     "Mac unreachable".
   - Rejected: embedding a WireGuard/Tailscale-style tunnel (NAT traversal +
     iOS NEPacketTunnelProvider — not solo-maintainer territory); cloudflared
     (third-party traffic path, no auth in front by default).
2. **QR pairing replaces the URL TextInput.** `GET /api/pair/payload`
   (protected) returns `{ url, workspace, v }` where url is discovered relay →
   tailscale serve status → LAN IP. `consistent-world pair` renders a terminal
   QR; desktop/web render the same payload. Mobile adds `expo-camera` scan →
   `setGatewayUrl` → `/health` → existing device-approval sign-in. No secret
   ever rides in the QR.
3. **Actions: chat enqueues, executor executes.** New chat tool
   `RunOnMyComputerTool` (`{ instruction, kind: files|shell|browser|general,
   entity_id? }`) creates a task with `task_kind: "machine_action"`,
   `requires_human_approval: true`, `metadata { executable: true, source:
   "mobile-chat", actionKind }`, `status: Queued` (new internal
   `queue_immediately` on `CreateTaskRequest`), plus a linked
   `ApprovalRequest`. Approve on the phone (existing PATCH) → executor picks it
   up → results via `TASK_STATUS` trailer + feed + push. Chat's `done` payload
   carries an `action_request` ChatAction (taskId/approvalId) rendered as an
   inline ApprovalCard that walks pending → approved → running → done/blocked.
   Denial: `patch_approval` on `Rejected` must cancel the linked task.
   Widening chat tools with direct shell/browser powers is rejected (synchronous
   streamed turn, no approval gate, no run record).
4. **Browser actions.** `BrowserTool` gains `user_data_dir` (dedicated
   automation profile under `~/.consistent-world/browser-profile`; never the
   personal Chrome profile; headful by default on macOS). New
   `consistent-world mcp-browser` stdio MCP server wraps one BrowserTool
   (navigate/click/type/text/evaluate/wait_for/screenshot/close) with policy in
   Rust env (`[browser_actions]` config: allowed_domains unattended for
   read/screenshot, everything else approval-gated; payment/banking domains
   deny-listed by default). `agent_sessions.rs::build_args` appends
   `--mcp-config` when the task's `actionKind == "browser"` (threaded via a new
   `SessionRequest.mcp` field). Screenshots land in
   `~/.consistent-world/artifacts/<run_id>/`, served by new path-jailed
   `GET /api/agent-runs/{id}/artifacts[/{name}]` endpoints.
5. **Always-on.**
   - `install.sh` (and its `sveltfront/static/` mirror) gains
     `install_launchd()`: render `~/Library/LaunchAgents/world.consistent.plist`
     with the real binary path, `KeepAlive`, logs under
     `~/Library/Logs/consistent-world/`, `launchctl bootstrap gui/$(id -u)`
     (bootout first for repair). LaunchAgent, not daemon (keychain, claude CLI
     login, Chrome all need the user session).
   - Desktop sidecar: probe `/health` before spawning; attach if already up.
   - Sleep: hybrid — new `power.rs` spawns `caffeinate -is -w <pid>` while
     in-flight runs / fresh pending approvals / active remote sessions exist,
     drops it when idle; document `sudo pmset -c sleep 0` for hard always-on.
   - Phone health honesty: mock-fixture fallback only when the configured URL is
     the default (demo mode); otherwise an explicit "Mac unreachable since …"
     banner. New `GET /api/remote/status` (uptime, tailscale/relay detection,
     power assertion, launchd detection) powers a connection-details screen.
6. **Security rails before leaving the LAN.** Remote = own-auth sessions only
   (static token stays loopback-only by construction); per-principal rate
   limits on protected routes (generous ceilings); session TTL + sliding
   refresh, shorter TTL for remote-paired sessions; Devices & sessions screen
   on mobile (server endpoints exist); keep no CORS (single-origin allowlist
   from config only if the dashboard ever moves origins — never `*`); passkeys
   client (`react-native-passkeys`) only against a stable relay domain
   (origin-bound), scheduled with Phase 4.

## Phases

### Phase 1 — Car scenario end-to-end (Tailscale stopgap + action round trip)
1. `consistent-world remote setup` / `remote status` CLI +
   `crates/gateway/src/remote_doctor.rs` (tailscale detect/serve/URL). README
   docs.
2. QR pairing: `/api/pair/payload`, `consistent-world pair` terminal QR,
   `expo-camera` scanner in settings, wired into device-approval login.
3. Action round trip: `RunOnMyComputerTool` + `queue_immediately` +
   linked approval; inline `action_request` ApprovalCard in chat;
   `requeue_task` clears attempt markers; `Rejected` cancels the task.
4. `TaskBlocked` added to notify allowlist + titles.
5. Health honesty: demo-only mock fallback, unreachable banner.

### Phase 2 — Reliability: always-on + push
launchd install/repair + static mirror sync; desktop `/health` probe;
`power.rs`; push-on flow (`mobile_push = true`, EAS projectId, deep links);
`/api/remote/status` + connection details; `mobile/app/task/[id].tsx`.

### Phase 3 — Browser actions + voice-initiated actions
BrowserTool profile + `[browser_actions]` config; `mcp-browser` subcommand;
`SessionRequest.mcp` + `--mcp-config`; artifacts endpoints + screenshots on the
task screen; Gemini Live `run_on_computer` function → same enqueue path, spoken
"queued — approve on your screen".

### Phase 4 — `consistent-relay`
New `crates/relay` (axum + WSS) + `crates/gateway/src/relay_client.rs`
(outbound dial, multiplexed HTTP/WS frames incl. NDJSON + voice WS);
account-scoped gateway registration; presence → "Mac unreachable" push;
passkeys client against the relay domain; self-host deployment recipe.

### Phase 5 — Hardening polish
Per-principal rate limits; session TTL/sliding refresh; Devices & sessions
screen; optional E2E layer over the managed relay; per-domain unattended
browser policy; optional read-only trusted-command fast lane (per-entity
opt-in).

## Interface contract with Plan A (heartbeat action loop)
Phone actions arrive as tasks with `status=Queued`, `metadata.executable=true`,
`metadata.actionKind`, `requires_human_approval` cleared by approval; results
are reported via `TASK_STATUS` trailer + operator-feed events; retries must
clear attempt markers. Shared fixes (built once): requeue clearing, TaskBlocked
notify, executor durability.

## Critical files
- `crates/gateway/src/chat_tools.rs`
- `crates/gateway/src/control_api.rs`
- `crates/gateway/src/notify.rs`
- `mobile/src/api/client.ts`
- `install.sh`
