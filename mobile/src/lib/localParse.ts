//! The phone's sentence parser in TypeScript: a line-for-line port of the Rust
//! core's (`mobile/rust/src/lib.rs`). `rustBridge.ts` uses it wherever the
//! Rust module is missing (Android, web, an old build) or a call fails, so
//! both must turn the same sentence into the same reminder. Change one, change
//! the other, and keep `localParse.test.mjs` in step with the Rust tests.
//!
//! No imports, on purpose: `npm test` runs this file under plain Node.

/** Parsed reminder from natural language. Mirrors Rust `ParsedReminder`. */
export interface ParsedReminder {
  what: string;
  fireAt: string | null; // ISO 8601 UTC
  inMinutes: number | null;
  confidence: number;
}

/** Parsed note from natural language. Mirrors Rust `ParsedNote`. */
export interface ParsedNote {
  content: string;
  when: string | null;
}

const REMINDER_PREFIXES = [
  'remind me to ',
  'remind me ',
  'set a reminder to ',
  'set a reminder for ',
  'set reminder ',
  'reminder to ',
  'reminder ',
];

const NOTE_PREFIXES = ['save note ', 'note ', 'remember to ', 'remember ', 'save '];

/**
 * Parse a reminder relative to `now`, in the device's time zone. A sentence
 * with no time gets tomorrow 9:00 at confidence 0.5.
 */
export function parseReminder(input: string, now: Date = new Date()): ParsedReminder {
  const body = stripPrefixCi(input.trim(), REMINDER_PREFIXES);
  const words = tokenize(body);
  const phrase = scanTimePhrase(words);
  const what = rest(phrase, words) || 'reminder';

  if (phrase.inMinutes !== null) {
    return { what, fireAt: null, inMinutes: phrase.inMinutes, confidence: 0.95 };
  }
  const { day, clock } = phrase;
  const [fire, confidence] =
    day === null && clock === null
      ? [atDay(now, 'tomorrow', null), 0.5]
      : day === 'today' && clock === null
        ? [atDay(now, 'today', null), 0.7]
        : [atDay(now, day ?? 'today', clock), 0.9];
  return { what, fireAt: fire.toISOString(), inMinutes: null, confidence };
}

/**
 * Parse a note, splitting off its time phrase. Without one, `when` is null
 * and the whole text is the content.
 */
export function parseNote(input: string): ParsedNote {
  const body = stripPrefixCi(input.trim(), NOTE_PREFIXES);
  const words = tokenize(body);
  const phrase = scanTimePhrase(words);
  const content = rest(phrase, words);
  const when = taken(phrase, words);
  if (!content || !when) return { content: tidy(body), when: null };
  return { content, when };
}

/** Case-insensitive prefix strip that keeps the remainder's original case. */
function stripPrefixCi(text: string, prefixes: readonly string[]): string {
  for (const prefix of prefixes) {
    if (text.slice(0, prefix.length).toLowerCase() === prefix) {
      return text.slice(prefix.length).trimStart();
    }
  }
  return text;
}

interface Word {
  /** As written, so the reminder keeps the user's case. */
  raw: string;
  /** Lowercased with surrounding punctuation removed, for matching. */
  key: string;
}

function tokenize(text: string): Word[] {
  return text
    .split(/\s+/)
    .filter((raw) => raw !== '')
    .map((raw) => ({ raw, key: raw.replace(/^[,.;!?()"]+|[,.;!?()"]+$/g, '').toLowerCase() }));
}

type Day = 'today' | 'tonight' | 'tomorrow';

function dayFromKey(key: string): Day | null {
  if (key === 'today' || key === 'tonight' || key === 'tomorrow') return key;
  return key === 'tmrw' ? 'tomorrow' : null;
}

interface Clock {
  hour: number;
  minute: number;
  /** A bare 1–12 hour ("at 9", "9:30") that could be morning or evening. */
  ambiguous: boolean;
}

const at = (hour: number, minute: number): Clock => ({ hour, minute, ambiguous: false });

/** The time words found in a sentence, and which words they used up. */
interface TimePhrase {
  day: Day | null;
  clock: Clock | null;
  inMinutes: number | null;
  used: boolean[];
}

function take(phrase: TimePhrase, start: number, len: number): void {
  phrase.used.fill(true, start, start + len);
}

