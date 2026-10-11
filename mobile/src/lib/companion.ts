import * as Notifications from 'expo-notifications';
import Constants from 'expo-constants';
import { router } from 'expo-router';
import * as SecureStore from 'expo-secure-store';
import { AppState, type AppStateStatus, Platform } from 'react-native';

import { http } from '@/api/client';
import type { GatewayFeedEvent } from '@/api/types';
import { log } from '@/lib/logger';
import { getBoolPref, PREF_PUSH_ON } from '@/lib/prefs';

import {
  ensureNotificationPermission,
  feedEventIdOf,
  feedEventShown,
  markFeedEventShown,
  notify,
  presentedFeedEventIds,
} from './notifications';

/**
 * The companion loop: makes the gateway's operator feed *reach* the phone.
 *
 * Two delivery paths, and the default is deliberately the quiet one:
 *
 * 1. **Foreground poll** (always on). While the app is in the foreground we
 *    poll `/api/operator-feed` and raise a local notification for anything
 *    new and interruption-worthy. Nothing leaves the operator's own machines.
 * 2. **Push** (opt-in). If the operator turns push on, we register an Expo
 *    push token with the gateway so the phone can be reached with the app
 *    closed. That routes notification titles/bodies through Expo's servers,
 *    which is why it is off until asked for — and why the gateway also gates
 *    it behind `[notifications] mobile_push`.
 *
 * The notifiable-kind list mirrors `is_notifiable` in
 * `crates/gateway/src/notify.rs` — keep the two identical, and the labels in
 * step with `notification_title` there. Keeping it duplicated (rather than
 * deriving it from the server) means the poll path stays correct even against
 * an older gateway that doesn't know the newer kinds.
 *
 * With push on, both paths can see the same event. Each banner carries the
 * event's `feedEventId` (the gateway's push data already does), and
 * `notifications.ts` shows any given id once.
 */

const INSTALL_ID_KEY = 'cw.installId';
const CURSOR_KEY = 'cw.feedCursor';
const POLL_MS = 15_000;
/** How many feed rows to inspect per tick. Generous enough to cover a gap. */
const FEED_LOOKBACK = 25;
/** Don't dump a backlog as N banners after a long time away. */
const MAX_NOTIFICATIONS_PER_TICK = 3;

// `OperatorFeedKind` serialises snake_case, so this is the spelling that
// actually arrives: `agent_run_completed`, never `AgentRunCompleted`. Listing
// the Rust variant names meant no kind ever matched and the phone raised zero
// banners — silently, because "no notifications" is indistinguishable from
// "nothing happened".
const NOTIFIABLE_KINDS = new Set([
  'scheduled_task_fired',
  'approval_needed',
  'approval_request_created',
  'mission_user_input_needed',
  'heartbeat_result',
  'agent_run_completed',
  'agent_run_failed',
  'mission_stalled',
  'task_stuck',
  'task_blocked',
  'mission_completed',
  'task_overdue',
  'daily_digest',
  'entity_workspace_needed',
  // Each of these parks work on a person; feed-only meant nobody was told.
  'verification_needs_review',
  'mission_review_due',
  'connection_check_failed',
  'mission_report_ready',
]);

const KIND_LABELS: Record<string, string> = {
  scheduled_task_fired: 'Reminder',
  approval_needed: 'Needs your approval',
  approval_request_created: 'Needs your approval',
  mission_user_input_needed: 'Needs your input',
  heartbeat_result: 'Heartbeat',
  agent_run_completed: 'Run finished',
  agent_run_failed: 'Run failed',
  mission_stalled: 'Mission stalled',
  task_stuck: 'Task stuck',
  task_blocked: 'Task blocked',
  mission_completed: 'Mission completed',
  task_overdue: 'Overdue',
  daily_digest: 'Your day',
  entity_workspace_needed: 'Where should this live?',
  verification_needs_review: 'Needs review',
  mission_review_due: 'Review due',
  connection_check_failed: 'Connection failed',
  mission_report_ready: 'Report ready',
};

