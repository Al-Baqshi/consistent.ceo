// Shared API client for the Consistent gateway
// Uses VITE_consistent_world_API_BASE for base URL, falling back to same-origin.

import { setApiError } from '$lib/errors';

// For dev, we call the gateway via the Vite proxy (same origin), so API_BASE stays relative.
// In production builds you can still set VITE_consistent_world_API_BASE if you serve the UI from elsewhere.
export const API_BASE = import.meta.env.VITE_consistent_world_API_BASE || '';
const WORKSPACE_ID_KEY = 'consistent_world_workspace_id';

export function getAuthToken(): string | null {
  if (typeof window === 'undefined') return null;
  return (
    // session token from UI
    sessionStorage.getItem('consistent_world_token') ||
    // or injected at build-time via Vite
    import.meta.env.VITE_consistent_world_TOKEN ||
    null
  );
}

export function getTenantId(): string {
  if (typeof window === 'undefined') return 'default';
  return sessionStorage.getItem(WORKSPACE_ID_KEY) || 'default';
}

export type PaginatedResponse<T> = {
  items: T[];
  total: number;
  limit: number;
  offset: number;
  hasMore: boolean;
};

export type PaginationParams = {
  limit?: number;
  offset?: number;
};

export type TenantSummary = {
  id: string;
  name: string;
  status?: string;
  entityCount?: number;
};

export async function listTenants(): Promise<TenantSummary[]> {
  return api<TenantSummary[]>('/api/workspaces');
}

/**
 * A non-2xx answer from the gateway. `message` is the response body (or
 * `HTTP <status>` when there is none), exactly as before; `status` lets a
 * caller tell "gone" (404) from "conflict" (409) from "the gateway is down"
 * without parsing text.
 */
export class ApiError extends Error {
  readonly status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
  }
}

/** The HTTP status of a failed `api()` call, or `null` for a network failure. */
export function apiErrorStatus(err: unknown): number | null {
  return err instanceof ApiError ? err.status : null;
}

export async function api<T>(
  path: string,
  options: RequestInit & {
    /** Skip the global error banner. For background probes whose failure is an
     *  expected answer the caller handles itself (e.g. asking a provider with
     *  no API key what models it serves) — not for user-initiated actions. */
    silent?: boolean;
  } = {}
): Promise<T> {
  const { silent, ...init } = options;
  const token = getAuthToken();
  const headers: HeadersInit = {
    'Content-Type': 'application/json',
    ...(init.headers as Record<string, string>),
  };

  if (token) {
    (headers as Record<string, string>)['Authorization'] = `Bearer ${token}`;
  }
  (headers as Record<string, string>)['x-workspace-id'] = getTenantId();

  const res = await fetch(`${API_BASE}${path}`, { ...init, headers });

  if (!res.ok) {
    const text = await res.text();
    const msg = text || `HTTP ${res.status}`;
    if (!silent) {
      try {
        setApiError(res.status === 401 ? 'Unauthorized' : msg);
      } catch (_) {}
    }
    throw new ApiError(res.status, msg);
  }

  if (res.status === 204 || res.headers.get('content-length') === '0') {
    return undefined as T;
  }

  const ct = res.headers.get('content-type') || '';
  if (!ct.includes('application/json')) {
    throw new Error(`Expected JSON from ${path} but got ${ct || 'unknown'} — is the gateway running?`);
  }

  return res.json() as Promise<T>;
}

/** The gateway's hard ceiling on one page of a list endpoint. */
const MAX_PAGE_SIZE = 200;

/**
 * Every row of a paginated list endpoint, not just the first page.
 *
 * The gateway defaults a bare list request to 50 rows in insertion order and
 * caps any page at 200, so `GET /api/tasks` alone quietly shows the workspace's
 * oldest 50 tasks and nothing newer. This follows `hasMore` until the list is
 * exhausted (or `max` rows have been read). `path` may already carry filters;
 * `limit`/`offset` are appended. An endpoint that answers with a bare array is
 * returned as-is.
 *
 * Lists come back oldest-first, so `last: n` reads only the newest `n` rows
 * (one tiny probe for the total, then the tail) — for views that poll often
 * and only care about recent work.
 */
export async function fetchAllPages<T>(
  path: string,
  options: { max?: number; last?: number; silent?: boolean } = {}
): Promise<T[]> {
  const sep = path.includes('?') ? '&' : '?';
  let max = options.max ?? 5000;
  let offset = 0;
  if (options.last != null) {
    const probe = await api<PaginatedResponse<T> | T[]>(`${path}${sep}limit=1&offset=0`, {
      silent: options.silent
    });
    if (Array.isArray(probe)) return probe.slice(-options.last);
    offset = Math.max(0, (probe?.total ?? 0) - options.last);
    max = Math.min(max, options.last);
  }
  const tail = options.last != null;
  const rows: T[] = [];
  // In tail mode keep reading to the end (rows may have landed since the
  // probe) and keep the newest; otherwise stop once `max` rows are in.
  for (let pages = 0; pages < 100 && (tail || rows.length < max); pages += 1) {
    const page = await api<PaginatedResponse<T> | T[]>(
      `${path}${sep}limit=${MAX_PAGE_SIZE}&offset=${offset}`,
      { silent: options.silent }
    );
    if (Array.isArray(page)) {
      rows.push(...page);
      break;
    }
    const items = page?.items ?? [];
    rows.push(...items);
    if (!page?.hasMore || items.length === 0) break;
    offset += items.length;
  }
  return tail ? rows.slice(-max) : rows.slice(0, max);
}

/**
 * POST to an NDJSON streaming endpoint and hand each parsed line to `onLine`
 * as it arrives. Resolves when the stream closes. Throws when the endpoint
 * refuses the request or the response has no readable body, so callers can
 * fall back to the blocking variant of the same endpoint.
 */
export async function streamNdjson(
  path: string,
  body: unknown,
  onLine: (line: unknown) => void,
  options: { signal?: AbortSignal } = {}
): Promise<void> {
  const token = getAuthToken();
  const headers: Record<string, string> = { 'Content-Type': 'application/json' };
  if (token) headers['Authorization'] = `Bearer ${token}`;
  headers['x-workspace-id'] = getTenantId();

  const res = await fetch(`${API_BASE}${path}`, {
    method: 'POST',
    headers,
    body: JSON.stringify(body),
    signal: options.signal
  });
  if (!res.ok || !res.body) {
    const text = await res.text().catch(() => '');
    throw new Error(text || `HTTP ${res.status}`);
  }

  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  const deliver = (raw: string) => {
    const line = raw.trim();
    if (!line) return;
    try {
      onLine(JSON.parse(line));
    } catch {
      // A malformed line must not kill the stream; later lines may be fine.
    }
  };
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    let newline = buffer.indexOf('\n');
    while (newline >= 0) {
      deliver(buffer.slice(0, newline));
      buffer = buffer.slice(newline + 1);
      newline = buffer.indexOf('\n');
    }
  }
  deliver(buffer);
}

export function setAuthToken(token: string) {
  if (typeof window !== 'undefined') {
    sessionStorage.setItem('consistent_world_token', token);
  }
}

export function clearAuthToken() {
  if (typeof window !== 'undefined') {
    sessionStorage.removeItem('consistent_world_token');
  }
}

export function setTenantId(tenantId: string) {
  if (typeof window !== 'undefined') {
    sessionStorage.setItem(WORKSPACE_ID_KEY, tenantId.trim() || 'default');
  }
}

// ===== Auth =====
//
// Everything here talks to the local gateway. There is no hosted identity
// service, no email, and no redirect off this origin — a self-hosted instance
// on a plane behaves exactly like a connected one.

export type AuthSession = {
  token: string;
  userId: string;
  username: string;
  workspaceId: string;
  /** Present only on register — shown once and never retrievable again. */
  recoveryCodes?: string[];
};

/** Sign-in and registration are public: no bearer, so `api()` isn't used. */
async function authPost<T>(path: string, body: unknown): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!res.ok) {
    throw new Error(await authErrorMessage(res));
  }
  return res.json() as Promise<T>;
}

