// The phone's pairing key: an Ed25519 keypair made on this phone, whose
// secret half never leaves its keychain.
//
// It is what lets the phone sign itself back in to the computer it was paired
// with, from anywhere, without a password. The computer only ever sees the
// public half. Protocol and message formats mirror
// `crates/gateway/src/pairing.rs` exactly; change both together.
//
// Randomness comes from `expo-crypto` (the OS CSPRNG), never from the
// gateway: a key the gateway helped choose would be a key it could know.

import * as ed from '@noble/ed25519';
import { sha256, sha512 } from '@noble/hashes/sha2.js';
import { getRandomBytes } from 'expo-crypto';

ed.hashes.sha512 = sha512;

const B64URL = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_';

/** base64url without padding, matching the gateway's `URL_SAFE_NO_PAD`. */
export function toB64url(bytes: Uint8Array): string {
  let out = '';
  let i = 0;
  for (; i + 2 < bytes.length; i += 3) {
    const n = (bytes[i] << 16) | (bytes[i + 1] << 8) | bytes[i + 2];
    out += B64URL[(n >> 18) & 63] + B64URL[(n >> 12) & 63] + B64URL[(n >> 6) & 63] + B64URL[n & 63];
  }
  const rest = bytes.length - i;
  if (rest === 1) {
    const n = bytes[i] << 16;
    out += B64URL[(n >> 18) & 63] + B64URL[(n >> 12) & 63];
  } else if (rest === 2) {
    const n = (bytes[i] << 16) | (bytes[i + 1] << 8);
    out += B64URL[(n >> 18) & 63] + B64URL[(n >> 12) & 63] + B64URL[(n >> 6) & 63];
  }
  return out;
}

export function fromB64url(text: string): Uint8Array {
  const clean = text.trim().replace(/=+$/, '');
  const out: number[] = [];
  let buf = 0;
  let bits = 0;
  for (const ch of clean) {
    const v = B64URL.indexOf(ch);
    if (v < 0) throw new Error('invalid base64url');
    buf = (buf << 6) | v;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      out.push((buf >> bits) & 0xff);
    }
  }
  return Uint8Array.from(out);
}

function utf8(text: string): Uint8Array {
  // UTF-8 without relying on TextEncoder, which not every JS engine the app
  // runs on provides. Pairing strings are ASCII in practice.
  const out: number[] = [];
  for (const ch of text) {
    let c = ch.codePointAt(0) ?? 0;
    if (c < 0x80) out.push(c);
    else if (c < 0x800) out.push(0xc0 | (c >> 6), 0x80 | (c & 63));
    else if (c < 0x10000) out.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63));
    else {
      out.push(0xf0 | (c >> 18), 0x80 | ((c >> 12) & 63), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63));
    }
  }
  return Uint8Array.from(out);
}

export type PairingKey = {
  /** 32-byte secret, base64url. Stored only in this phone's keychain. */
  secretKey: string;
  /** 32-byte public key, base64url. The only half the computer sees. */
  publicKey: string;
};

export function newPairingKey(): PairingKey {
  const secret = getRandomBytes(32);
  return {
    secretKey: toB64url(secret),
    publicKey: toB64url(ed.getPublicKey(secret)),
  };
}

export function sign(secretKey: string, message: string): string {
  return toB64url(ed.sign(utf8(message), fromB64url(secretKey)));
}

export const enrollMessage = (nonce: string, publicKey: string) =>
  `consistent-pair-enroll|${nonce}|${publicKey}`;
export const collectMessage = (requestId: string) => `consistent-pair-collect|${requestId}`;
export const loginMessage = (challengeId: string, challenge: string) =>
  `consistent-device-login|${challengeId}|${challenge}`;

/**
 * The six digits both screens show. Computed here before anything is sent,
 * so the number on this phone comes from this phone's own key.
 */
export function confirmationCode(nonce: string, publicKey: string): string {
  const d = sha256(utf8(`consistent-pair-code|${nonce}|${publicKey}`));
  const n = ((d[0] << 24) >>> 0) + (d[1] << 16) + (d[2] << 8) + d[3];
  return String(n % 1_000_000).padStart(6, '0');
}

/** "482913" → "482 913", how both screens print it. */
export function spacedCode(code: string): string {
  return code.length === 6 ? `${code.slice(0, 3)} ${code.slice(3)}` : code;
}
