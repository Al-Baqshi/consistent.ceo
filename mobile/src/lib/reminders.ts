import * as Notifications from 'expo-notifications';
import { router } from 'expo-router';
import { AppState, type AppStateStatus, Platform } from 'react-native';
import { create } from 'zustand';

import { APIError } from '@/api/client';
import {
  createReminder as postReminder,
  listReminders,
  markRemindersLocalScheduled,
  reminderAction as postReminderAction,
} from '@/api/endpoints';
import type { CreateReminderInput, ReminderActionInput, ReminderList, ReminderView } from '@/api/types';
import { log } from '@/lib/logger';

import {
  ensureNotificationPermission,
  isReminderNotification,
  REMINDER_CATEGORY,
  setLocalReminderIds,
} from './notifications';
import { resolveWhen, tomorrowMorning } from './when';
import {
  clearOutbox,
  enqueueOutbox,
  getAllOutbox,
  getAllReminders,
  kvGet,
  kvRemove,
  kvSet,
  removeOutboxEntry,
  replaceAllReminders,
  upsertReminder,
  DbReminder,
  deleteReminder,
} from './db';

/**
 * The phone is the alarm clock; the computer is the brain.
 *
 * Every open reminder the gateway knows about is mirrored here as local
 * notifications — the first nudge and the follow-ups planned after it — so it
 * rings even when the app is closed, the phone is offline, or the computer is
 * asleep. Action buttons (Done / Snooze 1h / Tomorrow) work without opening
 * the app; when the gateway can't be reached the action is applied on the
 * phone at once and queued in an outbox that replays on the next sync.
 * Reminders captured offline are scheduled immediately under a `local:` id
 * and POSTed (idempotent by `clientId`) once the gateway answers.
 *
 * Local notification ids are `rem:<reminderId>:<isoTime>`, so a sync only
 * cancels and schedules what changed.
 *
 * Storage: SQLite (`reminders`, `reminder_outbox` tables) instead of
 * AsyncStorage, so the phone can query, index and transact locally.
 */

const OUTBOX_KEY = 'cw.reminders.outbox';
const MAX_PER_REMINDER = 4;
/** iOS keeps at most 64 pending local notifications per app. */
const MAX_TOTAL = 60;
const FOLLOW_UP_MS = 2 * 3_600_000;
const FOREGROUND_SYNC_MS = 60_000;
const BACKGROUND_TASK = 'cw.reminders.sync';
const NOTIFICATION_TASK = 'cw.reminders.notification';

const ACTION_DONE = 'done';
const ACTION_SNOOZE = 'snooze';
const ACTION_TOMORROW = 'tomorrow';

export type QuickReminderAction = 'done' | 'snooze' | 'tomorrow' | 'drop';

export interface ReminderResult {
  ok: boolean;
  /** Applied on the phone; the gateway hears about it on the next sync. */
  queued?: boolean;
  error?: string;
  item?: ReminderView;
}

type OutboxEntry =
  | { type: 'create'; clientId: string; input: CreateReminderInput; queuedAt: number }
  | { type: 'action'; id: string; input: ReminderActionInput; queuedAt: number };

interface RemindersState {
  items: ReminderView[];
  /** False while demo fixtures stand in for a computer. */
  live: boolean;
  loaded: boolean;
  syncing: boolean;
  /** The last sync couldn't reach the gateway; the list is what the phone knows. */
  offline: boolean;
  /** Changes waiting to reach the gateway. */
  pending: number;
  lastSyncAt: number;
}

export const useReminders = create<RemindersState>(() => ({
  items: [],
  live: false,
  loaded: false,
  syncing: false,
  offline: false,
  pending: 0,
  lastSyncAt: 0,
}));

export function isLocalReminderId(id: string): boolean {
  return id.startsWith('local:');
}