async function authErrorMessage(res: Response): Promise<string> {
  const body = (await res.text()).trim();
  if (res.status === 401) return 'Those details did not match an account.';
  if (res.status === 409) return 'That username is taken.';
  if (res.status === 429) return 'Too many attempts. Wait a minute and try again.';
  if (res.status === 403) return body || 'This gateway requires an invite code.';
  if (res.status === 503) return 'This gateway has no account store configured.';
  return body || `Sign-in failed (HTTP ${res.status})`;
}

export function login(username: string, password: string, device?: string) {
  return authPost<AuthSession>('/api/auth/login', { username, password, device });
}

export function registerAccount(
  username: string,
  password: string,
  opts: { device?: string; inviteCode?: string } = {},
) {
  return authPost<AuthSession>('/api/auth/register', { username, password, ...opts });
}

/**
 * Recover with a backup code, optionally setting a new password in the same
 * step — which is the only way to set one without knowing the old password.
 * Succeeding signs every other session out.
 */
export function recoverAccount(
  username: string,
  code: string,
  newPassword?: string,
  device?: string,
) {
  return authPost<AuthSession>('/api/auth/recover', { username, code, newPassword, device });
}

export function checkUsername(username: string) {
  return authPost<{ available: boolean }>('/api/auth/check-username', { username });
}

// ----- Passkeys -----

function b64urlToBytes(s: string): Uint8Array {
  const pad = s.replace(/-/g, '+').replace(/_/g, '/');
  const raw = atob(pad + '='.repeat((4 - (pad.length % 4)) % 4));
  return Uint8Array.from(raw, (c) => c.charCodeAt(0));
}

function bytesToB64url(b: ArrayBuffer): string {
  return btoa(String.fromCharCode(...new Uint8Array(b)))
    .replace(/\+/g, '-')
    .replace(/\//g, '_')
    .replace(/=+$/, '');
}

/** WebAuthn wants ArrayBuffers where the gateway's JSON has base64url strings. */
function reviveCredentialOptions<T extends Record<string, any>>(options: T): T {
  const o: any = structuredClone(options);
  if (o.challenge) o.challenge = b64urlToBytes(o.challenge);
  if (o.user?.id) o.user.id = b64urlToBytes(o.user.id);
  for (const key of ['allowCredentials', 'excludeCredentials'] as const) {
    if (Array.isArray(o[key])) {
      o[key] = o[key].map((c: any) => ({ ...c, id: b64urlToBytes(c.id) }));
    }
  }
  return o;
}

export function passkeysAvailable(): boolean {
  return typeof window !== 'undefined' && !!window.PublicKeyCredential;
}

/**
 * Sign in with a passkey. The account comes from the ceremony the gateway
 * opened for this username — the finish call carries no username of its own,
 * because a client-chosen one was an account-takeover primitive.
 */
export async function passkeyLogin(username: string): Promise<AuthSession> {
  const start = await authPost<{ challenge: any; stateId: string }>(
    '/api/auth/passkey/login/start',
    { username },
  );
  const assertion = (await navigator.credentials.get({
    publicKey: reviveCredentialOptions(start.challenge.publicKey ?? start.challenge),
  })) as PublicKeyCredential | null;
  if (!assertion) throw new Error('No passkey was used.');
  const r = assertion.response as AuthenticatorAssertionResponse;

  return authPost<AuthSession>('/api/auth/passkey/login/finish', {
    stateId: start.stateId,
    credential: {
      id: assertion.id,
      rawId: bytesToB64url(assertion.rawId),
      type: assertion.type,
      response: {
        authenticatorData: bytesToB64url(r.authenticatorData),
        clientDataJSON: bytesToB64url(r.clientDataJSON),
        signature: bytesToB64url(r.signature),
        userHandle: r.userHandle ? bytesToB64url(r.userHandle) : null,
      },
    },
  });
}

/** Add a passkey to the signed-in account. */
export async function passkeyRegister(): Promise<void> {
  const start = await api<{ challenge: any; stateId: string }>(
    '/api/auth/passkey/register/start',
    { method: 'POST' },
  );
  const created = (await navigator.credentials.create({
    publicKey: reviveCredentialOptions(start.challenge.publicKey ?? start.challenge),
  })) as PublicKeyCredential | null;
  if (!created) throw new Error('No passkey was created.');
  const r = created.response as AuthenticatorAttestationResponse;

  await api('/api/auth/passkey/register/finish', {
    method: 'POST',
    body: JSON.stringify({
      stateId: start.stateId,
      credential: {
        id: created.id,
        rawId: bytesToB64url(created.rawId),
        type: created.type,
        response: {
          attestationObject: bytesToB64url(r.attestationObject),
          clientDataJSON: bytesToB64url(r.clientDataJSON),
        },
      },
    }),
  });
}

// ----- Device approval (the desktop app's login) -----

export type ApprovalDetail = {
  requestId: string;
  status: string;
  context: string | null;
  expiresAt: string;
};

/** What is asking to sign in. No code — that is read off the app's own window. */
export function getApprovalRequest(requestId: string) {
  return api<ApprovalDetail>(`/api/auth/approvals/${encodeURIComponent(requestId)}`);
}

/**
 * Approve a sign-in. Uses `fetch` directly rather than `api()` so each refusal
 * gets the sentence that tells the person what to do about it — a mistyped code
 * and an expired request are very different problems.
 */
export async function approveSignIn(requestId: string, code: string, device?: string) {
  const res = await fetch(
    `${API_BASE}/api/auth/approvals/${encodeURIComponent(requestId)}/approve`,
    {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        Authorization: `Bearer ${getAuthToken() ?? ''}`,
      },
      body: JSON.stringify({ code, device }),
    },
  );
  if (res.ok) return;
  throw new Error(
    {
      403: "That code doesn't match the one in the app.",
      404: 'That sign-in request no longer exists.',
      409: 'That request was already answered.',
      410: 'That request expired. Start the sign-in again from the app.',
      429: 'Too many wrong codes. Start the sign-in again from the app.',
      401: 'Sign in first, then approve.',
    }[res.status] ?? `Could not approve (HTTP ${res.status})`,
  );
}

export function denySignIn(requestId: string) {
  return api<void>(`/api/auth/approvals/${encodeURIComponent(requestId)}/deny`, {
    method: 'POST',
    body: JSON.stringify({}),
  });
}

// ----- Sessions -----

export type SessionInfo = {
  id: string;
  deviceLabel: string | null;
  createdAt: string;
  expiresAt: string;
  lastUsedAt: string | null;
  current: boolean;
};

/**
 * Signed-in devices. Named for auth deliberately: `listSessions` already means
 * chat/memory sessions in this file, and confusing the two is how a UI ends up
 * offering to sign you out of a conversation.
 */
export function listAuthSessions() {
  return api<SessionInfo[]>('/api/auth/sessions');
}

export function revokeAuthSession(id: string) {
  return api<void>(`/api/auth/sessions/${encodeURIComponent(id)}`, { method: 'DELETE' });
}

/** Sign out everywhere but here. */
export function revokeOtherSessions() {
  return api<{ revokedSessions: number }>('/api/auth/sessions', { method: 'DELETE' });
}

// ── Pairing a phone (gateway `pairing.rs`) ──────────────────────────────────

/** A one-time code to show as a QR. Works once, for 90 seconds. */
export interface PairOffer {
  nonce: string;
  expiresAt: string;
  /** Where the phone will reach this computer; null when nothing reaches it yet. */
  url: string | null;
  via: 'relay' | 'tailnet' | 'lan' | null;
  payload: Record<string, unknown>;
  /** The QR as an SVG document, or null when there is no reachable address. */
  qrSvg: string | null;
}

/** A phone that scanned the code and is waiting for Allow. */
export interface PendingPairing {
  requestId: string;
  label: string;
  platform: string;
  /** Six digits the phone also shows. Allow only when they match. */
  code: string;
  createdAt: string;
  expiresAt: string;
}

