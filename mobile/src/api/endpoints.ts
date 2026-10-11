import { Platform } from 'react-native';

import { getInstallId } from '@/lib/companion';
import { log } from '@/lib/logger';

import {
  APIError,
  DEFAULT_GATEWAY_URL,
  gatewayState,
  getAuthToken,
  getGatewayUrl,
  http,
  isDemoGateway,
  recordGatewayFailure,
  streamNdjson,
  timed,
  tryGateway,
} from './client';
import * as fx from './mock/fixtures';
import type {
  Approval,
  CompanionFeed,
  DailyReview,
  Entity,
  EntityDetail,
  EntityFile,
  EntityMemory,
  EntityNote,
  EntityType,
  FeedMessage,
  FeedVariant,
  GatewayActivityItem,
  GatewayApproval,
  GatewayCapture,
  GatewayDailyReview,
  GatewayEntityFile,
  GatewayFeedEvent,
  GatewayMemoryPreview,
  GatewayMetrics,
  GatewayPendingApproval,
  GatewayRuntimeStatus,
  GatewayScheduleTask,
  GatewayTaskLite,
  Mission,
  MissionStatus,
  Paginated,
  QuickAction,
  ReviewSection,
  RunArtifact,
  ScheduleItem,
  Session,
  SystemHealth,
  TaskDetail,
  TaskEvent,
  TaskEvidence,
  TrustedDevice,
  Worker,
} from './types';

/**
 * Public API layer — real gateway first, mock fallback.
 *
 * Each method calls the live Rust gateway (routes verified against
 * `crates/gateway/src/server.rs` / `state.rs`) and maps the real response
 * shapes. Fixtures answer only in demo mode (the built-in default URL, nobody
 * paired yet) — see `tryGateway`. A live gateway that returns an empty list is
 * a real answer and is shown as empty, never padded with sample data.
 *
 * Wire-format reminder: most gateway structs are camelCase, but `Task`
 * (`state.rs`) and `RawCapture` have no `rename_all` and arrive snake_case.
 */

const delay = (ms: number) => new Promise((r) => setTimeout(r, ms));
// A touch of jitter so mock responses feel like a real network.
const mockLatency = () => delay(180 + Math.round(Math.random() * 220));

let uid = 0;
const nextId = (p: string) => `${p}-${Date.now()}-${uid++}`;

// ── Mappers (gateway → domain) ─────────────────────────────────────────

function guessEntityType(raw: Record<string, unknown>): EntityType {
  const t = String(raw.type ?? raw.kind ?? '').toLowerCase();
  if (t.includes('person') || t.includes('people')) return 'person';
  if (t.includes('company') || t.includes('org')) return 'company';
  if (t.includes('agent')) return 'agent';
  if (t.includes('mission') || t.includes('task')) return 'mission';
  if (t.includes('file') || t.includes('doc')) return 'file';
  if (t.includes('memory')) return 'memory';
  return 'project';
}

/** A trimmed, non-empty string — or undefined. Never invents a value. */
function str(v: unknown): string | undefined {
  const s = typeof v === 'string' ? v.trim() : '';
  return s.length > 0 ? s : undefined;
}

/** `#rgb` / `#rrggbb` only — anything else falls back to the type accent. */
function hexColour(v: unknown): string | undefined {
  const s = str(v);
  return s && /^#([0-9a-f]{3}|[0-9a-f]{6})$/i.test(s) ? s : undefined;
}

/**
 * Gateway `Entity` (`state.rs`, camelCase) → domain `Entity`.
 *
 * `Entity` carries no timestamps, so `lastActiveAt` stays absent rather than
 * being stamped "now" — a recency sort on invented times is a lie about which
 * venture moved last. `fileCount` and the operator-picked `colour` are real
 * fields and are carried through.
 */
function mapGatewayEntity(raw: Record<string, unknown>, i: number): Entity {
  const fileCount = Number(raw.fileCount ?? raw.file_count ?? 0);
  return {
    id: String(raw.id ?? nextId('ent')),
    name: String(raw.name ?? 'Untitled'),
    type: guessEntityType(raw),
    summary: String(raw.description ?? raw.summary ?? ''),
    stage: str(raw.stage),
    // Prominence on the map follows the gateway's own ordering; it is a layout
    // weight, not a claim about the entity.
    weight: Math.max(0.3, 1 - i * 0.07),
    pinned: false,
    colour: hexColour(raw.colour),
    fileCount: Number.isFinite(fileCount) && fileCount > 0 ? fileCount : 0,
    personal: raw.isPersonal === true,
    relatedIds: [],
    stats: { files: Number.isFinite(fileCount) ? fileCount : 0, notes: 0, missions: 0, memories: 0 },
  };
}

/**
 * Gateway `TaskStatus` (kebab-case: `backlog | queued | in-progress | failed |
 * completed | cancelled`) → the app's mission status. Cancelled keeps its own
 * state: folding it into `failed` made a stopped task look broken, and the
 * Today tab offered to "complete" it.
 */
function mapGatewayStatus(s: unknown): MissionStatus {
  const t = String(s ?? '').toLowerCase();
  if (t.includes('cancel')) return 'cancelled';
  if (t.includes('run') || t.includes('progress') || t.includes('active')) return 'running';
  if (t.includes('block')) return 'blocked';
  if (t.includes('review')) return 'review';
  if (t.includes('done') || t.includes('complete') || t.includes('success')) return 'done';
  if (t.includes('fail')) return 'failed';
  return 'queued';
}

/**
 * Gateway `Task` → `Mission`.
 *
 * `Task` has no `rename_all`, so its fields are snake_case (`entity_id`,
 * `assigned_agent_id`, `created_at`, `status_changed_at`). Reading camelCase
 * only meant every live row lost its entity and read "updated just now".
 * Both spellings are accepted; nothing is defaulted to the current time.
 */
function mapGatewayTask(raw: Record<string, unknown>): Mission {
  const status = mapGatewayStatus(raw.status);
  // `progress` is 0..100 on the wire. Only trust it when it says something;
  // otherwise fall back to a coarse position for the bar.
  const reported = Number(raw.progress);
  const progress =
    status === 'done'
      ? 1
      : Number.isFinite(reported) && reported > 0
        ? Math.min(1, reported / 100)
        : status === 'running'
          ? 0.5
          : status === 'review'
            ? 0.85
            : 0.05;
  const createdAt = str(raw.created_at) ?? str(raw.createdAt);
  return {
    id: String(raw.id ?? nextId('task')),
    title: String(raw.title ?? raw.taskDescription ?? 'Task'),
    status,
    entityId: str(raw.entity_id) ?? str(raw.entityId),
    entityName: str(raw.entityName),
    agentId: str(raw.assigned_agent_id) ?? str(raw.agentId),
    agentName: str(raw.agentName),
    progress,
    dept: str(raw.dept),
    dueAt: str(raw.due_at) ?? str(raw.dueAt),
    createdAt,
    updatedAt:
      str(raw.status_changed_at) ??
      str(raw.statusChangedAt) ??
      str(raw.updatedAt) ??
      createdAt ??
      '',
  };
}

/**
 * `ApprovalRequest` carries no risk rating, so live approvals get none. The
 * old mapper called anything with a `connectionId` "high risk" — a label the
 * gateway never assigned, on the one screen where trust matters most.
 */
function mapGatewayApproval(a: GatewayApproval): Approval {
  return {
    id: a.id,
    title: a.action,
    detail: a.detail ?? '',
    requestedBy: a.createdBy ?? 'System',
    createdAt: a.createdAt,
    status: a.status === 'expired' ? 'rejected' : a.status,
    taskId: a.taskId,
  };
}

/** Resolve `entityName` on missions from a loaded entity list. */
function withEntityNames(missions: Mission[], entities: Record<string, unknown>[]): Mission[] {
  if (entities.length === 0) return missions;
  const names = new Map<string, string>();
  for (const e of entities) {
    if (e.id != null && e.name != null) names.set(String(e.id), String(e.name));
  }
  return missions.map((m) =>
    m.entityName || !m.entityId ? m : { ...m, entityName: names.get(m.entityId) },
  );
}

// ── Companion ──────────────────────────────────────────────────────────

/**
 * Chips built from what this workspace actually contains.
 *
 * The fixture set advertised seven actions unconditionally — including
 * "Authenticate session" when no device had asked to sign in, and an "Ask about
 * projects" chip the screen expanded into a hardcoded project name. A chip that
 * leads nowhere is a small lie, so each one here has to earn its place:
 * `approve` needs something pending, `auth` needs a real sign-in request, `ask`
 * needs an entity to ask about (and carries its real name in `payload`).
 */