function newClientId(): string {
  return `m-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

// ── Storage (SQLite) ──────────────────────────────────────────────────

function reminderToDb(r: ReminderView, live: boolean): DbReminder {
  return {
    id: r.id,
    title: r.title,
    what: r.what,
    kind: r.kind,
    entityId: r.entityId,
    entityName: r.entityName,
    state: r.state,
    fireAt: r.fireAt,
    followUps: r.followUps,
    firstFiredAt: r.firstFiredAt,
    nudges: r.nudges,
    createdAt: r.createdAt,
    delegatedTaskId: r.delegatedTaskId ?? null,
    delegatedApprovalId: r.delegatedApprovalId ?? null,
    clientId: isLocalReminderId(r.id) ? r.id.slice('local:'.length) : null,
    isLocal: isLocalReminderId(r.id),
    live,
  };
}

function dbToReminder(r: DbReminder): ReminderView {
  return {
    id: r.id,
    title: r.title,
    what: r.what,
    kind: r.kind as ReminderView['kind'],
    entityId: r.entityId,
    entityName: r.entityName,
    state: r.state as ReminderView['state'],
    fireAt: r.fireAt,
    followUps: r.followUps,
    firstFiredAt: r.firstFiredAt,
    nudges: r.nudges,
    createdAt: r.createdAt,
    delegatedTaskId: r.delegatedTaskId,
    delegatedApprovalId: r.delegatedApprovalId,
  };
}

async function readCache(): Promise<{ items: ReminderView[]; live: boolean }> {
  const rows = await getAllReminders();
  const liveRaw = await kvGet('cw.reminders.live');
  return { items: rows.map(dbToReminder), live: liveRaw === '1' };
}

async function writeCache(items: ReminderView[], live: boolean): Promise<void> {
  await replaceAllReminders(items.map((r) => reminderToDb(r, live)), live);
  await kvSet('cw.reminders.live', live ? '1' : '0');
}

async function readOutbox(): Promise<OutboxEntry[]> {
  const rows = await getAllOutbox();
  const entries: OutboxEntry[] = [];
  for (const row of rows) {
    const input = JSON.parse(row.input) as Record<string, unknown>;
    if (row.type === 'create') {
      entries.push({
        type: 'create',
        clientId: row.clientId!,
        input: input as unknown as CreateReminderInput,
        queuedAt: row.queuedAt,
      });
    } else {
      entries.push({
        type: 'action',
        id: String(row.id),
        input: input as unknown as ReminderActionInput,
        queuedAt: row.queuedAt,
      });
    }
  }
  return entries;
}

async function writeOutbox(box: OutboxEntry[]): Promise<void> {
  await clearOutbox();
  for (const entry of box) {
    await enqueueOutbox({
      type: entry.type,
      clientId: entry.type === 'create' ? entry.clientId : null,
      input: JSON.stringify(entry.input),
      queuedAt: entry.queuedAt,
    });
  }
  const count = (await getAllOutbox()).length;
  useReminders.setState({ pending: count });
}

async function enqueue(entry: OutboxEntry): Promise<void> {
  await enqueueOutbox({
    type: entry.type,
    clientId: entry.type === 'create' ? entry.clientId : null,
    input: JSON.stringify(entry.input),
    queuedAt: entry.queuedAt,
  });
  useReminders.setState({ pending: (await getAllOutbox()).length });
}

// ── Planning ───────────────────────────────────────────────────────────

interface Nudge {
  identifier: string;
  reminderId: string;
  at: number;
  title: string;
  body: string;
  /** The exact `fireAt` string, when this nudge is the reminder's next fire. */
  fireAt?: string;
}

/** Soonest first, at most `MAX_PER_REMINDER` each and `MAX_TOTAL` overall. */
export function planNudges(items: ReminderView[], now = Date.now()): Nudge[] {
  const out: Nudge[] = [];
  for (const r of items) {
    if (r.state !== 'scheduled' && r.state !== 'due') continue;
    const times = [r.fireAt, ...(r.followUps ?? [])].filter((t): t is string => !!t);
    const seen = new Set<number>();
    times.forEach((iso, idx) => {
      const at = Date.parse(iso);
      if (!Number.isFinite(at) || at <= now + 1000 || seen.has(at) || seen.size >= MAX_PER_REMINDER) return;
      seen.add(at);
      const isFireAt = idx === 0 && r.fireAt === iso;
      const last = idx === times.length - 1 && idx > 0;
      const title =
        isFireAt && !r.firstFiredAt
          ? r.title || r.what
          : last
            ? `Still relevant? ${r.what}`
            : `Still open: ${r.what}`;
      out.push({
        identifier: `rem:${r.id}:${new Date(at).toISOString()}`,
        reminderId: r.id,
        at,
        title,
        body: r.entityName ?? '',
        fireAt: isFireAt ? iso : undefined,
      });
    });
  }
  return out.sort((a, b) => a.at - b.at).slice(0, MAX_TOTAL);
}

let planChain: Promise<unknown> = Promise.resolve();

/**
 * Make the pending `rem:` notifications match the plan: cancel what's no
 * longer wanted, schedule what's missing, leave the rest alone. Serialised so
 * two syncs can't double-schedule. Sample (demo) reminders are never
 * scheduled; ones the operator made on this phone always are.
 */
function applyPlan(items: ReminderView[], live: boolean): Promise<Nudge[]> {
  const run = planChain.then(() => applyPlanNow(items, live));
  planChain = run.catch(() => undefined);
  return run;
}

async function applyPlanNow(items: ReminderView[], live: boolean): Promise<Nudge[]> {
  if (Platform.OS === 'web') return [];
  const plan = planNudges(items.filter((r) => live || isLocalReminderId(r.id)));
  let existing: Notifications.NotificationRequest[];
  try {
    existing = await Notifications.getAllScheduledNotificationsAsync();
  } catch {
    return [];
  }
  const granted = await Notifications.getPermissionsAsync()
    .then((p) => p.granted)
    .catch(() => false);
  const existingIds = new Set(existing.map((n) => n.identifier).filter((id) => id.startsWith('rem:')));
  const wanted = new Set(plan.map((n) => n.identifier));

  for (const id of existingIds) {
    if (!wanted.has(id)) await Notifications.cancelScheduledNotificationAsync(id).catch(() => undefined);
  }

  const scheduled: Nudge[] = [];
  for (const n of plan) {
    if (existingIds.has(n.identifier)) {
      scheduled.push(n);
      continue;
    }
    if (!granted) continue;
    try {
      await Notifications.scheduleNotificationAsync({
        identifier: n.identifier,
        content: {
          title: n.title,
          body: n.body,
          sound: 'default',
          data: { taskId: n.reminderId, kind: 'reminder' },
          categoryIdentifier: REMINDER_CATEGORY,
        },
        trigger: { type: Notifications.SchedulableTriggerInputTypes.DATE, date: new Date(n.at) },
      });
      scheduled.push(n);
    } catch (e) {
      log('warn', 'system', `reminders: schedule failed: ${String(e)}`);
    }
  }
  setLocalReminderIds(scheduled.map((n) => n.reminderId));
  return scheduled;
}

/** Persist the list, re-plan the notifications and publish to the UI. */
async function commit(items: ReminderView[], live: boolean): Promise<Nudge[]> {
  await writeCache(items, live);
  useReminders.setState({ items, live, loaded: true });
  return applyPlan(items, live);
}

// ── Sync ───────────────────────────────────────────────────────────────

function isUnreachable(e: unknown): boolean {
  return !(e instanceof APIError) || e.status === 401 || e.status === 503;
}

/** Replay queued changes in order. False when the gateway still can't be reached. */
async function replayOutbox(): Promise<boolean> {
  const remaining = await readOutbox();
  if (remaining.length === 0) return true;
  while (remaining.length > 0) {
    const entry = remaining[0];
    try {
      if (entry.type === 'create') await postReminder({ ...entry.input, clientId: entry.clientId });
      else await postReminderAction(entry.id, entry.input);
      remaining.shift();
      await writeOutbox(remaining);
    } catch (e) {
      if (isUnreachable(e)) {
        await writeOutbox(remaining);
        return false;
      }
      log('warn', 'system', `reminders: gateway refused queued ${entry.type}: ${String(e)}`);
      remaining.shift();
      await writeOutbox(remaining);
    }
  }
  await clearOutbox();
  return true;
}

function localItemFor(clientId: string, input: CreateReminderInput): ReminderView {
  const fireAt = input.fireAt ?? resolveWhen(input.when).date.toISOString();
  return {
    id: `local:${clientId}`,
    title: input.what,
    what: input.what,
    kind: 'reminder',
    entityId: input.entityId ?? null,
    entityName: null,
    state: 'scheduled',
    fireAt,
    followUps: [new Date(Date.parse(fireAt) + FOLLOW_UP_MS).toISOString()],
    firstFiredAt: null,
    nudges: 0,
    createdAt: new Date().toISOString(),
  };
}

let inflight: Promise<void> | null = null;

/**
 * Fetch open reminders, mirror them as local notifications and tell the
 * gateway which fires the phone now owns. Works offline from the last plan.
 * Concurrent callers share one run.
 */
export function syncReminders(): Promise<void> {
  if (!inflight) {
    inflight = doSync().finally(() => {
      inflight = null;
    });
  }
  return inflight;
}

async function doSync(): Promise<void> {
  useReminders.setState({ syncing: true });
  try {
    await ensureReminderCategory();
    const reached = await replayOutbox();
    let list: ReminderList | null = null;
    if (reached) {
      list = await listReminders('open').catch(() => null);
    }
    const cache = await readCache();
    let items: ReminderView[];
    let live: boolean;
    if (list) {
      const outbox = await readOutbox();
      const local = outbox.flatMap((e) => (e.type === 'create' ? [localItemFor(e.clientId, e.input)] : []));
      items = [...list.items, ...local];
      live = list.live;
    } else {
      items = cache.items;
      live = cache.live;
    }
    const scheduled = await commit(items, live);
    useReminders.setState({ offline: !list, lastSyncAt: Date.now(), pending: (await readOutbox()).length });

    if (list?.live) {
      const owned = scheduled
        .filter((n) => n.fireAt && !isLocalReminderId(n.reminderId))
        .map((n) => ({ id: n.reminderId, fireAt: n.fireAt as string }));
      await markRemindersLocalScheduled(owned).catch(() => undefined);
    }
  } catch (e) {
    log('warn', 'system', `reminders: sync failed: ${String(e)}`);
  } finally {
    useReminders.setState({ syncing: false });
  }
}

// ── Creating ───────────────────────────────────────────────────────────

async function upsert(item: ReminderView, replaceId?: string): Promise<void> {
  const cache = await readCache();
  const rest = cache.items.filter((r) => r.id !== item.id && r.id !== replaceId);
  await commit([item, ...rest], cache.live);
}

/**
 * Schedule a reminder on the phone right now, without the gateway. It is
 * queued and POSTed (idempotent by `clientId`) as soon as the gateway answers.
 */
export async function createReminderLocal({
  what,
  fireAt,
  entityId,
  clientId = newClientId(),
}: {
  what: string;
  fireAt: Date | string;
  entityId?: string;
  clientId?: string;
}): Promise<ReminderView> {
  await ensureNotificationPermission();
  const input: CreateReminderInput = {
    what,
    fireAt: typeof fireAt === 'string' ? fireAt : fireAt.toISOString(),
    entityId,
    followUp: true,
  };
  const item = localItemFor(clientId, input);
  await enqueue({ type: 'create', clientId, input, queuedAt: Date.now() });
  await upsert(item);
  void syncReminders();
  return item;
}

/**
 * Add a reminder: the gateway first (it resolves `when` and plans
 * follow-ups); if it can't be reached, on the phone.
 */
export async function addReminder({
  what,
  when,
  entityId,
}: {
  what: string;
  when?: string;
  entityId?: string;
}): Promise<ReminderResult> {
  const clientId = newClientId();
  try {
    const item = await postReminder({ what, when: when || undefined, entityId, clientId, followUp: true });
    await upsert(item);
    void syncReminders();
    return { ok: true, item };
  } catch (e) {
    if (!isUnreachable(e)) {
      return { ok: false, error: e instanceof APIError && e.detail ? e.detail : 'Your computer refused that reminder.' };
    }
    const item = await createReminderLocal({ what, fireAt: resolveWhen(when).date, entityId, clientId });
    return { ok: true, queued: true, item };
  }
}

/**
 * Capture one spoken or typed sentence ("send ABT the photo tomorrow at
 * 9am"): the gateway splits out the time; offline, the phone's own parser
 * finds it. The path behind `consistentworld://remind?text=…` (Siri
 * Shortcuts, the share sheet, widgets).
 */