export interface PairedDevice {
  deviceId: string;
  label: string;
  platform: string;
  createdAt: string;
  lastSeen: string;
  /** Holds its own key and signs itself back in without a password. */
  paired: boolean;
}

export function createPairOffer() {
  return api<PairOffer>('/api/pair/offer', { method: 'POST' });
}

export function listPairRequests() {
  return api<PendingPairing[]>('/api/pair/requests', { silent: true });
}

export function approvePairRequest(id: string) {
  return api<{ deviceId: string }>(`/api/pair/requests/${encodeURIComponent(id)}/approve`, {
    method: 'POST'
  });
}

export function denyPairRequest(id: string) {
  return api<void>(`/api/pair/requests/${encodeURIComponent(id)}/deny`, { method: 'POST' });
}

export function listDevices() {
  return api<PairedDevice[]>('/api/auth/devices');
}

/** Remove a device. A paired phone is signed out at once. */
export function removeDevice(id: string) {
  return api<void>(`/api/auth/devices/${encodeURIComponent(id)}`, { method: 'DELETE' });
}

// ----- The machine-owner account -----

export type AccountStatus = {
  /** False while this account exists only on this computer. */
  hasPassword: boolean;
  username: string;
};

export function getAccountStatus() {
  return api<AccountStatus>('/api/auth/account');
}

/**
 * Give this machine's account a username and password so it can be reached
 * from a phone or another browser. Same account, so everything already in it
 * comes along.
 */
export function claimAccount(username: string, password: string) {
  return api<{ username: string; recoveryCodes: string[] }>('/api/auth/claim', {
    method: 'POST',
    body: JSON.stringify({ username, password }),
  });
}

export function changePassword(currentPassword: string, newPassword: string) {
  return api<{ revokedSessions: number }>('/api/auth/change-password', {
    method: 'POST',
    body: JSON.stringify({ currentPassword, newPassword }),
  });
}

export async function logout(): Promise<void> {
  try {
    await api('/api/auth/logout', { method: 'POST' });
  } catch (_) {
    // Best-effort server revoke; the local token goes either way.
  }
  clearAuthToken();
}

/** Apply enrichment draft to entity (partial apply: pass field names, or empty for all). */
export async function applyEnrichment(
  entityId: string,
  fields: string[] = [],
  draftVersion?: number
): Promise<unknown> {
  return api(`/api/entities/${entityId}/enrichment/apply`, {
    method: 'POST',
    body: JSON.stringify({ fields, draft_version: draftVersion }),
  });
}

/** Discard enrichment draft. */
export async function discardEnrichment(entityId: string): Promise<unknown> {
  return api(`/api/entities/${entityId}/enrichment/discard`, {
    method: 'POST',
    body: JSON.stringify({}),
  });
}

/** Regenerate enrichment draft for an entity. */
export async function regenerateEnrichment(entityId: string): Promise<unknown> {
  return api(`/api/entities/${entityId}/enrichment/regenerate`, {
    method: 'POST',
    body: JSON.stringify({}),
  });
}


export type EntityFile = {
  id: string;
  entity_id: string;
  filename: string;
  content_type: string;
  size_bytes: number;
  uploaded_at: string;
  path: string;
};

export async function uploadEntityFile(entityId: string, file: File): Promise<EntityFile> {
  const token = getAuthToken();
  const headers: HeadersInit = {};
  if (token) {
    (headers as Record<string, string>)['Authorization'] = `Bearer ${token}`;
  }
  (headers as Record<string, string>)['x-workspace-id'] = getTenantId();

  const formData = new FormData();
  formData.append('file', file);

  const res = await fetch(`${API_BASE}/api/entities/${entityId}/files`, {
    method: 'POST',
    headers,
    body: formData,
  });

  if (!res.ok) {
    const text = await res.text();
    const msg = text || `HTTP ${res.status}`;
    try {
      setApiError(res.status === 401 ? 'Unauthorized' : msg);
    } catch (_) {}
    throw new Error(msg);
  }

  return res.json() as Promise<EntityFile>;
}

export async function listEntityFiles(entityId: string): Promise<EntityFile[]> {
  return api<EntityFile[]>(`/api/entities/${entityId}/files`);
}

export type ChatAttachment = {
  id: string;
  filename: string;
  content_type: string;
  size_bytes: number;
  session_id: string;
  uploaded_at: string;
  path: string;
};

export async function uploadChatAttachment(sessionId: string, file: File): Promise<ChatAttachment> {
  const token = getAuthToken();
  const headers: HeadersInit = {};
  if (token) {
    (headers as Record<string, string>)['Authorization'] = `Bearer ${token}`;
  }
  (headers as Record<string, string>)['x-workspace-id'] = getTenantId();

  const formData = new FormData();
  formData.append('file', file);
  if (sessionId) formData.append('session_id', sessionId);

  const res = await fetch(`${API_BASE}/api/chat/attachments`, {
    method: 'POST',
    headers,
    body: formData,
  });

  if (!res.ok) {
    const text = await res.text();
    const msg = text || `HTTP ${res.status}`;
    try {
      setApiError(res.status === 401 ? 'Unauthorized' : msg);
    } catch (_) {}
    throw new Error(msg);
  }

  return res.json() as Promise<ChatAttachment>;
}

export async function deleteEntity(entityId: string): Promise<void> {
  await api<void>(`/api/entities/${entityId}`, {
    method: 'DELETE',
  });
}

export type MissionStatus =
  | 'planned'
  | 'active'
  | 'blocked'
  | 'user-input-needed'
  | 'completed'
  | 'cancelled';

export type Mission = {
  id: string;
  entity_id: string;
  title: string;
  summary: string;
  status: MissionStatus;
  priority: string;
  goal: string;
  owner?: string;
  created_by?: string;
  source?: string;
  created_at: string;
  updated_at: string;
  last_reviewed_at?: string;
  review_cadence_hours?: number;
  final_report?: string;
};

export type CreateMissionInput = {
  entity_id: string;
  title: string;
  summary?: string;
  status?: MissionStatus;
  priority?: string;
  goal?: string;
  owner?: string;
  created_by?: string;
  source?: string;
  review_cadence_hours?: number;
};

export type UpdateMissionInput = {
  title?: string;
  summary?: string;
  status?: MissionStatus;
  priority?: string;
  goal?: string;
  owner?: string;
  last_reviewed_at?: string;
  review_cadence_hours?: number;
};

export type MissionActionInput = {
  action: 'review_now' | 'snooze_review' | 'accept_proposed_next_step' | 'dismiss_no_action' | 'mark_user_input_needed';
  hours?: number;
  note?: string;
};

export type MissionMetrics = {
  recommendationEmitted: number;
  recommendationAccepted: number;
  recommendationDismissed: number;
  duplicateSuppressed: number;
  staleUnresolved?: number;
  byBranch?: Record<string, Record<string, number>>;
  /** Seconds from the first-ever completed mission's creation to its completion.
   *  Absent until a workspace has completed one. The activation metric. */
  timeToFirstCompletedMissionSeconds?: number | null;
};

export type CreateTaskInput = {
  title: string;
  dept?: string;
  roles?: string[];
  entity_id?: string;
  mission_id?: string;
  priority?: string;
  requires_human_approval?: boolean;
  task_kind?: string;
  connection_id?: string;
  external_source?: string;
  external_id?: string;
  metadata?: Record<string, unknown>;
};

export async function listMissions(
  params?: { entityId?: string; status?: string } & PaginationParams
): Promise<PaginatedResponse<Mission>> {
  const sp = new URLSearchParams();
  if (params?.entityId) sp.set('entity_id', params.entityId);
  if (params?.status) sp.set('status', params.status);
  if (params?.limit != null) sp.set('limit', String(params.limit));
  if (params?.offset != null) sp.set('offset', String(params.offset));
  const q = sp.toString();
  return api<PaginatedResponse<Mission>>(`/api/missions${q ? `?${q}` : ''}`);
}

