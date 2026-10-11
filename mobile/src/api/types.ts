/**
 * Domain models for the Consistent mobile app.
 *
 * Shapes mirror the Rust gateway REST surface (see
 * `ios/ConsistentWorld/Services/Endpoints.swift`): a `Paginated<T>` envelope
 * plus `/api/entities`, `/api/tasks`, `/api/agents`, `/api/capture/chat`.
 * The mock layer returns these exact shapes so swapping to the real gateway
 * is a one-line change per method in `endpoints.ts`.
 */

import type { EntityType } from '@/theme/tokens';

export type { EntityType };

/** Matches the gateway's `PaginatedResponse<T>`. */
export interface Paginated<T> {
  items: T[];
  total: number;
  limit: number;
  offset: number;
  hasMore: boolean;
}

// ── Entities ───────────────────────────────────────────────────────────
export interface Entity {
  id: string;
  name: string;
  type: EntityType;
  summary: string;
  stage?: string;
  /** 0..1 — drives node size / prominence in the Brain map. */
  weight: number;
  pinned: boolean;
  /**
   * ISO. Absent for live entities: the gateway's `Entity` carries no
   * timestamps, and an invented one would make "recent" a lie.
   */
  lastActiveAt?: string;
  /** Operator-picked accent (`Entity.colour`, hex). Wins over the type accent. */
  colour?: string;
  /** Files the gateway counts for this entity (`Entity.fileCount`). */
  fileCount?: number;
  /** The signed-in user's private Personal Space. */
  personal?: boolean;
  relatedIds: string[];
  stats: {
    files: number;
    notes: number;
    missions: number;
    memories: number;
  };
}

export interface EntityNote {
  id: string;
  entityId: string;
  title: string;
  body: string;
  /**
   * Absent for the entity's own freeform `notes` field, which the gateway
   * stores without a timestamp — the UI must not date it.
   */
  createdAt?: string;
  backlinks: string[]; // entity ids referenced
}

export interface EntityFile {
  id: string;
  entityId: string;
  name: string;
  kind: 'doc' | 'image' | 'audio' | 'link' | 'code';
  size: string;
  updatedAt: string;
}

export interface ActivityEvent {
  id: string;
  entityId?: string;
  actor: string; // agent or "You" / "System"
  action: string;
  at: string;
}

/**
 * One line of entity-scoped memory.
 *
 * `GET /api/entities/{id}/memory-preview` returns bare strings with no
 * timestamps, so `at` is absent for live memories — the UI says which store the
 * line came from instead of inventing an age.
 */
export interface EntityMemory {
  id: string;
  text: string;
  at?: string;
  scope?: 'short' | 'long';
}

export interface EntityDetail extends Entity {
  goals?: string;
  operatingRules?: string;
  profile?: string;
  activeContext?: string;
  whatItIs?: string;
  primaryGoal?: string;
  successCriteria?: string;
  /** Joined "backend · frontend · database" — omitted parts drop their separator. */
  stack?: string;
  repoPath?: string;
  readiness?: {
    percent: number;
    label: string;
    nextQuestion?: string;
  };
  knowledgeIndex?: {
    status: 'indexing' | 'ready' | 'failed' | 'skipped';
    docCount: number;
    embeddedCount: number;
    indexedAt?: string;
    error?: string;
    source?: string;
  };
  notes: EntityNote[];
  files: EntityFile[];
  missions: Mission[];
  memories: EntityMemory[];
  activity: ActivityEvent[];
  related: Entity[];
  /** True when this detail was assembled from the live gateway, not fixtures. */
  live: boolean;
}

// ── Missions / workers ─────────────────────────────────────────────────
export type MissionStatus =
  | 'queued'
  | 'running'
  | 'blocked'
  | 'review'
  | 'done'
  | 'failed'
  /** Stopped by a person or a cancelled parent mission. Terminal, not a failure. */
  | 'cancelled';

export interface Mission {
  id: string;
  title: string;
  status: MissionStatus;
  entityId?: string;
  entityName?: string;
  agentId?: string;
  agentName?: string;
  /** 0..1 */
  progress: number;
  dept?: string;
  /** RFC3339 deadline (`Task.due_at`), when the task is time-bound. */
  dueAt?: string;
  createdAt?: string;
  /** Last status change, else creation. Empty when the gateway gave neither. */
  updatedAt: string;
}

