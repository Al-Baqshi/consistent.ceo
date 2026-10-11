// Vault API — password manager and environment secrets.
//
// Two deviations from the rest of `endpoints.ts`, both deliberate:
//
// 1. **No mock fallback.** Every other screen degrades to fixtures when the
//    gateway is unreachable; a credential store must not. If the gateway is
//    down the vault screen says so rather than showing invented secrets.
// 2. **Long timeouts on unlock paths.** Deriving the vault key runs Argon2id at
//    64 MiB with three iterations, which takes far longer than the 2.5s default
//    request timeout — the same reason `http.postAuth` exists for login.

import { APIError, getAuthToken, getGatewayUrl } from './client';

export class VaultLockedError extends Error {
  constructor() {
    super('Vault is locked');
    this.name = 'VaultLockedError';
  }
}

const QUICK_TIMEOUT_MS = 8_000;
/** Argon2id derivation is intentionally slow; do not shorten this. */
const KDF_TIMEOUT_MS = 30_000;

async function request<T>(
  method: string,
  path: string,
  body?: unknown,
  timeoutMs: number = QUICK_TIMEOUT_MS,
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

    if (res.status === 423) throw new VaultLockedError();

    if (!res.ok) {
      let message = `${method} ${path} → ${res.status}`;
      try {
        const parsed = await res.json();
        if (parsed?.error) message = parsed.error;
      } catch {
        // Non-JSON body; the status line is all we have.
      }
      throw new APIError(message, res.status);
    }

    // 204 No Content has an empty body, so res.json() would reject.
    if (res.status === 204) return undefined as T;
    const text = await res.text();
    return (text ? JSON.parse(text) : undefined) as T;
  } finally {
    clearTimeout(timer);
  }
}

export type ItemKind = 'login' | 'secure-note' | 'api-credential' | 'card';

export const KIND_LABELS: Record<ItemKind, string> = {
  login: 'Login',
  'secure-note': 'Secure note',
  'api-credential': 'API credential',
  card: 'Card',
};

export type PasswordHistoryEntry = { password: string; changedAt: string };

export type ItemSecret = {
  username?: string | null;
  password?: string | null;
  totpSecret?: string | null;
  notes?: string | null;
  passwordHistory?: PasswordHistoryEntry[];
};

export type ItemFlag = { id: string; name: string; scope: string; detail: string };

export type HealthReport = {
  totalWithPasswords: number;
  weak: ItemFlag[];
  reused: ItemFlag[];
  stale: ItemFlag[];
  score: number;
};

export type BreachResult = {
  checked: number;
  breached: { id: string; name: string; scope: string; count: number }[];
  unreachable: boolean;
};

export type ItemSummary = {
  id: string;
  kind: ItemKind;
  name: string;
  /** The entity this item belongs to: `global` or `entity:{id}`. */
  scope: string;
  folder: string | null;
  uris: string[];
  favorite: boolean;
  sharedFrom: string | null;
  updatedAt: string;
};

export type ItemScopeSummary = { scope: string; label: string; itemCount: number };

export type UnlockKind = 'master-password' | 'passkey' | 'recovery-key' | 'device-key';

export const UNLOCK_KIND_LABELS: Record<UnlockKind, string> = {
  'master-password': 'Master password',
  passkey: 'Passkey',
  'recovery-key': 'Recovery code',
  'device-key': 'Device',
};

/** An enrolled way into the vault. Listable while locked — the app has to know
 *  a device key exists before it can offer Face ID. */
export type UnlockMethod = {
  id: string;
  kind: UnlockKind;
  label: string;
  params?: Record<string, string>;
  createdAt: string;
  lastUsedAt?: string;
};

export type VaultStatus = {
  initialized: boolean;
  unlocked: boolean;
  locksInSeconds?: number;
  idleTimeoutSeconds: number;
  unlockMethods: UnlockMethod[];
};

export type ScopeSummary = { scope: string; label: string; keyCount: number };

export type AuditEntry = { at: string; action: string; detail?: string };

export type TotpCode = {
  code: string;
  secondsRemaining: number;
  period: number;
  issuer?: string | null;
  account?: string | null;
};

export type PasswordStrength = {
  score: number;
  label: string;
  acceptable: boolean;
  suggestion?: string;
  warning?: string;
  guessesLog10: string;
};

export const GLOBAL_SCOPE = 'global';

export const entityScope = (entityId: string) => `entity:${entityId}`;

const scopeQuery = (scope: string) => `?scope=${encodeURIComponent(scope)}`;

