// The TS parser against the Rust core's own cases (`mobile/rust/src/lib.rs`
// tests): same sentences, same `now`, same answers. Run with `npm test`; Node
// strips the TypeScript types from localParse.ts itself.
import assert from 'node:assert/strict';
import { test } from 'node:test';

import { parseNote, parseReminder } from './localParse.ts';

// The Rust tests' clock: 15:00 on 5 Nov 2026 in Auckland (UTC+13).
process.env.TZ = 'Pacific/Auckland';
const now = new Date('2026-11-05T02:00:00.000Z');

const reminder = (text) => parseReminder(text, now);
const fire = (text) => reminder(text).fireAt;

test('runs in the Rust tests time zone', () => {
  assert.equal(now.getTimezoneOffset(), -13 * 60);
  assert.equal(now.getHours(), 15);
});

test('parses tomorrow 9am', () => {
  assert.deepEqual(reminder('remind me to call Mom tomorrow 9am'), {
    what: 'call Mom',
    fireAt: '2026-11-05T20:00:00.000Z',
    inMinutes: null,
    confidence: 0.9,
  });
});

test('parses in minutes', () => {
  assert.deepEqual(reminder('remind me in 30 minutes'), {
    what: 'reminder',
    fireAt: null,
    inMinutes: 30,
    confidence: 0.95,
  });
});

test('parses a note with a when', () => {
  assert.deepEqual(parseNote('buy milk tomorrow'), { content: 'buy milk', when: 'tomorrow' });
});

test('tomorrow resolves in the device zone', () => {
  // 09:00 on the 6th at UTC+13 is 20:00 on the 5th in UTC.
  assert.equal(fire('call Mom tomorrow'), '2026-11-05T20:00:00.000Z');
  assert.equal(fire('Remind me to email Sarah tomorrow at 3:30 pm'), '2026-11-06T02:30:00.000Z');
});

test('keeps case and drops the time words', () => {
  assert.equal(reminder('Remind me to email Sarah tomorrow, at 3pm.').what, 'email Sarah');
  assert.equal(reminder('remind me tomorrow 9am to call Mom').what, 'call Mom');
  assert.equal(reminder('remind me to Pay Rent by tomorrow').what, 'Pay Rent');
});

test('tonight defaults to eight and reads bare hours as evening', () => {
  assert.equal(fire('take the bins out tonight'), '2026-11-05T07:00:00.000Z'); // 20:00 local
  assert.equal(fire('call Dad tonight at 9'), '2026-11-05T08:00:00.000Z'); // 21:00 local
});

test('a time without a day is its next occurrence', () => {
  // 17:30 local is still ahead today.
  assert.equal(fire('call Bob at 5:30pm'), '2026-11-05T04:30:00.000Z');
  // 9am has passed, so it is tomorrow's.
  assert.equal(fire('call Bob at 9am'), '2026-11-05T20:00:00.000Z');
  // A bare "at 7" at 15:00 is 19:00, not 07:00 tomorrow.
  assert.equal(fire('call Bob at 7'), '2026-11-05T06:00:00.000Z');
  assert.equal(fire('stand-up at 09:30'), '2026-11-05T20:30:00.000Z');
  assert.equal(fire('lunch at noon'), '2026-11-05T23:00:00.000Z');
});

test('relative delays', () => {
  const p = reminder('remind me in 30 minutes to stretch');
  assert.deepEqual([p.what, p.inMinutes, p.fireAt], ['stretch', 30, null]);
  assert.equal(reminder('check the oven in 2h').inMinutes, 120);
  assert.equal(reminder('call back in an hour').inMinutes, 60);
});

test('plain numbers and places are not times', () => {
  const p = reminder('buy 3 apples');
  assert.deepEqual([p.what, p.confidence, p.fireAt], ['buy 3 apples', 0.5, '2026-11-05T20:00:00.000Z']);
  assert.deepEqual(parseNote('put the keys in the drawer'), {
    content: 'put the keys in the drawer',
    when: null,
  });
  assert.equal(parseNote('meet at the cafe').when, null);
});

test('invalid times do not throw', () => {
  for (const text of [
    'call at 13pm',
    'call at 25:00',
    'call at 9:75',
    'call at 99',
    'in 9999999 minutes',
    'İstanbul at 9:',
    '',
  ]) {
    const p = reminder(text);
    assert.ok(p.fireAt !== null || p.inMinutes !== null, text);
  }
});

test('a note keeps its time phrase as written', () => {
  assert.deepEqual(parseNote('Remember to buy milk tomorrow'), { content: 'buy milk', when: 'tomorrow' });
  assert.deepEqual(parseNote('note send the Deck to Ana at 5pm'), {
    content: 'send the Deck to Ana',
    when: 'at 5pm',
  });
  assert.deepEqual(parseNote('ideas for the launch'), { content: 'ideas for the launch', when: null });
});

test('replies have the shape rustBridge.ts validates', () => {
  assert.deepEqual(Object.keys(reminder('call Mom tomorrow')).sort(), [
    'confidence',
    'fireAt',
    'inMinutes',
    'what',
  ]);
  assert.deepEqual(Object.keys(parseNote('buy milk')).sort(), ['content', 'when']);
});
