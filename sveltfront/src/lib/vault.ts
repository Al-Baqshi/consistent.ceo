// Vault API client.
//
// Deliberately does not go through `$lib/api`'s `api()` helper: that treats every
// non-2xx as an error and raises the global error banner, but this API uses
// `423 Locked` as ordinary flow control — it is the signal to show the unlock
// prompt, not a failure worth shouting about.

import { getAuthToken, getTenantId } from '$lib/api';

const API_BASE = import.meta.env.VITE_consistent_world_API_BASE || '';

export class VaultLockedError extends Error {
  constructor() {
    super('Vault is locked');
    this.name = 'VaultLockedError';
  }
}

/** Any other non-2xx answer. `status` lets a caller tell `503` (no vault on this
 *  gateway) from `401` (session expired) from `400` (a request that will never
 *  succeed as sent); a network failure never reaches here — `fetch` throws. */
export class VaultHttpError extends Error {
  constructor(
    message: string,
    public readonly status: number
  ) {
    super(message);
    this.name = 'VaultHttpError';
  }
}

/** The unlock throttle is in effect. `retryAfterSeconds` is how long to wait. */
export class TooManyAttemptsError extends Error {
  constructor(
    message: string,
    public readonly retryAfterSeconds: number
  ) {
    super(message);
    this.name = 'TooManyAttemptsError';
  }
}

async function request<T>(path: string, options: RequestInit = {}): Promise<T> {
  const token = getAuthToken();
  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
    ...((options.headers as Record<string, string>) ?? {})
  };
  if (token) headers['Authorization'] = `Bearer ${token}`;
  headers['x-workspace-id'] = getTenantId();

  const res = await fetch(`${API_BASE}${path}`, { ...options, headers });

  if (res.status === 423) throw new VaultLockedError();

  if (res.status === 429) {
    // Prefer the standard Retry-After header, fall back to the body's number.
    const header = Number(res.headers.get('retry-after'));
    let seconds = Number.isFinite(header) && header > 0 ? header : 30;
    let message = 'Too many attempts. Please wait a moment.';
    try {
      const body = await res.json();
      if (body?.error) {
        message = body.error;
        const match = /(\d+)\s*s/.exec(body.error);
        if (match) seconds = Number(match[1]);
      }
    } catch {
      // Header-only; keep the default message.
    }
    throw new TooManyAttemptsError(message, seconds);
  }

  if (!res.ok) {
    let message = `HTTP ${res.status}`;
    try {
      const body = await res.json();
      if (body?.error) message = body.error;
    } catch {
      // Non-JSON error body; the status line is all we have.
    }
    throw new VaultHttpError(message, res.status);
  }

  if (res.status === 204 || res.headers.get('content-length') === '0') {
    return undefined as T;
  }
  return (await res.json()) as T;
}

export type ItemKind = 'login' | 'secure-note' | 'api-credential' | 'card';

export type CustomField = { name: string; value: string; hidden: boolean };

export type PasswordHistoryEntry = { password: string; changedAt: string };