/**
 * What actually changed on disk during the run that closed a task — the
 * gateway's `EvidencePack` (`evidence.rs`), stored verbatim under
 * `task.metadata.evidence` and camelCase on the wire.
 *
 * `isGit` false means the run directory has no git history: such a pack can
 * prove presence of change but never absence, so the UI must say the numbers
 * aren't available rather than render zeroes as "nothing happened".
 */
export interface TaskEvidence {
  isGit: boolean;
  commits: number;
  filesChanged: number;
  insertions: number;
  deletions: number;
  /** Paths left dirty (or newly untracked) by the run, uncommitted. */
  dirtyDelta: string[];
}

/**
 * A task with the fields the list mapper drops — the agent's own report, the
 * finer-grained verdict, and the evidence pack. Everything here is optional
 * because a queued task has none of it yet.
 */
export interface TaskDetail extends Mission {
  /** The agent's report, written into `Task.result` when the run closes. */
  result?: string;
  /** `TaskOutcome`, snake_case: `completed`, `needs_review`, … */
  outcome?: string;
  evidence?: TaskEvidence;
  /** `metadata.activeRunId` — present only while a run is in flight. */
  activeRunId?: string;
  /** `metadata.lastRunId` — the run that produced this task's output, kept
   *  after it finishes (which is when its screenshots become interesting). */
  lastRunId?: string;
  createdAt?: string;
}

/** One feed event scoped to a single task, for its timeline. */
export interface TaskEvent {
  id: string;
  /** Snake_case `OperatorFeedKind`, e.g. `agent_run_completed`. */
  kind: string;
  title: string;
  detail?: string;
  at: string;
}

/**
 * A file an agent run left behind — `GET /api/agent-runs/{id}/artifacts`.
 * `size` is pre-formatted the same way entity files are, since a byte count
 * is never what the row wants to show.
 */
export interface RunArtifact {
  name: string;
  size: string;
}

export interface Worker {
  id: string;
  name: string;
  role: string;
  groupId: string;
  /**
   * Absent for live agents: `/api/agents` is a roster, not a run-state feed,
   * so the app shows no status rather than calling every agent "idle".
   */
  status?: 'idle' | 'running' | 'paused' | 'error';
  currentMissionId?: string;
  load: number; // 0..1
}

// ── Companion feed ─────────────────────────────────────────────────────
export type FeedRole = 'user' | 'assistant' | 'system';

export interface Approval {
  id: string;
  title: string;
  detail: string;
  /**
   * Only when the source actually rated it. The gateway's `ApprovalRequest`
   * has no risk field, so live approvals leave this unset and show no pill.
   */
  risk?: 'low' | 'medium' | 'high';
  requestedBy: string;
  createdAt: string;
  status: 'pending' | 'approved' | 'rejected';
  /** The task that runs once this is approved (gateway `taskId`) — what the
   *  inline chat card polls after a decision. */
  taskId?: string;
}

/**
 * How a system (operator-feed) row presents. Derived from the gateway's
 * snake_case `OperatorFeedKind` so an approval, an overdue commitment, the
 * morning digest and a failure don't all look like the same grey pill.
 */
export type FeedVariant =
  | 'heartbeat'
  | 'status'
  | 'approval'
  | 'input'
  | 'overdue'
  | 'digest'
  | 'failure'
  | 'success'
  | 'review'
  | 'default';

export interface FeedMessage {
  id: string;
  role: FeedRole;
  /** System messages carry a variant for how they present (see `FeedVariant`). */
  variant?: FeedVariant;
  /** Short label for a system row ("Needs your approval", "Overdue", …). */
  label?: string;
  /** Where tapping a system row should go, from the feed event's own ids. */
  link?: { approvalId?: string; taskId?: string; entityId?: string };
  /** Small caption under a chat bubble — e.g. the entity a turn was scoped to. */
  meta?: string;
  /**
   * An assistant bubble that reports a failure (signed out, out of credits,
   * unreachable) rather than an answer. Styled as such and never sent back to
   * the model as conversation history.
   */
  error?: boolean;
  text: string;
  at: string;
  /** Optional embedded approval card. */
  approval?: Approval;
  /**
   * Approval-gated actions the assistant queued during this turn (the chat
   * stream's `done` payload, `action_type: "action_request"`). Each renders as
   * an inline card that fetches the approval and tracks the linked task.
   */
  actionRequests?: { approvalId: string; description: string }[];
  /** Suggested follow-up chips shown under an assistant message. */
  followUps?: string[];
  pending?: boolean; // streaming in progress
}

