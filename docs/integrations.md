# Integrations

Two doors between Consistent CEO and the rest of your stack:

- **Inbound** — a scoped API key lets any external platform (Zapier, n8n, a cron
  box, another agent) call the gateway's HTTP API.
- **Outbound** — a webhook subscription pushes operator-feed events to a URL you
  own.

Everything below is asserted by `crates/gateway/tests/connectivity_test.rs`. If a
shape here is wrong, that test is the place it gets fixed.

---

## Inbound: API keys

An API key **is** a session — the same `auth_sessions` row an interactive login
mints, marked `kind: "api"` with a ~10-year expiry. That means a key works on
**every** `/api/*` route, with exactly the permissions of the user who created
it, and revoking it takes effect on the next request.

There is no narrower scope than "this user's workspace" today. Treat a key as
equivalent to that user's password, and mint one key per integration so you can
revoke them independently.

### Mint a key

Authenticate as yourself first (a normal login token, or an existing key):

```bash
TOKEN=$(curl -s -X POST https://your-gateway/api/auth/login \
  -H 'content-type: application/json' \
  -d '{"username":"you","password":"..."}' | jq -r .token)

curl -s -X POST https://your-gateway/api/auth/api-keys \
  -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"name":"Zapier"}'
```

```json
{
  "id": "6f1c…",
  "name": "Zapier",
  "createdAt": "2026-08-01T09:00:00+00:00",
  "expiresAt": "2036-07-29T09:00:00+00:00",
  "token": "9a3f…"
}
```

**`token` is shown exactly once.** Only its SHA-256 is stored; no later read can
recover it. Lose it and you mint a new one.

| Method | Path | Notes |
| --- | --- | --- |
| `POST` | `/api/auth/api-keys` | Body `{"name": "..."}` (optional, defaults to `API key`, capped at 64 chars). Returns the raw token once. |
| `GET` | `/api/auth/api-keys` | `[{id, name, createdAt, expiresAt}]` for the calling user only. Never includes tokens. |
| `DELETE` | `/api/auth/api-keys/{id}` | `204` on success, `404` if the key isn't yours. The token stops authenticating immediately. |

### Call the API with it

```bash
curl -H "Authorization: Bearer $API_KEY" https://your-gateway/api/captures
```

Two endpoints are the natural integration surface.

#### `POST /api/captures` — the inbound firehose

A capture is raw, pre-interpretation input: a message, a transcript, a note,
anything an external system saw and you want the CEO to know about. Nothing is
interpreted at write time — captures land in the workspace inbox for routing and
promotion.

Request fields are **snake_case**. Only `raw_text` is required:

```bash
curl -X POST https://your-gateway/api/captures \
  -H "Authorization: Bearer $API_KEY" \
  -H 'content-type: application/json' \
  -d '{
    "raw_text": "Investor asked for the Q3 deck by Friday",
    "source": "zapier",
    "source_ref": "msg_123"
  }'
```

| Field | Type | Meaning |
| --- | --- | --- |
| `raw_text` | string, **required** | The text itself. Empty → `400`. |
| `entity_id` | string | Attach to one entity. Omit for the workspace inbox. Unknown id → `400`; not visible to you → `403`. |
| `source` | string | Producer channel. Defaults to `chat`. |
| `source_ref` | string | Your stable id (message id, run id, URL). |
| `session_id` | string | Conversation/thread id, if you have one. |
| `provider` | string | Model/provider that produced it, when relevant. |
| `summary` | string | Pre-computed summary, if you have one. |
| `metadata` | object | Anything else; stored verbatim. |

Response is the stored capture (`200`), including `id`, `workspace_id`,
`status: "raw"`, and `created_at`. `GET /api/captures` lists them.

#### `POST /api/missions` — state a goal

A mission is a goal with a home. Creating one kicks off task planning (an LLM
planner when a brain is configured, deterministic starter tasks otherwise), so
this is how an external system says "make this happen" rather than "here's some
input".

```bash
curl -X POST https://your-gateway/api/missions \
  -H "Authorization: Bearer $API_KEY" \
  -H 'content-type: application/json' \
  -d '{
    "entity_id": "e_ops",
    "title": "Ship the Q3 deck",
    "goal": "Investor-ready deck reviewed and sent by Friday"
  }'
```

`entity_id` and `title` are required; the entity must be one you can see (find
ids with `GET /api/entities`). `goal`, `summary`, `priority`, `owner`, `source`,
`review_cadence_hours`, and `assigned_director_id` are optional. Returns the
created mission (`200`) with its `id` — poll `GET /api/missions/{id}` and
`GET /api/tasks?mission_id=…` to watch it develop.

#### `POST /api/entities/{id}/records` — the entity ledger

A capture is raw and a mission is a goal; a record is neither — it is one
**structured, sourced fact** about the entity's world: a customer, a lead, a
deal, a piece of content, a connected account, a file, a metric snapshot, an
event. This is the door an external system uses to tell Consistent what it
actually has, not just that something happened.

Every record carries where it came from. `source` names the producer (e.g.
`"baqshi_crm"`, `"monday"`); `source_ref` is the row's own id in that system.
Together they make the write idempotent: re-posting the same `(source, kind,
source_ref)` overwrites the row in place — `created_at` is preserved, `fields`
and `updated_at`/`fetched_at` are refreshed — so a nightly sync never
duplicates what it already sent.

