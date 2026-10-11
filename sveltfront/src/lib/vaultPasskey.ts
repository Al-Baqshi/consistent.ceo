// Unlocking the vault with a passkey, via the WebAuthn PRF extension.
//
// # What PRF actually gives us
//
// A passkey normally proves *who you are*. The PRF extension (`hmac-secret` at
// the CTAP layer) additionally lets a relying party ask the authenticator to
// evaluate a pseudorandom function over a salt, using a secret that never leaves
// the hardware. Same authenticator + same credential + same salt always yields
// the same 32 bytes; nothing else can produce them, and they cannot be extracted.
//
// That is exactly the shape of a key-wrapping key. So the vault stores its
// `user_key` wrapped under a key derived from a PRF result, and Face ID becomes a
// way *in* rather than merely a way to log in.
//
// # Why the ceremony is not server-verified
//
// Registration and login ceremonies exist to convince a server that a challenge
// it issued was signed by a credential it trusts. This flow is not doing that —
// the signature is irrelevant here and is never checked. What matters is only the
// PRF output, and its security comes from the authenticator holding the secret
// plus the user verification gesture, not from challenge freshness.
//
// The operations that *do* need authority are guarded server-side instead:
// enrolling a new way in requires an already-unlocked vault, so an attacker with
// a stolen session cannot quietly add their own authenticator.
//
// # Why the raw PRF output never leaves the browser
//
// We HKDF it first. The gateway is trusted with the derived key (it has to be —
// see the trust-model note in `vault_api.rs`), but the authenticator's raw PRF
// output is a more general-purpose secret that may be evaluated for other salts
// and other purposes. Sending a derivation of it costs one WebCrypto call and
// keeps the primitive itself on the device.

import {
  addUnlockMethod,
  getPrfSalt,
  unlockWithKey,
  type UnlockMethod,
  type VaultStatus
} from '$lib/vault';

/** Domain separator, so this key can never collide with another use of the same
 *  PRF output. Changing it invalidates every enrolled passkey — don't. */
const HKDF_INFO = 'consistent-ceo/vault/passkey-unlock/v1';

const PARAM_PRF_SALT = 'prfSaltB64';
const PARAM_CREDENTIAL_ID = 'credentialIdB64';

export class PasskeyUnsupportedError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'PasskeyUnsupportedError';
  }
}

// ---- encoding ----------------------------------------------------------------

function toBase64(bytes: ArrayBuffer | Uint8Array): string {
  const view = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  let binary = '';
  for (const byte of view) binary += String.fromCharCode(byte);
  return btoa(binary);
}

// The explicit `Uint8Array<ArrayBuffer>` return types matter: since TypeScript
// 5.7 a bare `Uint8Array` is `Uint8Array<ArrayBufferLike>`, which could be backed
// by a SharedArrayBuffer and so is not assignable to WebAuthn's `BufferSource`.
// Allocating the ArrayBuffer explicitly pins the narrower type.
function fromBase64(b64: string): Uint8Array<ArrayBuffer> {
  const binary = atob(b64);
  const out = new Uint8Array(new ArrayBuffer(binary.length));
  for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
  return out;
}

/** `evalByCredential` is keyed by base64url credential ids, not standard base64. */
function toBase64Url(bytes: ArrayBuffer | Uint8Array): string {
  return toBase64(bytes).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

function randomBytes(length: number): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(new ArrayBuffer(length));
  crypto.getRandomValues(out);
  return out;
}

// ---- capability ---------------------------------------------------------------

/** Whether this browser can do WebAuthn at all. PRF support is per-authenticator
 *  and cannot be detected up front — it is discovered during enrolment. */
export function isPasskeySupported(): boolean {
  return (
    typeof window !== 'undefined' &&
    typeof window.PublicKeyCredential !== 'undefined' &&
    !!navigator.credentials
  );
}

export function passkeyMethods(methods: UnlockMethod[]): UnlockMethod[] {
  return methods.filter((m) => m.kind === 'passkey' && m.params?.[PARAM_CREDENTIAL_ID]);
}

// ---- key derivation ------------------------------------------------------------

/** HKDF-SHA256 the PRF output to the 32-byte key the vault wraps under. */
async function deriveVaultKey(prfOutput: ArrayBuffer): Promise<string> {
  const baseKey = await crypto.subtle.importKey('raw', prfOutput, 'HKDF', false, ['deriveBits']);
  const bits = await crypto.subtle.deriveBits(
    {
      name: 'HKDF',
      hash: 'SHA-256',
      salt: new Uint8Array(0),
      info: new TextEncoder().encode(HKDF_INFO)
    },
    baseKey,
    256
  );
  return toBase64(bits);
}

type PrfResults = { first?: ArrayBuffer };

function prfResultsOf(credential: PublicKeyCredential): PrfResults | undefined {
  const ext = credential.getClientExtensionResults() as {
    prf?: { enabled?: boolean; results?: PrfResults };
  };
  return ext.prf?.results;
}

// ---- enrolment -----------------------------------------------------------------

