import { describeMissionFeedEvent, extractFeedReason, parseFeedDetail } from '$lib/missionFeed';

export type MissionExecutionStatus = 'active' | 'queued' | 'failed' | 'recently-active' | 'idle';

export type MissionExecutionTaskLike = {
  id: string;
  title: string;
  status?: string;
  progress?: number;
  assigned_director_id?: string;
  assigned_agent_id?: string;
  task_kind?: string;
  result?: string;
  requires_human_approval?: boolean;
};

export type MissionExecutionEventLike = {
  id?: string;
  timestamp?: string;
  kind?: string;
  title?: string;
  detail?: string;
};

export type MissionExecutionSummary = {
  status: MissionExecutionStatus;
  label: string;
  tone: 'success' | 'info' | 'warn' | 'muted' | 'danger';
  detail: string;
  activeCount: number;
  queuedCount: number;
  failedCount: number;
  recentlyActiveAt?: string;
  blockingReason?: string;
  latestEventKind?: string;
};

const RECENTLY_ACTIVE_WINDOW_MS = 30 * 60 * 1000;

function safeDate(value?: string): Date | null {
  if (!value) return null;
  const parsed = new Date(value);
  return Number.isNaN(parsed.getTime()) ? null : parsed;
}

function extractReason(detail?: string): string | undefined {
  if (!detail) return undefined;
  const reason = extractFeedReason(detail);
  if (reason) return reason;
  const lines = detail.split('\n').map((line) => line.trim()).filter(Boolean);
  return lines[0];
}

export function deriveMissionExecutionSummary(
  tasks: MissionExecutionTaskLike[],
  events: MissionExecutionEventLike[] = [],
  now = new Date()
): MissionExecutionSummary {
  const activeTasks = tasks.filter((task) => task.status === 'in-progress');
  const queuedTasks = tasks.filter((task) => task.status === 'queued' || task.status === 'backlog');
  const failedTasks = tasks.filter((task) => task.status === 'failed');
  const latestEvent = events[0];
  const latestExecutionEvent = events.find((event) =>
    ['task_completed', 'task_blocked', 'mission_stalled', 'mission_followup_suppressed', 'mission_missing_next_task'].includes(event.kind ?? '')
  );
  const latestExecutionSummary = latestExecutionEvent ? describeMissionFeedEvent(latestExecutionEvent) : null;
  const latestExecutionAt = safeDate(latestExecutionEvent?.timestamp);
  const recentlyActive = latestExecutionAt
    ? now.getTime() - latestExecutionAt.getTime() <= RECENTLY_ACTIVE_WINDOW_MS
    : false;

  if (activeTasks.length > 0) {
    const primary = activeTasks[0];
    const actorBits = [primary.assigned_director_id, primary.assigned_agent_id].filter(Boolean);
    return {
      status: 'active',
      label: 'Worker active',
      tone: 'success',
      detail: actorBits.length > 0
        ? `${primary.title} is running via ${actorBits.join(' / ')}.`
        : `${primary.title} is currently running.`,
      activeCount: activeTasks.length,
      queuedCount: queuedTasks.length,
      failedCount: failedTasks.length,
      latestEventKind: latestEvent?.kind
    };
  }

  if (queuedTasks.length > 0) {
    const primary = queuedTasks[0];
    const latestParsed = parseFeedDetail(latestEvent?.detail);
    const queueReason = extractReason(latestEvent?.detail);
    return {
      status: 'queued',
      label: latestParsed.reason === 'autorun_blocked' ? 'Auto-run blocked' : 'Queued for runner',
      tone: 'info',
      detail: latestExecutionSummary
        ?? (queueReason
          ? `${primary.title} is waiting to run (${queueReason}).`
          : `${primary.title} is queued and waiting for a worker pickup.`),
      activeCount: 0,
      queuedCount: queuedTasks.length,
      failedCount: failedTasks.length,
      blockingReason: queueReason,
      latestEventKind: latestEvent?.kind
    };
  }

  if (failedTasks.length > 0) {
    const primary = failedTasks[0];
    const blockingReason = extractReason(primary.result) ?? extractReason(latestExecutionEvent?.detail);
    return {
      status: 'failed',
      label: 'Needs attention',
      tone: 'danger',
      detail: latestExecutionSummary
        ?? (blockingReason
          ? `${primary.title} failed or stalled (${blockingReason}).`
          : `${primary.title} failed or stalled and needs intervention.`),
      activeCount: 0,
      queuedCount: 0,
      failedCount: failedTasks.length,
      blockingReason,
      latestEventKind: latestEvent?.kind
    };
  }

  if (recentlyActive && latestExecutionAt) {
    return {
      status: 'recently-active',
      label: 'Recently active',
      tone: 'warn',
      detail: `Latest execution signal was ${latestExecutionEvent?.kind?.replaceAll('_', ' ') ?? 'recent activity'} at ${latestExecutionAt.toLocaleString()}.`,
      activeCount: 0,
      queuedCount: 0,
      failedCount: 0,
      recentlyActiveAt: latestExecutionEvent?.timestamp,
      latestEventKind: latestEvent?.kind
    };
  }

  return {
    status: 'idle',
    label: 'Idle',
    tone: 'muted',
    detail: tasks.length === 0
      ? 'No mission work has been queued yet.'
      : 'No worker is active and nothing is currently queued.',
    activeCount: 0,
    queuedCount: 0,
    failedCount: 0,
    latestEventKind: latestEvent?.kind
  };
}