Body is a batch, snake_case, one POST for many rows:

```bash
curl -X POST https://your-gateway/api/entities/e_qsme/records \
  -H "Authorization: Bearer $API_KEY" \
  -H 'content-type: application/json' \
  -d '{
    "records": [
      {
        "kind": "customer",
        "source": "baqshi_crm",
        "source_ref": "cust_482",
        "fields": {"name": "Acme Builders", "stage": "active"},
        "evidence": "https://crm.example.com/crm/customers/482"
      },
      {
        "kind": "content",
        "source": "baqshi_crm",
        "source_ref": "idea_119",
        "fields": {"title": "A subbie put his price up 8%", "status": "approved"}
      }
    ]
  }'
```

| Field | Type | Meaning |
| --- | --- | --- |
| `kind` | string, **required** | What this is. Documented: `contact`, `customer`, `deal`, `content`, `account`, `file`, `metric`, `event` — but any string is accepted, so a new source is never blocked on a schema change here. |
| `source` | string, **required** | The producer, e.g. `"baqshi_crm"`, `"monday"`, `"manual"`. |
| `source_ref` | string, **required** | The row's stable id in that source system. This plus `source` (and `kind`) is the whole upsert key. |
| `fields` | object | Open shape — Consistent does not know a CRM customer from a Monday item, only that it has fields worth keeping. |
| `evidence` | string | A URL, an API response id, a screenshot reference — where a human could go verify this claim. Optional. |
| `fetched_at` | string | When the source last confirmed this row was current. Defaults to now. |

Response (`200`) is `{"upserted": N, "records": [...]}`, each record carrying
its resolved `id`, `createdAt` and `updatedAt`. A `404` means the entity
either doesn't exist or isn't visible to you; a `400` means one record was
missing `kind`/`source`/`source_ref`.

`GET /api/entities/{id}/records` lists an entity's ledger, most recently
fetched first; add `?kind=customer` to narrow it to one kind. These records
are also what the Architect's prompt assembly and Verify step read, so a
well-sourced ledger is what turns "you have some open deals" into "you have 7
open deals" with a place to check each one.

---

## Outbound: webhooks

Subscribe a URL to this workspace's operator feed. Each matching event is a
single `POST`:

```json
{
  "event": {
    "id": "evt_…",
    "timestamp": "2026-08-01T09:00:00+00:00",
    "kind": "task_overdue",
    "title": "Ship the Q3 deck",
    "detail": "Due 2 days ago",
    "taskId": "t_…",
    "entityId": "e_ops"
  },
  "workspaceId": "…"
}
```

The event body is the operator-feed event verbatim (camelCase; optional fields
omitted when absent). `kind` is the snake_case feed kind.

| Method | Path | Notes |
| --- | --- | --- |
| `GET` | `/api/webhooks` | Subscriptions for your workspace. |
| `POST` | `/api/webhooks` | Body `{"url": "https://…", "kinds": ["task_overdue"]}`. `201` with the created subscription. Non-`http(s)` or relative URLs → `400`. |
| `DELETE` | `/api/webhooks/{id}` | `204`; `404` if it belongs to another workspace. |

`kinds` filters on feed-kind names; **omit it or pass `[]` to receive
everything delivered**. Matching is case-insensitive and trims whitespace.

### What actually gets delivered

Delivery happens inside the notification dispatcher, which today only receives
the *notifiable* subset of feed kinds (the same allowlist that governs push —
see `is_notifiable` in `crates/gateway/src/notify.rs`). So a subscription can
currently receive:

`scheduled_task_fired`, `approval_needed`, `approval_request_created`,
`mission_user_input_needed`, `heartbeat_result`, `agent_run_completed`,
`agent_run_failed`, `mission_stalled`, `task_stuck`, `task_overdue`,
`daily_digest`.

Subscribing to any other kind is accepted but will not fire until the dispatcher
is fed the full stream.

### Delivery guarantees

There are none worth the name, and that is deliberate:

- **Best effort, no retries, no queue.** One `POST` per event per subscription,
  10-second timeout. A non-2xx or a timeout is logged and dropped.
- **No ordering guarantee** across subscriptions.
- **No signature yet.** Use an unguessable path or a query token in the URL you
  register, and treat the payload as untrusted input.
- **A down subscriber never affects the gateway.** Failures cannot propagate
  into feed writes or notifications.

If you need durability, have your endpoint acknowledge quickly and reconcile
against `GET /api/operator-feed`, which is the authoritative history.

### Not to be confused with `ALERT_WEBHOOK_URL`

`ALERT_WEBHOOK_URL` is a separate, single-URL, Slack-shaped hook for *deployment*
alerts (backup failures and the like) aimed at whoever operates the server. It is
unrelated to these subscriptions and unaffected by them.

---

## Security notes

- A key carries its creator's full access. Mint one per integration; revoke
  rather than rotate secrets by hand.
- Keys can mint keys (a key authenticates `/api/auth/api-keys` like any session).
  Revoking a key does **not** revoke keys it created — audit
  `GET /api/auth/api-keys` after a suspected leak.
- Raw tokens are never logged and never returned by any read endpoint.
- List and revoke are always scoped to the calling user; ids alone are not
  authority.