export async function getMission(id: string): Promise<Mission> {
  return api<Mission>(`/api/missions/${id}`);
}

export type DodStatus = 'not_started' | 'in_progress' | 'blocked' | 'needs_review' | 'met';

/** Goal-level Definition of Done: is the whole mission finished? */
export type DodAssessment = {
  status: DodStatus;
  reason: string;
  totalTasks: number;
  completed: number;
  open: number;
  failed: number;
  needsReview: number;
  pendingApproval: number;
};

export async function getMissionDod(id: string): Promise<DodAssessment> {
  return api<DodAssessment>(`/api/missions/${id}/dod`);
}

// ─── Durable overnight loop: cancel + final report ──────────────────────────

export async function cancelMission(id: string): Promise<Mission> {
  return api<Mission>(`/api/missions/${id}/cancel`, { method: 'POST' });
}

export async function cancelTask(id: string): Promise<Task> {
  return api<Task>(`/api/tasks/${id}/cancel`, { method: 'POST' });
}

export async function cancelEntityAgentRun(
  entityId: string,
  runId: string
): Promise<EntityAgentRunResponse> {
  return api<EntityAgentRunResponse>(`/api/entities/${entityId}/agent-runs/${runId}/cancel`, {
    method: 'POST'
  });
}

export type MissionReport = { mission_id: string; report: string };

export async function getMissionReport(id: string): Promise<MissionReport> {
  return api<MissionReport>(`/api/missions/${id}/report`);
}

export async function createMission(input: CreateMissionInput): Promise<Mission> {
  return api<Mission>('/api/missions', {
    method: 'POST',
    body: JSON.stringify(input),
  });
}

export async function updateMission(id: string, input: UpdateMissionInput): Promise<Mission> {
  return api<Mission>(`/api/missions/${id}`, {
    method: 'PATCH',
    body: JSON.stringify(input),
  });
}

export async function createTask(input: CreateTaskInput): Promise<unknown> {
  return api('/api/tasks', {
    method: 'POST',
    body: JSON.stringify(input),
  });
}

export type CreateScheduledTaskInput = {
  cronExpression: string;
  agentId: string;
  taskDescription: string;
  entityId?: string;
  connectionId?: string;
};

export async function createScheduledTask(input: CreateScheduledTaskInput): Promise<unknown> {
  return api('/api/schedule', {
    method: 'POST',
    body: JSON.stringify(input),
  });
}

export async function deleteScheduledTask(id: string): Promise<void> {
  await api<void>(`/api/schedule/${encodeURIComponent(id)}`, { method: 'DELETE' });
}

// ─── Conversational entity creation ─────────────────────────────────────────

export type EntityOnboardingProgress = {
  answered: number;
  total: number;
  percent: number;
};

export type EntityColourSwatch = {
  hex: string;
  name: string;
};

export type StartEntityCreationResponse = {
  sessionId: string;
  question: string;
  help: string;
  progress: EntityOnboardingProgress;
  proposedColour: string;
  palette: EntityColourSwatch[];
};

export type UpdateEntityCreationColourResponse = {
  colour: string;
};

export type AnswerEntityCreationResponse = {
  nextQuestion: string | null;
  help: string | null;
  progress: EntityOnboardingProgress;
  complete: boolean;
  fieldsAutoFilled: string[];
};

export type CompleteEntityCreationResponse = {
  entity: {
    id: string;
    name: string;
    [key: string]: unknown;
  };
};

export async function startEntityCreation(input: {
  entityName: string;
  workspaceId?: string;
}): Promise<StartEntityCreationResponse> {
  return api<StartEntityCreationResponse>('/api/entities/conversational/start', {
    method: 'POST',
    body: JSON.stringify(input),
  });
}

export async function answerEntityCreation(input: {
  sessionId: string;
  answer: string;
  skip?: boolean;
}): Promise<AnswerEntityCreationResponse> {
  return api<AnswerEntityCreationResponse>('/api/entities/conversational/answer', {
    method: 'POST',
    body: JSON.stringify(input),
  });
}

export async function completeEntityCreation(input: {
  sessionId: string;
  forceComplete?: boolean;
}): Promise<CompleteEntityCreationResponse> {
  return api<CompleteEntityCreationResponse>('/api/entities/conversational/complete', {
    method: 'POST',
    body: JSON.stringify(input),
  });
}

export async function cancelEntityCreation(sessionId: string): Promise<void> {
  await api<void>(`/api/entities/conversational/${encodeURIComponent(sessionId)}`, {
    method: 'DELETE',
  });
}

export async function updateEntityCreationColour(input: {
  sessionId: string;
  colour: string;
}): Promise<UpdateEntityCreationColourResponse> {
  return api<UpdateEntityCreationColourResponse>(
    '/api/entities/conversational/colour',
    {
      method: 'POST',
      body: JSON.stringify(input),
    }
  );
}

export type CaptureInterpretLite = {
  intent: string;
  confidence: number;
  clarifyingQuestion?: string | null;
  draft?: {
    entityName?: string | null;
    entityType?: string | null;
    entitySummary?: string | null;
  };
};

/// Convenience: run the capture interpret pipeline asking for create_entity intent.
/// Returns the proposed entity name/summary or null if the LLM didn't propose one.
export async function interpretAsEntityProposal(
  text: string
): Promise<{ name: string; summary: string | null } | null> {
  try {
    const res = await api<CaptureInterpretLite>('/api/capture/interpret', {
      method: 'POST',
      body: JSON.stringify({ text, requestedAction: 'create_entity' }),
    });
    const name = res.draft?.entityName?.trim();
    if (!name) return null;
    return { name, summary: res.draft?.entitySummary?.trim() || null };
  } catch {
    return null;
  }
}

export async function applyMissionAction(id: string, input: MissionActionInput): Promise<Mission> {
  return api<Mission>(`/api/missions/${id}/actions`, {
    method: 'POST',
    body: JSON.stringify(input),
  });
}

export async function getMissionMetrics(): Promise<MissionMetrics> {
  return api<MissionMetrics>('/api/missions/metrics');
}

export type EntityConnection = {
  id: string;
  entityId: string;
  kind: string;
  provider: string;
  label: string;
  status: string;
  capabilityState?: Record<string, unknown>;
  config?: Record<string, unknown>;
  secretRef?: string;
  externalAccountId?: string;
  externalHandle?: string;
  scopes?: string[];
  lastCheckedAt?: string;
  lastError?: string;
  createdBy?: string;
  createdAt: string;
  updatedAt: string;
};

export type EntityConnectionEvent = {
  id: string;
  timestamp: string;
  entityId: string;
  connectionId: string;
  kind: string;
  detail?: string;
};

export type EntityAgentRunResponse = {
  runId: string;
  provider: string;
  entityId: string;
  taskId?: string;
  status: string;
  cwd: string;
  output: string;
  error?: string;
  startedAt: string;
  completedAt?: string;
  /** none | manual | auto */
  reviewPolicy?: string;
  /** pending_review | review_running | reviewed | needs_review */
  reviewStatus?: string;
  /** pass | needs_review */
  reviewVerdict?: string;
  reviewRunId?: string;
  reviewedRunId?: string;
};

export type EntityConnectionSession = {
  id: string;
  entityId: string;
  provider: string;
  requestedCapabilities?: string[];
  state: string;
  pkceVerifierRef?: string;
  authorizationUrl?: string;
  verificationUri?: string;
  userCode?: string;
  pollIntervalSeconds?: number;
  providerMessage?: string;
  status: string;
  expiresAt: string;
  createdBy?: string;
  createdAt: string;
};

export async function listEntityConnections(entityId: string): Promise<EntityConnection[]> {
  return api<EntityConnection[]>(`/api/entities/${entityId}/connections`);
}

// --- Entity workspace: "does this thing have a repo?" ---

/** Where an entity's work lives. `unset` and an unchosen vault both need an answer. */
export type WorkspaceKind = 'repo' | 'local_folder' | 'vault_only' | 'unset';

