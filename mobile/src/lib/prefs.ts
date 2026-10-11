import * as SecureStore from 'expo-secure-store';
import { Platform } from 'react-native';

/**
 * Tiny persisted preferences, SecureStore-backed like the rest of the app
 * (with the same in-memory shim on web). For non-secret flags SecureStore is
 * overkill but keeps a single storage story and zero new dependencies.
 */

const mem: Record<string, string> = {};

export async function getPref(key: string): Promise<string | null> {
  if (Platform.OS === 'web') return mem[key] ?? null;
  try {
    return await SecureStore.getItemAsync(key);
  } catch {
    return null;
  }
}

export async function setPref(key: string, value: string): Promise<void> {
  if (Platform.OS === 'web') {
    mem[key] = value;
    return;
  }
  try {
    await SecureStore.setItemAsync(key, value);
  } catch {
    /* best-effort */
  }
}

export async function getBoolPref(key: string): Promise<boolean> {
  return (await getPref(key)) === '1';
}

export async function setBoolPref(key: string, value: boolean): Promise<void> {
  await setPref(key, value ? '1' : '0');
}

export const PREF_NOTIF_ON = 'cw.pref.notifOn';
export const PREF_PUSH_ON = 'cw.pref.pushOn';
export const PREF_VOICE_MUTED = 'cw.pref.voiceMuted';
