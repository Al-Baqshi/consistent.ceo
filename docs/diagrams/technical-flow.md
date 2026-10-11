# Consistent CEO — Technical Flow (current state)

> **Scope:** what is actually wired **today**, verified against the source, not
> the target architecture. Where something is designed but not enforced, it is
> drawn as a dashed line and called out. `docs/diagrams/consistent-world-future-state.v2.mmd`
> is the destination; this file is the position.
>
> Last verified: 2026-08-11, branch `feat/on-device-dictation`.

---

## 1. System at a glance

Three surfaces, one Rust binary, one embedded database. The gateway *is* the
product; every client is a view onto it.

```mermaid
flowchart TB
    subgraph clients["Surfaces"]
        desktop["Tauri desktop shell<br/>macOS DMG"]
        web["SvelteKit web UI<br/>served by the gateway"]
        mobile["Expo mobile app<br/>iOS / Android"]
    end

    subgraph gw["Gateway — single Rust binary, axum"]
        auth["Auth layer<br/>require_bearer_token"]
        api["HTTP API<br/>/api/*"]
        chat["Chat agent<br/>tool loop"]
        loops["Background loops<br/>reminders, scheduler, heartbeats"]
        files["Local file jail<br/>loopback only"]
    end

    subgraph data["State"]
        surreal[("SurrealDB<br/>embedded surrealkv")]
        vault["Encrypted vault<br/>Argon2id + device keys"]
        disk["Your disk<br/>~/Desktop, ~/Consistent"]
    end

    subgraph brains["Model providers"]
        claudecli["claude CLI<br/>your subscription"]
        ollama["Ollama<br/>local models"]
        cloud["Anthropic / Google / OpenAI<br/>API keys"]
    end

    desktop -->|"localhost:18800"| auth
    web --> auth
    mobile --> auth
    auth --> api
    api --> chat
    chat --> surreal
    chat --> files
    files --> disk
    api --> vault
    loops --> surreal
    chat --> claudecli
    chat --> ollama
    chat --> cloud

    desktop -.->|"spawns as sidecar"| gw
```

**Note the sidecar edge.** The desktop app ships the gateway inside the DMG and
launches it. There is no server to install and nothing to sign up for.

---

## 2. Authentication — three branches, in this order

`server.rs::require_bearer_token`. Order matters and is not arbitrary: the
static branch returns an open `anonymous` context when no token is configured,
so anything placed after it would never be reached.

```mermaid
flowchart TD
    req["Request with<br/>Authorization: Bearer ..."] --> s1{"Opaque session?<br/>SHA-256 lookup in auth_sessions"}
    s1 -->|hit| userctx["AuthContext<br/>kind = User"]
    s1 -->|miss| s2{"Looks like a JWT<br/>and an issuer is configured?"}

    s2 -->|"yes"| verify["Verify against issuer JWKS<br/>iss / aud / exp / kid<br/>algorithm from the JWK, never the header"]
    verify -->|valid| mapuser["Map subject onto a local auth_users row<br/>id = hash of issuer + subject"]
    mapuser --> userctx
    verify -->|invalid| s3
    s2 -->|"no issuer configured — the default"| s3

    s3{"Static deploy token?"} -->|"bare secret"| staticctx["AuthContext<br/>kind = Static"]
    s3 -->|"secret.workspace.principal"| gated{"static_principals<br/>opted in?"}
    gated -->|yes| staticctx
    gated -->|"no — the default"| deny["401"]
    s3 -->|"no match"| deny

    userctx --> handler["Handler runs"]
    staticctx --> handler
```

| Branch | Enabled by default | What it means |
|---|---|---|
| Opaque session | ✅ always | A real person, or their API key |
| External JWT | ❌ off | Only when `[gateway.auth.external] issuer` is set |
| Static token | ✅ loopback | "Whoever holds this machine" — the desktop sidecar |

`PrincipalKind` exists so a handler can tell a signed-in person from the holder
of a deploy secret. `/api/auth/local-session` is the one endpoint that requires
`Static`, and it also requires a loopback bind.

---

## 3. First run and sign-in

Two paths. The first is what almost everyone gets.

