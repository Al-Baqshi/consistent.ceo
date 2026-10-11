/**
 * In-app log buffer — lets the operator inspect what the app is doing
 * (gateway calls, worker heartbeats, approvals) from the Logs screen.
 * Ring buffer + subscribers; no persistence (privacy: logs stay in memory).
 */

export type LogLevel = 'info' | 'warn' | 'error';
export type LogTag = 'api' | 'system' | 'worker' | 'user';

export interface LogEntry {
  id: number;
  at: string; // ISO
  level: LogLevel;
  tag: LogTag;
  text: string;
}

const MAX_ENTRIES = 300;

let seq = 0;
let entries: LogEntry[] = [];
const listeners = new Set<() => void>();

export function log(level: LogLevel, tag: LogTag, text: string): void {
  entries = [
    { id: ++seq, at: new Date().toISOString(), level, tag, text },
    ...entries,
  ].slice(0, MAX_ENTRIES);
  listeners.forEach((fn) => fn());
}

export function getLogs(): LogEntry[] {
  return entries;
}

export function clearLogs(): void {
  entries = [];
  listeners.forEach((fn) => fn());
}

export function subscribeLogs(fn: () => void): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

// Seed a few lines so the screen never opens empty.
log('info', 'system', 'Consistent mobile started');
log('info', 'system', 'Theme: following system appearance');