export const vault = {
  status: () => request<VaultStatus>('GET', '/api/vault/status'),

  /** Creating a vault also mints its recovery code — the only time it is
   *  readable. Show it before doing anything else. */
  init: (masterPassword: string) =>
    request<VaultStatus & { recoveryCode: string }>(
      'POST',
      '/api/vault/init',
      { masterPassword },
      KDF_TIMEOUT_MS,
    ),

  unlock: (masterPassword: string) =>
    request<VaultStatus>('POST', '/api/vault/unlock', { masterPassword }, KDF_TIMEOUT_MS),

  /** Unlock with a 32-byte key — on mobile, the biometric-gated device key.
   *  No Argon2 involved, so this is fast and needs no extended timeout. */
  unlockWithKey: (methodId: string, key: string) =>
    request<VaultStatus>('POST', '/api/vault/unlock/key', { methodId, key }),

  unlockWithRecoveryCode: (recoveryCode: string) =>
    request<VaultStatus>('POST', '/api/vault/unlock/recovery', { recoveryCode }),

  lock: () => request<void>('POST', '/api/vault/lock'),

  // ---- ways in --------------------------------------------------------------

  unlockMethods: () =>
    request<{ methods: UnlockMethod[] }>('GET', '/api/vault/unlock-methods').then(
      (r) => r.methods,
    ),

  addUnlockMethod: (input: {
    kind: UnlockKind;
    label: string;
    key: string;
    params?: Record<string, string>;
  }) => request<{ id: string }>('POST', '/api/vault/unlock-methods', input).then((r) => r.id),

  removeUnlockMethod: (id: string) =>
    request<void>('DELETE', `/api/vault/unlock-methods/${encodeURIComponent(id)}`),

  /** 32 fresh CSPRNG bytes from the gateway, used here as the device key.
   *  React Native ships no WebCrypto; see the endpoint's own note. */
  randomKey: () =>
    request<{ keyB64: string }>('GET', '/api/vault/random-key').then((r) => r.keyB64),

  newRecoveryCode: () =>
    request<{ recoveryCode: string }>('POST', '/api/vault/recovery-code').then(
      (r) => r.recoveryCode,
    ),

  changeMasterPassword: (currentPassword: string, newPassword: string) =>
    request<void>(
      'POST',
      '/api/vault/change-password',
      { currentPassword, newPassword },
      KDF_TIMEOUT_MS,
    ),

  /** List items. Omit `scope` for every item; pass `entity:{id}` / `global` for
   *  one entity's set. */
  listItems: (scope?: string) =>
    request<{ items: ItemSummary[] }>(
      'GET',
      `/api/vault/items${scope ? scopeQuery(scope) : ''}`,
    ).then((r) => r.items),

  /** Which entities hold items, for a switcher. */
  itemScopes: () =>
    request<{ scopes: ItemScopeSummary[] }>('GET', '/api/vault/item-scopes').then(
      (r) => r.scopes,
    ),

  itemSecret: (id: string) =>
    request<ItemSecret>('GET', `/api/vault/items/${encodeURIComponent(id)}`),

  createItem: (input: {
    kind: ItemKind;
    name: string;
    scope?: string;
    folder?: string | null;
    uris?: string[];
    secret: ItemSecret;
  }) => request<{ id: string }>('POST', '/api/vault/items', input).then((r) => r.id),

  updateItem: (id: string, secret: ItemSecret) =>
    request<void>('PUT', `/api/vault/items/${encodeURIComponent(id)}`, secret),

  /** Move an item to another entity's set, keeping its id, key and secret. */
  moveItem: (id: string, scope: string) =>
    request<void>('PUT', `/api/vault/items/${encodeURIComponent(id)}/scope`, { scope }),

  deleteItem: (id: string) =>
    request<void>('DELETE', `/api/vault/items/${encodeURIComponent(id)}`),

  /** Live TOTP code for an item with a stored 2FA seed. The seed stays on the
   *  server; only the digits and a countdown come back. */
  itemTotp: (id: string) =>
    request<TotpCode>('GET', `/api/vault/items/${encodeURIComponent(id)}/totp`),

  /** Estimate a candidate master password's strength. Needs no unlocked vault —
   *  it is called at creation. `context` should include the user's workspace. */
  passwordStrength: (password: string, context: string[] = []) =>
    request<PasswordStrength>('POST', '/api/vault/password-strength', { password, context }),

  // ---- environment variables -------------------------------------------------
  //
  // Scoped: `global` holds account-wide defaults, `entity:{id}` holds one
  // entity's own values which shadow global for that entity only.

  listSecrets: (scope: string = GLOBAL_SCOPE) =>
    request<{ scope: string; keys: string[] }>(
      'GET',
      `/api/vault/secrets${scopeQuery(scope)}`,
    ).then((r) => r.keys),

  getSecret: (key: string, scope: string = GLOBAL_SCOPE) =>
    request<{ key: string; value: string }>(
      'GET',
      `/api/vault/secrets/${encodeURIComponent(key)}${scopeQuery(scope)}`,
    ).then((r) => r.value),

  setSecret: (key: string, value: string, scope: string = GLOBAL_SCOPE) =>
    request<void>('POST', '/api/vault/secrets', { key, value, scope }),

  deleteSecret: (key: string, scope: string = GLOBAL_SCOPE) =>
    request<void>(
      'DELETE',
      `/api/vault/secrets/${encodeURIComponent(key)}${scopeQuery(scope)}`,
    ),

  listScopes: () =>
    request<{ scopes: ScopeSummary[] }>('GET', '/api/vault/scopes').then((r) => r.scopes),

  auditLog: () =>
    request<{ entries: AuditEntry[] }>('GET', '/api/vault/audit').then((r) => r.entries),

  /** Weak/reused/stale password report. Value-free. */
  health: () => request<HealthReport>('GET', '/api/vault/health'),

  /** Have I Been Pwned check (privacy-preserving k-anonymity). External calls,
   *  so it can be slow; a longer timeout than the quick default. */
  breachCheck: () => request<BreachResult>('GET', '/api/vault/breach-check', undefined, 30_000),

  generate: (options: {
    mode?: 'password' | 'passphrase';
    length?: number;
    digits?: boolean;
    symbols?: boolean;
    avoidAmbiguous?: boolean;
    words?: number;
  }) => request<{ value: string }>('POST', '/api/vault/generate', options).then((r) => r.value),

  publicKey: () =>
    request<{ publicKey: string }>('GET', '/api/vault/public-key').then((r) => r.publicKey),

  createShare: (input: {
    itemId: string;
    recipientPublicKey: string;
    scenario?: string;
    expiresAt?: string;
  }) => request<Record<string, unknown>>('POST', '/api/vault/share', input),

  acceptShare: (envelope: Record<string, unknown>) =>
    request<{ id: string }>('POST', '/api/vault/share/accept', envelope).then((r) => r.id),
};