export async function addReminderText(text: string): Promise<ReminderResult> {
  const sentence = text.trim();
  if (!sentence) return { ok: false, error: 'Nothing to remember.' };
  const clientId = newClientId();
  try {
    const item = await postReminder({ what: '', text: sentence, clientId, followUp: true });
    await upsert(item);
    void syncReminders();
    return { ok: true, item };
  } catch (e) {
    if (!isUnreachable(e)) {
      return { ok: false, error: e instanceof APIError && e.detail ? e.detail : 'Your computer refused that reminder.' };
    }
    const what = sentence.replace(/^\s*(?:please\s+)?(?:remind\s+me\s+(?:to\s+)?|don'?t\s+let\s+me\s+forget\s+(?:to\s+)?)/i, '') || sentence;
    const item = await createReminderLocal({ what, fireAt: resolveWhen(sentence).date, clientId });
    return { ok: true, queued: true, item };
  }
}

// ── Acting ─────────────────────────────────────────────────────────────

function actionInput(quick: QuickReminderAction): ReminderActionInput {
  switch (quick) {
    case 'done':
      return { action: 'done' };
    case 'drop':
      return { action: 'cancel' };
    case 'snooze':
      return { action: 'snooze', minutes: 60 };
    case 'tomorrow':
      return { action: 'reschedule', when: 'tomorrow 9am', fireAt: tomorrowMorning().toISOString() };
  }
}

/** What the action means on the phone, before (or without) the gateway. Null = gone. */
function applyLocally(item: ReminderView, input: ReminderActionInput): ReminderView | null {
  if (input.action === 'done' || input.action === 'cancel') return null;
  const fireAt =
    input.action === 'snooze'
      ? new Date(Date.now() + (input.minutes ?? 60) * 60_000).toISOString()
      : (input.fireAt ?? resolveWhen(input.when).date.toISOString());
  return {
    ...item,
    state: 'scheduled',
    fireAt,
    followUps: [new Date(Date.parse(fireAt) + FOLLOW_UP_MS).toISOString()],
    firstFiredAt: null,
  };
}

async function dismissDelivered(id: string): Promise<void> {
  if (Platform.OS === 'web') return;
  try {
    const presented = await Notifications.getPresentedNotificationsAsync();
    await Promise.all(
      presented
        .filter((n) => (n.request.content.data as Record<string, unknown> | null)?.taskId === id)
        .map((n) => Notifications.dismissNotificationAsync(n.request.identifier)),
    );
  } catch {
    /* best-effort */
  }
}

/**
 * Done / Snooze 1h / Tomorrow / Drop. Applied on the phone first, so the
 * pending nudges change immediately; then sent to the gateway, or queued if it
 * can't be reached.
 */
export async function actOnReminder(id: string, quick: QuickReminderAction): Promise<ReminderResult> {
  const input = actionInput(quick);
  const cache = await readCache();
  const current = cache.items.find((r) => r.id === id);
  const next = current ? applyLocally(current, input) : null;
  const items = cache.items.flatMap((r) => (r.id !== id ? [r] : next ? [next] : []));
  await commit(items, cache.live);
  void dismissDelivered(id);

  if (isLocalReminderId(id)) {
    // Never reached the gateway: rewrite (or drop) the queued create instead.
    const clientId = id.slice('local:'.length);
    const box = await readOutbox();
    await writeOutbox(
      box.flatMap((e) => {
        if (e.type !== 'create' || e.clientId !== clientId) return [e];
        return next?.fireAt ? [{ ...e, input: { ...e.input, fireAt: next.fireAt, when: undefined } }] : [];
      }),
    );
    void syncReminders();
    return { ok: true, queued: true, item: next ?? undefined };
  }

  try {
    const item = await postReminderAction(id, input);
    void syncReminders();
    return { ok: true, item };
  } catch (e) {
    if (isUnreachable(e)) {
      await enqueue({ type: 'action', id, input, queuedAt: Date.now() });
      return { ok: true, queued: true, item: next ?? undefined };
    }
    // The gateway answered no — put back what it actually has.
    void syncReminders();
    return {
      ok: false,
      error:
        e instanceof APIError && e.status === 404
          ? 'That reminder no longer exists.'
          : `Your computer refused that${e instanceof APIError ? ` (error ${e.status})` : ''}.`,
    };
  }
}

/**
 * "Do it for me": hand the reminder to the computer as an approval-gated
 * task. Needs the gateway (the work runs there), so nothing is queued.
 */
export async function delegateReminder(id: string): Promise<ReminderResult> {
  if (isLocalReminderId(id)) {
    return { ok: false, error: 'This one hasn’t reached your computer yet.' };
  }
  try {
    const item = await postReminderAction(id, { action: 'delegate' });
    void dismissDelivered(id);
    void syncReminders();
    return { ok: true, item };
  } catch (e) {
    return {
      ok: false,
      error: isUnreachable(e)
        ? 'Your computer isn’t reachable right now.'
        : `Your computer refused that${e instanceof APIError ? ` (error ${e.status})` : ''}.`,
    };
  }
}

// ── Notification category & responses ─────────────────────────────────

let categoryReady = false;

async function ensureReminderCategory(): Promise<void> {
  if (categoryReady || Platform.OS === 'web') return;
  try {
    await Notifications.setNotificationCategoryAsync(REMINDER_CATEGORY, [
      { identifier: ACTION_DONE, buttonTitle: 'Done', options: { opensAppToForeground: false } },
      { identifier: ACTION_SNOOZE, buttonTitle: 'Snooze 1h', options: { opensAppToForeground: false } },
      { identifier: ACTION_TOMORROW, buttonTitle: 'Tomorrow', options: { opensAppToForeground: false } },
    ]);
    categoryReady = true;
  } catch (e) {
    log('warn', 'system', `reminders: category setup failed: ${String(e)}`);
  }
}

function navigateWhenReady(path: string, attempt = 0): void {
  try {
    router.navigate(path);
  } catch {
    if (attempt < 5) setTimeout(() => navigateWhenReady(path, attempt + 1), 400);
  }
}

let handledResponseKey: string | null = null;

/**
 * A tap on a reminder: an action button acts without opening anything; the
 * body opens the reminders list (top of the Today tab). Returns false for
 * notifications that aren't about reminders.
 */
export async function handleReminderResponse(response: Notifications.NotificationResponse): Promise<boolean> {
  const request = response.notification.request;
  if (!isReminderNotification(request.content)) return false;
  const key = `${request.identifier}|${response.actionIdentifier}`;
  if (key === handledResponseKey) return true;
  handledResponseKey = key;
  void Notifications.clearLastNotificationResponseAsync().catch(() => undefined);

  const data = (request.content.data ?? {}) as Record<string, unknown>;
  const id = typeof data.taskId === 'string' && data.taskId ? data.taskId : undefined;
  const quick: QuickReminderAction | null =
    response.actionIdentifier === ACTION_DONE
      ? 'done'
      : response.actionIdentifier === ACTION_SNOOZE
        ? 'snooze'
        : response.actionIdentifier === ACTION_TOMORROW
          ? 'tomorrow'
          : null;

  if (!quick || !id) {
    navigateWhenReady('/review');
    return true;
  }
  log('info', 'system', `reminders: ${quick} from notification`);
  // A reminder the gateway pushed may not be in the phone's list yet.
  if (!(await readCache()).items.some((r) => r.id === id)) await syncReminders();
  await actOnReminder(id, quick);
  void Notifications.dismissNotificationAsync(request.identifier).catch(() => undefined);
  return true;
}

// ── Background work ────────────────────────────────────────────────────
//
// Loaded lazily: a dev client built before these modules were added has no
// native side for them, and must keep working (foreground sync only).

type TaskManagerModule = typeof import('expo-task-manager');
type BackgroundTaskModule = typeof import('expo-background-task');

function optional<T>(load: () => T): T | null {
  try {
    return load();
  } catch {
    return null;
  }
}

const TaskManager =
  Platform.OS === 'web' ? null : optional<TaskManagerModule>(() => require('expo-task-manager'));
const BackgroundTask =
  Platform.OS === 'web' ? null : optional<BackgroundTaskModule>(() => require('expo-background-task'));

// Task definitions must live at module scope so a headless launch finds them.
try {
  if (TaskManager && BackgroundTask && !TaskManager.isTaskDefined(BACKGROUND_TASK)) {
    TaskManager.defineTask(BACKGROUND_TASK, async () => {
      try {
        await syncReminders();
        return BackgroundTask.BackgroundTaskResult.Success;
      } catch {
        return BackgroundTask.BackgroundTaskResult.Failed;
      }
    });
  }
  // Android delivers action-button taps on a killed app only to this task.
  if (TaskManager && Platform.OS === 'android' && !TaskManager.isTaskDefined(NOTIFICATION_TASK)) {
    TaskManager.defineTask<Notifications.NotificationTaskPayload>(NOTIFICATION_TASK, async ({ data }) => {
      if (data && 'actionIdentifier' in data) await handleReminderResponse(data);
    });
  }
} catch (e) {
  log('warn', 'system', `reminders: background tasks unavailable: ${String(e)}`);
}

async function registerBackgroundWork(): Promise<void> {
  if (!TaskManager || !BackgroundTask) return;
  try {
    if (!(await TaskManager.isTaskRegisteredAsync(BACKGROUND_TASK))) {
      await BackgroundTask.registerTaskAsync(BACKGROUND_TASK, { minimumInterval: 30 });
    }
  } catch (e) {
    log('warn', 'system', `reminders: background sync not registered: ${String(e)}`);
  }
  if (Platform.OS === 'android') {
    await Notifications.registerTaskAsync(NOTIFICATION_TASK).catch(() => undefined);
  }
}

/**
 * Start the reminder engine: load the last plan, sync now, again on every
 * return to the foreground and once a minute while open, and register the
 * opportunistic background sync. Returns a stop function.
 */
export function startReminders(): () => void {
  if (Platform.OS === 'web') return () => undefined;

  void readCache().then((cache) => {
    if (!useReminders.getState().loaded) useReminders.setState({ items: cache.items, live: cache.live, loaded: true });
  });
  void readOutbox().then((box) => useReminders.setState({ pending: box.length }));
  void syncReminders();
  void registerBackgroundWork();

  let timer: ReturnType<typeof setInterval> | null = null;
  const startTimer = () => {
    if (!timer) timer = setInterval(() => void syncReminders(), FOREGROUND_SYNC_MS);
  };
  const stopTimer = () => {
    if (timer) clearInterval(timer);
    timer = null;
  };
  if (AppState.currentState === 'active') startTimer();
  const appStateSub = AppState.addEventListener('change', (next: AppStateStatus) => {
    if (next === 'active') {
      void syncReminders();
      startTimer();
    } else {
      stopTimer();
    }
  });

  const responseSub = Notifications.addNotificationResponseReceivedListener((r) => {
    void handleReminderResponse(r);
  });
  void Notifications.getLastNotificationResponseAsync()
    .then((r) => (r ? handleReminderResponse(r) : false))
    .catch(() => undefined);

  return () => {
    stopTimer();
    appStateSub.remove();
    responseSub.remove();
  };
}