export interface QuickAction {
  id: string;
  label: string;
  icon: string;
  intent:
    | 'ask'
    | 'capture'
    | 'approve'
    | 'mission'
    | 'summary'
    | 'entity'
    | 'auth';
  /**
   * Concrete subject for the action — the entity name a live `ask` chip was
   * built from, for instance. Absent on the fixture chips, which is exactly why
   * the screen must not hardcode a project name of its own.
   */
  payload?: string;
}

export interface CompanionFeed {
  messages: FeedMessage[];
  needsAttention: Approval[];
  quickActions: QuickAction[];
  /** True when this feed came from the gateway, not the sample conversation. */
  live: boolean;
}

// ── Daily review ───────────────────────────────────────────────────────
export interface ReviewSection {
  id: string;
  kind:
    | 'summary'
    | 'completed'
    | 'questions'
    | 'missed'
    | 'next'
    | 'updates'
    | 'heartbeats';
  title: string;
  items: { id: string; text: string; meta?: string; entityId?: string }[];
}

/** Completion counts and the consecutive-days streak, from the gateway. */
export interface ReviewStreak {
  currentStreakDays: number;
  completedToday: number;
  completedLast7: number;
  completedLast30: number;
}

export interface DailyReview {
  date: string;
  headline: string;
  sections: ReviewSection[];
  /** Present only when the live gateway answered — mock fixtures have none. */
  streak?: ReviewStreak;
  /** True when this review came from `GET /api/daily-review`, not fixtures. */
  live: boolean;
}

// ── System / control ───────────────────────────────────────────────────

/**
 * What this phone can actually know about the platform.
 *
 * Deliberately missing: CPU and memory. The gateway exposes no host telemetry
 * (`/api/metrics` is an LLM cost/token tracker, `/api/runtime/status` is an
 * adapter inventory), so there is nothing to show and the app shows nothing
 * rather than a plausible percentage.
 *
 * Every optional field is "undefined = not known", and the UI renders an
 * em-dash for those rather than a zero that reads like a measurement.
 */
export interface SystemHealth {
  gatewayConnected: boolean;
  gatewayUrl: string;
  /** Measured round-trip of this device's own health request. */
  latencyMs?: number;
  /**
   * This phone is connected to the computer running Consistent CEO: the
   * gateway (which IS the desktop) answered an authenticated call. It used to
   * mean "some other non-phone device row exists", which read "Not paired" on
   * every phone that was, in fact, talking to the desktop.
   */
  desktopPaired: boolean;
  /** The gateway answered but refused this phone's credentials (401/403). */
  signedOut?: boolean;
  /** Label of the desktop device row, when the gateway lists one. */
  desktopName?: string;
  /**
   * Demo mode: no computer paired yet (the built-in default URL), so sample
   * data is standing in. The only case the UI may say "sample data".
   */
  demo?: boolean;
  /** Agents the gateway runtime has registered. */
  activeWorkers?: number;
  /** Tasks sitting in `queued` right now. */
  queuedMissions?: number;
  /** Lifetime LLM requests the gateway has served (`/api/metrics`). */
  llmRequests?: number;
  /** Lifetime model spend in USD (`/api/metrics`). */
  llmCostUsd?: number;
  /**
   * Local-only. The gateway has no platform-wide pause switch, so this never
   * comes back from a real endpoint — see `getSystemHealth`.
   */
  paused: boolean;
  /** True when the numbers above came from the gateway, not fixtures. */
  live: boolean;
}

export interface TrustedDevice {
  id: string;
  name: string;
  platform: string;
  lastSeen: string;
  current: boolean;
  trusted: boolean;
}

/**
 * A sign-in waiting on this phone to approve it — the gateway's
 * `PendingApproval` (`/api/auth/approvals`), not a chat session.
 */
export interface Session {
  id: string;
  device: string;
  location?: string;
  requestedAt: string;
  status: 'pending' | 'approved' | 'rejected';
  /** Short code the requesting device is showing; must match before approving. */
  code?: string;
  expiresAt?: string;
}

/** Mirrors the gateway's `PendingApproval` (`auth_store.rs`, camelCase). */
export interface GatewayPendingApproval {
  requestId: string;
  code: string;
  context?: string | null;
  createdAt: string;
  expiresAt: string;
}