/** The words that were not part of the time phrase. */
function rest(phrase: TimePhrase, words: Word[]): string {
  let text = tidy(words.filter((_, i) => !phrase.used[i]).map((w) => w.raw).join(' '));
  // "remind me tomorrow to call Mom" leaves "to call Mom".
  text = stripPrefixCi(text, ['to ']);
  // "call Mom by tomorrow" leaves a dangling "by".
  if (phrase.used.some(Boolean)) {
    for (const tail of [' at', ' on', ' by']) {
      if (text.length > tail.length && text.slice(-tail.length).toLowerCase() === tail) {
        text = text.slice(0, -tail.length);
      }
    }
  }
  return tidy(text);
}

/** The time phrase itself, as written. */
function taken(phrase: TimePhrase, words: Word[]): string {
  return tidy(words.filter((_, i) => phrase.used[i]).map((w) => w.raw).join(' '));
}

function tidy(text: string): string {
  return text.replace(/^[\s,;:-]+|[\s,;:-]+$/g, '');
}

function scanTimePhrase(words: Word[]): TimePhrase {
  const phrase: TimePhrase = {
    day: null,
    clock: null,
    inMinutes: null,
    used: words.map(() => false),
  };
  let i = 0;
  while (i < words.length) {
    if (phrase.inMinutes === null && words[i].key === 'in') {
      const relative = parseRelative(words.slice(i + 1));
      if (relative) {
        phrase.inMinutes = relative[0];
        take(phrase, i, 1 + relative[1]);
        i += 1 + relative[1];
        continue;
      }
    }
    if (phrase.day === null) {
      const day = dayFromKey(words[i].key);
      if (day) {
        phrase.day = day;
        take(phrase, i, 1);
        i += 1;
        continue;
      }
    }
    if (phrase.clock === null) {
      const clock = parseClockPhrase(words.slice(i));
      if (clock) {
        phrase.clock = clock[0];
        take(phrase, i, clock[1]);
        i += clock[1];
        continue;
      }
    }
    i += 1;
  }
  return phrase;
}

/** Length of the leading run of ASCII digits. */
function digitsEnd(key: string): number {
  return key.match(/^[0-9]*/)![0].length;
}

/**
 * "30 minutes", "an hour", "2h", "2 days" after an "in", as [minutes, words
 * used]. A number that ends the sentence ("remind me in 30") counts as minutes.
 */
function parseRelative(words: Word[]): [number, number] | null {
  const first = words[0]?.key;
  if (first === undefined) return null;
  const end = digitsEnd(first);
  let count: number;
  let unit: string;
  let len: number;
  if (first === 'a' || first === 'an') {
    const next = words[1]?.key;
    if (next === undefined) return null;
    [count, unit, len] = [1, next, 2];
  } else if (end === 0 || end > 6) {
    return null;
  } else if (end < first.length) {
    [count, unit, len] = [Number(first.slice(0, end)), first.slice(end), 1];
  } else if (words[1]) {
    [count, unit, len] = [Number(first), words[1].key, 2];
  } else {
    return [Number(first), 1];
  }
  const perUnit = minutesPer(unit);
  return perUnit === null ? null : [count * perUnit, len];
}

function minutesPer(unit: string): number | null {
  if (['m', 'min', 'mins', 'minute', 'minutes'].includes(unit)) return 1;
  if (['h', 'hr', 'hrs', 'hour', 'hours'].includes(unit)) return 60;
  if (['d', 'day', 'days'].includes(unit)) return 24 * 60;
  return null;
}

/**
 * A clock time with its lead-in, as [clock, words used]: "9am", "at 9",
 * "at 5:30 pm", "21:00", "noon", "this evening", "in the morning".
 */
function parseClockPhrase(words: Word[]): [Clock, number] | null {
  const key = (i: number): string | undefined => words[i]?.key;
  const first = key(0);
  if (first === undefined) return null;
  if (first === 'at') {
    const clock = parseClock(words.slice(1), true);
    return clock && [clock[0], clock[1] + 1];
  }
  if (first === 'this') {
    const next = key(1);
    const clock = next === undefined ? null : partOfDay(next);
    return clock && [clock, 2];
  }
  if (first === 'in' && key(1) === 'the') {
    const part = key(2);
    const clock = part === undefined ? null : partOfDay(part);
    return clock && [clock, 3];
  }
  return parseClock(words, false);
}