export type WorkspaceOption = {
  choice: string;
  label: string;
  detail: string;
  needsPath: boolean;
  /** Endpoint the client must drive itself (GitHub's existing device-code flow). */
  via?: string;
};

export type EntityWorkspaceStatus = {
  entityId: string;
  kind: WorkspaceKind;
  path?: string;
  exists: boolean;
  /** The only candidate is the folder the gateway made, which nobody chose. */
  isVaultFallback: boolean;
  /** Autonomous work is holding until this is answered. */
  needsDecision: boolean;
  choice?: string;
  askedAt?: string;
  question?: string;
  options: WorkspaceOption[];
};

export type WorkspaceDecisionInput = {
  choice: 'local_folder' | 'new_project' | 'vault_only' | 'github' | 'repo';
  path?: string;
  gitInit?: boolean;
};

export type WorkspaceDecisionResult = {
  ok: boolean;
  message: string;
  gitInitialized: boolean;
  /** Present for `github`: the connection-session flow the client drives next. */
  nextEndpoint?: string;
  status: EntityWorkspaceStatus;
};

export type PendingWorkspaceQuestion = {
  entityId: string;
  entityName: string;
  question: string;
  askedAt?: string;
  status: EntityWorkspaceStatus;
};

export async function getEntityWorkspace(entityId: string): Promise<EntityWorkspaceStatus> {
  return api<EntityWorkspaceStatus>(`/api/entities/${encodeURIComponent(entityId)}/workspace`);
}

export async function setEntityWorkspace(
  entityId: string,
  decision: WorkspaceDecisionInput
): Promise<WorkspaceDecisionResult> {
  return api<WorkspaceDecisionResult>(`/api/entities/${encodeURIComponent(entityId)}/workspace`, {
    method: 'POST',
    body: JSON.stringify(decision)
  });
}

/** Every entity with queued work and nowhere chosen to do it. */
export async function listPendingWorkspaceQuestions(): Promise<PendingWorkspaceQuestion[]> {
  return api<PendingWorkspaceQuestion[]>('/api/entity-workspace/pending');
}

export async function createEntityConnection(
  entityId: string,
  input: Record<string, unknown>
): Promise<EntityConnection> {
  return api<EntityConnection>(`/api/entities/${entityId}/connections`, {
    method: 'POST',
    body: JSON.stringify(input)
  });
}

export async function updateEntityConnection(
  entityId: string,
  connectionId: string,
  input: Record<string, unknown>
): Promise<EntityConnection> {
  return api<EntityConnection>(`/api/entities/${entityId}/connections/${connectionId}`, {
    method: 'PATCH',
    body: JSON.stringify(input)
  });
}

export async function testEntityConnection(entityId: string, connectionId: string): Promise<EntityConnection> {
  return api<EntityConnection>(`/api/entities/${entityId}/connections/${connectionId}/test`, {
    method: 'POST',
    body: JSON.stringify({})
  });
}

export async function verifyEntityConnection(
  entityId: string,
  connectionId: string,
  input: Record<string, unknown> = {}
): Promise<EntityConnection> {
  return api<EntityConnection>(`/api/entities/${entityId}/connections/${connectionId}/verify`, {
    method: 'POST',
    body: JSON.stringify(input)
  });
}

export async function deleteEntityConnection(entityId: string, connectionId: string): Promise<void> {
  await api<void>(`/api/entities/${entityId}/connections/${connectionId}`, {
    method: 'DELETE'
  });
}

export async function listEntityConnectionEvents(entityId: string): Promise<EntityConnectionEvent[]> {
  return api<EntityConnectionEvent[]>(`/api/entities/${entityId}/connection-events`);
}

export async function createEntityAgentRun(
  entityId: string,
  input: {
    kind: 'claude' | 'opencode' | 'cursor-agent';
    prompt: string;
    cwd?: string;
    timeoutSecs?: number;
    title?: string;
    createTask?: boolean;
    reviewPolicy?: 'none' | 'manual' | 'auto';
    reviewerProvider?: 'claude' | 'opencode' | 'cursor-agent';
  }
): Promise<EntityAgentRunResponse> {
  return api<EntityAgentRunResponse>(`/api/entities/${entityId}/agent-runs`, {
    method: 'POST',
    body: JSON.stringify(input)
  });
}

export async function getEntityAgentRun(
  entityId: string,
  runId: string
): Promise<EntityAgentRunResponse> {
  return api<EntityAgentRunResponse>(`/api/entities/${entityId}/agent-runs/${runId}`);
}

export async function createEntityConnectionSession(
  entityId: string,
  input: Record<string, unknown>
): Promise<EntityConnectionSession> {
  return api<EntityConnectionSession>(`/api/entities/${entityId}/connections/sessions`, {
    method: 'POST',
    body: JSON.stringify(input)
  });
}

export async function finalizeEntityConnectionSession(
  entityId: string,
  sessionId: string,
  input: Record<string, unknown>
): Promise<EntityConnection> {
  return api<EntityConnection>(`/api/entities/${entityId}/connections/sessions/${sessionId}`, {
    method: 'POST',
    body: JSON.stringify(input)
  });
}

export async function performEntityAction(
  entityId: string,
  input: Record<string, unknown>
): Promise<Record<string, unknown>> {
  return api<Record<string, unknown>>(`/api/entities/${entityId}/actions/perform`, {
    method: 'POST',
    body: JSON.stringify(input)
  });
}

export type EntityRuntimePatch = {
  automationMode?: 'manual' | 'suggest' | 'auto_run';
  heartbeatMode?: 'off' | 'suggest' | 'execute';
  defaultChatSessionId?: string | null;
  quietHoursLocal?: { start: string; end: string } | null;
};

export type EntitySetup = {
  whatItIs?: string | null;
  primaryGoal?: string | null;
  successCriteria?: string | null;
  primaryDomain?: string | null;
  primaryEmail?: string | null;
  repoPath?: string | null;
  backendStack?: string | null;
  frontendStack?: string | null;
  databaseStack?: string | null;
};

export type EntityPatch = {
  name?: string | null;
  description?: string | null;
  department?: string | null;
  status?: string | null;
  entityType?: string | null;
  stage?: string | null;
  goals?: string | null;
  profile?: string | null;
  activeContext?: string | null;
  operatingRules?: string | null;
  notes?: string | null;
  contentPipeline?: string | null;
  localPath?: string | null;
  setup?: EntitySetup;
  entityRuntime?: EntityRuntimePatch;
  preferredModelTier?: string | null;
  flowDiagrams?: EntityFlowDiagrams | null;
};

export type EntityFlowDiagrams = {
  previous?: string | null;
  current?: string | null;
  future?: string | null;
  updatedAt?: string | null;
};

/** PATCH /api/entities/:id — partial update (camelCase body). */
export async function patchEntity(entityId: string, patch: EntityPatch): Promise<unknown> {
  return api(`/api/entities/${entityId}`, {
    method: 'PATCH',
    body: JSON.stringify(patch),
  });
}

export type ApprovalRequest = {
  id: string;
  createdAt: string;
  updatedAt: string;
  entityId: string;
  connectionId?: string;
  taskId?: string;
  action: string;
  status: 'pending' | 'approved' | 'rejected' | 'expired';
  detail?: string;
  createdBy?: string;
};

export async function listApprovals(query?: { entityId?: string; status?: string }): Promise<ApprovalRequest[]> {
  const sp = new URLSearchParams();
  if (query?.entityId) sp.set('entity_id', query.entityId);
  if (query?.status) sp.set('status', query.status);
  const q = sp.toString();
  return api<ApprovalRequest[]>(`/api/approvals${q ? `?${q}` : ''}`);
}

export async function patchApproval(
  id: string,
  status: 'approved' | 'rejected' | 'expired'
): Promise<ApprovalRequest> {
  return api<ApprovalRequest>(`/api/approvals/${id}`, {
    method: 'PATCH',
    body: JSON.stringify({ status }),
  });
}

// ─── Conversational Entity Creation ───────────────────────────────────────────