// ── Calendar / schedule ────────────────────────────────────────────────
export interface ScheduleItem {
  id: string;
  title: string;
  at: string; // ISO
  /** `scheduled` = a cron job's next run; `mission` = a task's `due_at`. */
  kind: 'scheduled' | 'mission' | 'review';
  /** Set for `mission` rows — the task to open when tapped. */
  taskId?: string;
  agentName?: string;
  entityId?: string;
  entityName?: string;
}

/** Mirrors the gateway's `ScheduleTaskResponse` (camelCase). */
export interface GatewayScheduleTask {
  id: string;
  cronExpression: string;
  nextRun: string;
  agentId: string;
  taskDescription: string;
  entityId?: string | null;
  connectionId?: string | null;
}

/** Mirrors the gateway's `ApprovalRequest` (camelCase). */
export interface GatewayApproval {
  id: string;
  createdAt: string;
  updatedAt: string;
  entityId: string;
  connectionId?: string;
  taskId?: string;
  action: string;
  status: 'pending' | 'approved' | 'rejected' | 'expired';
  detail?: string;
  createdBy?: string;
}

/**
 * Mirrors the gateway's `TaskLite` (control_api.rs). `Option` fields serialize
 * as explicit `null`, never omitted — hence `| null` rather than `?`.
 */
export interface GatewayTaskLite {
  id: string;
  title: string;
  entityId: string | null;
  entityName: string | null;
  missionId: string | null;
  priority: string | null;
  dueAt: string | null;
  /** Kebab-case: `backlog | queued | in-progress | failed | completed | cancelled`. */
  status: string;
  /** Whole hours past the deadline; null unless overdue. */
  overdueHours: number | null;
}

/** Mirrors the gateway's `DailyReviewResponse` (control_api.rs). */
export interface GatewayDailyReview {
  generatedAt: string;
  overdue: GatewayTaskLite[];
  dueToday: GatewayTaskLite[];
  pendingApprovals: { id: string; entityId: string; title: string; createdAt: string }[];
  missionsAwaitingInput: { id: string; title: string; entityId: string }[];
  heartbeatHighlights: {
    entityId: string | null;
    entityName: string | null;
    summary: string;
    firedAt: string;
  }[];
  streak: ReviewStreak;
  suggestedFocus: GatewayTaskLite[];
}

/**
 * Mirrors the gateway's `RawCapture` (`state.rs`).
 *
 * Snake_case on the wire: unlike most of the control API, `RawCapture` carries
 * no `#[serde(rename_all = "camelCase")]`. Getting this wrong reads as an empty
 * notes tab, not an error.
 */
export interface GatewayCapture {
  id: string;
  entity_id?: string;
  source: string;
  raw_text: string;
  summary?: string;
  status: string;
  created_at: string;
  updated_at: string;
}

/** Mirrors the gateway's uploaded `EntityFile` (`control_api.rs`, snake_case). */
export interface GatewayEntityFile {
  id: string;
  entity_id: string;
  filename: string;
  content_type: string;
  size_bytes: number;
  uploaded_at: string;
  path: string;
}

/** Mirrors the gateway's `ActivityItem` (`activity.rs`, camelCase). */
export interface GatewayActivityItem {
  id: string;
  timestamp: string;
  /** `feed` | `capture` | `agent_run`. */
  source: string;
  kind: string;
  title: string;
  detail?: string;
  status?: string;
  entityId?: string;
  missionId?: string;
  taskId?: string;
  refId?: string;
}

/** Mirrors the gateway's `EntityMemoryPreviewResponse` (camelCase). */
export interface GatewayMemoryPreview {
  shortTerm: string[];
  longTerm: string[];
}

/** Mirrors the gateway's `MetricsSnapshot` (`common/telemetry.rs`, snake_case). */
export interface GatewayMetrics {
  total_requests: number;
  total_input_tokens: number;
  total_output_tokens: number;
  total_cost_usd: number;
}

/** Mirrors the gateway's `RuntimeStatus` (`state.rs`, camelCase). */
export interface GatewayRuntimeStatus {
  messagingEnabled: boolean;
  adapters: string[];
  agentIds: string[];
  pluginCount: number;
}

/** Mirrors the gateway's `OperatorFeedEvent` (camelCase). */
export interface GatewayFeedEvent {
  id: string;
  timestamp: string;
  kind: string;
  taskId?: string;
  missionId?: string;
  entityId?: string;
  title: string;
  detail?: string;
  approvalId?: string;
}