```mermaid
sequenceDiagram
    autonumber
    participant U as You
    participant App as Desktop shell
    participant GW as Gateway
    participant Br as System browser

    Note over U,GW: Path A — first run, no forms
    U->>App: double-click
    App->>GW: spawn sidecar, wait for /health
    App->>GW: POST /api/auth/local-session<br/>Bearer &lt;machine token&gt;
    GW->>GW: check kind = Static AND loopback bind
    GW-->>App: real session token
    App->>U: workspace opens

    Note over U,Br: Path B — sign in from a browser, or add a device
    App->>GW: POST /api/auth/approval/start<br/>+ PKCE challenge
    GW-->>App: request_id + 6-digit code
    App->>Br: open /login?approval=&lt;id&gt;
    U->>Br: sign in, type the 6 digits
    Br->>GW: POST /approvals/{id}/approve {code}
    App->>GW: poll /approval/{id}?verifier=...
    GW-->>App: session token
```

The PKCE verifier never leaves the Tauri process — the webview never sees it.
The request id travels through a URL, so it is not treated as a secret.

---

## 4. A chat turn, end to end

```mermaid
sequenceDiagram
    autonumber
    participant U as You
    participant API as POST /api/capture/chat
    participant CA as chat_agent::run_chat_turn
    participant LLM as Model provider
    participant T as Tools
    participant DB as SurrealDB
    participant FS as Your disk

    U->>API: message
    API->>CA: with AuthContext + workspace + entity
    CA->>CA: build_workspace_tools()
    CA->>LLM: prompt + tool schemas
    LLM-->>CA: tool call, e.g. save_note_to_desktop
    CA->>T: execute
    alt workspace tool
        T->>DB: create entity / task / mission / note / reminder
    else local file tool — loopback only
        T->>FS: write inside the jail, or refuse
    end
    T-->>CA: result JSON
    CA->>LLM: tool result
    LLM-->>CA: final reply
    CA-->>U: reply + ChatAction effects
    CA--)DB: Architect shadow run (default on; ARCHITECT_SHADOW=0 opts out)
```

### The twelve tools

```mermaid
flowchart LR
    subgraph ws["Workspace tools — always available, write to the database"]
        a["create_workspace"]
        b["create_entity"]
        c["create_note"]
        d["record_lesson"]
        e["create_task"]
        f["create_mission"]
        g["set_reminder"]
        h["list_entities"]
        i["recall_recent"]
    end

    subgraph lf["Local file tools — only when bound to loopback"]
        j["save_note_to_desktop"]
        k["create_folder"]
        l["write_local_file"]
    end

    lf --> jail{"jail()"}
    jail -->|"inside ~/Desktop or ~/Consistent"| ok["write"]
    jail -->|"'..' , absolute path, or symlink escape"| refuse["refuse, with a reason"]
```

On a network-bound gateway the file tools are **absent from the tool list
entirely**, not present-and-refusing — "your Desktop" would be a server's disk.

---

## 5. Background loops

Nothing here needs a browser open. This is what makes it a companion rather
than a chat window.

```mermaid
flowchart TD
    subgraph loops["Loops started at boot"]
        rt["ReminderTicker — every 20s"]
        sch["Scheduler — cron tasks"]
        hb["Heartbeat ticker — per entity"]
        orc["Orchestrator run_loop"]
    end

    rt -->|"due_at reached"| fire["Mark task complete<br/>push ScheduledTaskFired"]
    sch --> fire
    hb -->|"entity pulse"| ideas["Heartbeat result<br/>→ suggestions / tasks"]
    fire --> feed[("Operator feed")]
    ideas --> feed

    feed --> dpoll["Desktop companion polls<br/>/api/operator-feed"]
    feed --> mpoll["Mobile app polls while open"]
    dpoll --> native["Native OS notification"]
    mpoll --> push["In-app alert<br/>+ optional Expo push"]
```

Every entity gets a default heartbeat on creation — no entity without a pulse.

---

## 6. Where state lives