/**
 * Create a passkey and enrol it as a way into the vault.
 *
 * Requires the vault to already be unlocked — the server rejects it otherwise.
 *
 * A *new* credential is created rather than reusing the account-login passkey,
 * because most authenticators only make PRF available for credentials that
 * requested it at creation time. Reusing an existing one would work on some
 * devices and mysteriously fail on others.
 */
export async function enrollPasskey(
  label: string,
  user: { id: string; name: string }
): Promise<string> {
  if (!isPasskeySupported()) {
    throw new PasskeyUnsupportedError('This browser does not support passkeys.');
  }

  const saltB64 = await getPrfSalt();

  const created = (await navigator.credentials.create({
    publicKey: {
      // Never verified — see the note at the top of this file.
      challenge: randomBytes(32),
      rp: { id: window.location.hostname, name: 'Consistent' },
      user: {
        id: new TextEncoder().encode(user.id),
        name: user.name,
        displayName: user.name
      },
      pubKeyCredParams: [
        { type: 'public-key', alg: -7 }, // ES256
        { type: 'public-key', alg: -257 } // RS256
      ],
      authenticatorSelection: {
        residentKey: 'preferred',
        // The whole point is that opening the vault takes a deliberate gesture.
        userVerification: 'required'
      },
      timeout: 60_000,
      extensions: { prf: {} } as AuthenticationExtensionsClientInputs
    }
  })) as PublicKeyCredential | null;

  if (!created) throw new Error('Passkey creation was cancelled.');

  const enabled = (
    created.getClientExtensionResults() as { prf?: { enabled?: boolean } }
  ).prf?.enabled;
  if (enabled === false) {
    throw new PasskeyUnsupportedError(
      'This authenticator cannot derive an encryption key (no PRF support). Try a platform passkey — Face ID, Touch ID or Windows Hello — or a recent security key.'
    );
  }

  // PRF results are not reliably returned by create(), so evaluate it now with a
  // get() against the credential we just made.
  const keyB64 = await evaluatePrf(created.rawId, saltB64);

  return addUnlockMethod({
    kind: 'passkey',
    label,
    key: keyB64,
    params: {
      [PARAM_PRF_SALT]: saltB64,
      [PARAM_CREDENTIAL_ID]: toBase64(created.rawId)
    }
  });
}

/** Run one PRF evaluation against a known credential. */
async function evaluatePrf(credentialId: ArrayBuffer, saltB64: string): Promise<string> {
  const assertion = (await navigator.credentials.get({
    publicKey: {
      challenge: randomBytes(32),
      allowCredentials: [{ type: 'public-key', id: credentialId }],
      userVerification: 'required',
      timeout: 60_000,
      extensions: {
        prf: { eval: { first: fromBase64(saltB64) } }
      } as AuthenticationExtensionsClientInputs
    }
  })) as PublicKeyCredential | null;

  if (!assertion) throw new Error('Passkey verification was cancelled.');

  const first = prfResultsOf(assertion)?.first;
  if (!first) {
    throw new PasskeyUnsupportedError(
      'This authenticator did not return a key (no PRF support).'
    );
  }
  return deriveVaultKey(first);
}

// ---- unlocking -------------------------------------------------------------------

/**
 * Open the vault with an enrolled passkey.
 *
 * Every enrolled passkey is offered at once. Each has its own salt, so the salts
 * are passed per credential via `evalByCredential` — a single `eval` would give
 * every authenticator the same salt and only the first-enrolled one would produce
 * the right key.
 */
export async function unlockWithPasskey(methods: UnlockMethod[]): Promise<VaultStatus> {
  if (!isPasskeySupported()) {
    throw new PasskeyUnsupportedError('This browser does not support passkeys.');
  }

  const candidates = passkeyMethods(methods);
  if (candidates.length === 0) {
    throw new Error('No passkey is enrolled for this vault.');
  }

  const evalByCredential: Record<string, { first: Uint8Array }> = {};
  const byCredentialId = new Map<string, UnlockMethod>();

  for (const method of candidates) {
    const credentialIdB64 = method.params![PARAM_CREDENTIAL_ID];
    const salt = method.params![PARAM_PRF_SALT];
    if (!salt) continue;
    const raw = fromBase64(credentialIdB64);
    evalByCredential[toBase64Url(raw)] = { first: fromBase64(salt) };
    byCredentialId.set(toBase64(raw), method);
  }

  const assertion = (await navigator.credentials.get({
    publicKey: {
      challenge: randomBytes(32),
      allowCredentials: candidates.map((m) => ({
        type: 'public-key' as const,
        id: fromBase64(m.params![PARAM_CREDENTIAL_ID])
      })),
      userVerification: 'required',
      timeout: 60_000,
      extensions: { prf: { evalByCredential } } as AuthenticationExtensionsClientInputs
    }
  })) as PublicKeyCredential | null;

  if (!assertion) throw new Error('Passkey verification was cancelled.');

  // Which of the offered credentials actually answered.
  const method = byCredentialId.get(toBase64(assertion.rawId));
  if (!method) {
    throw new Error('That passkey is not enrolled for this vault.');
  }

  const first = prfResultsOf(assertion)?.first;
  if (!first) {
    throw new PasskeyUnsupportedError(
      'This authenticator did not return a key (no PRF support).'
    );
  }

  return unlockWithKey(method.id, await deriveVaultKey(first));
}
