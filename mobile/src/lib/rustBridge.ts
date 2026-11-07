//! TypeScript bridge to the Rust local agent core (`mobile/rust/`).
//!
//! On iOS the Rust core is a static XCFramework vendored by the local Expo
//! module `modules/cw-rust-bridge`, whose Swift `CWRustBridge` module calls the
//! C ABI (`mobile/rust/include/cw_mobile.h`). There is no Android module yet.
//! This file declares the module interface and falls back to the TS port of
//! the parser (`localParse.ts`) whenever the module is missing (Android, web,
//! a binary built without it) or a call fails or returns an unexpected shape.

import { requireOptionalNativeModule } from 'expo';

import { parseNote, parseReminder, type ParsedNote, type ParsedReminder } from './localParse';

export type { ParsedNote, ParsedReminder };

/**
 * The native module (`modules/cw-rust-bridge/ios/CWRustBridgeModule.swift`).
 * Each parse call resolves to the Rust core's JSON string.
 */
interface CWRustBridgeModule {
  version(): Promise<string>;
  parseReminder(text: string): Promise<string>;
  parseNote(text: string): Promise<string>;
}

const bridge = requireOptionalNativeModule<CWRustBridgeModule>('CWRustBridge');

/** Call the Rust bridge, or return null when it isn't linked or the call fails. */
async function callBridge<T>(
  fn: (bridge: CWRustBridgeModule) => Promise<T | null>,
): Promise<T | null> {
  if (!bridge) return null;
  try {
    return await fn(bridge);
  } catch (e) {
    logRust('warn', `bridge call failed: ${String(e)}`);
    return null;
  }
}

export function logRust(level: 'info' | 'warn', msg: string): void {
  // eslint-disable-next-line no-console
  console[level](`[rust-bridge] ${msg}`);
}

const isStringOrNull = (v: unknown): boolean => v === null || typeof v === 'string';

function isParsedReminder(v: unknown): v is ParsedReminder {
  const r = v as Partial<ParsedReminder> | null;
  return (
    !!r &&
    typeof r.what === 'string' &&
    typeof r.confidence === 'number' &&
    isStringOrNull(r.fireAt) &&
    (r.inMinutes === null || typeof r.inMinutes === 'number')
  );
}

function isParsedNote(v: unknown): v is ParsedNote {
  const n = v as Partial<ParsedNote> | null;
  return !!n && typeof n.content === 'string' && isStringOrNull(n.when);
}

/**
 * Parse a Rust JSON reply, or null if it isn't the shape TS expects. A shape
 * drift then shows up as a warning and the TS parser, not as `undefined`
 * fields (the Rust core once sent `fire_at`, which read as `fireAt: undefined`).
 */
function decode<T>(json: string, valid: (v: unknown) => v is T, what: string): T | null {
  const value: unknown = JSON.parse(json);
  if (valid(value)) return value;
  logRust('warn', `unexpected ${what} shape from Rust: ${json}`);
  return null;
}

/** Parse natural language into a reminder via Rust core, fallback to TS parser. */
export async function parseReminderViaRust(text: string): Promise<ParsedReminder | null> {
  const result = await callBridge(async (b) =>
    decode(await b.parseReminder(text), isParsedReminder, 'reminder'),
  );
  return result ?? parseReminder(text);
}

/** Parse natural language into a note via Rust core, fallback to TS parser. */
export async function parseNoteViaRust(text: string): Promise<ParsedNote | null> {
  const result = await callBridge(async (b) => decode(await b.parseNote(text), isParsedNote, 'note'));
  return result ?? parseNote(text);
}

/** Whether the Rust native module is linked in this build. */
export function isRustAvailable(): boolean {
  return bridge !== null;
}

/** Rust version string, for diagnostics. */
export async function rustVersion(): Promise<string | null> {
  return callBridge((b) => b.version());
}