```mermaid
flowchart LR
    subgraph sdb["SurrealDB — embedded surrealkv by default"]
        t1["auth_users / auth_sessions"]
        t2["auth_devices / auth_approvals / auth_passkeys"]
        t3["entities"]
        t4["missions / tasks"]
        t5["operator feed"]
        t6["memory / lessons"]
        t7["canvas"]
    end

    subgraph fsx["Filesystem"]
        v["~/.consistent-world/<br/>config, db, vault, gateway_auth_token"]
        d["~/Desktop — notes"]
        c["~/Consistent — folders and files"]
    end
```

---

## 7. What is wired vs. what is not

This is the section to read before planning.

```mermaid
flowchart TD
    subgraph live["Working today"]
        w1["Entities, missions, tasks, notes"]
        w2["Chat with tool calling"]
        w3["Reminders → feed → OS notification"]
        w4["Local files: Desktop notes, folders"]
        w5["Auth: local accounts, passkeys, sessions, device approval"]
        w6["Encrypted vault"]
        w7["macOS DMG build in CI"]
    end

    subgraph partial["Built but not enforced"]
        p1["Architect pipeline<br/>shadow only (default on; ARCHITECT_SHADOW=0 opts out)<br/>fire-and-forget after the reply"]
        p2["architect_bindings<br/>NullMemoryReader / NullLessonsReader"]
        p3["/api/compat/*<br/>served and tested, no caller since iOS was removed"]
    end

    subgraph missing["Not built"]
        m1["Runtime agents: no agents block in any default config<br/>→ filesystem / shell / browser tools unreachable from task execution"]
        m2["Wallet / Stripe"]
        m3["Windows + Linux builds"]
        m4["Code signing + notarization"]
    end
```

### The three that matter most

1. **The evidence promise is not enforced.** `VISION.md` says every consequential
   claim must resolve to a cite, via the Architect's Verify stage. Today the
   Architect runs *after* the reply is produced, fire-and-forget, in shadow mode
   (default on; `ARCHITECT_SHADOW=0` opts out), and writes to `audit_trail`.
   Nothing the user sees is gated by it. This is Milestone 2 and it is the gap
   between the product's stated category and its behaviour.

2. **No runtime agent exists on a fresh install.** `FileSystemTool`, `ShellTool`,
   `BrowserTool` and the coding-agent tool are registered per `[[agents]]` entry,
   and no shipped config defines one — so the loop at `main.rs:371` never
   executes. The new chat file tools are deliberately independent of this, which
   is why "make me a folder" works while task execution still has no hands.

3. **Nobody outside can install it.** Unsigned DMG plus a private repo whose
   release assets 404 anonymously. Both are configuration, not code.

---

## 8. Shipping pipeline

```mermaid
flowchart LR
    tag["git tag v*"] --> ci["release-desktop.yml"]
    ci --> arm["macOS arm64 DMG"]
    ci --> intel["macOS Intel DMG"]
    ci -.->|"commented out:<br/>openssl-sys fails on windows-msvc"| win["Windows installer"]
    arm --> draft["Draft GitHub release"]
    intel --> draft
    draft --> mirror{"MIRROR_* secrets set?"}
    mirror -->|yes| bucket["Public R2/S3 bucket<br/>consistent.ceo/downloads"]
    mirror -->|"no — current state"| skip["Step skips.<br/>Private repo assets 404 for visitors"]
    draft --> pub["Publish release"]
    bucket --> page["/download page buttons work"]
    skip -.-> dead["Download buttons resolve to nothing"]
```

⚠️ The `/download` page advertises a **Windows** installer that CI does not
build. That link is dead today — fix the build or remove the button.

---

## 9. Risks worth tracking

| Risk | Why it matters | Where |
|---|---|---|
| Architect not enforcing | The core product claim is unverified in practice | `chat_agent.rs:628` |
| No default runtime agent | Task execution cannot touch files or shell | `main.rs:371` |
| Model absent on first run | Needs `claude` CLI or Ollama; neither ships in the DMG | `DEFAULT_CONFIG` |
| Unsigned builds | Gatekeeper blocks; most users stop there | `release-desktop.yml` |
| `localhost` vs `127.0.0.1` | Passkeys are origin-bound; the app now uses `localhost` throughout, relying on the browser's `::1` → IPv4 fallback | `login.rs` |
| Compat layer with no caller | Dead surface that still costs maintenance | `/api/compat/*` |