export type ItemSecret = {
  username?: string | null;
  password?: string | null;
  totpSecret?: string | null;
  notes?: string | null;
  customFields?: CustomField[];
  passwordHistory?: PasswordHistoryEntry[];
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

/** An enrolled way into the vault. Readable while still locked — that is the
 *  point, since the client has to know a passkey exists before it can offer it. */
export type UnlockMethod = {
  id: string;
  kind: UnlockKind;
  label: string;
  /** Public parameters needed to reproduce this method's key, e.g. a passkey's
   *  `prfSaltB64` and `credentialIdB64`. Never contains key material. */
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

export type ItemFlag = { id: string; name: string; scope: string; detail: string };

export type HealthReport = {
  totalWithPasswords: number;
  weak: ItemFlag[];
  reused: ItemFlag[];
  stale: ItemFlag[];
  score: number;
};

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

export type ImportOutcome = {
  format: string;
  detected: number;
  imported: number;
  committed: boolean;
  skipped: string[];
};

/** A passphrase-encrypted backup. Opaque here — the client only ships it around. */
export type VaultBackup = Record<string, unknown>;

export type ShareEnvelope = Record<string, unknown>;

export const UNLOCK_KIND_LABELS: Record<UnlockKind, string> = {
  'master-password': 'Master password',
  passkey: 'Passkey',
  'recovery-key': 'Recovery code',
  'device-key': 'Device'
};

export const KIND_LABELS: Record<ItemKind, string> = {
  login: 'Login',
  'secure-note': 'Secure note',
  'api-credential': 'API credential',
  card: 'Card'
};

// ---- lifecycle --------------------------------------------------------------

export const getStatus = () => request<VaultStatus>('/api/vault/status');

/** Creating a vault also mints its recovery code. This is the only time it is
 *  ever readable — the caller must show it before doing anything else. */
export const initVault = (masterPassword: string) =>
  request<VaultStatus & { recoveryCode: string }>('/api/vault/init', {
    method: 'POST',
    body: JSON.stringify({ masterPassword })
  });

export const unlockVault = (masterPassword: string) =>
  request<VaultStatus>('/api/vault/unlock', {
    method: 'POST',
    body: JSON.stringify({ masterPassword })
  });

/** Unlock with a 32-byte key: a passkey PRF result, or a device key. */
export const unlockWithKey = (methodId: string, keyB64: string) =>
  request<VaultStatus>('/api/vault/unlock/key', {
    method: 'POST',
    body: JSON.stringify({ methodId, key: keyB64 })
  });

export const unlockWithRecoveryCode = (recoveryCode: string) =>
  request<VaultStatus>('/api/vault/unlock/recovery', {
    method: 'POST',
    body: JSON.stringify({ recoveryCode })
  });

export const lockVault = () => request<void>('/api/vault/lock', { method: 'POST' });

export const changeMasterPassword = (currentPassword: string, newPassword: string) =>
  request<void>('/api/vault/change-password', {
    method: 'POST',
    body: JSON.stringify({ currentPassword, newPassword })
  });

// ---- ways in -----------------------------------------------------------------

export const listUnlockMethods = () =>
  request<{ methods: UnlockMethod[] }>('/api/vault/unlock-methods').then((r) => r.methods);

export const addUnlockMethod = (input: {
  kind: UnlockKind;
  label: string;
  key: string;
  params?: Record<string, string>;
}) =>
  request<{ id: string }>('/api/vault/unlock-methods', {
    method: 'POST',
    body: JSON.stringify(input)
  }).then((r) => r.id);

export const removeUnlockMethod = (id: string) =>
  request<void>(`/api/vault/unlock-methods/${encodeURIComponent(id)}`, { method: 'DELETE' });

/** 32 fresh CSPRNG bytes, used here as the salt for a WebAuthn PRF evaluation. */
export const getPrfSalt = () =>
  request<{ saltB64: string }>('/api/vault/random-key').then((r) => r.saltB64);

/** Mint an extra recovery code. Shown once. */
export const newRecoveryCode = () =>
  request<{ recoveryCode: string }>('/api/vault/recovery-code', { method: 'POST' }).then(
    (r) => r.recoveryCode
  );

// ---- items ------------------------------------------------------------------

/** List items. Omit `scope` for every item (each tagged with its own scope), or
 *  pass `entity:{id}` / `global` for one entity's set. */
export const listItems = (scope?: string) =>
  request<{ items: ItemSummary[] }>(
    `/api/vault/items${scope ? scopeQuery(scope) : ''}`
  ).then((r) => r.items);

/** The entities that hold at least one item, for a switcher. */
export const itemScopes = () =>
  request<{ scopes: ItemScopeSummary[] }>('/api/vault/item-scopes').then((r) => r.scopes);

export const createItem = (input: {
  kind: ItemKind;
  name: string;
  scope?: string;
  folder?: string | null;
  uris?: string[];
  favorite?: boolean;
  secret: ItemSecret;
}) =>
  request<{ id: string }>('/api/vault/items', {
    method: 'POST',
    body: JSON.stringify(input)
  }).then((r) => r.id);

/** Move an item to a different entity's set, keeping its id, key and secret. */
export const moveItem = (id: string, scope: string) =>
  request<void>(`/api/vault/items/${encodeURIComponent(id)}/scope`, {
    method: 'PUT',
    body: JSON.stringify({ scope })
  });

export const getItemSecret = (id: string) =>
  request<ItemSecret>(`/api/vault/items/${encodeURIComponent(id)}`);

export const updateItemSecret = (id: string, secret: ItemSecret) =>
  request<void>(`/api/vault/items/${encodeURIComponent(id)}`, {
    method: 'PUT',
    body: JSON.stringify(secret)
  });

/** Rename, refile or (un)favourite an item without touching its secret half. */
export const updateItemMetadata = (
  id: string,
  changes: { name?: string; folder?: string | null; uris?: string[]; favorite?: boolean }
) =>
  request<void>(`/api/vault/items/${encodeURIComponent(id)}/metadata`, {
    method: 'PUT',
    body: JSON.stringify(changes)
  });

export const deleteItem = (id: string) =>
  request<void>(`/api/vault/items/${encodeURIComponent(id)}`, { method: 'DELETE' });

/** The live TOTP code for an item that has a stored 2FA seed: just the digits and
 *  a countdown, so the client never has to implement RFC 6238. This does not keep
 *  the seed off the client — `getItemSecret` returns it as `totpSecret` so it can
 *  be edited. Reading a code does not count as activity, so polling it cannot
 *  hold the vault open past its idle auto-lock. */
export const itemTotp = (id: string) =>
  request<TotpCode>(`/api/vault/items/${encodeURIComponent(id)}/totp`);

// ---- migration from other password managers ---------------------------------

/** Parse another manager's CSV export. With `commit: false` (the default here)
 *  it reports what would import without storing anything, for a confirm screen. */
export const importCsv = (csv: string, commit: boolean, scope?: string) =>
  request<ImportOutcome>('/api/vault/import/csv', {
    method: 'POST',
    body: JSON.stringify({ csv, commit, scope })
  });

// ---- master password strength ------------------------------------------------

/** Estimate a candidate master password's strength. `context` should include the
 *  user's email and workspace so a password built from them scores low. */
export const passwordStrength = (password: string, context: string[] = []) =>
  request<PasswordStrength>('/api/vault/password-strength', {
    method: 'POST',
    body: JSON.stringify({ password, context })
  });

// ---- environment variables / machine credentials ----------------------------
//
// Every call takes a scope. `global` holds account-wide defaults; `entity:{id}`
// holds one entity's own values, which shadow global for that entity only.

export const GLOBAL_SCOPE = 'global';

export const entityScope = (entityId: string) => `entity:${entityId}`;

const scopeQuery = (scope: string) => `?scope=${encodeURIComponent(scope)}`;

export const listSecrets = (scope: string = GLOBAL_SCOPE) =>
  request<{ scope: string; keys: string[] }>(`/api/vault/secrets${scopeQuery(scope)}`).then(
    (r) => r.keys
  );

export const setSecret = (key: string, value: string, scope: string = GLOBAL_SCOPE) =>
  request<void>('/api/vault/secrets', {
    method: 'POST',
    body: JSON.stringify({ key, value, scope })
  });

/** Read one value from exactly this scope. Pass `resolve` to follow the
 *  entity → global fallback the way a running agent would. */
export const getSecret = (key: string, scope: string = GLOBAL_SCOPE, resolve = false) =>
  request<{ key: string; value: string; scope: string }>(
    `/api/vault/secrets/${encodeURIComponent(key)}${scopeQuery(scope)}${resolve ? '&resolve=true' : ''}`
  ).then((r) => r.value);

export const deleteSecret = (key: string, scope: string = GLOBAL_SCOPE) =>
  request<void>(`/api/vault/secrets/${encodeURIComponent(key)}${scopeQuery(scope)}`, {
    method: 'DELETE'
  });

/** Every scope currently holding at least one variable. */
export const listScopes = () =>
  request<{ scopes: ScopeSummary[] }>('/api/vault/scopes').then((r) => r.scopes);

/** The full environment an entity runs with: global overlaid with its own. */
export const resolvedEnv = (scope: string) =>
  request<{ scope: string; env: Record<string, string> }>(
    `/api/vault/env${scopeQuery(scope)}`
  ).then((r) => r.env);

// ---- durability ---------------------------------------------------------------

/** A portable backup, encrypted under its own passphrase. Opens with nothing but
 *  that passphrase — no device, no passkey, no copy of the original vault. */
export const exportVault = (passphrase: string) =>
  request<VaultBackup>('/api/vault/export', {
    method: 'POST',
    body: JSON.stringify({ passphrase })
  });

export const importVault = (backup: VaultBackup, passphrase: string) =>
  request<{ itemsAdded: number; variablesAdded: number }>('/api/vault/import', {
    method: 'POST',
    body: JSON.stringify({ backup, passphrase })
  });

export const auditLog = () =>
  request<{ entries: AuditEntry[] }>('/api/vault/audit').then((r) => r.entries);

/** Password-hygiene report: weak, reused and stale passwords. Value-free —
 *  only ids, names and reasons come back. */
export const vaultHealth = () => request<HealthReport>('/api/vault/health');

/** Check every password against Have I Been Pwned (privacy-preserving: only a
 *  5-char SHA-1 prefix per password leaves the host). Makes external calls, so
 *  it can be slow for a large vault. */
export const breachCheck = () => request<BreachResult>('/api/vault/breach-check');

// ---- generator --------------------------------------------------------------

export type GenerateOptions = {
  mode?: 'password' | 'passphrase';
  length?: number;
  lowercase?: boolean;
  uppercase?: boolean;
  digits?: boolean;
  symbols?: boolean;
  avoidAmbiguous?: boolean;
  words?: number;
  separator?: string;
  capitalize?: boolean;
};

export const generate = (options: GenerateOptions = {}) =>
  request<{ value: string }>('/api/vault/generate', {
    method: 'POST',
    body: JSON.stringify(options)
  }).then((r) => r.value);

// ---- sharing ----------------------------------------------------------------

export const getPublicKey = () =>
  request<{ publicKey: string }>('/api/vault/public-key').then((r) => r.publicKey);

export const createShare = (input: {
  itemId: string;
  recipientPublicKey: string;
  scenario?: string;
  expiresAt?: string;
}) =>
  request<ShareEnvelope>('/api/vault/share', {
    method: 'POST',
    body: JSON.stringify(input)
  });

export const acceptShare = (envelope: ShareEnvelope) =>
  request<{ id: string }>('/api/vault/share/accept', {
    method: 'POST',
    body: JSON.stringify(envelope)
  }).then((r) => r.id);

/** How long a copied secret lingers on the clipboard before being cleared. */
export const CLIPBOARD_CLEAR_SECONDS = 30;

let clipboardClearTimer: ReturnType<typeof setTimeout> | null = null;

async function writeClipboard(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // Fall through to the legacy path.
  }
  try {
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.setAttribute('readonly', '');
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand('copy');
    document.body.removeChild(ta);
    return ok;
  } catch {
    return false;
  }
}

/// Copy to the clipboard, falling back to a hidden textarea where the async
/// Clipboard API is unavailable (non-secure origins, older WebViews).
///
/// `sensitive` copies (passwords, TOTP codes, recovery material) are cleared
/// after CLIPBOARD_CLEAR_SECONDS — the same courtesy Bitwarden and 1Password
/// provide, so a password does not sit in the paste buffer for the next app that
/// reads it. Best effort: the clear only fires if the clipboard still holds what
/// we put there, so it never wipes something the user copied afterwards.
export async function copyToClipboard(text: string, sensitive = false): Promise<boolean> {
  const ok = await writeClipboard(text);
  if (!ok || !sensitive) return ok;

  if (clipboardClearTimer) clearTimeout(clipboardClearTimer);
  clipboardClearTimer = setTimeout(async () => {
    try {
      if (navigator.clipboard && window.isSecureContext) {
        const current = await navigator.clipboard.readText().catch(() => null);
        // Only clear if it is still our secret sitting there.
        if (current === null || current === text) {
          await navigator.clipboard.writeText('');
        }
      }
    } catch {
      // Clipboard read can be denied; nothing more we can safely do.
    }
  }, CLIPBOARD_CLEAR_SECONDS * 1000);

  return ok;
}