// The flow itself is driven by `startEntityCreation` / `answerEntityCreation` /
// `completeEntityCreation` above (camelCase bodies). The snake_case duplicates
// that used to live here had no callers and sent fields the gateway ignores.

export type SessionSummary = {
  sessionId: string;
  entityName: string;
  createdAt: string;
  complete: boolean;
  fieldsFilled: number;
  totalFields: number;
};

export type ListSessionsResponse = {
  sessions: SessionSummary[];
};

export async function listCreationSessions(): Promise<ListSessionsResponse> {
  return api<ListSessionsResponse>('/api/entities/conversational/sessions');
}

export type Task = {
  id: string;
  title: string;
  dept: string;
  roles: string[];
  status: string;
  tries: string;
  progress: number;
  entity_id?: string;
  mission_id?: string;
  priority?: string;
  requires_human_approval: boolean;
  assigned_director_id?: string;
  assigned_agent_id?: string;
  result?: string;
  task_kind?: string;
  outcome?: string;
  created_at: string;
  source_task_id?: string;
};

export async function listTasks(
  params?: {
    status?: string;
    entity_id?: string;
    mission_id?: string;
  } & PaginationParams
): Promise<PaginatedResponse<Task>> {
  const sp = new URLSearchParams();
  if (params?.status) sp.set('status', params.status);
  if (params?.entity_id) sp.set('entity_id', params.entity_id);
  if (params?.mission_id) sp.set('mission_id', params.mission_id);
  if (params?.limit != null) sp.set('limit', String(params.limit));
  if (params?.offset != null) sp.set('offset', String(params.offset));
  const q = sp.toString();
  return api<PaginatedResponse<Task>>(`/api/tasks${q ? `?${q}` : ''}`);
}

// ─── Raw captures (mission-control inbox) ───────────────────────────────────

export type RawCaptureStatus =
  | 'raw'
  | 'dismissed'
  | 'promoted_to_task'
  | 'promoted_to_memory'
  | 'narrative_updated';

export type RouteSuggestion = {
  entityId?: string;
  entityName?: string;
  confidence: number;
  reason: string;
  signals: string[];
};

export type RawCapture = {
  id: string;
  workspace_id: string;
  entity_id?: string;
  source: string;
  source_ref?: string;
  session_id?: string;
  provider?: string;
  raw_text: string;
  summary?: string;
  status: RawCaptureStatus;
  promoted_task_id?: string;
  metadata?: unknown;
  created_at: string;
  updated_at: string;
  /** Present only when the list was requested with `with_route=true`. */
  route?: RouteSuggestion;
};

export type CreateCaptureInput = {
  raw_text: string;
  entity_id?: string;
  source?: string;
  source_ref?: string;
  session_id?: string;
  provider?: string;
  summary?: string;
  metadata?: Record<string, unknown>;
};

export async function listCaptures(
  params?: {
    entity_id?: string;
    status?: RawCaptureStatus;
    with_route?: boolean;
    selected_entity_id?: string;
  } & PaginationParams
): Promise<PaginatedResponse<RawCapture>> {
  const sp = new URLSearchParams();
  if (params?.entity_id) sp.set('entity_id', params.entity_id);
  if (params?.status) sp.set('status', params.status);
  if (params?.with_route) sp.set('with_route', 'true');
  if (params?.selected_entity_id) sp.set('selected_entity_id', params.selected_entity_id);
  if (params?.limit != null) sp.set('limit', String(params.limit));
  if (params?.offset != null) sp.set('offset', String(params.offset));
  const q = sp.toString();
  return api<PaginatedResponse<RawCapture>>(`/api/captures${q ? `?${q}` : ''}`);
}

export async function createCapture(input: CreateCaptureInput): Promise<RawCapture> {
  return api<RawCapture>('/api/captures', {
    method: 'POST',
    body: JSON.stringify(input),
  });
}

export async function dismissCapture(id: string): Promise<RawCapture> {
  return api<RawCapture>(`/api/captures/${id}/dismiss`, {
    method: 'POST',
    body: JSON.stringify({}),
  });
}

export async function promoteCaptureToTask(
  id: string,
  input: { title?: string; priority?: string; director_id?: string; task_kind?: string } = {}
): Promise<{ capture: RawCapture; task: Task }> {
  return api<{ capture: RawCapture; task: Task }>(`/api/captures/${id}/promote-task`, {
    method: 'POST',
    body: JSON.stringify(input),
  });
}

export type CaptureReviewResponse = { capture: RawCapture; note_path: string };

export async function updateCaptureNarrative(
  id: string,
  input: { entity_id?: string; note?: string } = {}
): Promise<CaptureReviewResponse> {
  return api<CaptureReviewResponse>(`/api/captures/${id}/update-narrative`, {
    method: 'POST',
    body: JSON.stringify(input),
  });
}

export async function promoteCaptureToMemory(
  id: string,
  input: { title?: string; entity_id?: string; kind?: 'canonical' | 'lessons'; summary?: string } = {}
): Promise<CaptureReviewResponse> {
  return api<CaptureReviewResponse>(`/api/captures/${id}/promote-memory`, {
    method: 'POST',
    body: JSON.stringify(input),
  });
}

export type RouteCaptureResponse = {
  capture: RawCapture;
  suggestion: RouteSuggestion;
  applied: boolean;
};

export async function routeCapture(
  id: string,
  input: { selected_entity_id?: string; auto_apply?: boolean } = {}
): Promise<RouteCaptureResponse> {
  return api<RouteCaptureResponse>(`/api/captures/${id}/route`, {
    method: 'POST',
    body: JSON.stringify(input),
  });
}

export async function attachCapture(id: string, entity_id: string): Promise<RawCapture> {
  return api<RawCapture>(`/api/captures/${id}/attach`, {
    method: 'POST',
    body: JSON.stringify({ entity_id }),
  });
}

// ─── External capture ingestion ─────────────────────────────────────────────
// Arbitrary tools (Claude Code, Codex, Manus, OpenClaw, browser, shell, webhooks)
// post logs/prompts/results here and they land in the raw capture loop.

export type ExternalCaptureSource =
  | 'claude_code'
  | 'codex'
  | 'manus'
  | 'openclaw'
  | 'browser'
  | 'shell'
  | 'webhook'
  | (string & {});

export type ExternalCaptureInput = {
  raw_text: string;
  source: ExternalCaptureSource;
  source_ref?: string;
  external_id?: string;
  provider?: string;
  session_id?: string;
  entity_id?: string;
  cwd?: string;
  repo_url?: string;
  url?: string;
  title?: string;
  summary?: string;
  metadata?: Record<string, unknown>;
  auto_route?: boolean;
  selected_entity_id?: string;
};

export type ExternalCaptureResponse = {
  capture: RawCapture;
  suggestion: RouteSuggestion;
  applied: boolean;
  existing: boolean;
};

export type ExternalCaptureBatchItemResult = {
  ok: boolean;
  capture?: RawCapture;
  suggestion?: RouteSuggestion;
  applied: boolean;
  existing: boolean;
  error_status?: number;
};

export type ExternalCaptureBatchResponse = {
  results: ExternalCaptureBatchItemResult[];
  created: number;
  existing: number;
  failed: number;
};

export async function createExternalCapture(
  input: ExternalCaptureInput
): Promise<ExternalCaptureResponse> {
  return api<ExternalCaptureResponse>('/api/external-captures', {
    method: 'POST',
    body: JSON.stringify(input),
  });
}

export async function createExternalCapturesBatch(
  items: ExternalCaptureInput[]
): Promise<ExternalCaptureBatchResponse> {
  return api<ExternalCaptureBatchResponse>('/api/external-captures/batch', {
    method: 'POST',
    body: JSON.stringify({ items }),
  });
}

/** An entity as the gateway serializes it (`state::Entity`, camelCase; the
 *  type field crosses the wire as `type`). */
