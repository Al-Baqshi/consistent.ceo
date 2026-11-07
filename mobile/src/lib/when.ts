/**
 * On-device "when" resolution for reminders made without the gateway (offline
 * capture, the local model). The gateway has its own, richer parser; this one
 * covers the phrases people actually type and otherwise falls back to
 * tomorrow 9:00 local — a reminder at a guessed time beats one never set.
 */

const DEFAULT_HOUR = 9;
const WEEKDAYS = ['sunday', 'monday', 'tuesday', 'wednesday', 'thursday', 'friday', 'saturday'];
const PARTS_OF_DAY: Record<string, number> = {
  morning: 9,
  noon: 12,
  midday: 12,
  afternoon: 14,
  evening: 18,
  tonight: 20,
  night: 20,
};

export interface ResolvedWhen {
  date: Date;
  /** False when nothing in the text was understood and the default was used. */
  understood: boolean;
}

function atTime(base: Date, hour: number, minute = 0, dayOffset = 0): Date {
  const d = new Date(base);
  d.setDate(d.getDate() + dayOffset);
  d.setHours(hour, minute, 0, 0);
  return d;
}

/** "9am", "9:30 pm", "17:00", "at 5" → [hour, minute]. */
function clockIn(text: string): [number, number] | null {
  for (const m of text.matchAll(/\b(at\s+)?(\d{1,2})(?::(\d{2}))?\s*(am|pm|a\.m\.|p\.m\.)?(?![\d:])/gi)) {
    const hasAt = !!m[1];
    const hasMinutes = m[3] != null;
    const mer = m[4]?.toLowerCase().replace(/\./g, '');
    // A bare number only counts as a time after "at" ("at 5"), not "in 5 days".
    if (!mer && !hasMinutes && !hasAt) continue;
    let hour = Number(m[2]);
    const minute = Number(m[3] ?? 0);
    if (hour > 23 || minute > 59 || (mer && hour > 12)) continue;
    if (mer === 'pm' && hour < 12) hour += 12;
    if (mer === 'am' && hour === 12) hour = 0;
    // "at 5" with no meridiem in waking hours means the afternoon.
    if (!mer && !hasMinutes && hour >= 1 && hour <= 7) hour += 12;
    return [hour, minute];
  }
  return null;
}

export function resolveWhen(raw: string | undefined | null, now: Date = new Date()): ResolvedWhen {
  const text = (raw ?? '').trim().toLowerCase();
  if (!text) return { date: atTime(now, DEFAULT_HOUR, 0, 1), understood: false };

  if (/\b(now|right away|asap)\b/.test(text)) {
    return { date: new Date(now.getTime() + 60_000), understood: true };
  }

  const rel = text.match(/\bin\s+(an?|half an|\d+(?:\.\d+)?)\s*(min(?:ute)?s?|m|h(?:ou)?rs?|h|hours?|days?|d|weeks?|w)\b/);
  if (rel) {
    const n = rel[1] === 'half an' ? 0.5 : rel[1] === 'a' || rel[1] === 'an' ? 1 : Number(rel[1]);
    const unit = rel[2];
    const mins = unit.startsWith('m')
      ? n
      : unit.startsWith('h')
        ? n * 60
        : unit.startsWith('d')
          ? n * 1440
          : n * 10080;
    return { date: new Date(now.getTime() + Math.max(1, Math.round(mins)) * 60_000), understood: true };
  }
  if (/\bin half an hour\b/.test(text)) {
    return { date: new Date(now.getTime() + 30 * 60_000), understood: true };
  }

  const clock = clockIn(text);
  const part = Object.keys(PARTS_OF_DAY).find((p) => new RegExp(`\\b${p}\\b`).test(text));
  const [hour, minute] = clock ?? (part ? [PARTS_OF_DAY[part], 0] : [DEFAULT_HOUR, 0]);

  if (/\btomorrow\b/.test(text)) return { date: atTime(now, hour, minute, 1), understood: true };
  if (/\bnext week\b/.test(text)) return { date: atTime(now, hour, minute, 7), understood: true };

  const weekday = WEEKDAYS.findIndex((d) => new RegExp(`\\b${d}\\b`).test(text));
  if (weekday >= 0) {
    let ahead = (weekday - now.getDay() + 7) % 7;
    if (ahead === 0 && atTime(now, hour, minute) <= now) ahead = 7;
    return { date: atTime(now, hour, minute, ahead), understood: true };
  }

  if (clock || part) {
    const today = atTime(now, hour, minute);
    // A time that already passed today means the next one.
    return { date: today > now ? today : atTime(now, hour, minute, 1), understood: true };
  }
  if (/\btoday\b/.test(text)) {
    return { date: new Date(now.getTime() + 2 * 3_600_000), understood: true };
  }
  return { date: atTime(now, DEFAULT_HOUR, 0, 1), understood: false };
}

/** Tomorrow at 9:00 local — what the "Tomorrow" action means. */
export function tomorrowMorning(now: Date = new Date()): Date {
  return atTime(now, DEFAULT_HOUR, 0, 1);
}

/** "in 5 min", "in 3 h", "tomorrow 9:00", "Mon 9:00", "20 min ago". */
export function friendlyTime(iso: string | null | undefined, now: Date = new Date()): string {
  if (!iso) return 'no time set';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return 'no time set';
  const diffMin = Math.round((d.getTime() - now.getTime()) / 60_000);
  const clock = d.toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' });
  if (diffMin < 0) {
    const ago = -diffMin;
    if (ago < 60) return `${ago} min ago`;
    if (ago < 24 * 60) return `${Math.round(ago / 60)} h ago`;
    return `${Math.round(ago / 1440)} d ago`;
  }
  if (diffMin < 1) return 'now';
  if (diffMin < 60) return `in ${diffMin} min`;
  const sameDay = d.toDateString() === now.toDateString();
  if (sameDay) return diffMin < 180 ? `in ${Math.round(diffMin / 60)} h` : `today ${clock}`;
  const tomorrow = new Date(now);
  tomorrow.setDate(tomorrow.getDate() + 1);
  if (d.toDateString() === tomorrow.toDateString()) return `tomorrow ${clock}`;
  if (diffMin < 7 * 1440) return `${d.toLocaleDateString(undefined, { weekday: 'short' })} ${clock}`;
  return `${d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' })} ${clock}`;
}
