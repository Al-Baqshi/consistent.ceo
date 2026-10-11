// Unlocking the vault with this computer, when running inside the desktop shell.
//
// The heavy lifting is in Rust (`desktop/src-tauri/src/vault.rs`), because the
// device key belongs in the OS keychain and a webview has no business holding it.
// This module is the thin bridge, plus the one piece of logic that has to live on
// this side: deciding whether to *offer* the button.
//
// In a plain browser every export here is inert — `isDesktopUnlockSupported()`
// returns false and the vault page falls back to passkeys. See `$lib/tauri`.

import { invoke, isTauri } from '$lib/tauri';
import type { UnlockMethod, VaultStatus } from '$lib/vault';

export type DesktopDeviceStatus = {
  supported: boolean;
  enrolled: boolean;
  methodId?: string;
};

/** Whether this is the desktop shell rather than a browser tab. */
export function isDesktopUnlockSupported(): boolean {
  return isTauri();
}

export async function desktopDeviceStatus(): Promise<DesktopDeviceStatus> {
  if (!isTauri()) return { supported: false, enrolled: false };
  try {
    return await invoke<DesktopDeviceStatus>('vault_device_status');
  } catch {
    // An older shell without these commands, or no keychain on this box.
    return { supported: false, enrolled: false };
  }
}

/**
 * Whether to offer the unlock button.
 *
 * Checks the vault's own method list, not just the keychain: a machine revoked
 * from another device still has its key locally, and a button that can only fail
 * is worse than no button.
 */
export async function canUnlockWithComputer(methods: UnlockMethod[]): Promise<boolean> {
  const status = await desktopDeviceStatus();
  return (
    status.enrolled &&
    status.methodId !== undefined &&
    methods.some((m) => m.id === status.methodId)
  );
}

/** Enrol this machine. Requires the vault to be unlocked; the gateway enforces it. */
export async function enrollThisComputer(label: string): Promise<string> {
  return invoke<string>('vault_enroll_device', { label });
}

/** Unlock with this machine's key. Returns the gateway's fresh vault status. */
export async function unlockWithComputer(): Promise<VaultStatus> {
  return invoke<VaultStatus>('vault_unlock_with_device');
}

/** Revoke this machine everywhere, then forget it locally. */
export async function revokeThisComputer(): Promise<void> {
  return invoke<void>('vault_revoke_device');
}