export type Entity = {
  id: string;
  name: string;
  description?: string;
  department?: string;
  owner?: string;
  status: string;
  type?: string;
  dept: string;
  /** True for the user's private, auto-provisioned Personal Space. */
  isPersonal?: boolean;
  personalUserId?: string;
  fileCount: number;
  path?: string;
  workspaceId?: string;
  localPath?: string;
  stage?: string;
  domains: string[];
  markets: string[];
  profile?: string;
  goals?: string;
  activeContext?: string;
  operatingRules?: string;
  notes?: string;
  contentPipeline?: string;
  relationships: { type: string; target: string; status: string; notes?: string }[];
  resources: {
    id: string;
    kind: string;
    root: string;
    readAllowed?: boolean;
    writeAllowed?: boolean;
    execAllowed?: boolean;
  }[];
  /** Set once the source has been (or is being) indexed into the knowledge base. */
  knowledgeIndex?: EntityKnowledgeIndex;
  [key: string]: unknown;
};

export async function listEntities(
  params?: PaginationParams
): Promise<PaginatedResponse<Entity>> {
  const sp = new URLSearchParams();
  if (params?.limit != null) sp.set('limit', String(params.limit));
  if (params?.offset != null) sp.set('offset', String(params.offset));
  const q = sp.toString();
  return api<PaginatedResponse<Entity>>(`/api/entities${q ? `?${q}` : ''}`);
}

export type IngestEntitySourceResponse = {
  entity: Entity;
  created: boolean;
  attached: boolean;
  sourceUrl: string;
  summary: string;
};

export async function ingestEntitySource(input: {
  url: string;
  entityId?: string;
  name?: string;
}): Promise<IngestEntitySourceResponse> {
  return api<IngestEntitySourceResponse>('/api/entity-source-ingest', {
    method: 'POST',
    body: JSON.stringify(input),
  });
}

// ===== Entity import (observe a repo/folder/URL → draft → review → commit) =====

/** What the operator tells the engine about the entity before it analyses the
 *  source. Treated as authoritative: it steers the goals, the suggested tasks
 *  and what gets indexed. */
export type EntityImportContext = {
  whatItIs?: string;
  primaryGoal?: string;
  successCriteria?: string;
  stage?: string;
  activeContext?: string;
  operatingRules?: string;
  notes?: string;
};

export type EntityImportSuggestedTask = {
  title: string;
  description?: string;
  priority?: string;
  /** 'task' | 'reminder' */
  kind: string;
  dueAt?: string;
  include: boolean;
};

export type EntityImportSetup = {
  whatItIs?: string;
  primaryGoal?: string;
  successCriteria?: string;
  primaryDomain?: string;
  primaryEmail?: string;
  repoPath?: string;
  backendStack?: string;
  frontendStack?: string;
  databaseStack?: string;
  workspaceChoice?: string;
};

export type EntityImportDraft = {
  name: string;
  description?: string;
  entityType?: string;
  stage?: string;
  profile?: string;
  goals?: string;
  activeContext?: string;
  operatingRules?: string;
  notes?: string;
  setup: EntityImportSetup;
  summary?: string;
  suggestedTasks: EntityImportSuggestedTask[];
};

export type EntityImportJob = {
  id: string;
  source: string;
  sourceKind: string;
  status: 'running' | 'ready' | 'failed' | 'committed';
  draft?: EntityImportDraft;
  error?: string;
  entityId?: string;
  startedAt: string;
  completedAt?: string;
};

export async function analyzeEntityImport(input: {
  source: string;
  name?: string;
  /** Fill this existing entity instead of drafting a new one. */
  entityId?: string;
  /** Extra URLs (repo, live site, docs) folded into the analysis. */
  links?: string[];
  /** The operator's own account of the entity — authoritative over the analysis. */
  context?: EntityImportContext;
}): Promise<{ jobId: string; sourceKind: string }> {
  return api('/api/entity-import/analyze', {
    method: 'POST',
    body: JSON.stringify(input),
  });
}

export async function getEntityImportJob(jobId: string): Promise<EntityImportJob> {
  return api(`/api/entity-import/jobs/${encodeURIComponent(jobId)}`);
}

export async function commitEntityImport(input: {
  jobId?: string;
  entityId?: string;
  draft: EntityImportDraft;
}): Promise<{
  entity: Entity;
  created: boolean;
  createdTasks: number;
  createdReminders: number;
  /** True when the gateway kicked off indexing the source into the knowledge base. */
  indexing: boolean;
}> {
  return api('/api/entity-import/commit', {
    method: 'POST',
    body: JSON.stringify(input),
  });
}

// ===== Entity knowledge base (the indexed source behind chats & heartbeats) =====

export type EntityKnowledgeIndex = {
  status: 'indexing' | 'ready' | 'failed' | 'skipped';
  source: string;
  sourceKind: string;
  docCount: number;
  embeddedCount: number;
  startedAt: string;
  indexedAt?: string;
  error?: string;
};

export type EntityKnowledgeDoc = {
  id: string;
  title: string;
  source: string;
  url?: string;
  enabled: boolean;
  chars: number;
  hasEmbedding: boolean;
  content: string;
  createdAt: string;
  updatedAt: string;
};

export async function getEntityKnowledge(
  entityId: string
): Promise<{ index?: EntityKnowledgeIndex; docs: EntityKnowledgeDoc[] }> {
  return api(`/api/entities/${encodeURIComponent(entityId)}/knowledge`);
}

export async function reindexEntityKnowledge(
  entityId: string
): Promise<{ started: boolean; reason?: string; index?: EntityKnowledgeIndex }> {
  return api(`/api/entities/${encodeURIComponent(entityId)}/knowledge/reindex`, {
    method: 'POST',
    body: JSON.stringify({}),
  });
}

export async function setEntityKnowledgeDocEnabled(
  entityId: string,
  docId: string,
  enabled: boolean
): Promise<EntityKnowledgeDoc> {
  return api(
    `/api/entities/${encodeURIComponent(entityId)}/knowledge/docs/${encodeURIComponent(docId)}`,
    { method: 'PATCH', body: JSON.stringify({ enabled }) }
  );
}

export async function deleteEntityKnowledgeDoc(entityId: string, docId: string): Promise<void> {
  await api<void>(
    `/api/entities/${encodeURIComponent(entityId)}/knowledge/docs/${encodeURIComponent(docId)}`,
    { method: 'DELETE' }
  );
}

export type Session = {
  id: string;
  agent_id: string;
  channel: string;
  peer_id: string;
  scope_type: string;
  scope_id: string;
  created_at: string;
  last_active: string;
};

export async function listSessions(
  params?: { agent_id?: string } & PaginationParams
): Promise<PaginatedResponse<Session>> {
  const sp = new URLSearchParams();
  if (params?.agent_id) sp.set('agent_id', params.agent_id);
  if (params?.limit != null) sp.set('limit', String(params.limit));
  if (params?.offset != null) sp.set('offset', String(params.offset));
  const q = sp.toString();
  return api<PaginatedResponse<Session>>(`/api/sessions${q ? `?${q}` : ''}`);
}

/** One operator-feed row as the gateway serializes it (`state::OperatorFeedEvent`,
 *  camelCase). `kind` is snake_case — see `$lib/feedKinds` for labels. The
 *  `listOperatorFeed` *query* parameters below stay snake_case: those are
 *  `OperatorFeedQuery`, a different struct. */
export type OperatorFeedEvent = {
  id: string;
  timestamp: string;
  kind: string;
  title: string;
  detail?: string;
  taskId?: string;
  missionId?: string;
  entityId?: string;
  connectionId?: string;
  approvalId?: string;
};

export async function listOperatorFeed(
  params?: {
    task_id?: string;
    mission_id?: string;
    entity_id?: string;
    connection_id?: string;
  } & PaginationParams
): Promise<PaginatedResponse<OperatorFeedEvent>> {
  const sp = new URLSearchParams();
  if (params?.task_id) sp.set('task_id', params.task_id);
  if (params?.mission_id) sp.set('mission_id', params.mission_id);
  if (params?.entity_id) sp.set('entity_id', params.entity_id);
  if (params?.connection_id) sp.set('connection_id', params.connection_id);
  if (params?.limit != null) sp.set('limit', String(params.limit));
  if (params?.offset != null) sp.set('offset', String(params.offset));
  const q = sp.toString();
  return api<PaginatedResponse<OperatorFeedEvent>>(`/api/operator-feed${q ? `?${q}` : ''}`);
}

