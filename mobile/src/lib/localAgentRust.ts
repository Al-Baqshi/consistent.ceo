import type { ReminderView } from '@/api/types';
import { log } from '@/lib/logger';

import { createReminderLocal } from './reminders';
import {
  parseNoteViaRust,
  parseReminderViaRust,
  isRustAvailable,
  logRust,
  rustVersion,
} from './rustBridge';
import { friendlyTime } from './when';

/**
 * The offline brain: the Rust core parses natural language into reminders and
 * notes, which land on the phone's own alarm clock via `createReminderLocal`,
 * then sync to the gateway once it's back.
 *
 * Unlike the llama.rn version (mobile/src/lib/localAgent.ts), this needs no
 * model download — the Rust parser runs entirely on-device, end-to-end
 * independent of the gateway.
 */

export interface LocalTurn {
  text: string;
  reminders: ReminderView[];
}

/** Below this, the reminder parser found no time in the sentence and guessed. */
const TIMED_CONFIDENCE = 0.6;

const NOT_CAUGHT: LocalTurn = {
  text: "Sorry, I didn't catch that. What should I remind you about?",
  reminders: [],
};

/**
 * One chat turn on the device, powered by the Rust parser.
 *
 * Every turn becomes one reminder. Its time always comes from the reminder
 * parse: both parsers scan the same words, so a note's `when` is only that
 * time phrase as written. A sentence with no time ("remember to buy milk")
 * gets the parser's guess, tomorrow 9:00, and the reply says it was a guess.
 * It used to ring the moment it was saved.
 */
export async function runLocalTurnRust(userText: string): Promise<LocalTurn> {
  const parsed = await parseReminderViaRust(userText);
  if (!parsed) return NOT_CAUGHT;

  const timed = parsed.confidence > TIMED_CONFIDENCE;
  // Untimed text reads as a note, so take the note's wording: it drops the
  // "note" / "remember to" lead-in the reminder parser keeps.
  const what = timed
    ? parsed.what
    : (await parseNoteViaRust(userText))?.content.trim() || parsed.what;

  // "in 30 minutes" comes back as inMinutes with no fireAt.
  const fireAt =
    parsed.fireAt ??
    (parsed.inMinutes !== null
      ? new Date(Date.now() + parsed.inMinutes * 60_000).toISOString()
      : null);
  if (!what.trim() || !fireAt) return NOT_CAUGHT;

  const made = await createReminderLocal({ what, fireAt, entityId: undefined });
  const guess = timed ? '' : " You didn't say when, so I picked tomorrow morning.";
  return {
    text: `Got it — I'll remind you: “${made.what}” ${friendlyTime(made.fireAt)}.${guess}`,
    reminders: [made],
  };
}

/**
 * Check whether the Rust core answers, and log which parser this build uses
 * to the Logs screen and the JS console. Called once at app start.
 */
export async function checkRustAvailability(): Promise<boolean> {
  const version = isRustAvailable() ? await rustVersion() : null;
  const line = version
    ? `rust bridge available: ${version}`
    : isRustAvailable()
      ? 'rust bridge linked but not answering — using TS fallback parser'
      : 'rust bridge not linked — using TS fallback parser';
  log(version ? 'info' : 'warn', 'system', line);
  logRust(version ? 'info' : 'warn', line);
  return version !== null;
}
