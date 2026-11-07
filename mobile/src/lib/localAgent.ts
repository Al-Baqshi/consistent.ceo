import type { ReminderView } from '@/api/types';
import { log } from '@/lib/logger';

import { llamaContext } from './llama';
import { readyModelPath } from './localModel';
import { createReminderLocal } from './reminders';
import { friendlyTime, resolveWhen } from './when';

/**
 * The offline brain: the downloaded model answers chat while the computer is
 * unreachable. It can do exactly two things — set a reminder, save a note
 * (which is also reminded) — and both land on the phone's own alarm clock via
 * `createReminderLocal`, which hands them to the gateway once it's back.
 */

const TOOLS = [
  {
    type: 'function',
    function: {
      name: 'set_reminder',
      description: 'Remind the user about something at a time.',
      parameters: {
        type: 'object',
        properties: {
          what: { type: 'string', description: 'What to remind them about, short, in their words.' },
          when: { type: 'string', description: 'When, in plain words: "tomorrow 9am", "at 5pm", "tonight".' },
          in_minutes: { type: 'integer', description: 'Minutes from now, if they gave a duration.' },
        },
        required: ['what'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'create_note',
      description: 'Save a thought or note so it is followed up later.',
      parameters: {
        type: 'object',
        properties: {
          content: { type: 'string', description: 'The note, in their words.' },
          when: { type: 'string', description: 'When to bring it back up, if they said.' },
        },
        required: ['content'],
      },
    },
  },
];

function systemPrompt(now: Date): string {
  const day = now.toLocaleDateString(undefined, { weekday: 'long', month: 'long', day: 'numeric' });
  const time = now.toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' });
  return [
    "You are Consistent, the user's personal reminder, running on their phone.",
    'Turn anything they need to do into a reminder with set_reminder; save other thoughts with create_note.',
    'Confirm in one short sentence. Never claim you set something unless the tool ran.',
    `It is ${day}, ${time}.`,
  ].join(' ');
}

export interface LocalTurn {
  text: string;
  reminders: ReminderView[];
}

function parseArgs(raw: string): Record<string, unknown> {
  try {
    const parsed = JSON.parse(raw) as unknown;
    return parsed && typeof parsed === 'object' ? (parsed as Record<string, unknown>) : {};
  } catch {
    return {};
  }
}

const text = (v: unknown) => (typeof v === 'string' ? v.trim() : '');

async function runTool(name: string, args: Record<string, unknown>): Promise<ReminderView | null> {
  if (name === 'set_reminder') {
    const what = text(args.what);
    if (!what) return null;
    const mins = Number(args.in_minutes);
    const fireAt =
      Number.isFinite(mins) && mins > 0 ? new Date(Date.now() + mins * 60_000) : resolveWhen(text(args.when)).date;
    return createReminderLocal({ what, fireAt });
  }
  if (name === 'create_note') {
    const content = text(args.content);
    if (!content) return null;
    // Everything captured gets followed up; a note without a time comes back tomorrow morning.
    return createReminderLocal({ what: content, fireAt: resolveWhen(text(args.when ?? args.remind_when)).date });
  }
  return null;
}

/** One chat turn on the device. Throws when the model can't be loaded. */
export async function runLocalTurn(
  userText: string,
  history: { role: 'user' | 'assistant'; content: string }[] = [],
): Promise<LocalTurn> {
  const path = await readyModelPath();
  if (!path) throw new Error('The on-device model isn’t downloaded.');
  const ctx = await llamaContext(path);

  const result = await ctx.completion({
    messages: [
      { role: 'system', content: systemPrompt(new Date()) },
      ...history.slice(-6),
      { role: 'user', content: userText },
    ],
    tools: TOOLS,
    tool_choice: 'auto',
    jinja: true,
    enable_thinking: false,
    n_predict: 256,
    temperature: 0.3,
  });

  const reminders: ReminderView[] = [];
  for (const call of result.tool_calls ?? []) {
    try {
      const made = await runTool(call.function.name, parseArgs(call.function.arguments));
      if (made) reminders.push(made);
    } catch (e) {
      log('warn', 'system', `local agent: ${call.function.name} failed: ${String(e)}`);
    }
  }

  // The confirmation is written from what actually ran, not what the model
  // said it did — a small model will happily claim a reminder it never set.
  if (reminders.length > 0) {
    const lines = reminders.map((r) => `“${r.what}” ${friendlyTime(r.fireAt)}`);
    return {
      text: `Got it — I’ll remind you: ${lines.join('; ')}. I’ll keep nudging until it’s done.`,
      reminders,
    };
  }
  const reply = (result.content || result.text || '').trim();
  return { text: reply || 'Sorry, I didn’t catch that. What should I remind you about?', reminders };
}