// ─── Mission Control activity timeline ──────────────────────────────────────
// Unified read-only feed: captures + tasks + agent runs + verification warnings +
// approvals/questions + mission DoD events + next actions, newest first.

export type ActivitySource = 'feed' | 'capture' | 'agent_run' | 'verification';

export type ActivityItem = {
  id: string;
  timestamp: string;
  source: ActivitySource;
  kind: string;
  title: string;
  detail?: string;
  status?: string;
  entityId?: string;
  missionId?: string;
  taskId?: string;
  refId?: string;
};

export async function listActivity(params?: {
  entity_id?: string;
  mission_id?: string;
  kind?: string;
  status?: string;
  source?: ActivitySource;
  limit?: number;
}): Promise<ActivityItem[]> {
  const sp = new URLSearchParams();
  if (params?.entity_id) sp.set('entity_id', params.entity_id);
  if (params?.mission_id) sp.set('mission_id', params.mission_id);
  if (params?.kind) sp.set('kind', params.kind);
  if (params?.status) sp.set('status', params.status);
  if (params?.source) sp.set('source', params.source);
  if (params?.limit != null) sp.set('limit', String(params.limit));
  const q = sp.toString();
  return api<ActivityItem[]>(`/api/activity${q ? `?${q}` : ''}`);
}

// ─── Verification actions (shell test / browser URL check) ──────────────────

export type VerificationRun = {
  id: string;
  workspaceId: string;
  entityId?: string;
  taskId?: string;
  missionId?: string;
  /** shell | browser */
  kind: string;
  command?: string;
  url?: string;
  /** running | passed | failed */
  status: string;
  exitCode?: number;
  logs: string;
  artifacts: string[];
  followUpTaskId?: string;
  createdAt: string;
  completedAt?: string;
};

export type RunVerificationInput = {
  kind: 'shell' | 'browser';
  command?: string;
  url?: string;
  entity_id?: string;
  task_id?: string;
  mission_id?: string;
  cwd?: string;
  timeout_secs?: number;
};

export async function runVerification(input: RunVerificationInput): Promise<VerificationRun> {
  return api<VerificationRun>('/api/verifications', {
    method: 'POST',
    body: JSON.stringify(input)
  });
}

export async function listVerifications(params?: {
  entity_id?: string;
  task_id?: string;
  mission_id?: string;
  status?: string;
  kind?: string;
  limit?: number;
}): Promise<VerificationRun[]> {
  const sp = new URLSearchParams();
  if (params?.entity_id) sp.set('entity_id', params.entity_id);
  if (params?.task_id) sp.set('task_id', params.task_id);
  if (params?.mission_id) sp.set('mission_id', params.mission_id);
  if (params?.status) sp.set('status', params.status);
  if (params?.kind) sp.set('kind', params.kind);
  if (params?.limit != null) sp.set('limit', String(params.limit));
  const q = sp.toString();
  return api<VerificationRun[]>(`/api/verifications${q ? `?${q}` : ''}`);
}

// ─── Daily review (the Today screen) ────────────────────────────────────────
// One aggregated read of the operator's day: what slipped, what's due, what
// needs a decision, and whether the streak held. Backed by GET /api/daily-review
// (control_api.rs), which is deliberately LLM-free so the screen opens instantly.

export type TaskLiteStatus =
  | 'backlog'
  | 'queued'
  | 'in-progress'
  | 'failed'
  | 'completed'
  | 'cancelled';

/**
 * The trimmed task shape the day view renders — no orchestration bookkeeping.
 *
 * The gateway serializes Rust `Option`s without `skip_serializing_if`, so absent
 * values arrive as `null`, not as missing keys. Types say so.
 */
export type TaskLite = {
  id: string;
  title: string;
  entityId?: string | null;
  entityName?: string | null;
  missionId?: string | null;
  priority?: string | null;
  dueAt?: string | null;
  status: TaskLiteStatus;
  /** Whole hours past the deadline; null unless the task is overdue. */
  overdueHours?: number | null;
};

export type PendingApprovalLite = {
  id: string;
  entityId: string;
  title: string;
  createdAt: string;
};

export type MissionAwaitingInputLite = {
  id: string;
  title: string;
  entityId: string;
};

export type HeartbeatHighlight = {
  entityId?: string | null;
  entityName?: string | null;
  summary: string;
  firedAt: string;
};

export type UpcomingHeartbeat = {
  id: string;
  name: string;
  targetId?: string | null;
  targetName?: string | null;
  nextFireAt?: string | null;
};

export type StreakSummary = {
  currentStreakDays: number;
  completedToday: number;
  completedLast7: number;
  completedLast30: number;
};

export type DailyReview = {
  generatedAt: string;
  overdue: TaskLite[];
  dueToday: TaskLite[];
  pendingApprovals: PendingApprovalLite[];
  missionsAwaitingInput: MissionAwaitingInputLite[];
  heartbeatHighlights: HeartbeatHighlight[];
  upcomingHeartbeats: UpcomingHeartbeat[];
  streak: StreakSummary;
  suggestedFocus: TaskLite[];
};

export async function getDailyReview(): Promise<DailyReview> {
  return api<DailyReview>('/api/daily-review');
}

/** Mark a task done. `completed` is the kebab-case serde name for TaskStatus. */
export async function completeTask(id: string): Promise<Task> {
  return api<Task>(`/api/tasks/${encodeURIComponent(id)}`, {
    method: 'PATCH',
    body: JSON.stringify({ status: 'completed' }),
  });
}

/** Push a deadline out and clear the overdue markers. Server clamps to 1..=168. */
export async function snoozeTask(id: string, hours = 24): Promise<Task> {
  return api<Task>(`/api/tasks/${encodeURIComponent(id)}/snooze`, {
    method: 'POST',
    body: JSON.stringify({ hours }),
  });
}

export type ActionEvent = {
  id: string;
  timestamp: string;
  task_id?: string;
  entity_id?: string;
  agent_id?: string;
  capability: string;
  outcome: string;
  detail?: string;
};

export async function listActionEvents(
  params?: {
    task_id?: string;
    entity_id?: string;
    agent_id?: string;
    capability?: string;
    outcome?: string;
  } & PaginationParams
): Promise<PaginatedResponse<ActionEvent>> {
  const sp = new URLSearchParams();
  if (params?.task_id) sp.set('task_id', params.task_id);
  if (params?.entity_id) sp.set('entity_id', params.entity_id);
  if (params?.agent_id) sp.set('agent_id', params.agent_id);
  if (params?.capability) sp.set('capability', params.capability);
  if (params?.outcome) sp.set('outcome', params.outcome);
  if (params?.limit != null) sp.set('limit', String(params.limit));
  if (params?.offset != null) sp.set('offset', String(params.offset));
  const q = sp.toString();
  return api<PaginatedResponse<ActionEvent>>(`/api/actions${q ? `?${q}` : ''}`);
}

// ===== Chat attachments (binary, authenticated) =====

/**
 * Fetch an uploaded chat attachment's bytes with the session's credentials and
 * hand back an object URL for an `<img>`. Plain `<img src="/api/…">` can't
 * carry the bearer token, so the gateway would refuse it. Callers own the URL
 * and revoke it when the element goes away.
 */
export async function fetchChatAttachmentUrl(id: string): Promise<string> {
  const token = getAuthToken();
  const headers: Record<string, string> = {};
  if (token) headers['Authorization'] = `Bearer ${token}`;
  headers['x-workspace-id'] = getTenantId();
  const res = await fetch(`${API_BASE}/api/chat/attachments/${encodeURIComponent(id)}/thumb`, {
    headers,
  });
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  return URL.createObjectURL(await res.blob());
}
