import { fetch as streamingFetch } from 'expo/fetch';
import * as SecureStore from 'expo-secure-store';
import { Platform } from 'react-native';

import { log } from '@/lib/logger';

import type { Paginated } from './types';

/**
 * Gateway HTTP client — real-first, honest about failures.
 *
 * Every high-level method in `endpoints.ts` tries the live Rust gateway
 * (default http://127.0.0.1:18800) with a short timeout. What happens on
 * failure depends on which URL is configured: the built-in default means
 * nobody has paired a real gateway yet (demo mode), so the mock fixtures
 * answer and the app stays explorable. A real configured URL means fixtures
 * would impersonate the operator's actual workspace — those failures
 * propagate, screens show their empty/error states, and the tab banner says
 * the computer is unreachable. Screens say "sample data" only in demo mode.
 */

export const DEFAULT_GATEWAY_URL = 'http://127.0.0.1:18800';

const GATEWAY_URL_KEY = 'cw.gatewayUrl';
const TOKEN_KEY = 'cw.authToken';

const REQUEST_TIMEOUT_MS = 2500;
const UNREACHABLE_BACKOFF_MS = 20_000;

export const gatewayState = {
  /** null = unknown, true = last real call succeeded, false = unreachable. */
  reachable: null as boolean | null,
  lastFailureAt: 0,
  /** Last time a real round-trip completed successfully. 0 = never. */
  lastOkAt: 0,
  /** When the current outage began; 0 while reachable (or unknown). */
  unreachableSince: 0,
  /**
   * True while the latest `tryGateway` read in demo mode fell back to the
   * sample fixtures; false once a real answer arrives. Lets screens without a
   * per-payload `live` flag (Entities, Calendar) show the sample-data notice.
   */
  servedSample: false,
};

/** Record a real round-trip that reached the gateway. */
export function recordGatewayOk(): void {
  gatewayState.reachable = true;
  gatewayState.lastOkAt = Date.now();
  gatewayState.unreachableSince = 0;
}

/**
 * Record a failure to reach the gateway at all — network-level, not an HTTP
 * status. The first failure of an outage anchors `unreachableSince` so the
 * banner can say how long the computer has been away, not just that it is.
 */
export function recordGatewayFailure(): void {
  gatewayState.reachable = false;
  gatewayState.lastFailureAt = Date.now();
  if (gatewayState.unreachableSince === 0) gatewayState.unreachableSince = Date.now();
}

// SecureStore is unavailable on web — fall back to an in-memory shim.
const memStore: Record<string, string> = {};
const store = {
  async get(key: string): Promise<string | null> {
    if (Platform.OS === 'web') return memStore[key] ?? null;
    return SecureStore.getItemAsync(key);
  },
  async set(key: string, value: string): Promise<void> {
    if (Platform.OS === 'web') {
      memStore[key] = value;
      return;
    }
    await SecureStore.setItemAsync(key, value);
  },
  async remove(key: string): Promise<void> {
    if (Platform.OS === 'web') {
      delete memStore[key];
      return;
    }
    await SecureStore.deleteItemAsync(key);
  },
};

export async function getGatewayUrl(): Promise<string> {
  return (await store.get(GATEWAY_URL_KEY)) ?? DEFAULT_GATEWAY_URL;
}

export async function setGatewayUrl(url: string): Promise<void> {
  await store.set(GATEWAY_URL_KEY, url);
  // A new URL deserves a fresh reachability check.
  gatewayState.reachable = null;
  gatewayState.lastFailureAt = 0;
  gatewayState.unreachableSince = 0;
}

export async function getAuthToken(): Promise<string | null> {
  return store.get(TOKEN_KEY);
}

export async function setAuthToken(token: string): Promise<void> {
  await store.set(TOKEN_KEY, token);
}

export async function clearAuthToken(): Promise<void> {
  await store.remove(TOKEN_KEY);
}

// ── Paired-device identity (gateway `pairing.rs`) ───────────────────────────
//
// A phone paired by scanning the computer's code holds its own Ed25519 key.
// When its session lapses (14 days unused, 30 days old, or a gateway that
// forgot it), it signs back in with that key instead of asking for a password.
// The identity is bound to the gateway URL it was paired with: a key is never
// offered to a different computer.

