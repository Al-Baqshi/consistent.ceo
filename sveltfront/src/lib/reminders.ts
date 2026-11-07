// Client-side helpers for the reminders surface: pulling a time phrase out of a
// typed sentence (the gateway only parses a bare phrase like "tomorrow 9am"),
// and friendly next-time labels.

/** Fired on `window` when anything should re-read `/api/reminders`. */
export const REMINDERS_CHANGED_EVENT = 'consistent-world:reminders-changed';
/** Fired on `window` to scroll to and expand the home Reminders panel. */
export const OPEN_REMINDERS_EVENT = 'consistent-world:open-reminders';
export const REMINDERS_HASH = '#reminders';

/** Used when someone asks for a reminder without saying when. */
export const DEFAULT_WHEN = 'tomorrow 9am';

const UNIT = '(s|secs?|seconds?|m|mins?|minutes?|h|hrs?|hours?|d|days?|w|weeks?)';
const QTY = '(a|an|one|two|three|\\d+(?:\\.\\d+)?)';
const CLOCK = '(\\d{1,2}(?::\\d{2})?\\s*(?:am|pm)?)';

const PART_OF_DAY: Record<string, string> = {
  morning: '9am',
  afternoon: '3pm',
  evening: '6pm',
  night: '8pm'
};

type Match = { index: number; length: number; when: string };

function find(text: string): Match | null {
  const candidates: Match[] = [];
  const push = (re: RegExp, norm: (m: RegExpExecArray) => string | null) => {
    const m = re.exec(text);
    if (!m) return;
    const when = norm(m);
    if (when) candidates.push({ index: m.index, length: m[0].length, when });
  };

  push(/\b(?:in|after)\s+half\s+an?\s+hour\b/i, () => 'in 30 minutes');
  push(new RegExp(`\\b(?:in|after)\\s+${QTY}\\s*${UNIT}\\b`, 'i'), (m) => `in ${m[1].toLowerCase()} ${m[2].toLowerCase()}`);
  push(new RegExp(`\\b${QTY}\\s*${UNIT}\\s+from\\s+now\\b`, 'i'), (m) => `in ${m[1].toLowerCase()} ${m[2].toLowerCase()}`);
  push(/\btomorrow\s+(morning|afternoon|evening|night)\b/i, (m) => `tomorrow ${PART_OF_DAY[m[1].toLowerCase()]}`);
  push(new RegExp(`\\btomorrow(?:\\s+(?:at\\s+)?${CLOCK})?(?![\\w:])`, 'i'), (m) =>
    m[1] ? `tomorrow ${m[1].replace(/\s+/g, '').toLowerCase()}` : DEFAULT_WHEN
  );
  push(/\b(tonight|this evening|this afternoon|this morning|in the morning|at noon|noon|midday|at midnight|midnight|end of (?:the )?day|eod)\b/i, (m) =>
    m[1].toLowerCase().replace('end of the day', 'end of day')
  );
  push(new RegExp(`\\b(?:today\\s+)?at\\s+${CLOCK}(?![\\w:])(?:\\s+today\\b)?`, 'i'), (m) => {
    const clock = m[1].replace(/\s+/g, '').toLowerCase();
    // "at 5" alone is ambiguous with "at 5 Main St"; only take it with a meridiem or a colon.
    return /am|pm|:/.test(clock) ? `at ${clock}` : null;
  });
  push(/\b(\d{1,2}(?::\d{2})?\s*(?:am|pm))\b/i, (m) => m[1].replace(/\s+/g, '').toLowerCase());

  if (candidates.length === 0) return null;
  // Longest wins: "tomorrow at 9am" beats the bare "9am" inside it.
  candidates.sort((a, b) => b.length - a.length || a.index - b.index);
  return candidates[0];
}

/**
 * Split "remind me to call mum tomorrow at 9am" into
 * `{ what: "call mum", when: "tomorrow 9am" }`. `when` is null when no time
 * phrase the gateway understands was found.
 */
export function splitReminderText(raw: string): { what: string; when: string | null } {
  const text = raw.trim();
  const match = find(text);
  let what = match ? `${text.slice(0, match.index)} ${text.slice(match.index + match.length)}` : text;
  what = what
    .replace(/^\s*(?:please\s+)?(?:remind\s+me\s+(?:to\s+|about\s+|that\s+)?|reminder\s*[:-]?\s*)/i, '')
    .replace(/\s{2,}/g, ' ')
    .replace(/^[\s,;:-]+|[\s,;:-]+$/g, '')
    .trim();
  return { what: what || text, when: match?.when ?? null };
}

function sameDay(a: Date, b: Date): boolean {
  return a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
}

/** "in 20 min", "today 3:00 PM", "tomorrow 9:00 AM", "Fri 9:00 AM", "2h ago". */
export function friendlyTime(iso: string | null, now = new Date()): string {
  if (!iso) return 'No time set';
  const at = new Date(iso);
  if (Number.isNaN(at.getTime())) return '';
  const diffMin = Math.round((at.getTime() - now.getTime()) / 60_000);
  if (diffMin < 0) {
    const ago = -diffMin;
    if (ago < 60) return `${ago || 1} min ago`;
    if (ago < 24 * 60) return `${Math.round(ago / 60)}h ago`;
    return `${Math.round(ago / (24 * 60))}d ago`;
  }
  if (diffMin < 60) return diffMin <= 1 ? 'now' : `in ${diffMin} min`;
  const time = at.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
  if (sameDay(at, now)) return `today ${time}`;
  const tomorrow = new Date(now);
  tomorrow.setDate(now.getDate() + 1);
  if (sameDay(at, tomorrow)) return `tomorrow ${time}`;
  if (diffMin < 6 * 24 * 60) return `${at.toLocaleDateString([], { weekday: 'short' })} ${time}`;
  return `${at.toLocaleDateString([], { month: 'short', day: 'numeric' })} ${time}`;
}
