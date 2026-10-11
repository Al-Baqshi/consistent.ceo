import * as Notifications from 'expo-notifications';
import { Platform } from 'react-native';

/**
 * Local-notification helpers for heartbeats, approvals, and reminders.
 * Sound is on by default (banner + default system sound). All best-effort —
 * the app works fully without notification permission.
 *
 * One operator-feed event can reach the phone twice: as a gateway push
 * (`notify.rs`, data carries `feedEventId`) and as a banner the foreground
 * feed poll raises itself (`companion.ts`, same key). Every banner that
 * carries a `feedEventId` is shown once; the second arrival is silenced here.
 */

/** Recently shown feed-event ids, oldest first. Bounded — it is a de-dupe window, not a log. */
const SHOWN_LIMIT = 200;
const shownFeedEvents = new Set<string>();

/** The operator-feed event a notification is about, if it says. */
export function feedEventIdOf(data: unknown): string | undefined {
  const id = (data as Record<string, unknown> | null | undefined)?.feedEventId;
  return typeof id === 'string' && id.length > 0 ? id : undefined;
}

export function markFeedEventShown(id: string): void {
  shownFeedEvents.delete(id);
  shownFeedEvents.add(id);
  while (shownFeedEvents.size > SHOWN_LIMIT) {
    const oldest = shownFeedEvents.values().next().value;
    if (oldest === undefined) break;
    shownFeedEvents.delete(oldest);
  }
}

export function feedEventShown(id: string): boolean {
  return shownFeedEvents.has(id);
}

/**
 * Feed-event ids already sitting in Notification Center — typically pushes
 * delivered while the app was in the background, which never pass through the
 * foreground handler below.
 */
export async function presentedFeedEventIds(): Promise<Set<string>> {
  if (Platform.OS === 'web') return new Set();
  try {
    const presented = await Notifications.getPresentedNotificationsAsync();
    const ids = presented
      .map((n) => feedEventIdOf(n.request.content.data))
      .filter((id): id is string => id !== undefined);
    return new Set(ids);
  } catch {
    return new Set();
  }
}

const SHOW = {
  shouldShowBanner: true,
  shouldShowList: true,
  shouldPlaySound: true,
  shouldSetBadge: false,
} as const;

const SILENT = {
  shouldShowBanner: false,
  shouldShowList: false,
  shouldPlaySound: false,
  shouldSetBadge: false,
} as const;

Notifications.setNotificationHandler({
  handleNotification: async (notification) => {
    const id = feedEventIdOf(notification.request.content.data);
    if (id && feedEventShown(id)) return SILENT;
    if (id) markFeedEventShown(id);
    return SHOW;
  },
});

export async function ensureNotificationPermission(): Promise<boolean> {
  if (Platform.OS === 'web') return false;
  try {
    const settings = await Notifications.getPermissionsAsync();
    if (settings.granted) return true;
    const req = await Notifications.requestPermissionsAsync();
    return req.granted;
  } catch {
    return false;
  }
}

interface NotifyOptions {
  seconds?: number;
  sound?: boolean;
  /**
   * Attached to the notification payload. The companion tap listener routes on
   * ids in here (`approvalId`, `taskId`, `entityId`) — same contract as the
   * gateway's remote pushes.
   */
  data?: Record<string, unknown>;
}

/** Schedule a local notification (with default sound unless disabled). */
export async function notify(
  title: string,
  body: string,
  { seconds = 2, sound = true, data }: NotifyOptions = {},
): Promise<string | null> {
  try {
    const granted = await ensureNotificationPermission();
    if (!granted) return null;
    return await Notifications.scheduleNotificationAsync({
      content: { title, body, sound: sound ? 'default' : undefined, data },
      trigger: {
        type: Notifications.SchedulableTriggerInputTypes.TIME_INTERVAL,
        seconds: Math.max(1, seconds),
      },
    });
  } catch {
    return null;
  }
}

/** Heartbeat/reminder from a worker — kept as a named alias for callers. */
export async function scheduleHeartbeat(
  title: string,
  body: string,
  seconds = 5,
): Promise<string | null> {
  return notify(title, body, { seconds });
}

/** One-tap check that banners AND sound work on this device. */
export async function sendTestNotification(): Promise<string | null> {
  return notify(
    'Consistent CEO',
    'Notifications and sound are working ✓',
    { seconds: 3 },
  );
}

export async function cancelAllHeartbeats(): Promise<void> {
  try {
    await Notifications.cancelAllScheduledNotificationsAsync();
  } catch {
    /* no-op */
  }
}