// SecureStore is unavailable on web — same in-memory shim shape the API
// client uses.
const memStore: Record<string, string> = {};

async function readKey(key: string): Promise<string | null> {
  if (Platform.OS === 'web') return memStore[key] ?? null;
  try {
    return await SecureStore.getItemAsync(key);
  } catch {
    return null;
  }
}

async function writeKey(key: string, value: string): Promise<void> {
  if (Platform.OS === 'web') {
    memStore[key] = value;
    return;
  }
  try {
    await SecureStore.setItemAsync(key, value);
  } catch {
    /* best-effort */
  }
}

/**
 * Stable per-install id. This is the idempotency key for device registration:
 * reopening the app re-registers the same row instead of accumulating one per
 * launch, and it preserves the server-side delivery cursor.
 */
export async function getInstallId(): Promise<string> {
  const existing = await readKey(INSTALL_ID_KEY);
  if (existing) return existing;
  const fresh = `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
  await writeKey(INSTALL_ID_KEY, fresh);
  return fresh;
}

function projectId(): string | undefined {
  return (
    Constants.expoConfig?.extra?.eas?.projectId ??
    (Constants.easConfig as { projectId?: string } | undefined)?.projectId
  );
}

/** Why push isn't active even when registration itself may have succeeded. */
export type PushUnavailableReason =
  | 'gateway-unreachable'
  | 'permission-denied'
  | 'push-unavailable';

/**
 * Ask Expo for a push token. A null token is a normal state, not an error —
 * the poll path still works — but the reason is reported so the UI can say
 * *why* the phone can only be reached while the app is open.
 */
async function fetchPushToken(askPermission: boolean): Promise<{
  token: string | null;
  reason?: PushUnavailableReason;
}> {
  if (Platform.OS === 'web') return { token: null, reason: 'push-unavailable' };
  // A launch-time re-registration must never raise a permission prompt; only
  // the operator turning push on (Settings) asks.
  const granted = askPermission
    ? await ensureNotificationPermission()
    : await Notifications.getPermissionsAsync()
        .then((p) => p.granted)
        .catch(() => false);
  if (!granted) return { token: null, reason: 'permission-denied' };
  const id = projectId();
  if (!id) {
    log('warn', 'system', 'companion: no EAS projectId; push unavailable, polling only');
    return { token: null, reason: 'push-unavailable' };
  }
  try {
    const res = await Notifications.getExpoPushTokenAsync({ projectId: id });
    return { token: res.data };
  } catch (e) {
    log('warn', 'system', `companion: expo push token failed: ${String(e)}`);
    return { token: null, reason: 'push-unavailable' };
  }
}

export interface RegisterDeviceOptions {
  /** Request and register an Expo push token so the app can be reached closed. */
  push?: boolean;
  label?: string;
  /**
   * May this call show the notification permission prompt? True for the
   * Settings toggle (the operator just asked for push); false for the silent
   * re-registration at launch.
   */
  askPermission?: boolean;
}

export interface RegisterDeviceResult {
  /** The gateway accepted the registration. */
  ok: boolean;
  /** The Expo push token that was registered, if any. */
  pushToken: string | null;
  /** Set when push was requested but isn't active (or the call failed). */
  reason?: PushUnavailableReason;
}

/**
 * Register this device with the gateway. Safe to call repeatedly.
 * Passing `push: false` (the default) registers a poll-only device — the row
 * exists so the gateway knows about the phone, but carries no push token.
 *
 * `push` also travels explicitly in the body, so a gateway that understands it
 * can tell "keep my push token" (`push: true` with no token this time, e.g. a
 * transient Expo failure) from "stop pushing to me" (`push: false`). Today's
 * gateway clears the stored token whenever `pushToken` is absent — see
 * `register_device` in `crates/gateway/src/control_api/devices_api.rs`.
 */
export async function registerDevice(
  { push = false, label, askPermission = true }: RegisterDeviceOptions = {},
): Promise<RegisterDeviceResult> {
  const tokenResult = push ? await fetchPushToken(askPermission) : { token: null };
  try {
    const installId = await getInstallId();
    await http.post('/api/devices/register', {
      platform: Platform.OS,
      installId,
      label: label ?? Platform.OS,
      pushToken: tokenResult.token ?? undefined,
      push,
    });
    log('info', 'system', `companion: device registered (push=${tokenResult.token ? 'on' : 'off'})`);
    return {
      ok: true,
      pushToken: tokenResult.token,
      reason: push && !tokenResult.token ? tokenResult.reason : undefined,
    };
  } catch (e) {
    // Gateway unreachable is the common case on a phone away from the desk.
    log('warn', 'system', `companion: device registration failed: ${String(e)}`);
    return { ok: false, pushToken: null, reason: 'gateway-unreachable' };
  }
}

function formatTitle(event: GatewayFeedEvent): string {
  const label = KIND_LABELS[event.kind] ?? 'Consistent CEO';
  return `${label} · ${event.title}`;
}

/**
 * Ids a tapped notification needs to land somewhere actionable — the same
 * shape the gateway attaches to its remote pushes, so local poll-path banners
 * and real pushes route identically.
 */
function eventLinkData(event: GatewayFeedEvent): Record<string, string> {
  const data: Record<string, string> = {};
  if (event.approvalId) data.approvalId = event.approvalId;
  if (event.taskId) data.taskId = event.taskId;
  if (event.entityId) data.entityId = event.entityId;
  return data;
}

/** Navigate, retrying briefly — a cold-start tap can beat the navigator mount. */
function navigateWhenReady(path: string, attempt = 0): void {
  try {
    router.navigate(path);
  } catch {
    if (attempt < 5) setTimeout(() => navigateWhenReady(path, attempt + 1), 400);
  }
}

/**
 * Route a tapped notification to where the operator can act:
 * - `approvalId` → `/control`, the approvals inbox: every action approval and
 *   sign-in request in one list. The id is not used to deep-link a single card
 *   on purpose — by the time a push is tapped the queue has usually moved, and
 *   landing on the whole queue is more useful than landing on one row of it.
 * - `taskId` → that task's own screen: status, the agent's report, what
 *   changed on disk and the files the run left behind. A banner about one
 *   task used to land on a list of many, leaving the operator to find it.
 * - `entityId` alone → the owning entity's workspace.
 */
function routeNotificationData(data: Record<string, unknown>): void {
  const approvalId = typeof data.approvalId === 'string' && data.approvalId ? data.approvalId : undefined;
  const taskId = typeof data.taskId === 'string' && data.taskId ? data.taskId : undefined;
  const entityId = typeof data.entityId === 'string' && data.entityId ? data.entityId : undefined;

  let target: string | null = null;
  if (approvalId) target = '/control';
  else if (taskId) target = `/task/${encodeURIComponent(taskId)}`;
  else if (entityId) target = `/entity/${encodeURIComponent(entityId)}`;
  if (!target) return;

  log('info', 'system', `companion: notification tap → ${target}`);
  navigateWhenReady(target);
}

// The same response can surface twice (live listener + the cold-start
// `getLastNotificationResponseAsync` sweep) — route each tap exactly once.
let handledResponseId: string | null = null;

function handleNotificationResponse(response: Notifications.NotificationResponse): void {
  const id = response.notification.request.identifier;
  if (id && id === handledResponseId) return;
  handledResponseId = id;
  const data = (response.notification.request.content.data ?? {}) as Record<string, unknown>;
  // A push the operator already tapped must not come back as a poll banner.
  const eventId = feedEventIdOf(data);
  if (eventId) markFeedEventShown(eventId);
  routeNotificationData(data);
}

function formatBody(event: GatewayFeedEvent): string {
  const raw = (event.detail ?? event.title).trim();
  return raw.length > 180 ? `${raw.slice(0, 179).trimEnd()}…` : raw;
}

/**
 * Events newer than the cursor, oldest-first so notifications arrive in the
 * order things happened. The feed comes back newest-first; everything before
 * the cursor id is new. An unknown cursor (row aged out of the ring) yields the
 * whole window, which the per-tick cap then trims.
 */
export function newEventsSince(
  feed: GatewayFeedEvent[],
  cursor: string | null,
): GatewayFeedEvent[] {
  if (!cursor) return [];
  const idx = feed.findIndex((e) => e.id === cursor);
  const fresh = idx === -1 ? feed.slice() : feed.slice(0, idx);
  return fresh.reverse();
}

async function tick(): Promise<void> {
  let feed: GatewayFeedEvent[];
  try {
    feed = await http.list<GatewayFeedEvent>(`/api/operator-feed?limit=${FEED_LOOKBACK}`);
  } catch {
    return; // Gateway unreachable — try again next tick.
  }
  if (feed.length === 0) return;

  const cursor = await readKey(CURSOR_KEY);
  // First run just anchors the cursor: the operator doesn't want a banner for
  // everything that happened before they installed the app.
  if (!cursor) {
    await writeKey(CURSOR_KEY, feed[0].id);
    return;
  }

  const fresh = newEventsSince(feed, cursor).filter((e) => NOTIFIABLE_KINDS.has(e.kind));
  // Skip anything a push already delivered: shown while foregrounded (the
  // handler recorded it) or still sitting in Notification Center from while
  // the app was in the background.
  const presented = fresh.length > 0 ? await presentedFeedEventIds() : new Set<string>();
  const unseen = fresh.filter((e) => !feedEventShown(e.id) && !presented.has(e.id));
  const toShow = unseen.slice(-MAX_NOTIFICATIONS_PER_TICK);
  for (const event of toShow) {
    await notify(formatTitle(event), formatBody(event), {
      seconds: 1,
      data: { ...eventLinkData(event), feedEventId: event.id },
    });
  }
  await writeKey(CURSOR_KEY, feed[0].id);
}

let pollTimer: ReturnType<typeof setInterval> | null = null;
let appStateSub: { remove: () => void } | null = null;

function startPolling(): void {
  if (pollTimer) return;
  void tick();
  pollTimer = setInterval(() => {
    void tick();
  }, POLL_MS);
}

function stopPolling(): void {
  if (pollTimer) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
}

/**
 * Start the companion loop. Polls only while the app is foregrounded — a
 * background timer would be killed by the OS anyway, and draining the battery
 * to poll a gateway that may be on another network helps nobody. Reaching a
 * backgrounded phone is what the push path is for.
 *
 * Returns a stop function; idempotent if called twice.
 */
export function startCompanion(): () => void {
  // Re-register with the operator's saved push choice. Registering with the
  // default (`push: false`) here switched push off on every cold start: the
  // gateway dropped the token while Settings still showed the toggle on.
  void getBoolPref(PREF_PUSH_ON).then((push) => registerDevice({ push, askPermission: false }));

  if (AppState.currentState === 'active') startPolling();

  appStateSub = AppState.addEventListener('change', (next: AppStateStatus) => {
    if (next === 'active') {
      startPolling();
    } else {
      stopPolling();
    }
  });

  // Deep links: a tapped notification lands the operator where they can act.
  // The listener covers taps while the app is alive; the last-response sweep
  // covers a push that launched the app cold. Best-effort on web.
  let responseSub: { remove: () => void } | null = null;
  if (Platform.OS !== 'web') {
    try {
      responseSub = Notifications.addNotificationResponseReceivedListener(handleNotificationResponse);
      void Notifications.getLastNotificationResponseAsync()
        .then((response) => {
          if (response) handleNotificationResponse(response);
        })
        .catch(() => undefined);
    } catch {
      /* notifications unavailable — polling still works */
    }
  }

  return () => {
    stopPolling();
    appStateSub?.remove();
    appStateSub = null;
    responseSub?.remove();
    responseSub = null;
  };
}
