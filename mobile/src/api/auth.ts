// Own-auth API: username/password, the device-approval authenticator, and
// account deletion. (The phone never shipped passkey sign-in; its unused
// client, which needed a native module the app doesn't bundle, was removed.)
//
// Unlike the rest of endpoints.ts, these MUST hit the live gateway (no mock
// fallback) — authentication can't be faked. On success the session token is
// stored in the device keychain via setAuthToken.
import {
  clearAuthToken,
  clearDeviceIdentity,
  getAuthToken,
  getDeviceIdentity,
  getGatewayUrl,
  http,
  setAuthToken,
  setDeviceIdentity,
} from './client';
// Loaded on first use: it pulls in the native random-bytes module, and an app
// build without it should still start and sign in the old way.
import type { PairingKey } from '@/lib/pairingKey';
const pairingKey = () => import('@/lib/pairingKey');

export type AuthUser = {
  userId: string;
  username: string;
  workspaceId: string;
  email?: string;
  phone?: string;
};

export type AuthToken = {
  token: string;
  userId: string;
  username: string;
  workspaceId: string;
  /** Present only on register — shown to the user exactly once. */
  recoveryCodes?: string[];
};

export type PendingApproval = {
  requestId: string;
  code: string;
  context?: string;
  createdAt: string;
  expiresAt: string;
};

async function persist(t: AuthToken): Promise<AuthToken> {
  await setAuthToken(t.token);
  return t;
}

export const auth = {
  async checkUsername(username: string): Promise<boolean> {
    const r = await http.post<{ available: boolean }>('/api/auth/check-username', { username });
    return r.available;
  },

  async register(
    username: string,
    password: string,
    opts?: { email?: string; phone?: string; device?: string },
  ): Promise<AuthToken> {
    return persist(await http.postAuth<AuthToken>('/api/auth/register', { username, password, ...opts }));
  },

  async login(username: string, password: string, device?: string): Promise<AuthToken> {
    return persist(await http.postAuth<AuthToken>('/api/auth/login', { username, password, device }));
  },

  async me(): Promise<AuthUser> {
    return http.get<AuthUser>('/api/auth/me');
  },

  async logout(): Promise<void> {
    // A paired phone that only dropped its session would sign itself straight
    // back in with its key. Signing out means leaving: remove the device on
    // the computer, then forget the key here.
    const identity = await getDeviceIdentity();
    try {
      if (identity) await http.del(`/api/auth/devices/${encodeURIComponent(identity.deviceId)}`);
      else await http.post('/api/auth/logout');
    } catch {
      // best-effort server revoke; always clear locally
    }
    await clearDeviceIdentity();
    await clearAuthToken();
  },

  // ----- Pairing by presence (gateway `pairing.rs`) -----

  /**
   * Answer a scanned pairing code. Makes this phone's key, sends the public
   * half signed, and returns the request to wait on plus the six digits to
   * show. The digits are computed here from this phone's own key.
   */
  async startPairing(
    nonce: string,
    label: string,
    platform: string,
  ): Promise<{ requestId: string; code: string; key: PairingKey }> {
    const { confirmationCode, enrollMessage, newPairingKey, sign } = await pairingKey();
    const key = newPairingKey();
    const code = confirmationCode(nonce, key.publicKey);
    const r = await http.post<{ requestId: string; code: string }>('/api/pair/enroll', {
      nonce,
      publicKey: key.publicKey,
      signature: sign(key.secretKey, enrollMessage(nonce, key.publicKey)),
      label,
      platform,
    });
    if (r.code !== code) {
      // The computer derived different digits from what this phone sent.
      throw new Error('The computer answered with a different code. Scan again.');
    }
    return { requestId: r.requestId, code, key };
  },

  /**
   * Ask whether the computer allowed this phone. On approval the session and
   * the device identity are stored, and the phone is signed in.
   */
  async collectPairing(
    requestId: string,
    key: PairingKey,
  ): Promise<'pending' | 'approved' | 'denied'> {
    const { collectMessage, sign } = await pairingKey();
    const r = await http.post<{ status: 'pending' | 'approved' | 'denied'; token?: string; deviceId?: string }>(
      `/api/pair/requests/${encodeURIComponent(requestId)}/collect`,
      { signature: sign(key.secretKey, collectMessage(requestId)) },
    );
    if (r.status === 'approved' && r.token && r.deviceId) {
      await setAuthToken(r.token);
      await setDeviceIdentity({
        gatewayUrl: await getGatewayUrl(),
        deviceId: r.deviceId,
        secretKey: key.secretKey,
      });
    }
    return r.status;
  },

  /** Whether this phone is paired by key (signs itself back in). */
  async isPairedByKey(): Promise<boolean> {
    const identity = await getDeviceIdentity();
    return !!identity && identity.gatewayUrl === (await getGatewayUrl());
  },

  async isSignedIn(): Promise<boolean> {
    return !!(await getAuthToken());
  },

  // ----- This phone as an authenticator (approves logins on other devices) -----

  async registerDevice(label: string, platform: string): Promise<string> {
    const r = await http.post<{ deviceId: string }>('/api/auth/devices', { label, platform });
    return r.deviceId;
  },

  async pendingApprovals(): Promise<PendingApproval[]> {
    return http.get<PendingApproval[]>('/api/auth/approvals');
  },

  /**
   * Approve a sign-in. `code` is the six digits the gateway returned with the
   * request and the app shows on screen — the server requires it, so a request
   * this phone never listed (i.e. not this user's) cannot be approved, and the
   * digits the person is comparing against the other device are the same ones
   * being checked.
   */
  async approve(requestId: string, code: string, device?: string): Promise<void> {
    await http.post(`/api/auth/approvals/${requestId}/approve`, { code, device });
  },

  async deny(requestId: string): Promise<void> {
    await http.post(`/api/auth/approvals/${requestId}/deny`, {});
  },

  /**
   * Delete this account from the computer it lives on (`DELETE
   * /api/auth/account`, App Review 5.1.1(v)). The gateway re-checks the
   * password, then removes the account's identity and sign-in data and its
   * personal vault; see `delete_account` in `crates/gateway/src/auth_api.rs`
   * for exactly what is and isn't removed. On success the session token is
   * already dead server-side, so it is cleared here too.
   */
  async deleteAccount(password: string): Promise<void> {
    await http.delAuth('/api/auth/account', { password });
    await clearAuthToken();
  },
};
