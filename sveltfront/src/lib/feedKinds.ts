// Operator-feed kinds: one human label, one tone, and one answer to "does this
// need a person?" for each — shared by the notification bell, the task board,
// the activity timeline, mission and entity pages, so the same event never
// reads three different ways.
//
// The authoritative list is `OperatorFeedKind` in
// `crates/gateway/src/state.rs` (serialized snake_case). A new kind there must
// be added here too — and to the other allowlists named in CLAUDE.md
// ("Wire-format gotchas"): mobile's MessageBubble/types and
// `notify.rs::is_notifiable` for push.

export const OPERATOR_FEED_KINDS = [
  'task_created',
  'task_completed',
  'task_blocked',
  'task_retry_scheduled',
  'task_stuck',
  'task_cancelled',
  'mission_cancelled',
  'mission_report_ready',
  'verification_needs_review',
  'approval_needed',
  'mission_created',
  'mission_completed',
  'mission_first_completed',
  'goal_loop_decision',
  'mission_iteration_advanced',
  'mission_stalled',
  'mission_missing_next_task',
  'mission_review_due',
  'mission_followup_suppressed',
  'mission_recommendation_accepted',
  'mission_recommendation_dismissed',
  'mission_snoozed',
  'mission_user_input_needed',
  'entity_source_imported',
  'approval_request_created',
  'approval_request_resolved',
  'connection_verified',
  'connection_check_failed',
  'scheduled_task_fired',
  'heartbeat_result',
  'agent_run_completed',
  'agent_run_failed',
  'task_overdue',
  'daily_digest',
  'entity_workspace_needed'
] as const;

export type OperatorFeedKind = (typeof OPERATOR_FEED_KINDS)[number];

/**
 * How a row should read at a glance — the coloured dot the operator scans
 * before the words. `alert`: something broke or slipped. `warn`: waiting on a
 * person. `good`: finished or confirmed. `info`: bookkeeping.
 */
export type FeedTone = 'alert' | 'warn' | 'good' | 'info';

const KINDS: Record<OperatorFeedKind, { label: string; tone: FeedTone }> = {
  task_created: { label: 'Task created', tone: 'info' },
  task_completed: { label: 'Task completed', tone: 'good' },
  task_blocked: { label: 'Task blocked', tone: 'alert' },
  task_retry_scheduled: { label: 'Retry scheduled', tone: 'warn' },
  task_stuck: { label: 'Task stuck', tone: 'alert' },
  task_cancelled: { label: 'Task cancelled', tone: 'info' },
  mission_cancelled: { label: 'Mission cancelled', tone: 'info' },
  mission_report_ready: { label: 'Mission report ready', tone: 'good' },
  verification_needs_review: { label: 'Needs review', tone: 'warn' },
  approval_needed: { label: 'Approval needed', tone: 'warn' },
  mission_created: { label: 'Mission created', tone: 'info' },
  mission_completed: { label: 'Mission completed', tone: 'good' },
  mission_first_completed: { label: 'First mission completed', tone: 'good' },
  goal_loop_decision: { label: 'Goal loop decision', tone: 'info' },
  mission_iteration_advanced: { label: 'Mission iteration', tone: 'info' },
  mission_stalled: { label: 'Mission stalled', tone: 'alert' },
  mission_missing_next_task: { label: 'Missing next task', tone: 'warn' },
  mission_review_due: { label: 'Review due', tone: 'warn' },
  mission_followup_suppressed: { label: 'Follow-up suppressed', tone: 'info' },
  mission_recommendation_accepted: { label: 'Recommendation accepted', tone: 'good' },
  mission_recommendation_dismissed: { label: 'Recommendation dismissed', tone: 'info' },
  mission_snoozed: { label: 'Review snoozed', tone: 'info' },
  mission_user_input_needed: { label: 'Input needed', tone: 'warn' },
  entity_source_imported: { label: 'Source imported', tone: 'good' },
  approval_request_created: { label: 'Approval requested', tone: 'warn' },
  approval_request_resolved: { label: 'Approval resolved', tone: 'good' },
  connection_verified: { label: 'Connection verified', tone: 'good' },
  connection_check_failed: { label: 'Connection check failed', tone: 'alert' },
  scheduled_task_fired: { label: 'Scheduled task ran', tone: 'info' },
  heartbeat_result: { label: 'Heartbeat', tone: 'info' },
  agent_run_completed: { label: 'Agent run finished', tone: 'good' },
  agent_run_failed: { label: 'Agent run failed', tone: 'alert' },
  task_overdue: { label: 'Task overdue', tone: 'alert' },
  daily_digest: { label: 'Daily brief', tone: 'info' },
  entity_workspace_needed: { label: 'Workspace needed', tone: 'warn' }
};

function known(kind: string): kind is OperatorFeedKind {
  return Object.prototype.hasOwnProperty.call(KINDS, kind);
}

/** "task_overdue" → "Task overdue". Unknown (newer-gateway) kinds fall back to
 *  a readable sentence-case form rather than raw snake_case. */
export function feedKindLabel(kind: string): string {
  if (known(kind)) return KINDS[kind].label;
  const words = kind.replaceAll('_', ' ').trim();
  return words ? words[0].toUpperCase() + words.slice(1) : 'Event';
}

export function feedKindTone(kind: string): FeedTone {
  return known(kind) ? KINDS[kind].tone : 'info';
}

/**
 * Kinds that are worth interrupting a person for — a browser notification and
 * a chime. Everything else is bookkeeping: it still lands in the bell list,
 * silently. Mirrors the spirit of `notify.rs::is_notifiable` (push), narrowed
 * to "a human has to look or act".
 */
const NEEDS_HUMAN = new Set<OperatorFeedKind>([
  'approval_needed',
  'approval_request_created',
  'mission_user_input_needed',
  'verification_needs_review',
  'mission_review_due',
  'mission_report_ready',
  'task_blocked',
  'task_stuck',
  'agent_run_failed',
  'connection_check_failed',
  'task_overdue',
  'daily_digest',
  'entity_workspace_needed',
  // A reminder the operator explicitly scheduled: firing it silently would
  // defeat the point of setting it.
  'scheduled_task_fired'
]);

export function feedKindNeedsHuman(kind: string): boolean {
  return known(kind) && NEEDS_HUMAN.has(kind);
}