function liveQuickActions(
  entities: Entity[],
  pendingApprovals: number,
  pendingSignIns: number,
): QuickAction[] {
  const actions: QuickAction[] = [];
  const subject = entities.find((e) => e.type === 'project' || e.type === 'company') ?? entities[0];
  if (subject) {
    actions.push({
      id: 'qa-ask',
      label: `Ask about ${subject.name}`,
      icon: '◆',
      intent: 'ask',
      payload: subject.name,
    });
  }
  actions.push({ id: 'qa-capture', label: 'Capture a thought', icon: '✎', intent: 'capture' });
  if (pendingApprovals > 0) {
    actions.push({
      id: 'qa-approve',
      label: pendingApprovals === 1 ? '1 approval' : `${pendingApprovals} approvals`,
      icon: '✓',
      intent: 'approve',
    });
  }
  actions.push({ id: 'qa-mission', label: 'Start a mission', icon: '➤', intent: 'mission' });
  actions.push({ id: 'qa-summary', label: 'Summarize today', icon: '✦', intent: 'summary' });
  if (entities.length > 0) {
    actions.push({ id: 'qa-entity', label: 'Open an entity', icon: '⬡', intent: 'entity' });
  }
  if (pendingSignIns > 0) {
    actions.push({
      id: 'qa-auth',
      label: pendingSignIns === 1 ? 'Approve a sign-in' : `${pendingSignIns} sign-in requests`,
      icon: '⚿',
      intent: 'auth',
    });
  }
  return actions;
}

/**
 * Presentation for each operator-feed kind (gateway `OperatorFeedKind`,
 * `state.rs`, snake_case on the wire). Labels are the desktop's own
 * (`sveltfront/src/lib/feedKinds.ts`) so one event reads the same on both
 * surfaces; the variant decides how loud the row is here — anything waiting on
 * a person stands out, bookkeeping stays quiet.
 */
const FEED_KIND_PRESENTATION: Record<string, { variant: FeedVariant; label: string }> = {
  task_created: { variant: 'status', label: 'Task created' },
  task_completed: { variant: 'success', label: 'Task completed' },
  task_blocked: { variant: 'failure', label: 'Task blocked' },
  task_retry_scheduled: { variant: 'status', label: 'Retry scheduled' },
  task_stuck: { variant: 'failure', label: 'Task stuck' },
  task_cancelled: { variant: 'status', label: 'Task cancelled' },
  mission_cancelled: { variant: 'status', label: 'Mission cancelled' },
  mission_report_ready: { variant: 'review', label: 'Mission report ready' },
  verification_needs_review: { variant: 'review', label: 'Needs review' },
  approval_needed: { variant: 'approval', label: 'Approval needed' },
  mission_created: { variant: 'status', label: 'Mission created' },
  mission_completed: { variant: 'success', label: 'Mission completed' },
  mission_first_completed: { variant: 'success', label: 'First mission completed' },
  goal_loop_decision: { variant: 'status', label: 'Goal loop decision' },
  mission_iteration_advanced: { variant: 'status', label: 'Mission iteration' },
  mission_stalled: { variant: 'failure', label: 'Mission stalled' },
  mission_missing_next_task: { variant: 'input', label: 'Missing next task' },
  mission_review_due: { variant: 'review', label: 'Review due' },
  mission_followup_suppressed: { variant: 'status', label: 'Follow-up suppressed' },
  mission_recommendation_accepted: { variant: 'success', label: 'Recommendation accepted' },
  mission_recommendation_dismissed: { variant: 'status', label: 'Recommendation dismissed' },
  mission_snoozed: { variant: 'status', label: 'Review snoozed' },
  mission_user_input_needed: { variant: 'input', label: 'Input needed' },
  entity_source_imported: { variant: 'success', label: 'Source imported' },
  approval_request_created: { variant: 'approval', label: 'Approval requested' },
  approval_request_resolved: { variant: 'success', label: 'Approval resolved' },
  connection_verified: { variant: 'success', label: 'Connection verified' },
  connection_check_failed: { variant: 'failure', label: 'Connection check failed' },
  scheduled_task_fired: { variant: 'status', label: 'Scheduled task ran' },
  heartbeat_result: { variant: 'heartbeat', label: 'Heartbeat' },
  agent_run_completed: { variant: 'success', label: 'Agent run finished' },
  agent_run_failed: { variant: 'failure', label: 'Agent run failed' },
  task_overdue: { variant: 'overdue', label: 'Task overdue' },
  daily_digest: { variant: 'digest', label: 'Daily brief' },
  entity_workspace_needed: { variant: 'input', label: 'Workspace needed' },
};

/** `task_overdue` → "Task overdue" for a kind newer than this app knows. */
function sentenceCase(kind: string): string | undefined {
  const words = kind.replace(/_/g, ' ').trim();
  return words ? words[0].toUpperCase() + words.slice(1) : undefined;
}

/** One operator-feed event → one system row in the conversation. */
function feedEventMessage(e: GatewayFeedEvent): FeedMessage {
  const kind = String(e.kind ?? '').toLowerCase();
  const p = FEED_KIND_PRESENTATION[kind];
  const link =
    e.approvalId || e.taskId || e.entityId
      ? { approvalId: e.approvalId, taskId: e.taskId, entityId: e.entityId }
      : undefined;
  return {
    id: e.id,
    role: 'system',
    variant: p?.variant ?? (kind.includes('heartbeat') ? 'heartbeat' : 'status'),
    label: p?.label ?? sentenceCase(kind),
    link,
    text: e.detail ? `${e.title} — ${e.detail}` : e.title,
    at: e.timestamp,
  };
}

/** How many operator-feed rows the conversation interleaves. */
const FEED_WINDOW = 30;

export async function getCompanionFeed(): Promise<CompanionFeed> {
  const real = await tryGateway('operator-feed', async () => {
    const events = await http.list<GatewayFeedEvent>(`/api/operator-feed?limit=${FEED_WINDOW}`);
    const approvals = (await http.list<GatewayApproval>('/api/approvals').catch(() => []))
      .filter((a) => a.status === 'pending')
      .map(mapGatewayApproval);
    // Both are best-effort: the chips they feed simply don't appear when the
    // call fails, which is the honest outcome.
    const entities = (
      await http.list<Record<string, unknown>>('/api/entities?limit=200').catch(() => [])
    ).map(mapGatewayEntity);
    const signIns = await http
      .get<GatewayPendingApproval[]>('/api/auth/approvals')
      .catch(() => [] as GatewayPendingApproval[]);
    // The gateway answers newest-first (`list_operator_feed` reverses its
    // ring). `slice(-30)` kept the 30 *oldest* and dropped the newest; take the
    // head, then flip it so the conversation reads oldest → newest.
    const messages: FeedMessage[] = events.slice(0, FEED_WINDOW).reverse().map(feedEventMessage);
    return {
      messages,
      approvals,
      quickActions: liveQuickActions(entities, approvals.length, signIns.length),
    };
  });

  // Same rule as the daily review: a live gateway with a quiet feed is a real
  // answer. Only an unreachable one falls back to the sample conversation.
  if (real) {
    return {
      messages: real.messages,
      needsAttention: real.approvals,
      quickActions: real.quickActions,
      live: true,
    };
  }

  await mockLatency();
  return {
    messages: [...fx.feedMessages],
    needsAttention: fx.approvals.filter((a) => a.status === 'pending'),
    quickActions: fx.quickActions,
    live: false,
  };
}

/** `suggestions` off a `CaptureChatResponse`, if it carried any. */
function suggestionsFrom(payload: Record<string, unknown> | undefined): string[] | undefined {
  const raw = payload?.suggestions;
  if (!Array.isArray(raw)) return undefined;
  const list = raw.map(String).filter((s) => s.trim().length > 0).slice(0, 3);
  return list.length > 0 ? list : undefined;
}

/**
 * Approval-gated actions off a `CaptureChatResponse` — entries shaped
 * `{ actionType: "action_request", resultId: <approvalId>, description }`.
 * Each becomes an inline card in the conversation that fetches the approval
 * and, once decided, follows the linked task.
 *
 * `ChatAction` is `rename_all = "camelCase"` (`capture.rs`). This used to read
 * `action_type` / `result_id` only, so against a live gateway no card ever
 * rendered — the approval sat in the queue with nothing in the chat pointing
 * at it. The snake_case spelling is still accepted for older gateways.
 */