/**
 * A clock time at the start of `words`. A bare hour ("9") only counts after
 * "at", so "buy 3 apples" keeps its 3.
 */
function parseClock(words: Word[], afterAt: boolean): [Clock, number] | null {
  const first = words[0]?.key;
  if (first === undefined) return null;
  const part = partOfDay(first);
  if (part) return [part, 1];
  const end = digitsEnd(first);
  if (end === 0 || end > 2) return null;
  const hour = Number(first.slice(0, end));
  let suffix = first.slice(end);
  let minute: number | null = null;
  if (suffix.startsWith(':')) {
    const mm = suffix.slice(1, 3);
    if (!/^[0-9]{2}$/.test(mm)) return null;
    minute = Number(mm);
    suffix = suffix.slice(3);
  }
  const minuteValue = minute ?? 0;
  if (minuteValue > 59) return null;

  let pm: boolean | null = null;
  let len = 1;
  if (suffix !== '') {
    pm = meridiem(suffix);
    if (pm === null) return null;
  } else {
    const next = words[1] ? meridiem(words[1].key) : null;
    if (next !== null) [pm, len] = [next, 2];
  }

  if (pm !== null) {
    if (hour < 1 || hour > 12) return null;
    const h = hour === 12 ? (pm ? 12 : 0) : pm ? hour + 12 : hour;
    return [at(h, minuteValue), len];
  }
  if (hour > 23) return null;
  // "21:00" and "09:30" are clearly clock times; so is "9" after "at".
  if (minute !== null || afterAt) {
    const zeroPadded = end === 2 && first.startsWith('0');
    const ambiguous = hour >= 1 && hour <= 12 && !zeroPadded;
    return [{ hour, minute: minuteValue, ambiguous }, 1];
  }
  return null;
}

function meridiem(key: string): boolean | null {
  if (key === 'am' || key === 'a.m') return false;
  if (key === 'pm' || key === 'p.m') return true;
  return null;
}

function partOfDay(key: string): Clock | null {
  switch (key) {
    case 'morning':
      return at(9, 0);
    case 'noon':
    case 'midday':
      return at(12, 0);
    case 'afternoon':
      return at(15, 0);
    case 'evening':
      return at(18, 0);
    case 'midnight':
      return at(0, 0);
    default:
      return null;
  }
}

/** The fire time for a day word and an optional clock, never in the past. */
function atDay(now: Date, day: Day, clock: Clock | null): Date {
  switch (day) {
    case 'tomorrow': {
      const c = clock ?? at(9, 0);
      return atClock(now, 1, c.hour, c.minute);
    }
    case 'tonight': {
      const c = clock ?? at(20, 0);
      // "tonight at 9" means 21:00.
      const hour = c.ambiguous && c.hour < 12 ? c.hour + 12 : c.hour;
      return nextAt(now, [[hour, c.minute]]);
    }
    case 'today': {
      if (!clock) {
        // "today" with no time: this evening, or soon if evening has passed.
        const evening = atClock(now, 0, 18, 0);
        return evening > now ? evening : new Date(now.getTime() + 3_600_000);
      }
      // A bare "at 7" is whichever of 7:00 and 19:00 comes first.
      const h = clock.hour;
      return clock.ambiguous
        ? nextAt(now, [
            [h % 12, clock.minute],
            [(h % 12) + 12, clock.minute],
          ])
        : nextAt(now, [[h, clock.minute]]);
    }
  }
}

/** The first of `times` still ahead of `now` today, else the first one tomorrow. */
function nextAt(now: Date, times: [number, number][]): Date {
  for (const [h, m] of times) {
    const t = atClock(now, 0, h, m);
    if (t > now) return t;
  }
  return atClock(now, 1, times[0][0], times[0][1]);
}

/** `hour:minute` local time, `dayOffset` days after `now`'s date. */
function atClock(now: Date, dayOffset: number, hour: number, minute: number): Date {
  const d = new Date(now);
  d.setDate(d.getDate() + dayOffset);
  d.setHours(hour, minute, 0, 0);
  return d;
}
