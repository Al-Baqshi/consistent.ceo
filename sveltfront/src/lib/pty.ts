// Typed bridge to the desktop PTY backend.
//
// Mirrors the Tauri commands and events implemented in
// `desktop/src-tauri/src/pty.rs`. Every call goes through the dependency-free
// `$lib/tauri` bridge, so in a plain browser (web / mobile) `ptyAvailable()`
// is false and nothing here should be called.
//
// Contract:
//   invoke  pty_spawn   { args: PtySpawnOptions }          -> session id
//   invoke  pty_write   { id, data }
//   invoke  pty_resize  { id, cols, rows }
//   invoke  pty_kill    { id }                              (idempotent)
//   invoke  open_signin_url { url }                         (allowlisted hosts only)
//   event   pty-output  { id, data: number[] }              raw bytes, may split UTF-8
//   event   pty-exit    { id, code: number | null }         exactly once per session

import { invoke, isTauri, listen } from '$lib/tauri';

export interface PtySpawnOptions {
  /** argv to execute (through the user's login shell so PATH is right). */
  command?: string[];
  /** Shell string run via `$SHELL -l -c`. Ignored when `command` is set. */
  script?: string;
  cwd?: string;
  cols: number;
  rows: number;
}

interface PtyOutputEvent {
  id: string;
  /** JSON-serialised `Vec<u8>`; tolerate a typed array in case the bridge changes. */
  data: number[] | Uint8Array;
}

interface PtyExitEvent {
  id: string;
  code: number | null;
}

/** True inside the Tauri desktop webview, where the PTY backend exists. */
export function ptyAvailable(): boolean {
  return isTauri();
}

/**
 * Spawn a session and resolve to its id. Rejects with an error string when the
 * backend refuses (e.g. the concurrent-session cap of 8).
 */
export function ptySpawn(opts: PtySpawnOptions): Promise<string> {
  return invoke<string>('pty_spawn', { args: opts });
}

export function ptyWrite(id: string, data: string): Promise<void> {
  return invoke<void>('pty_write', { id, data });
}

export function ptyResize(id: string, cols: number, rows: number): Promise<void> {
  return invoke<void>('pty_resize', { id, cols, rows });
}

export function ptyKill(id: string): Promise<void> {
  return invoke<void>('pty_kill', { id });
}

/**
 * Subscribe to raw output for every session; filter by id in the callback.
 * Bytes may end mid-UTF-8-sequence — feed them straight to a streaming decoder
 * (xterm's `write` handles this itself).
 */
export function onPtyOutput(cb: (id: string, bytes: Uint8Array) => void): Promise<() => void> {
  return listen<PtyOutputEvent>('pty-output', (payload) => {
    const bytes =
      payload.data instanceof Uint8Array ? payload.data : Uint8Array.from(payload.data);
    cb(payload.id, bytes);
  });
}

export function onPtyExit(cb: (id: string, code: number | null) => void): Promise<() => void> {
  return listen<PtyExitEvent>('pty-exit', (payload) => {
    cb(payload.id, payload.code);
  });
}

/**
 * Open an https sign-in URL in the system browser. The backend only accepts an
 * allowlist of OpenAI / Anthropic sign-in hosts and rejects anything else.
 */
export function openSignInUrl(url: string): Promise<void> {
  return invoke<void>('open_signin_url', { url });
}
