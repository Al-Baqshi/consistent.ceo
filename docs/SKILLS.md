# Skills

A **skill** is a short, reusable procedure the agent loads when a request calls
for it: how you run a weekly review, price a deal, brief a contractor, ship a
release. Skills are how the way you work compounds — written once, used by chat
and by unattended agent runs, on every venture they apply to.

Consistent CEO uses **Claude's skill format** unchanged, so a skill written here
works in Claude Code and a skill from `~/.claude/skills` works here. Importing is
a copy, not a conversion.

Code: `crates/gateway/src/skills.rs` (library, tools, prompt rendering),
`crates/gateway/src/control_api/skills_api.rs` (HTTP), built-ins in
`crates/gateway/skills/`. Dashboard: `/skills`. Mobile: Profile → Skills.

## Anatomy

A skill is a folder containing a `SKILL.md`:

```markdown
---
name: vendor-invoice-review
description: Review a vendor invoice against the contract and flag discrepancies. Use when the user forwards an invoice, asks whether a bill is right, or mentions a vendor charge.
---

# Vendor invoice review

Why this procedure exists, in a sentence or two.

## Steps
1. …

## Rules
- …
```

- `name` — lowercase letters, digits and hyphens, max 64 characters. It is also
  the folder name and the skill's identity.
- `description` — max 1024 characters. **This is what the agent sees when
  deciding whether to load the skill**, so it must say what the skill does and
  *when to use it*, in the words a user would actually type.
- Supporting files (templates, checklists, reference docs) sit next to
  `SKILL.md`; the instructions can point at them.

Other frontmatter keys (`allowed-tools`, `license`, `metadata`, …) are preserved
byte for byte when a skill is edited here.

## Scopes

| Scope | Where it applies | Editable |
|---|---|---|
| **Built-in** | Every workspace — the Consistent CEO playbook | No — switch off, or *customize* a copy |
| **Workspace** (`global`) | Every chat and agent run in the workspace | Yes |
| **Project** (`entity`) | Only while that entity is active | Yes |

When two skills share a name, the most specific wins: a project skill shadows a
workspace one inside that project, and a workspace skill shadows a built-in
everywhere. Switching the specific one off falls back to the next. The
dashboard shows "Replaced by …" on a shadowed skill.

The built-in playbook: `ceo-daily-brief`, `venture-weekly-review`,
`decision-memo`, `investor-update`, `mission-planner`, `meeting-to-actions`,
`delegation-brief`, `customer-signal-synthesis`, `skill-author`. Each is written
against the gateway's own chat tools (`review_today`, `recall_recent`,
`create_task`, `create_mission`, `update_entity`, …), so following one acts
inside the product rather than only advising.

## How the agent uses them

**Progressive disclosure, as Claude does it.** Only one line per active skill —
`name — description` — rides the system prompt (bounded to ~8k characters).
The body is loaded on demand:

- **Chat** (`chat_agent.rs`): the index is a system message on every turn, with
  or without an entity selected. The model calls the `load_skill` tool when a
  request matches, then follows the instructions it returns. `save_skill` turns
  a procedure the user approved into a new skill (project scope when a project
  is active, else workspace); the `skill-author` built-in teaches the model to
  draft, show, and only then save.
- **Unattended agent runs** (`task_executor.rs`): the `claude` CLI session gets
  the index with each skill's absolute `SKILL.md` path in its task prompt, plus
  an explicit exception to the directory jail for reading those files. Runs are
  `bypassPermissions`, so the CLI can read them where they live; built-ins are
  materialized under `builtin/` for this.

Skills are read fresh on every turn and run — switching one off takes effect
immediately.

## Storage

Skills live on disk, not in SurrealDB, because they are file bundles and because
agent sessions read them by path:

```text
~/.consistent-world/skills/
  builtin/<name>/SKILL.md                         # materialized copies for agent runs
  workspaces/<workspace>/global/<name>/SKILL.md
  workspaces/<workspace>/entities/<entity_id>/<name>/SKILL.md
  workspaces/<workspace>/state.json               # which skills are switched off
```

Writes are atomic (temp file + rename) and serialized by a lock.

## API

All routes are workspace-scoped through the normal bearer auth. Errors return
`{ "error": "<sentence for the user>" }`.

| Method & path | Purpose |
|---|---|
| `GET /api/skills` | Management view: every scope, every visible entity. Response `{ skills, entities, localImport }`. |
| `GET /api/skills?entityId=<id>` | What that entity's chat sees, with shadowing computed for it. |
| `GET /api/skills/{id}` | One skill with `body` and `raw` `SKILL.md`. |
| `POST /api/skills` | Create: `{ scope: "global"\|"entity", entityId?, name, description, body }`. |
| `PATCH /api/skills/{id}` | Edit: `{ description?, body? }` or `{ raw }` (whole `SKILL.md`; renaming is refused). |
| `DELETE /api/skills/{id}` | Delete (not built-ins). |
| `POST /api/skills/{id}/enabled` | `{ enabled }` — works for built-ins too. |
| `POST /api/skills/{id}/copy` | Copy into `{ scope, entityId? }` — "customize" a built-in, or adapt a skill for one project. |
| `POST /api/skills/upload` | Import an uploaded `{ skillMd, files: [{path, content}], scope, entityId?, overwrite }`. Works on any gateway. |
| `GET /api/skills/local` | List `~/.claude/skills` on the gateway's machine. |
| `POST /api/skills/local` | Import folders from the gateway's disk: `{ paths, scope, entityId?, overwrite }`. |

Skill ids are `builtin:<name>`, `global:<name>`, `entity:<entity_id>:<name>`.

## Limits and safety

- `SKILL.md` ≤ 256 KB; ≤ 64 supporting files, 2 MB each, 8 MB per skill; folder
  depth ≤ 6. `load_skill` returns at most 40k characters of instructions.
- **Ownership:** an entity-scoped skill is reachable only by someone who can see
  that entity (`is_entity_visible`); every skill lives under the caller's
  workspace directory, so one tenant never lists or loads another's.
- **Disk import is local-only:** `/api/skills/local` works only when the gateway
  is the user's own machine (the same `local_files` rule as the chat file tools).
  On a hosted gateway, upload the `SKILL.md` instead.
- **Import never follows symlinks** inside a skill, and skips hidden files,
  `node_modules`, `__pycache__` and `target`. Uploaded file paths must be plain
  relative paths (no `..`, no absolute paths, no hidden segments).
- **Ids are validated** before they touch the filesystem: names are
  `[a-z0-9-]`, entity ids `[A-Za-z0-9_-]`, so no id can address a path outside
  the library.
- **Agent-saved skills are user-approved:** `save_skill` is told to run only
  after the user saw the draft, and an existing skill is never overwritten
  without `replace: true`, which the model is told to use only after the user
  confirms. Every save shows as a "Skill saved" action chip in chat.
- The dashboard renders skill Markdown with an escape-first renderer — no HTML
  from a skill ever reaches the page.