const DEVICE_IDENTITY_KEY = 'cw.pairedDevice';

export type DeviceIdentity = {
  gatewayUrl: string;
  deviceId: string;
  /** Ed25519 secret, base64url. Never sent anywhere. */
  secretKey: string;
};

export async function getDeviceIdentity(): Promise<DeviceIdentity | null> {
  const raw = await store.get(DEVICE_IDENTITY_KEY);
  if (!raw) return null;
  try {
    return JSON.parse(raw) as DeviceIdentity;
  } catch {
    return null;
  }
}

export async function setDeviceIdentity(identity: DeviceIdentity): Promise<void> {
  await store.set(DEVICE_IDENTITY_KEY, JSON.stringify(identity));
}

export async function clearDeviceIdentity(): Promise<void> {
  await store.remove(DEVICE_IDENTITY_KEY);
}

let deviceLoginInFlight: Promise<boolean> | null = null;

/**
 * Sign back in with this phone's pairing key. Resolves true when a fresh
 * session was stored. Concurrent callers share one attempt, so a screen that
 * fires ten requests on a lapsed session signs in once, not ten times.
 */
export function deviceLogin(): Promise<boolean> {
  if (!deviceLoginInFlight) {
    deviceLoginInFlight = attemptDeviceLogin().finally(() => {
      deviceLoginInFlight = null;
    });
  }
  return deviceLoginInFlight;
}