function actionRequestsFrom(
  payload: Record<string, unknown> | undefined,
): FeedMessage['actionRequests'] {
  const raw = payload?.actions;
  if (!Array.isArray(raw)) return undefined;
  const requests: { approvalId: string; description: string }[] = [];
  for (const item of raw) {
    const rec = item as Record<string, unknown> | null;
    if (!rec) continue;
    const actionType = rec.actionType ?? rec.action_type;
    const resultId = rec.resultId ?? rec.result_id;
    if (actionType !== 'action_request' || resultId == null) continue;
    requests.push({
      approvalId: String(resultId),
      description: String(rec.description ?? 'Waiting for your approval'),
    });
  }
  return requests.length > 0 ? requests : undefined;
}

/** One prior turn sent back as context — the gateway's `CaptureChatMessage`. */
export interface ChatHistoryTurn {
  role: 'user' | 'assistant';
  content: string;
}

/** Scope and continuity for a chat turn: the same fields the desktop sends. */
export interface ChatTurnOptions {
  /** The entity this turn is about. Absent means the whole workspace. */
  entityId?: string;
  /** Gateway session id from an earlier turn's `meta` line, reused verbatim. */
  sessionId?: string;
  /** Earlier turns, oldest first. Trimmed to the last `CHAT_HISTORY_TURNS`. */
  history?: ChatHistoryTurn[];
}

export interface ChatChunk {
  delta: string;
  done: boolean;
  message?: FeedMessage;
  /** The gateway's session id once the stream names it — reuse it next turn. */
  sessionId?: string;
}

/** Matches the gateway's own `HISTORY_TURNS` trim in `chat_agent.rs`. */
export const CHAT_HISTORY_TURNS = 20;

/**
 * `CaptureChatRequest` (camelCase), mirroring desktop's `sendChatTurn`.
 *
 * Without `entityId` + `scope` the gateway resolved every phone turn to the
 * Personal Space, whatever entity the picker showed — the picker only ever
 * prefixed the text. Without `timezoneOffsetMinutes` a "remind me at 9am"
 * fired at 09:00 UTC. Without `sessionId` + `history` every message started a
 * brand-new conversation.
 */
function chatRequestBody(text: string, opts: ChatTurnOptions): Record<string, unknown> {
  return {
    message: text,
    entityId: opts.entityId,
    scope: opts.entityId ? 'entity' : 'workspace',
    // Minutes EAST of UTC; `getTimezoneOffset()` is west-positive.
    timezoneOffsetMinutes: -new Date().getTimezoneOffset(),
    sessionId: opts.sessionId,
    history: (opts.history ?? [])
      .filter((t) => t.content.trim().length > 0)
      .slice(-CHAT_HISTORY_TURNS),
  };
}

/**
 * Why a chat turn failed, in words the operator can act on. Every APIError
 * used to read "I can't reach your Mac", including a signed-out phone (401),
 * an exhausted credit balance (402) and a rejected request (400) — three
 * problems the gateway had already diagnosed.
 */
export function chatFailureText(e: unknown): string {
  if (e instanceof APIError) {
    switch (e.status) {
      case 401:
        return 'You’re signed out — sign in again to keep chatting.';
      case 402:
        // The gateway's usage limit. Nothing here may read as an offer to buy
        // anything (App Review 3.1.1) — the fix lives on the computer.
        return 'Your computer has paused AI replies for now. Check Consistent CEO on your computer to keep chatting.';
      case 403:
        return 'This account can’t chat in this workspace. Sign in with one that has access.';
      case 400:
        return e.detail
          ? `That message wasn’t accepted: ${e.detail}`
          : 'That message wasn’t accepted — the entity it was about may no longer exist. Try again from the whole workspace.';
      case 503:
        return 'Consistent CEO on your computer isn’t ready — make sure it’s open and awake, then try again.';
      default:
        return `Your computer hit an error answering that (error ${e.status}). Try again in a moment.`;
    }
  }
  if (e instanceof Error && e.name === 'AbortError') {
    return 'That took too long, so I stopped waiting. Try again in a moment.';
  }
  return 'I can’t reach your computer right now — the message wasn’t delivered. Check the connection in Settings and try again.';
}

/**
 * Streaming assistant reply, token by token, from the gateway's real NDJSON
 * endpoint (`POST /api/capture/chat/stream` → `capture_chat_stream_handler`).
 *
 * The wire protocol is one JSON object per line:
 *   `{"type":"meta","sessionId":…}` · `{"type":"token","text":…}` ·
 *   `{"type":"done","payload":CaptureChatResponse}`
 *
 * Tokens are emitted as the model produces them — nothing here paces or
 * re-types text that already arrived. Failures never throw: they finish the
 * stream with an `error` message that says what went wrong. Only in demo mode
 * (nothing paired) does an unreachable gateway fall back to a typed-out
 * fixture reply, because there is no real gateway to be honest about.
 */
export async function* sendMessageStream(
  text: string,
  opts: ChatTurnOptions = {},
): AsyncGenerator<ChatChunk> {
  const at = new Date().toISOString();
  let acc = '';
  let sessionId = opts.sessionId;
  try {
    let payload: Record<string, unknown> | undefined;
    for await (const event of streamNdjson('/api/capture/chat/stream', chatRequestBody(text, opts))) {
      if (event.type === 'meta') {
        sessionId = str(event.sessionId) ?? sessionId;
      } else if (event.type === 'token') {
        acc += String(event.text ?? '');
        yield { delta: acc, done: false, sessionId };
      } else if (event.type === 'done') {
        payload = event.payload as Record<string, unknown> | undefined;
      }
    }
    sessionId = str(payload?.sessionId) ?? sessionId;
    const finalText = String(payload?.message ?? acc).trim();
    if (finalText.length > 0) {
      log('info', 'api', 'computer ✓ capture/chat/stream');
      yield {
        delta: finalText,
        done: true,
        sessionId,
        message: {
          id: nextId('fm'),
          role: 'assistant',
          text: finalText,
          at,
          actionRequests: actionRequestsFrom(payload),
          // Only the gateway's own suggestions. The canned fixture chips
          // ("Open Fieldnote") have no business under a real answer.
          followUps: suggestionsFrom(payload),
        },
      };
      return;
    }
    // A live stream that produced nothing is a gateway problem, not an offline
    // one. Say so — a fixture reply here would read as an answer the model gave.
    log('warn', 'api', 'computer ✕ capture/chat/stream — empty stream');
    const empty =
      'Your computer answered but the model produced no text. Check the AI settings in Consistent CEO on your computer.';
    yield {
      delta: empty,
      done: true,
      sessionId,
      message: { id: nextId('fm'), role: 'assistant', text: empty, at, error: true },
    };
    return;
  } catch (e) {
    if (!(e instanceof APIError)) recordGatewayFailure();
    log(
      'warn',
      'api',
      `computer ✕ capture/chat/stream — ${e instanceof APIError ? `HTTP ${e.status}` : e instanceof Error ? e.message : 'error'}`,
    );
    // An HTTP status means a gateway answered and diagnosed the problem; a
    // paired gateway that didn't answer must say so. Either way a canned
    // fixture reply would read as an answer the operator's computer gave.
    if (e instanceof APIError || !(await isDemoGateway())) {
      const reason = chatFailureText(e);
      const partial = acc.trim();
      const shown = partial ? `${partial}\n\n— The reply was cut off. ${reason}` : reason;
      yield {
        delta: shown,
        done: true,
        sessionId,
        message: { id: nextId('fm'), role: 'assistant', text: shown, at, error: true },
      };
      return;
    }
  }

  // Demo path (nothing paired): a fixture reply, typed out so the composer
  // still behaves. Fixture follow-ups are fine here — it is all sample data.
  const message: FeedMessage = {
    id: nextId('fm'),
    role: 'assistant',
    text: mockReply(text),
    at,
    followUps: followUpsFor(text),
  };
  const words = message.text.split(' ');
  let typed = '';
  for (let i = 0; i < words.length; i++) {
    await delay(24 + Math.random() * 38);
    typed += (i === 0 ? '' : ' ') + words[i];
    yield { delta: typed, done: false };
  }
  yield { delta: typed, done: true, message };
}

/**
 * One whole reply, for callers that don't render tokens. Drains
 * `sendMessageStream`, so it shares its 90s budget and its honest failures —
 * and never rejects. (It used to be a plain POST on the 2.5s request timeout:
 * any real model turn timed out, the promise rejected unhandled, and the
 * gateway was marked unreachable.)
 */
