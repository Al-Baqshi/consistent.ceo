// Opening the vault with Face ID / Touch ID / the device biometric.
//
// # Why not a passkey
//
// On the web the vault unlocks via the WebAuthn PRF extension: the authenticator
// derives a key from a secret that never leaves the hardware. React Native has no
// WebAuthn, so mobile uses the closest equivalent the platform actually offers —
// a random 32-byte *device key* held in the iOS Keychain / Android Keystore with
// an access-control flag that forces a biometric check before it can be read.
//
// The vault treats both the same way: they are `unlock_methods` whose key
// material is 32 uniform bytes, wrapping the same `user_key`. The server does not
// care which platform produced them, which is why enrolling a phone does not
// disturb a passkey enrolled in a browser.
//
// # What this does and does not protect
//
// The key is per-device and per-install. Revoking a lost phone is a server-side
// operation (remove the unlock method) and takes effect immediately even though
// the key is still sitting in that phone's keychain — the wrapping it opens no
// longer exists.
//
// `requireAuthentication` is what makes this worth doing. Without it the key
// would be readable by anything running as this app, and "unlock" would mean
// nothing more than "the app is installed".

import { Platform } from 'react-native';
import * as SecureStore from 'expo-secure-store';

import { vault, type UnlockMethod, type VaultStatus } from '@/api/vault';

/** SecureStore keys must be alphanumeric plus `.`, `-` and `_`. */
const DEVICE_KEY = 'consistent.vault.deviceKey';
const METHOD_ID_KEY = 'consistent.vault.deviceMethodId';

/** Reading the key must cost a biometric check, and the key must never ride a
 *  backup to a new phone — that phone should enrol its own. */
const KEY_OPTIONS: SecureStore.SecureStoreOptions = {
  requireAuthentication: true,
  keychainAccessible: SecureStore.WHEN_UNLOCKED_THIS_DEVICE_ONLY,
  authenticationPrompt: 'Unlock your vault',
};

/** The method id is not secret and is read before any biometric prompt, so it is
 *  stored without one. Gating it too would mean a second prompt per unlock. */
const ID_OPTIONS: SecureStore.SecureStoreOptions = {
  keychainAccessible: SecureStore.WHEN_UNLOCKED_THIS_DEVICE_ONLY,
};

export class BiometricUnavailableError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'BiometricUnavailableError';
  }
}

/** SecureStore is a no-op shim on web; this whole flow is native-only. */
export function isDeviceUnlockSupported(): boolean {
  return Platform.OS === 'ios' || Platform.OS === 'android';
}

/** Whether this install has a device key enrolled and still knows its method id. */
export async function enrolledMethodId(): Promise<string | null> {
  if (!isDeviceUnlockSupported()) return null;
  try {
    return await SecureStore.getItemAsync(METHOD_ID_KEY, ID_OPTIONS);
  } catch {
    return null;
  }
}

/**
 * Whether device unlock is ready to offer.
 *
 * Checks the server's list too, not just local state: a phone whose method was
 * revoked from another device still has the key in its keychain, and offering a
 * Face ID button that always fails is worse than not offering one.
 */
export async function canUnlockWithDevice(methods: UnlockMethod[]): Promise<boolean> {
  const id = await enrolledMethodId();
  return id !== null && methods.some((m) => m.id === id);
}

/**
 * Enrol this device. Requires the vault to already be unlocked — the server
 * rejects it otherwise, which is what stops a stolen session adding its own way
 * in.
 */
export async function enrollDevice(label: string): Promise<string> {
  if (!isDeviceUnlockSupported()) {
    throw new BiometricUnavailableError('Device unlock is only available on iOS and Android.');
  }

  // Generated server-side: React Native ships no WebCrypto, and enrolling any
  // method means sending its key to the gateway regardless. What matters is
  // where it comes to rest — the keychain, behind biometrics.
  const keyB64 = await vault.randomKey();

  try {
    await SecureStore.setItemAsync(DEVICE_KEY, keyB64, KEY_OPTIONS);
  } catch (e) {
    throw new BiometricUnavailableError(
      'Could not store the key securely. Set up a passcode and Face ID or Touch ID on this device first.',
    );
  }

  let methodId: string;
  try {
    methodId = await vault.addUnlockMethod({ kind: 'device-key', label, key: keyB64 });
  } catch (e) {
    // Do not leave a key in the keychain that opens nothing.
    await SecureStore.deleteItemAsync(DEVICE_KEY, KEY_OPTIONS).catch(() => {});
    throw e;
  }

  await SecureStore.setItemAsync(METHOD_ID_KEY, methodId, ID_OPTIONS);
  return methodId;
}

/**
 * Open the vault with the device key. Prompts for biometrics as a side effect of
 * reading it.
 *
 * If the method has been revoked server-side the local key is cleared, so the
 * next launch offers the password instead of a button that cannot work.
 */
export async function unlockWithDevice(): Promise<VaultStatus> {
  const methodId = await enrolledMethodId();
  if (!methodId) throw new Error('This device is not enrolled.');

  // Throws if the user cancels or fails the biometric check.
  const keyB64 = await SecureStore.getItemAsync(DEVICE_KEY, KEY_OPTIONS);
  if (!keyB64) {
    await forgetDevice();
    throw new Error('The stored key is gone. Unlock with your master password and re-enrol.');
  }

  try {
    return await vault.unlockWithKey(methodId, keyB64);
  } catch (e) {
    // 401 here means the wrapping is gone: revoked elsewhere.
    if ((e as { status?: number }).status === 401) {
      await forgetDevice();
      throw new Error('This device was removed from your vault. Unlock with your password.');
    }
    throw e;
  }
}

/** Drop this device's local key. Does not revoke it server-side — use
 *  `vault.removeUnlockMethod` for that, which is what actually stops it working. */
export async function forgetDevice(): Promise<void> {
  await SecureStore.deleteItemAsync(DEVICE_KEY, KEY_OPTIONS).catch(() => {});
  await SecureStore.deleteItemAsync(METHOD_ID_KEY, ID_OPTIONS).catch(() => {});
}

/** Revoke this device everywhere, then forget it locally. */
export async function revokeThisDevice(): Promise<void> {
  const methodId = await enrolledMethodId();
  if (methodId) {
    await vault.removeUnlockMethod(methodId);
  }
  await forgetDevice();
}