async function attemptDeviceLogin(): Promise<boolean> {
  const identity = await getDeviceIdentity();
  const base = await getGatewayUrl();
  if (!identity || identity.gatewayUrl !== base) return false;
  const { loginMessage, sign } = await import('@/lib/pairingKey');
  const post = async (path: string, body: unknown) =>
    fetch(`${base}${path}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
  try {
    const ch = await post('/api/auth/device/challenge', { deviceId: identity.deviceId });
    if (!ch.ok) return false;
    const { challengeId, challenge } = (await ch.json()) as { challengeId: string; challenge: string };
    const res = await post('/api/auth/device/login', {
      challengeId,
      signature: sign(identity.secretKey, loginMessage(challengeId, challenge)),
    });
    if (res.status === 401) {
      // The computer removed this phone. The key is useless now; drop it so
      // the app asks to pair again instead of retrying forever.
      await clearDeviceIdentity();
      return false;
    }
    if (!res.ok) return false;
    const { token } = (await res.json()) as { token: string };
    await setAuthToken(token);
    return true;
  } catch {
    return false;
  }
}

export class APIError extends Error {
  constructor(
    message: string,
    readonly status: number,
    /**
     * The gateway's own explanation, when its error body carried one (a JSON
     * `error`/`message` field or a short plain-text line). Most handlers
     * answer with a bare status, so this is usually undefined.
     */
    readonly detail?: string,
  ) {
    super(message);
    this.name = 'APIError';
  }
}

/** A short, human-readable reason from an error response body, if any. */
async function errorDetail(res: { text(): Promise<string> }): Promise<string | undefined> {
  try {
    const raw = (await res.text()).trim();
    if (!raw) return undefined;
    try {
      const json = JSON.parse(raw) as Record<string, unknown>;
      const msg = json.error ?? json.message ?? json.detail;
      return typeof msg === 'string' && msg.trim() ? msg.trim().slice(0, 240) : undefined;
    } catch {
      // Plain text is fine; an HTML error page (a proxy, a relay) is not.
      return raw.startsWith('<') ? undefined : raw.slice(0, 240);
    }
  } catch {
    return undefined;
  }
}

/**
 * Reachability bookkeeping for an HTTP error status. A status IS an answer —
 * the gateway is reachable, it just refused — except 503: on the relay path
 * that answer comes from the *relay* saying the gateway is not connected
 * ("your computer is offline"), which is exactly the outage the banner exists
 * to report.
 */
function noteHttpError(status: number): void {
  if (status === 503) {
    recordGatewayFailure();
  } else {
    gatewayState.reachable = true;
    gatewayState.unreachableSince = 0;
  }
}

async function request<T>(
  method: string,
  path: string,
  body?: unknown,
  timeoutMs: number = REQUEST_TIMEOUT_MS,
): Promise<T> {
  try {
    return await requestOnce<T>(method, path, body, timeoutMs);
  } catch (e) {
    // A lapsed session on a paired phone: sign back in with the device key
    // and retry once. Credential endpoints are left alone, since a 401 there
    // is an answer about a password or a code, not about a stale session.
    if (
      e instanceof APIError &&
      e.status === 401 &&
      !isCredentialPath(path) &&
      (await deviceLogin())
    ) {
      return requestOnce<T>(method, path, body, timeoutMs);
    }
    throw e;
  }
}

const CREDENTIAL_PATHS = [
  '/api/auth/login',
  '/api/auth/register',
  '/api/auth/recover',
  '/api/auth/check-username',
  '/api/auth/approval/',
  '/api/auth/passkey/',
  '/api/auth/device/',
  '/api/auth/account',
  '/api/auth/change-password',
  '/api/pair/',
];

function isCredentialPath(path: string): boolean {
  return CREDENTIAL_PATHS.some((p) => path.startsWith(p));
}

async function requestOnce<T>(
  method: string,
  path: string,
  body?: unknown,
  timeoutMs: number = REQUEST_TIMEOUT_MS,
): Promise<T> {
  const base = await getGatewayUrl();
  const token = await getAuthToken();
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const res = await fetch(`${base}${path}`, {
      method,
      headers: {
        'Content-Type': 'application/json',
        ...(token ? { Authorization: `Bearer ${token}` } : {}),
      },
      body: body != null ? JSON.stringify(body) : undefined,
      signal: controller.signal,
    });
    if (!res.ok) {
      // Only network-level failures count as an outage — see `noteHttpError`.
      noteHttpError(res.status);
      throw new APIError(`${method} ${path} → ${res.status}`, res.status, await errorDetail(res));
    }
    recordGatewayOk();
    // 204 No Content (and any other empty body) is a success, not a parse
    // error — the auth approve/deny routes answer exactly that way.
    if (res.status === 204) return undefined as T;
    const text = await res.text();
    return (text.length > 0 ? JSON.parse(text) : undefined) as T;
  } catch (e) {
    if (!(e instanceof APIError)) recordGatewayFailure();
    throw e;
  } finally {
    clearTimeout(timer);
  }
}

/** Round-trip milliseconds of `fn`, measured on this device. Never invented. */
export async function timed<T>(fn: () => Promise<T>): Promise<{ value: T; ms: number }> {
  const started = Date.now();
  const value = await fn();
  return { value, ms: Date.now() - started };
}

export const http = {
  get: <T>(path: string) => request<T>('GET', path),
  post: <T>(path: string, body?: unknown) => request<T>('POST', path, body),
  /** Auth endpoints hash passwords server-side (Argon2) — give them time. */
  postAuth: <T>(path: string, body?: unknown) => request<T>('POST', path, body, 20_000),
  patch: <T>(path: string, body?: unknown) => request<T>('PATCH', path, body),
  /** A DELETE that carries a JSON body and re-checks a password (Argon2). */
  delAuth: (path: string, body?: unknown) => request<unknown>('DELETE', path, body, 20_000),
  del: (path: string) => request<unknown>('DELETE', path).catch((e) => {
    // 204 No Content has an empty body — res.json() rejects; that's success.
    if (e instanceof SyntaxError) return undefined;
    throw e;
  }),
  /** Accepts either a bare array or the gateway's `Paginated<T>` envelope. */
  async list<T>(path: string): Promise<T[]> {
    const raw = await request<Paginated<T> | T[]>('GET', path);
    return Array.isArray(raw) ? raw : raw.items;
  },
};

/**
 * POST a JSON body and yield each line of an `application/x-ndjson` response as
 * it arrives.
 *
 * React Native's global `fetch` is an XHR shim with no `response.body`, so this
 * uses `expo/fetch` — the Expo SDK 52+ WinterCG fetch, which exposes a real
 * `ReadableStream` (and installs `TextDecoder` alongside it). That is what makes
 * `/api/capture/chat/stream` genuinely streamable on the phone.
 *
 * The timeout guards the whole stream, not a single response, so it is far more
 * generous than `REQUEST_TIMEOUT_MS`: a tool-using chat turn can think for a
 * while before the first token lands.
 */
export async function* streamNdjson(
  path: string,
  body: unknown,
  timeoutMs = 90_000,
): AsyncGenerator<Record<string, unknown>> {
  const base = await getGatewayUrl();
  const token = await getAuthToken();
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const res = await streamingFetch(`${base}${path}`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        Accept: 'application/x-ndjson',
        ...(token ? { Authorization: `Bearer ${token}` } : {}),
      },
      body: JSON.stringify(body ?? {}),
      signal: controller.signal,
    });
    if (!res.ok) {
      noteHttpError(res.status);
      // Refresh a lapsed paired-phone session now, so the person's retry works.
      if (res.status === 401) void deviceLogin();
      throw new APIError(`POST ${path} → ${res.status}`, res.status, await errorDetail(res));
    }
    if (!res.body) throw new Error(`POST ${path} → no response stream`);
    recordGatewayOk();

    const reader = res.body.getReader();
    try {
      const decoder = new TextDecoder();
      let buffer = '';
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });
        let newline = buffer.indexOf('\n');
        while (newline >= 0) {
          const line = buffer.slice(0, newline).trim();
          buffer = buffer.slice(newline + 1);
          if (line.length > 0) yield JSON.parse(line) as Record<string, unknown>;
          newline = buffer.indexOf('\n');
        }
      }
      const tail = buffer.trim();
      if (tail.length > 0) yield JSON.parse(tail) as Record<string, unknown>;
    } finally {
      // A caller that stops iterating early (screen unmounted, user navigated
      // away) must not leave the socket draining in the background.
      await reader.cancel().catch(() => undefined);
    }
  } finally {
    clearTimeout(timer);
  }
}

/**
 * True when nobody has pointed the app at a real gateway yet — the only mode
 * in which mock fixtures are allowed to answer.
 */
export async function isDemoGateway(): Promise<boolean> {
  return (await getGatewayUrl()) === DEFAULT_GATEWAY_URL;
}

/**
 * Try the live gateway.
 *
 * Demo mode (the built-in default URL): a failure returns null so the caller
 * falls back to the mock fixtures, and the backoff window skips the network
 * round-trip entirely for a while after a failure.
 *
 * A real configured gateway: a failure PROPAGATES. Fixtures standing in for
 * the operator's actual workspace would be a lie — callers render their
 * empty/error states and `gatewayState` drives the "unreachable" banner.
 * No backoff skip either, so recovery is noticed on the next call.
 */
export async function tryGateway<T>(label: string, fn: () => Promise<T>): Promise<T | null> {
  const demo = await isDemoGateway();
  if (
    demo &&
    gatewayState.reachable === false &&
    Date.now() - gatewayState.lastFailureAt < UNREACHABLE_BACKOFF_MS
  ) {
    gatewayState.servedSample = true;
    return null;
  }
  try {
    const result = await fn();
    gatewayState.servedSample = false;
    log('info', 'api', `computer ✓ ${label}`);
    return result;
  } catch (e) {
    // `request` already recorded network-level failures; a composite `fn`
    // that threw something else still counts as the gateway not answering.
    if (!(e instanceof APIError)) recordGatewayFailure();
    if (!demo) {
      log('warn', 'api', `computer ✕ ${label} — ${e instanceof Error ? e.message : 'error'}`);
      throw e;
    }
    gatewayState.servedSample = true;
    log('warn', 'api', `computer ✕ ${label} — not paired, showing sample data (${e instanceof Error ? e.message : 'error'})`);
    return null;
  }
}

/**
 * Whether what the last read returned is sample data: demo mode, and the
 * built-in fixtures answered rather than a computer. On the device the default
 * address is the phone's own loopback, so in demo mode this is true; it is
 * false for any paired computer, reachable or not.
 */
export async function showingSampleData(): Promise<boolean> {
  return (await isDemoGateway()) && gatewayState.servedSample;
}

/**
 * Probe `GET /health` on a candidate base URL — deliberately unauthenticated,
 * because first-time pairing runs before any token exists. True only for an
 * HTTP 2xx from that exact host.
 */
export async function pingGateway(url: string, timeoutMs = 4000): Promise<boolean> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const res = await fetch(`${url.replace(/\/+$/, '')}/health`, { signal: controller.signal });
    return res.ok;
  } catch {
    return false;
  } finally {
    clearTimeout(timer);
  }
}