export async function sendMessage(text: string, opts: ChatTurnOptions = {}): Promise<FeedMessage> {
  let last: FeedMessage | undefined;
  for await (const chunk of sendMessageStream(text, opts)) {
    if (chunk.message) last = chunk.message;
  }
  return (
    last ?? {
      id: nextId('fm'),
      role: 'assistant',
      text: chatFailureText(undefined),
      at: new Date().toISOString(),
      error: true,
    }
  );
}

// ── Entities ───────────────────────────────────────────────────────────
/**
 * Every entity in the workspace. `limit=200` is the gateway's page ceiling —
 * the default page of 50 silently dropped the rest.
 *
 * A live empty list is returned as empty. The old `real.length > 0` check
 * answered a brand-new workspace with the sample-world fixtures, i.e.
 * someone else's companies presented as yours.
 */
export async function listEntities(): Promise<Entity[]> {
  const real = await tryGateway('entities', () =>
    http.list<Record<string, unknown>>('/api/entities?limit=200'),
  );
  if (real) return real.map(mapGatewayEntity);
  await mockLatency();
  return [...fx.entities];
}

// ── Entity detail ──────────────────────────────────────────────────────
//
// There is no single "entity workspace" endpoint, but every tab on the detail
// screen has a real source, so the detail is assembled from six of them:
//
//   overview  GET /api/entities/{id}                  goals, operating rules
//   files     GET /api/entities/{id}/files            uploaded files
//   notes     GET /api/captures?entity_id=…           captures filed here
//   missions  GET /api/tasks?entity_id=…              this entity's work
//   memory    GET /api/entities/{id}/memory-preview   short + long term recall
//   activity  GET /api/activity?entity_id=…           unified timeline
//
// Related entities come from the entity's own `relationships`, resolved against
// `/api/entities`. Everything a tab shows is now something the gateway said.

const FILE_KINDS: { match: RegExp; kind: EntityFile['kind'] }[] = [
  { match: /^image\//, kind: 'image' },
  { match: /^audio\//, kind: 'audio' },
  { match: /\.(ts|tsx|js|jsx|rs|py|go|swift|kt|java|rb|sh|json|toml|ya?ml)$/i, kind: 'code' },
  { match: /^text\/uri-list$|^https?:/i, kind: 'link' },
];

function fileKind(filename: string, contentType: string): EntityFile['kind'] {
  for (const { match, kind } of FILE_KINDS) {
    if (match.test(contentType) || match.test(filename)) return kind;
  }
  return 'doc';
}

function humanBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '—';
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/** A capture's first line makes a serviceable title; the rest is the body. */
function captureNote(c: GatewayCapture, entityId: string): EntityNote {
  const body = (c.raw_text ?? '').trim();
  const firstLine = body.split('\n', 1)[0] ?? '';
  const title =
    (c.summary ?? '').trim() ||
    (firstLine.length > 72 ? `${firstLine.slice(0, 69)}…` : firstLine) ||
    'Capture';
  return {
    id: c.id,
    entityId,
    title,
    body,
    createdAt: c.created_at,
    backlinks: [],
  };
}

/** `feed` / `capture` / `agent_run` → who the timeline says did the thing. */
function activityActor(item: GatewayActivityItem): string {
  if (item.source === 'capture') return 'Capture';
  if (item.source === 'agent_run') return 'Agent';
  return 'System';
}

export async function getEntity(id: string): Promise<EntityDetail | null> {
  const path = encodeURIComponent(id);
  const real = await tryGateway(`entity ${id}`, async () => {
    const raw = await http
      .get<Record<string, unknown>>(`/api/entities/${path}`)
      .catch((e: unknown) => {
        // The gateway answered "no such entity" — a real answer, and not a
        // reason to fall back to a fixture with the same id.
        if (e instanceof APIError && e.status === 404) return null;
        throw e;
      });
    if (!raw) return { missing: true as const };

    // Each side call degrades to empty on its own: one 500 shouldn't blank the
    // whole screen, and an empty tab is honest about what came back.
    const [files, captures, tasks, memory, activity, allEntities] = await Promise.all([
      http.list<GatewayEntityFile>(`/api/entities/${path}/files`).catch(() => []),
      http.list<GatewayCapture>(`/api/captures?entity_id=${path}&limit=40`).catch(() => []),
      http.list<Record<string, unknown>>(`/api/tasks?entity_id=${path}&limit=50`).catch(() => []),
      http
        .get<GatewayMemoryPreview>(`/api/entities/${path}/memory-preview`)
        .catch(() => ({ shortTerm: [], longTerm: [] }) as GatewayMemoryPreview),
      http.list<GatewayActivityItem>(`/api/activity?entity_id=${path}&limit=30`).catch(() => []),
      http.list<Record<string, unknown>>('/api/entities?limit=200').catch(() => []),
    ]);

    const index = Math.max(0, allEntities.findIndex((e) => String(e.id) === id));
    const base = mapGatewayEntity(raw, index);

    // The entity's own freeform `notes` field is a note in its own right, and
    // the only one that exists before anything has been captured.
    const notes: EntityNote[] = [];
    const ownNotes = typeof raw.notes === 'string' ? raw.notes.trim() : '';
    if (ownNotes.length > 0) {
      notes.push({
        id: `${id}-notes`,
        entityId: id,
        title: 'Entity notes',
        body: ownNotes,
        // No timestamp: `Entity.notes` is a plain field with no history.
        backlinks: [],
      });
    }
    notes.push(...captures.map((c) => captureNote(c, id)));

    const memories: EntityMemory[] = [
      ...(memory.shortTerm ?? []).map((text, i) => ({
        id: `${id}-st-${i}`,
        text,
        scope: 'short' as const,
      })),
      ...(memory.longTerm ?? []).map((text, i) => ({
        id: `${id}-lt-${i}`,
        text,
        scope: 'long' as const,
      })),
    ];

    // `relationships[].target` is whatever the operator (or importer) wrote —
    // sometimes an id, sometimes a name. Match on both, drop what resolves to
    // nothing rather than inventing a node.
    const byKey = new Map<string, Entity>();
    allEntities.forEach((e, i) => {
      const mapped = mapGatewayEntity(e, i);
      byKey.set(mapped.id.toLowerCase(), mapped);
      byKey.set(mapped.name.toLowerCase(), mapped);
    });
    const relationships = Array.isArray(raw.relationships) ? raw.relationships : [];
    const related: Entity[] = [];
    for (const rel of relationships) {
      const target = String((rel as Record<string, unknown>)?.target ?? '').toLowerCase();
      const hit = byKey.get(target);
      if (hit && hit.id !== id && !related.some((r) => r.id === hit.id)) related.push(hit);
    }

    const missions = tasks.map(mapGatewayTask).sort(newestFirst);
    const mappedFiles: EntityFile[] = files.map((f) => ({
      id: f.id,
      entityId: id,
      name: f.filename,
      kind: fileKind(f.filename, f.content_type ?? ''),
      size: humanBytes(Number(f.size_bytes ?? 0)),
      updatedAt: f.uploaded_at,
    }));

    // Setup / readiness / knowledge-index are all optional side-channels the
    // gateway may or may not populate — read each defensively rather than
    // assuming shape.
    const nonBlank = (v: unknown): string | undefined =>
      typeof v === 'string' && v.trim() ? v : undefined;

    const setup =
      raw.setup && typeof raw.setup === 'object' ? (raw.setup as Record<string, unknown>) : {};
    const stackParts = [setup.backendStack, setup.frontendStack, setup.databaseStack]
      .map(nonBlank)
      .filter((s): s is string => Boolean(s));
    const stack = stackParts.length > 0 ? stackParts.join(' · ') : undefined;

    const rawReadiness =
      raw.readiness && typeof raw.readiness === 'object'
        ? (raw.readiness as Record<string, unknown>)
        : undefined;
    const readiness =
      rawReadiness && typeof rawReadiness.label === 'string' && rawReadiness.label.trim() && rawReadiness.percent != null
        ? {
            percent: Number(rawReadiness.percent),
            label: rawReadiness.label,
            nextQuestion: nonBlank(rawReadiness.nextQuestion),
          }
        : undefined;

    const rawKnowledgeIndex =
      raw.knowledgeIndex && typeof raw.knowledgeIndex === 'object'
        ? (raw.knowledgeIndex as Record<string, unknown>)
        : undefined;
    const knownStatuses = ['indexing', 'ready', 'failed', 'skipped'];
    const knowledgeIndexStatus =
      rawKnowledgeIndex &&
      typeof rawKnowledgeIndex.status === 'string' &&
      knownStatuses.includes(rawKnowledgeIndex.status)
        ? (rawKnowledgeIndex.status as 'indexing' | 'ready' | 'failed' | 'skipped')
        : undefined;
    const knowledgeIndex =
      rawKnowledgeIndex && knowledgeIndexStatus
        ? {
            status: knowledgeIndexStatus,
            docCount: Number(rawKnowledgeIndex.docCount ?? 0),
            embeddedCount: Number(rawKnowledgeIndex.embeddedCount ?? 0),
            indexedAt: nonBlank(rawKnowledgeIndex.indexedAt),
            error: nonBlank(rawKnowledgeIndex.error),
            source: nonBlank(rawKnowledgeIndex.source),
          }
        : undefined;

    const detail: EntityDetail = {
      ...base,
      relatedIds: related.map((r) => r.id),
      goals: typeof raw.goals === 'string' && raw.goals.trim() ? raw.goals : undefined,
      operatingRules:
        typeof raw.operatingRules === 'string' && raw.operatingRules.trim()
          ? raw.operatingRules
          : undefined,
      profile: nonBlank(raw.profile),
      activeContext: nonBlank(raw.activeContext),
      whatItIs: nonBlank(setup.whatItIs),
      primaryGoal: nonBlank(setup.primaryGoal),
      successCriteria: nonBlank(setup.successCriteria),
      stack,
      repoPath: nonBlank(setup.repoPath),
      readiness,
      knowledgeIndex,
      notes,
      files: mappedFiles,
      missions,
      memories,
      activity: activity.map((a) => ({
        id: a.id,
        entityId: a.entityId ?? id,
        actor: activityActor(a),
        action: a.detail ? `${a.title} — ${a.detail}` : a.title,
        at: a.timestamp,
      })),
      related,
      // Counts are what we just counted, not a stored guess.
      stats: {
        files: mappedFiles.length,
        notes: notes.length,
        missions: missions.length,
        memories: memories.length,
      },
      live: true,
    };
    return { missing: false as const, detail };
  });

  if (real) return real.missing ? null : real.detail;
  await mockLatency();
  return fx.buildEntityDetail(id);
}

/**
 * Write a note against an entity — `POST /api/captures`, the same inbox the
 * desktop and CLI file into. Snake_case body: `CreateCaptureRequest` has no
 * camelCase rename.
 *
 * The note sheet used to append to React state and nothing else, so a "saved"
 * note disappeared on the next refresh. This returns `false` when the write
 * didn't land so the caller can say so.
 */
export async function saveEntityNote(
  entityId: string,
  title: string,
  body: string,
): Promise<boolean> {
  const raw = [title, body].filter((s) => s.trim().length > 0).join('\n\n');
  const result = await taskAction(`note ${entityId}`, () =>
    http.post<Record<string, unknown>>('/api/captures', {
      raw_text: raw,
      entity_id: entityId,
      source: 'chat',
      summary: title.trim() || undefined,
    }),
  );
  return result.ok;
}

// ── Missions / workers ─────────────────────────────────────────────────

/** Newest first by creation (falling back to last change). Undated rows sort last. */
function newestFirst(a: Mission, b: Mission): number {
  const t = (m: Mission) => {
    const n = Date.parse(m.createdAt ?? m.updatedAt ?? '');
    return Number.isNaN(n) ? 0 : n;
  };
  return t(b) - t(a);
}

/** Task statuses that are still open work — what "Active missions" means. */
const OPEN_TASK_STATUSES = ['in-progress', 'queued', 'backlog'] as const;

/**
 * Open work across the workspace, newest first.
 *
 * `GET /api/tasks` with no filter returns the *oldest* 50 rows in every state
 * — cancelled and completed included — which is how a task cancelled months
 * ago sat under "Active missions" with a Done swipe that would complete it.
 * The gateway filters on one `status` at a time, so the open states are
 * fetched separately at the page ceiling and merged.
 */
export async function listMissions(): Promise<Mission[]> {
  const real = await tryGateway('tasks', async () => {
    const [pages, entities] = await Promise.all([
      Promise.all(
        OPEN_TASK_STATUSES.map((status) =>
          http.list<Record<string, unknown>>(`/api/tasks?status=${status}&limit=200`),
        ),
      ),
      http.list<Record<string, unknown>>('/api/entities?limit=200').catch(() => []),
    ]);
    const seen = new Set<string>();
    const missions: Mission[] = [];
    for (const raw of pages.flat()) {
      const m = mapGatewayTask(raw);
      if (seen.has(m.id) || m.status === 'cancelled' || m.status === 'done') continue;
      seen.add(m.id);
      missions.push(m);
    }
    return withEntityNames(missions, entities).sort(newestFirst);
  });
  if (real) return real;
  await mockLatency();
  return [...fx.missions];
}

/**
 * The agent roster. `/api/agents` says who exists, not what they are doing,
 * so live workers carry no status — the old mapper labelled every one "Idle".
 */
export async function listWorkers(): Promise<Worker[]> {
  const real = await tryGateway('agents', () =>
    http.list<Record<string, unknown>>('/api/agents'),
  );
  if (real) {
    return real.map((raw) => ({
      id: String(raw.id ?? nextId('agent')),
      name: String(raw.name ?? 'Agent'),
      role: String(raw.role ?? raw.mission ?? ''),
      groupId: String(raw.groupId ?? 'general'),
      load: 0,
    }));
  }
  await mockLatency();
  return [...fx.workers];
}

// ── Approvals ──────────────────────────────────────────────────────────
export async function listApprovals(): Promise<Approval[]> {
  const real = await tryGateway('approvals', () =>
    http.list<GatewayApproval>('/api/approvals'),
  );
  if (real) return real.filter((a) => a.status === 'pending').map(mapGatewayApproval);
  await mockLatency();
  return fx.approvals.filter((a) => a.status === 'pending');
}

/**
 * One approval by id, decided or not.
 *
 * Unlike `listApprovals` this keeps non-pending rows, because an inline chat
 * card must still render an approval that was already decided from another
 * surface. It used to fetch the whole list and find — the gateway had no
 * `GET /api/approvals/{id}` — which meant a decided approval was
 * indistinguishable from a deleted one. The endpoint exists now; a 404 is a
 * real "no such approval here".
 */
export async function getApproval(id: string): Promise<Approval | null> {
  try {
    const real = await tryGateway(`approval ${id}`, () =>
      http.get<GatewayApproval>(`/api/approvals/${encodeURIComponent(id)}`),
    );
    if (real) return mapGatewayApproval(real);
  } catch (e) {
    // A 404 is an answer, not a failure: this workspace has no such approval.
    // `tryGateway` rethrows against a live gateway, so without this the caller
    // would see an exception — and falling through to the fixtures below would
    // be worse still, rendering a fabricated card for an id the server denied.
    if (e instanceof APIError && e.status === 404) return null;
    throw e;
  }
  await mockLatency();
  const mock = fx.approvals.find((a) => a.id === id);
  return mock ? { ...mock } : null;
}

/**
 * `/api/approvals/{id}` is PATCH-only on the gateway (`control_api.rs`,
 * `patch_approval`) — POSTing here was an unconditional 405, so every decision
 * made from the phone silently fell through to the mock.
 */
export async function approveAction(id: string): Promise<Approval> {
  const real = await tryGateway(`approve ${id}`, () =>
    http.patch<GatewayApproval>(`/api/approvals/${id}`, { status: 'approved' }),
  );
  if (real) return mapGatewayApproval(real);
  await mockLatency();
  return { ...findApproval(id), status: 'approved' };
}

export async function rejectAction(id: string): Promise<Approval> {
  const real = await tryGateway(`reject ${id}`, () =>
    http.patch<GatewayApproval>(`/api/approvals/${id}`, { status: 'rejected' }),
  );
  if (real) return mapGatewayApproval(real);
  await mockLatency();
  return { ...findApproval(id), status: 'rejected' };
}

// ── Review ─────────────────────────────────────────────────────────────

function shortTime(iso: string | null): string | undefined {
  if (!iso) return undefined;
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return undefined;
  return d.toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' });
}

function relativeAge(iso: string): string | undefined {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return undefined;
  const mins = Math.max(0, Math.round((Date.now() - d.getTime()) / 60_000));
  if (mins < 60) return `${mins}m ago`;
  if (mins < 60 * 24) return `${Math.round(mins / 60)}h ago`;
  return `${Math.round(mins / (60 * 24))}d ago`;
}

function lateBy(hours: number): string {
  if (hours < 1) return 'just missed';
  if (hours < 24) return `${hours}h late`;
  const days = Math.round(hours / 24);
  return `${days}d late`;
}

/** Entity name + how late / when due — the second line of a task row. */
function taskMeta(t: GatewayTaskLite): string | undefined {
  const parts: string[] = [];
  if (t.entityName) parts.push(t.entityName);
  if (t.overdueHours != null) parts.push(lateBy(t.overdueHours));
  else {
    const at = shortTime(t.dueAt);
    if (at) parts.push(`due ${at}`);
  }
  return parts.length > 0 ? parts.join(' · ') : undefined;
}

function taskItems(tasks: GatewayTaskLite[]): ReviewSection['items'] {
  return tasks.map((t) => ({
    id: t.id,
    text: t.title,
    meta: taskMeta(t),
    entityId: t.entityId ?? undefined,
  }));
}

/** One honest line about the day, built from what the gateway actually returned. */
function reviewHeadline(r: GatewayDailyReview): string {
  const waiting = r.pendingApprovals.length + r.missionsAwaitingInput.length;
  const bits: string[] = [];
  if (r.overdue.length > 0) bits.push(`${r.overdue.length} overdue`);
  if (r.dueToday.length > 0) bits.push(`${r.dueToday.length} due today`);
  if (waiting > 0) bits.push(`${waiting} waiting on you`);
  if (bits.length === 0) {
    return r.streak.completedToday > 0
      ? `Nothing overdue and nothing left due — ${r.streak.completedToday} done today.`
      : 'Nothing overdue and nothing due today. The board is clear.';
  }
  const list =
    bits.length === 1 ? bits[0] : `${bits.slice(0, -1).join(', ')} and ${bits[bits.length - 1]}`;
  return `${list[0].toUpperCase()}${list.slice(1)}.`;
}

/**
 * Gateway `DailyReviewResponse` → the screen's `DailyReview`.
 *
 * The section kinds are the ones `ReviewCard` already styles, so the payload
 * lands in the existing visual language: overdue reads as `missed` (red),
 * decisions as `questions` (amber), the focus list as `next`.
 */
function mapGatewayReview(r: GatewayDailyReview): DailyReview {
  const sections: ReviewSection[] = [];
  const push = (
    id: string,
    kind: ReviewSection['kind'],
    title: string,
    items: ReviewSection['items'],
  ) => {
    if (items.length > 0) sections.push({ id, kind, title, items });
  };

  push('rv-focus', 'next', 'Start here', taskItems(r.suggestedFocus));
  push('rv-overdue', 'missed', 'Overdue', taskItems(r.overdue));
  push('rv-due', 'updates', 'Due today', taskItems(r.dueToday));
  push(
    'rv-approvals',
    'questions',
    'Waiting on your decision',
    r.pendingApprovals.map((a) => ({
      id: a.id,
      text: a.title,
      meta: relativeAge(a.createdAt),
      entityId: a.entityId,
    })),
  );
  push(
    'rv-missions',
    'questions',
    'Missions needing input',
    r.missionsAwaitingInput.map((m) => ({ id: m.id, text: m.title, entityId: m.entityId })),
  );
  push(
    'rv-hb',
    'heartbeats',
    'Heartbeat highlights',
    r.heartbeatHighlights.map((h, i) => ({
      id: `${h.entityId ?? 'hb'}-${i}`,
      text: h.entityName ? `${h.entityName} · ${h.summary}` : h.summary,
      meta: relativeAge(h.firedAt),
      entityId: h.entityId ?? undefined,
    })),
  );

  return {
    date: r.generatedAt,
    headline: reviewHeadline(r),
    sections,
    streak: r.streak,
    live: true,
  };
}

export async function getDailyReview(): Promise<DailyReview> {
  const real = await tryGateway('daily-review', () =>
    http.get<GatewayDailyReview>('/api/daily-review'),
  );
  // A live gateway with an empty day is a real answer, not a miss — only an
  // unreachable gateway falls back to fixtures.
  if (real) return mapGatewayReview(real);
  await mockLatency();
  return fx.dailyReview;
}

// ── Task actions ───────────────────────────────────────────────────────

export interface TaskActionResult {
  ok: boolean;
  /** Set when `ok` is false — safe to show to the operator verbatim. */
  error?: string;
}

/**
 * Mutations don't get the `tryGateway` treatment: the caller has to tell a
 * refused write from an unreachable gateway so it can revert its optimistic
 * update, and an HTTP 4xx must not flip the whole app into mock mode.
 */
async function taskAction(label: string, fn: () => Promise<unknown>): Promise<TaskActionResult> {
  try {
    await fn();
    log('info', 'api', `computer ✓ ${label}`);
    return { ok: true };
  } catch (e) {
    if (e instanceof APIError) {
      // The gateway answered — it just said no. Reachability stands.
      const error =
        e.status === 404
          ? 'That task no longer exists.'
          : e.status === 409
            ? 'That task is already closed.'
            : e.status === 401
              ? 'You’re signed out — sign in again, then retry.'
              : e.status === 403
                ? "That task isn't in your workspace."
                : `Your computer refused that (error ${e.status}).`;
      log('warn', 'api', `computer ✕ ${label} — ${error}`);
      return { ok: false, error };
    }
    recordGatewayFailure();
    log('warn', 'api', `computer ✕ ${label} — unreachable`);
    return { ok: false, error: "Couldn't reach your computer — nothing was saved." };
  }
}

/**
 * One task by id — `GET /api/tasks/{id}`. Live-only, no fixture fallback:
 * this is the poll behind an inline approval card following real work, and a
 * mock task would invent progress. Null when the gateway says 404; other
 * failures propagate so the poller can simply try again next tick.
 */
export async function getTask(id: string): Promise<Mission | null> {
  const raw = await http
    .get<Record<string, unknown>>(`/api/tasks/${encodeURIComponent(id)}`)
    .catch((e: unknown) => {
      if (e instanceof APIError && e.status === 404) return null;
      throw e;
    });
  return raw ? mapGatewayTask(raw) : null;
}

/**
 * `metadata.evidence` → `TaskEvidence`, or undefined when the run predates
 * evidence collection (or the directory wasn't one the executor could read).
 * Nothing is defaulted into existence: a pack that isn't there stays absent,
 * so the screen can say "no evidence" instead of rendering a row of zeroes.
 */
function parseEvidence(raw: unknown): TaskEvidence | undefined {
  if (raw == null || typeof raw !== 'object') return undefined;
  const e = raw as Record<string, unknown>;
  if (typeof e.isGit !== 'boolean') return undefined;
  return {
    isGit: e.isGit,
    commits: Number(e.commits ?? 0),
    filesChanged: Number(e.filesChanged ?? 0),
    insertions: Number(e.insertions ?? 0),
    deletions: Number(e.deletions ?? 0),
    dirtyDelta: Array.isArray(e.dirtyDelta) ? e.dirtyDelta.map(String) : [],
  };
}

/**
 * The same task `getTask` returns, plus what the list mapper drops: the
 * agent's report, the outcome verdict and the on-disk evidence pack. The
 * snake_case handling (`Task` has no `rename_all`) lives in `mapGatewayTask`,
 * so list rows and this screen agree on entity, agent and timestamps.
 */
export async function getTaskDetail(id: string): Promise<TaskDetail | null> {
  const raw = await http
    .get<Record<string, unknown>>(`/api/tasks/${encodeURIComponent(id)}`)
    .catch((e: unknown) => {
      if (e instanceof APIError && e.status === 404) return null;
      throw e;
    });
  if (!raw) return null;

  const base = mapGatewayTask(raw);
  const meta =
    raw.metadata != null && typeof raw.metadata === 'object'
      ? (raw.metadata as Record<string, unknown>)
      : {};
  // The owning entity's name for the heading; best-effort, never blocks.
  const entityName =
    base.entityName ??
    (base.entityId
      ? await http
          .get<Record<string, unknown>>(`/api/entities/${encodeURIComponent(base.entityId)}`)
          .then((e) => str(e?.name))
          .catch(() => undefined)
      : undefined);

  return {
    ...base,
    entityName,
    result: str(raw.result),
    outcome: str(raw.outcome),
    evidence: parseEvidence(meta.evidence),
    // The executor blanks this marker the moment a run closes, so an empty
    // string means "no run in flight" — not a run called "".
    activeRunId: str(meta.activeRunId),
    lastRunId: str(meta.lastRunId),
  };
}

/** Feed events for one task — the gateway filters on `task_id` itself. */
export async function listTaskEvents(id: string, limit = 40): Promise<TaskEvent[]> {
  const events = await http.list<GatewayFeedEvent>(
    `/api/operator-feed?task_id=${encodeURIComponent(id)}&limit=${limit}`,
  );
  return events.map((e) => ({
    id: e.id,
    kind: e.kind,
    title: e.title,
    detail: e.detail,
    at: e.timestamp,
  }));
}

/**
 * The agent run behind a task, when there is one.
 *
 * `metadata.activeRunId` names the run only while it is in flight — the
 * executor blanks it when the run closes, which is exactly when its
 * screenshots become worth looking at. So the gateway now also records
 * `metadata.lastRunId` on completion, and that is the answer for a finished
 * task.
 *
 * The activity-timeline scan remains as a fallback for tasks completed by a
 * gateway older than that field: `agent_run` rows carry the run id in `refId`
 * and the task they belong to in `taskId`, newest first.
 */
export async function getTaskRunId(task: TaskDetail): Promise<string | null> {
  // In flight, then the run the gateway recorded on completion. The activity
  // scan below is only for tasks finished by an older gateway that never wrote
  // `lastRunId`.
  if (task.activeRunId) return task.activeRunId;
  if (task.lastRunId) return task.lastRunId;
  const scope = task.entityId ? `&entity_id=${encodeURIComponent(task.entityId)}` : '';
  const runs = await http
    .list<GatewayActivityItem>(`/api/activity?source=agent_run&limit=50${scope}`)
    .catch(() => [] as GatewayActivityItem[]);
  return runs.find((r) => r.taskId === task.id && r.refId)?.refId ?? null;
}

/**
 * Files a run produced — `GET /api/agent-runs/{id}/artifacts`. Live-only, and
 * every HTTP answer (404 for a run this workspace can't see, 403 for one that
 * isn't ours) means "nothing to show" rather than an outage; only a gateway
 * that never answered propagates.
 */
export async function listRunArtifacts(runId: string): Promise<RunArtifact[]> {
  const items = await http
    .list<{ name?: unknown; bytes?: unknown }>(
      `/api/agent-runs/${encodeURIComponent(runId)}/artifacts`,
    )
    .catch((e: unknown) => {
      if (e instanceof APIError) return [];
      throw e;
    });
  return items.map((a) => ({ name: String(a.name ?? ''), size: humanBytes(Number(a.bytes ?? 0)) }));
}

/**
 * What an `<Image>` needs to pull one artifact straight from the gateway.
 *
 * The bytes route is bearer-authenticated like every other call, so a bare
 * `{ uri }` would come back 401. React Native's `ImageURISource` accepts
 * `headers`, which keeps the download on the native image loader (and its
 * cache) instead of copying a screenshot through JS as a base64 string.
 */
export async function runArtifactSource(
  runId: string,
  name: string,
): Promise<{ uri: string; headers: Record<string, string> }> {
  const base = await getGatewayUrl();
  const token = await getAuthToken();
  return {
    uri: `${base}/api/agent-runs/${encodeURIComponent(runId)}/artifacts/${encodeURIComponent(name)}`,
    headers: token ? { Authorization: `Bearer ${token}` } : {},
  };
}

/** `PATCH /api/tasks/{id}` with the kebab-case status the gateway expects. */
export async function completeTask(id: string): Promise<TaskActionResult> {
  return taskAction(`complete ${id}`, () =>
    http.patch<Record<string, unknown>>(`/api/tasks/${id}`, { status: 'completed' }),
  );
}

/** `POST /api/tasks/{id}/snooze` — hours are clamped to 1..168 server-side. */
export async function snoozeTask(id: string, hours = 24): Promise<TaskActionResult> {
  return taskAction(`snooze ${id} ${hours}h`, () =>
    http.post<Record<string, unknown>>(`/api/tasks/${id}/snooze`, { hours }),
  );
}

// ── Calendar / schedule ────────────────────────────────────────────────

/**
 * Everything with a date on it: cron jobs from `/api/schedule` (at their next
 * run) and open tasks that carry a `due_at` deadline. Names are resolved —
 * an agent's id is not its name, and showing `agent_7f3a` as one was noise.
 */
export async function listSchedule(): Promise<ScheduleItem[]> {
  const real = await tryGateway('schedule', async () => {
    const [jobs, pages, agents, entities] = await Promise.all([
      http.get<GatewayScheduleTask[]>('/api/schedule'),
      Promise.all(
        OPEN_TASK_STATUSES.map((status) =>
          http
            .list<Record<string, unknown>>(`/api/tasks?status=${status}&limit=200`)
            .catch(() => [] as Record<string, unknown>[]),
        ),
      ),
      http.list<Record<string, unknown>>('/api/agents').catch(() => []),
      http.list<Record<string, unknown>>('/api/entities?limit=200').catch(() => []),
    ]);
    const agentNames = new Map(agents.map((a) => [String(a.id), str(a.name)] as const));
    const entityNames = new Map(entities.map((e) => [String(e.id), str(e.name)] as const));

    const items: ScheduleItem[] = (jobs ?? []).map((t) => ({
      id: t.id,
      title: t.taskDescription,
      at: t.nextRun,
      kind: 'scheduled' as const,
      agentName: agentNames.get(t.agentId) ?? undefined,
      entityId: t.entityId ?? undefined,
      entityName: t.entityId ? entityNames.get(t.entityId) : undefined,
    }));

    const seen = new Set<string>();
    for (const raw of pages.flat()) {
      const m = mapGatewayTask(raw);
      if (!m.dueAt || seen.has(m.id) || m.status === 'cancelled' || m.status === 'done') continue;
      seen.add(m.id);
      items.push({
        id: `task:${m.id}`,
        taskId: m.id,
        title: m.title,
        at: m.dueAt,
        kind: 'mission',
        agentName: m.agentId ? agentNames.get(m.agentId) : undefined,
        entityId: m.entityId,
        entityName: m.entityId ? entityNames.get(m.entityId) : undefined,
      });
    }
    return items;
  });
  if (real) return real;
  await mockLatency();
  return [...fx.scheduleItems];
}

// ── System / sessions ──────────────────────────────────────────────────

/** A registered device that isn't a phone — i.e. something desktop-shaped. */
function isDesktopDevice(d: Record<string, unknown>): boolean {
  const platform = String(d.platform ?? '').toLowerCase();
  const label = String(d.label ?? '').toLowerCase();
  const phone = platform === 'ios' || platform === 'android';
  return !phone && (platform.length > 0 || label.length > 0) && d.enabled !== false;
}

/**
 * What this phone can honestly say about the platform.
 *
 * `/api/runtime/status` is an adapter/agent inventory (`messagingEnabled`,
 * `adapters`, `agentIds`, `pluginCount`) — it has never carried `activeWorkers`,
 * `queuedTasks` or `paused`. Each figure comes from an endpoint that actually
 * reports it, or is left undefined:
 *
 * - `activeWorkers`  ← `runtime/status.agentIds.length`
 * - `queuedMissions` ← `GET /api/tasks?status=queued`
 * - `llmRequests` / `llmCostUsd` ← `GET /api/metrics` (a CostTracker snapshot)
 * - `desktopPaired`  ← the gateway answered an authenticated call. The gateway
 *   *is* the desktop app, so a 200 from it is the pairing. (This used to look
 *   for some other non-phone row in `/api/devices`, and read "Not paired" on
 *   every phone that was talking to the desktop.)
 * - `signedOut`      ← the gateway answered 401/403: reachable, not trusted.
 * - `desktopName`    ← the desktop's own device row, when it registered one
 * - `latencyMs`      ← measured here, round-trip of the status call
 * - `cpu` / `memory` ← **gone**. The gateway reports no host telemetry.
 * - `paused`         ← local only; there is no platform pause endpoint.
 */
export async function getSystemHealth(): Promise<SystemHealth> {
  // The URL this phone is actually configured to call, not the default the
  // fixture happens to carry.
  const gatewayUrl = await getGatewayUrl().catch(() => fx.systemHealth.gatewayUrl);
  const demo = gatewayUrl === DEFAULT_GATEWAY_URL;
  let refused = false;
  // This is itself the health check, so it must resolve rather than throw —
  // a real gateway that can't be reached is an answer ("offline"), reported
  // below without fixture figures standing in for measurements.
  const real = await tryGateway('runtime/status', async () => {
    const { value: runtime, ms } = await timed(() =>
      http.get<GatewayRuntimeStatus>('/api/runtime/status'),
    );
    const [queued, metrics, devices] = await Promise.all([
      // `total` rather than `items.length` — the page size would cap the count.
      http
        .get<Paginated<Record<string, unknown>>>('/api/tasks?status=queued&limit=1')
        .catch(() => null),
      http.get<GatewayMetrics>('/api/metrics').catch(() => null),
      http.list<Record<string, unknown>>('/api/devices').catch(() => null),
    ]);
    return { runtime, ms, queued, metrics, devices };
  }).catch((e: unknown) => {
    if (e instanceof APIError && (e.status === 401 || e.status === 403)) refused = true;
    return null;
  });

  if (real) {
    const desktop = real.devices?.find(isDesktopDevice);
    return {
      gatewayConnected: true,
      gatewayUrl,
      latencyMs: real.ms,
      desktopPaired: true,
      desktopName: desktop ? str(desktop.label) : undefined,
      activeWorkers: real.runtime.agentIds?.length,
      queuedMissions: real.queued?.total,
      llmRequests: real.metrics?.total_requests,
      llmCostUsd: real.metrics?.total_cost_usd,
      paused: false,
      live: true,
      demo: false,
    };
  }

  if (refused) {
    // Reachable, but this phone's credentials were refused: connected to the
    // computer, not trusted by it. Sign-in is the fix, not the network.
    return {
      gatewayConnected: true,
      gatewayUrl,
      desktopPaired: false,
      signedOut: true,
      paused: false,
      live: true,
      demo,
    };
  }

  if (!demo) {
    // Real gateway, unreachable: every count is honestly unknown (the UI
    // renders em-dashes), never a fixture number.
    return { gatewayConnected: false, gatewayUrl, desktopPaired: false, paused: false, live: false, demo: false };
  }

  await mockLatency();
  return { ...fx.systemHealth, gatewayUrl, gatewayConnected: false, live: false, demo: true };
}

export async function listTrustedDevices(): Promise<TrustedDevice[]> {
  const real = await tryGateway('devices', () =>
    http.list<Record<string, unknown>>('/api/devices'),
  );
  if (real) {
    const thisInstall = await getInstallId().catch(() => '');
    return real.map((d) => {
      const installId = String(d.installId ?? '');
      return {
        id: String(d.id ?? installId),
        name: String(d.label ?? d.platform ?? 'Unknown device'),
        platform: String(d.platform ?? 'unknown'),
        lastSeen: String(d.lastSeenAt ?? d.createdAt ?? ''),
        // The gateway has no notion of "this device" — only the client knows
        // which install id is its own.
        current: installId !== '' && installId === thisInstall,
        trusted: d.enabled !== false,
      };
    });
  }
  await mockLatency();
  return [...fx.trustedDevices];
}

/**
 * Sign-ins waiting on this phone: `GET /api/auth/approvals`.
 *
 * This used to read `/api/sessions`, which lists **chat/memory sessions** and
 * has no `status` field at all — so `s.status ?? 'pending'` matched every row
 * and the Control tab presented ordinary conversations as devices asking to log
 * in. Nothing else on the gateway answers this question.
 *
 * There is no mock fallback: a sign-in request is a security prompt, and an
 * unreachable or signed-out gateway means "we don't know of any", not "here's a
 * sample one to approve".
 */
export async function listPendingSessions(): Promise<Session[]> {
  try {
    const pending = await http.get<GatewayPendingApproval[]>('/api/auth/approvals');
    return (pending ?? []).map((p) => ({
      id: p.requestId,
      device: p.context?.trim() || 'A device wants to sign in',
      location: `Code ${p.code} — must match the device`,
      requestedAt: p.createdAt,
      status: 'pending' as const,
      code: p.code,
      expiresAt: p.expiresAt,
    }));
  } catch (e) {
    // 401 = this phone isn't signed in to the gateway, so it can't be anyone's
    // authenticator yet. Both cases show an empty list, never a fixture.
    log(
      'info',
      'system',
      `no sign-in requests — ${e instanceof APIError ? `computer said ${e.status}` : 'computer unreachable'}`,
    );
    return [];
  }
}

/**
 * Approve or deny a sign-in from this phone —
 * `POST /api/auth/approvals/{id}/approve|deny`, both 204 on success.
 *
 * Requires a signed-in gateway session: the handlers read `AuthContext` and
 * only release a token for approvals belonging to that principal. Signed out,
 * the call 401s and this throws rather than reporting a decision that never
 * reached the server.
 */
export async function authenticateSession(
  sessionId: string,
  approve: boolean,
  code?: string,
): Promise<Session> {
  const decided = new Date().toISOString();
  if (approve) {
    if (!code) {
      // The gateway compares this against the request, so approving without it
      // would 4xx anyway — failing here says why.
      throw new Error('Missing the sign-in code for this request.');
    }
    await http.post(`/api/auth/approvals/${encodeURIComponent(sessionId)}/approve`, {
      code,
      device: `${Platform.OS} phone`,
    });
  } else {
    await http.post(`/api/auth/approvals/${encodeURIComponent(sessionId)}/deny`, {});
  }
  log('info', 'system', `computer ✓ sign-in ${sessionId} ${approve ? 'approved' : 'denied'}`);
  return {
    id: sessionId,
    device: 'Sign-in request',
    requestedAt: decided,
    status: approve ? 'approved' : 'rejected',
  };
}

export async function connectGateway(url: string): Promise<SystemHealth> {
  log('info', 'api', `Connecting to your computer at ${url}…`);
  gatewayState.reachable = null;
  gatewayState.lastFailureAt = 0;
  gatewayState.unreachableSince = 0;
  const health = await getSystemHealth();
  return { ...health, gatewayUrl: url };
}

// ── Mock helpers ───────────────────────────────────────────────────────
function findApproval(id: string): Approval {
  return (
    fx.approvals.find((a) => a.id === id) ?? {
      id,
      title: 'Action',
      detail: '',
      risk: 'low',
      requestedBy: 'System',
      createdAt: new Date().toISOString(),
      status: 'pending',
    }
  );
}

function mockReply(text: string): string {
  const t = text.toLowerCase();
  if (t.includes('fieldnote') || t.includes('beta') || t.includes('migration')) {
    return 'Fieldnote’s beta readiness is at 62%. The one thing waiting on you is the production data migration — it’s high-risk and can’t be rolled back once beta users sign in, so I’ve held it for your approval. Everything else on Fieldnote is either running or queued.';
  }
  if (t.includes('harbor') || t.includes('coffee') || t.includes('catalog')) {
    return 'Harbor Coffee is in launch prep. The bean catalog import is at 88% and in review, and the launch copy is queued. I can publish the catalog draft the moment you approve it.';
  }
  if (t.includes('summar') || t.includes('today')) {
    return 'Today: three missions advanced, one completed, one blocked on the terms review. The Fieldnote data migration is the only decision truly waiting on you. Want the full review?';
  }
  if (t.includes('mission') || t.includes('start')) {
    return 'I can start a mission. Tell me the goal and which entity it belongs to, and I’ll route it to the right worker and hold anything sensitive for your approval.';
  }
  return 'Got it. I’ve noted that. I can dig into any project, capture it as a thought, or turn it into a mission — what would help most right now?';
}

function followUpsFor(text: string): string[] {
  const t = text.toLowerCase();
  if (t.includes('fieldnote') || t.includes('migration')) {
    return ['Approve the migration', 'What’s the risk?', 'Open Fieldnote'];
  }
  if (t.includes('harbor') || t.includes('coffee')) {
    return ['Publish the catalog', 'Open Harbor Coffee', 'Draft the grower email'];
  }
  return ['Summarize today', 'Show approvals', 'Open the brain map'];
}

// Aggregate export so screens can `import { api } from '@/api'`.
export const api = {
  getCompanionFeed,
  sendMessage,
  sendMessageStream,
  listEntities,
  getEntity,
  saveEntityNote,
  listMissions,
  listWorkers,
  listApprovals,
  getApproval,
  approveAction,
  rejectAction,
  getDailyReview,
  completeTask,
  snoozeTask,
  getTask,
  getTaskDetail,
  listTaskEvents,
  getTaskRunId,
  listRunArtifacts,
  runArtifactSource,
  listSchedule,
  getSystemHealth,
  listTrustedDevices,
  listPendingSessions,
  authenticateSession,
  connectGateway,
};
