import React, { useCallback, useEffect, useRef, useState } from 'react';
import { Alert, Platform, Pressable, ScrollView, TextInput, View } from 'react-native';

import { NativeButton, NeedsComputer, Screen, SectionHeader, Txt } from '@/components';
import {
  GLOBAL_SCOPE,
  KIND_LABELS,
  UNLOCK_KIND_LABELS,
  VaultLockedError,
  vault,
  type BreachResult,
  type HealthReport,
  type ItemKind,
  type ItemScopeSummary,
  type ItemSecret,
  type ItemSummary,
  type PasswordStrength,
  type ScopeSummary,
  type TotpCode,
  type UnlockMethod,
  type VaultStatus,
} from '@/api/vault';
import {
  BiometricUnavailableError,
  canUnlockWithDevice,
  enrollDevice,
  isDeviceUnlockSupported,
  revokeThisDevice,
  unlockWithDevice,
} from '@/lib/vaultDeviceKey';
import { haptics } from '@/lib/haptics';
import { useDemoMode } from '@/lib/useDemoMode';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

type Tab = 'passwords' | 'environment' | 'security';

export default function VaultScreen() {
  const theme = useTheme();

  const [status, setStatus] = useState<VaultStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [tab, setTab] = useState<Tab>('passwords');
  // Demo mode has no vault to open; say what it is instead of a connection error.
  const demo = useDemoMode();

  const [masterPassword, setMasterPassword] = useState('');
  const [confirmPassword, setConfirmPassword] = useState('');

  const [items, setItems] = useState<ItemSummary[]>([]);
  // Each entity has its own password set; `itemScope` is which one is shown.
  const [itemScope, setItemScope] = useState(GLOBAL_SCOPE);
  const [itemScopeList, setItemScopeList] = useState<ItemScopeSummary[]>([]);
  const [newItemEntity, setNewItemEntity] = useState('');
  const [search, setSearch] = useState('');
  const [openId, setOpenId] = useState<string | null>(null);
  const [openSecret, setOpenSecret] = useState<ItemSecret | null>(null);
  const [revealed, setRevealed] = useState(false);

  const [secretKeys, setSecretKeys] = useState<string[]>([]);
  const [revealedSecrets, setRevealedSecrets] = useState<Record<string, string>>({});
  const [newKey, setNewKey] = useState('');
  const [newValue, setNewValue] = useState('');

  const [composing, setComposing] = useState(false);
  const [draftName, setDraftName] = useState('');
  const [draftUsername, setDraftUsername] = useState('');
  const [draftPassword, setDraftPassword] = useState('');

  // Which scope the Environment tab is editing.
  const [scope, setScope] = useState(GLOBAL_SCOPE);
  const [scopes, setScopes] = useState<ScopeSummary[]>([]);
  const [newScopeEntity, setNewScopeEntity] = useState('');

  // Shown once, at creation. Held here until acknowledged; there is nothing to
  // fetch it back from afterwards.
  const [recoveryCode, setRecoveryCode] = useState<string | null>(null);
  const [recoveryInput, setRecoveryInput] = useState('');
  const [showRecoveryUnlock, setShowRecoveryUnlock] = useState(false);

  // Whether this install can offer Face ID / Touch ID.
  const [deviceReady, setDeviceReady] = useState(false);
  const deviceSupported = isDeviceUnlockSupported();

  // Live TOTP for the open item, and master-password strength at creation.
  const [openTotp, setOpenTotp] = useState<TotpCode | null>(null);
  const totpTimer = useRef<ReturnType<typeof setInterval> | null>(null);
  const [strength, setStrength] = useState<PasswordStrength | null>(null);
  const strengthTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Password-health report and Have I Been Pwned breach check (Security tab).
  const [health, setHealth] = useState<HealthReport | null>(null);
  const [breach, setBreach] = useState<BreachResult | null>(null);
  const [breachRunning, setBreachRunning] = useState(false);

  // Stop timers when the screen unmounts.
  useEffect(
    () => () => {
      if (totpTimer.current) clearInterval(totpTimer.current);
      if (strengthTimer.current) clearTimeout(strengthTimer.current);
    },
    [],
  );

  const inputStyle = {
    backgroundColor: theme.colors.bgCard,
    borderWidth: 1,
    borderColor: theme.colors.border,
    borderRadius: radius.md,
    paddingHorizontal: spacing[3],
    paddingVertical: spacing[3],
    color: theme.colors.textPrimary,
    fontSize: 15,
  } as const;

  const cardStyle = {
    backgroundColor: theme.colors.bgCard,
    borderWidth: 1,
    borderColor: theme.colors.border,
    borderRadius: radius.lg,
    padding: spacing[4],
    gap: spacing[3],
  } as const;

  /// A 423 can arrive on any call once the gateway's idle timer fires; when it
  /// does, fall back to the unlock screen rather than surfacing an error.
  const guard = useCallback(async <T,>(fn: () => Promise<T>): Promise<T | null> => {
    try {
      setError(null);
      return await fn();
    } catch (e) {
      if (e instanceof VaultLockedError) {
        setOpenId(null);
        setOpenSecret(null);
        setStatus(await vault.status().catch(() => null));
        return null;
      }
      setError(e instanceof Error ? e.message : String(e));
      return null;
    }
  }, []);

  const loadAll = useCallback(async () => {
    const [loadedItems, loadedItemScopes, loadedKeys, loadedScopes] = await Promise.all([
      guard(() => vault.listItems(itemScope)),
      guard(() => vault.itemScopes()),
      guard(() => vault.listSecrets(scope)),
      guard(() => vault.listScopes()),
    ]);
    if (loadedItems) setItems(loadedItems);
    if (loadedItemScopes) setItemScopeList(loadedItemScopes);
    if (loadedKeys) setSecretKeys(loadedKeys);
    if (loadedScopes) setScopes(loadedScopes);
  }, [guard, scope, itemScope]);

  async function switchItemScope(next: string) {
    setItemScope(next);
    setOpenId(null);
    setOpenSecret(null);
    const [loaded, loadedScopes] = await Promise.all([
      guard(() => vault.listItems(next)),
      guard(() => vault.itemScopes()),
    ]);
    setItems(loaded ?? []);
    if (loadedScopes) setItemScopeList(loadedScopes);
  }

  function addItemEntity() {
    const id = newItemEntity.trim();
    if (!id) return;
    if (!/^[A-Za-z0-9._-]+$/.test(id)) {
      setError('An entity id may only contain letters, digits, dots, dashes and underscores');
      return;
    }
    setNewItemEntity('');
    switchItemScope(`entity:${id}`);
  }

  const refresh = useCallback(async () => {
    const s = await vault.status().catch(() => null);
    setStatus(s);
    // Only offer Face ID if this device is enrolled *and* the server still
    // recognises that enrolment — it may have been revoked from elsewhere.
    setDeviceReady(s ? await canUnlockWithDevice(s.unlockMethods ?? []) : false);
    if (s?.unlocked) await loadAll();
  }, [loadAll]);

  useEffect(() => {
    if (demo === null) return;
    if (demo) {
      setLoading(false);
      return;
    }
    refresh().finally(() => setLoading(false));
  }, [refresh, demo]);

  async function create() {
    if (masterPassword !== confirmPassword) return setError('Passwords do not match');
    if (masterPassword.length < 8) return setError('Use at least 8 characters');
    // A very weak master password is worth a deliberate confirmation — nothing
    // sits behind it, and it cannot be reset if forgotten.
    if (strength && strength.score <= 1) {
      const proceed = await new Promise<boolean>((resolve) =>
        Alert.alert(
          'Weak master password',
          `That password is ${strength.label.toLowerCase()}. It protects everything in your vault and cannot be reset. Use it anyway?`,
          [
            { text: 'Choose a stronger one', style: 'cancel', onPress: () => resolve(false) },
            { text: 'Use anyway', style: 'destructive', onPress: () => resolve(true) },
          ],
        ),
      );
      if (!proceed) return;
    }
    setBusy(true);
    const next = await guard(() => vault.init(masterPassword));
    setBusy(false);
    if (next) {
      haptics.success();
      // Capture before anything can navigate away — this response is the only
      // place the recovery code will ever exist.
      setRecoveryCode(next.recoveryCode);
      setStatus(next);
      setMasterPassword('');
      setConfirmPassword('');
      setStrength(null);
      await loadAll();
    }
  }

  async function unlock() {
    setBusy(true);
    const next = await guard(() => vault.unlock(masterPassword));
    setBusy(false);
    if (next) {
      haptics.success();
      setStatus(next);
      setMasterPassword('');
      await loadAll();
    }
  }

  async function unlockByDevice() {
    setBusy(true);
    try {
      const next = await unlockWithDevice();
      haptics.success();
      setStatus(next);
      await loadAll();
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      // A cancelled Face ID prompt is a normal thing to do, not an error.
      if (!/cancel|UserFallback|user_cancel/i.test(message)) setError(message);
      await refresh();
    } finally {
      setBusy(false);
    }
  }

  async function unlockByRecovery() {
    setBusy(true);
    const next = await guard(() => vault.unlockWithRecoveryCode(recoveryInput));
    setBusy(false);
    if (next) {
      haptics.success();
      setStatus(next);
      setRecoveryInput('');
      setShowRecoveryUnlock(false);
      await loadAll();
    }
  }

  async function loadHealth() {
    const report = await guard(() => vault.health());
    if (report) setHealth(report);
  }

  async function runBreachCheck() {
    setBreachRunning(true);
    const result = await guard(() => vault.breachCheck());
    setBreachRunning(false);
    if (result) setBreach(result);
  }

  async function enrollThisDevice() {
    setBusy(true);
    try {
      await enrollDevice(Platform.OS === 'ios' ? 'iPhone' : 'Android device');
      haptics.success();
      await refresh();
      Alert.alert('Device added', 'You can now unlock this vault with Face ID or Touch ID.');
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      if (e instanceof BiometricUnavailableError) setError(message);
      else if (!/cancel/i.test(message)) setError(message);
    } finally {
      setBusy(false);
    }
  }

  function confirmRevokeDevice() {
    Alert.alert(
      'Remove this device?',
      'Face ID will no longer open your vault here. You can still unlock with your master password or a recovery code.',
      [
        { text: 'Cancel', style: 'cancel' },
        {
          text: 'Remove',
          style: 'destructive',
          onPress: async () => {
            const ok = await guard(() => revokeThisDevice());
            if (ok !== null) await refresh();
          },
        },
      ],
    );
  }

  async function mintRecoveryCode() {
    const code = await guard(() => vault.newRecoveryCode());
    if (code) {
      setRecoveryCode(code);
      await refresh();
    }
  }

  async function switchScope(next: string) {
    setScope(next);
    setRevealedSecrets({});
    const keys = await guard(() => vault.listSecrets(next));
    setSecretKeys(keys ?? []);
  }

  function addEntityScope() {
    const id = newScopeEntity.trim();
    if (!id) return;
    if (!/^[A-Za-z0-9._-]+$/.test(id)) {
      setError('An entity id may only contain letters, digits, dots, dashes and underscores');
      return;
    }
    setNewScopeEntity('');
    switchScope(`entity:${id}`);
  }

  async function lock() {
    await vault.lock().catch(() => {});
    setOpenId(null);
    setOpenSecret(null);
    setItems([]);
    setSecretKeys([]);
    setRevealedSecrets({});
    await refresh();
  }

  async function toggleItem(id: string) {
    if (openId === id) {
      setOpenId(null);
      setOpenSecret(null);
      stopTotp();
      return;
    }
    const secret = await guard(() => vault.itemSecret(id));
    if (secret) {
      setOpenId(id);
      setOpenSecret(secret);
      setRevealed(false);
      startTotp(id, secret);
    }
  }

  /** Poll the live TOTP code for an item with a 2FA seed. The seed stays on the
   *  server; only the digits come back. */
  function startTotp(id: string, secret: ItemSecret) {
    stopTotp();
    if (!secret.totpSecret) {
      setOpenTotp(null);
      return;
    }
    const tick = async () => {
      const code = await vault.itemTotp(id).catch(() => null);
      setOpenTotp(code);
    };
    tick();
    totpTimer.current = setInterval(tick, 1000);
  }

  function stopTotp() {
    if (totpTimer.current) {
      clearInterval(totpTimer.current);
      totpTimer.current = null;
    }
    setOpenTotp(null);
  }

  /** Score the candidate master password, debounced, with the workspace as
   *  weak-for-this-user context. */
  function onMasterPasswordChange(value: string) {
    setMasterPassword(value);
    if (strengthTimer.current) clearTimeout(strengthTimer.current);
    if (value.length < 4) {
      setStrength(null);
      return;
    }
    strengthTimer.current = setTimeout(async () => {
      const result = await vault
        .passwordStrength(value, ['consistent', 'vault'])
        .catch(() => null);
      setStrength(result);
    }, 250);
  }

  async function saveDraft() {
    if (!draftName.trim()) return setError('Give the item a name');
    setBusy(true);
    const id = await guard(() =>
      vault.createItem({
        kind: 'login' as ItemKind,
        name: draftName.trim(),
        // New items land in whichever entity's set is open.
        scope: itemScope,
        secret: { username: draftUsername || null, password: draftPassword || null },
      }),
    );
    setBusy(false);
    if (id) {
      setComposing(false);
      setDraftName('');
      setDraftUsername('');
      setDraftPassword('');
      await loadAll();
    }
  }

  async function generatePassword() {
    const value = await guard(() => vault.generate({ mode: 'password', length: 20 }));
    if (value) setDraftPassword(value);
  }

  function removeItem(item: ItemSummary) {
    Alert.alert('Delete item', `Delete "${item.name}"? This cannot be undone.`, [
      { text: 'Cancel', style: 'cancel' },
      {
        text: 'Delete',
        style: 'destructive',
        onPress: async () => {
          const ok = await guard(() => vault.deleteItem(item.id));
          if (ok !== null) {
            setOpenId(null);
            setOpenSecret(null);
            await loadAll();
          }
        },
      },
    ]);
  }

  async function revealSecret(key: string) {
    if (revealedSecrets[key] !== undefined) {
      const next = { ...revealedSecrets };
      delete next[key];
      setRevealedSecrets(next);
      return;
    }
    const value = await guard(() => vault.getSecret(key, scope));
    if (value !== null) setRevealedSecrets({ ...revealedSecrets, [key]: value });
  }

  async function saveSecret() {
    if (!newKey.trim()) return setError('Key is required');
    setBusy(true);
    const ok = await guard(() => vault.setSecret(newKey.trim(), newValue, scope));
    setBusy(false);
    if (ok !== null) {
      setNewKey('');
      setNewValue('');
      await loadAll();
    }
  }

  function removeSecret(key: string) {
    Alert.alert('Delete variable', `Delete ${key}?`, [
      { text: 'Cancel', style: 'cancel' },
      {
        text: 'Delete',
        style: 'destructive',
        onPress: async () => {
          const ok = await guard(() => vault.deleteSecret(key, scope));
          if (ok !== null) await loadAll();
        },
      },
    ]);
  }

  const visibleItems = search.trim()
    ? items.filter((i) => i.name.toLowerCase().includes(search.trim().toLowerCase()))
    : items;

  return (
    <Screen
      scroll
      title="Vault"
      subtitle="Passwords, API keys, and environment variables"
      headerRight={
        // A tab has nowhere to close to; locking is the only header action.
        status?.unlocked ? (
          <NativeButton label="Lock" size="sm" variant="ghost" onPress={lock} />
        ) : null
      }
    >
      <View style={{ gap: spacing[4] }}>
        {error ? (
          <View
            style={{
              backgroundColor: theme.colors.bgCard,
              borderRadius: radius.md,
              padding: spacing[3],
              borderWidth: 1,
              borderColor: theme.colors.border,
            }}
          >
            <Txt variant="caption" tone="danger">
              {error}
            </Txt>
          </View>
        ) : null}

        {demo ? (
          <NeedsComputer
            icon="⚷"
            title="Your vault lives on your computer"
            body="Passwords, API keys and environment variables stay encrypted on your computer, under your master password. Pair this phone to open them here, and unlock with Face ID."
          />
        ) : loading ? (
          <Txt tone="secondary">Loading…</Txt>
        ) : !status ? (
          <View style={cardStyle}>
            <Txt tone="secondary">
              Can&apos;t reach your computer. Check that Consistent CEO is open on it, and the connection in
              Settings, then try again.
            </Txt>
          </View>
        ) : recoveryCode ? (
          // Shown once. Nothing can fetch it back, which is both the security
          // property and the reason this is deliberately hard to skip past.
          <View style={[cardStyle, { borderColor: theme.colors.borderFocus }]}>
            <Txt variant="heading">Write this down now</Txt>
            <Txt variant="caption" tone="secondary">
              This recovery code opens your vault if you lose your password and every device. It
              is shown once and is not stored anywhere we can retrieve it.
            </Txt>
            <View
              style={{
                backgroundColor: theme.colors.bgSurface,
                borderRadius: radius.md,
                padding: spacing[3],
              }}
            >
              <Txt selectable style={{ fontFamily: 'Menlo', letterSpacing: 1 }}>
                {recoveryCode}
              </Txt>
            </View>
            <NativeButton
              label="I have saved it"
              onPress={() => setRecoveryCode(null)}
              fullWidth
            />
          </View>
        ) : !status.initialized ? (
          <View style={cardStyle}>
            <Txt variant="heading">Create your vault</Txt>
            <Txt variant="caption" tone="secondary">
              Everything inside is encrypted with a key derived from your master password. You
              will also get a recovery code, shown once — then add Face ID so the password
              becomes the thing you rarely need.
            </Txt>
            <TextInput
              style={inputStyle}
              placeholder="Master password"
              placeholderTextColor={theme.colors.textMuted}
              secureTextEntry
              value={masterPassword}
              onChangeText={onMasterPasswordChange}
            />
            {strength ? (
              <View style={{ gap: spacing[1] }}>
                <View style={{ flexDirection: 'row', gap: 4 }}>
                  {[0, 1, 2, 3].map((i) => (
                    <View
                      key={i}
                      style={{
                        flex: 1,
                        height: 4,
                        borderRadius: 2,
                        backgroundColor:
                          i < strength.score
                            ? strength.score <= 1
                              ? theme.colors.danger
                              : strength.score === 2
                                ? theme.colors.warning
                                : theme.colors.success
                            : theme.colors.border,
                      }}
                    />
                  ))}
                </View>
                <Txt variant="caption" tone="secondary">
                  {strength.label} · ~{strength.guessesLog10} guesses to crack
                  {strength.warning ? ` · ${strength.warning}` : ''}
                </Txt>
              </View>
            ) : null}
            <TextInput
              style={inputStyle}
              placeholder="Confirm master password"
              placeholderTextColor={theme.colors.textMuted}
              secureTextEntry
              value={confirmPassword}
              onChangeText={setConfirmPassword}
            />
            <NativeButton
              label={busy ? 'Creating…' : 'Create vault'}
              onPress={create}
              disabled={busy}
              loading={busy}
              fullWidth
            />
          </View>
        ) : !status.unlocked ? (
          <View style={cardStyle}>
            <Txt variant="heading">Vault locked</Txt>

            {deviceReady ? (
              <>
                <NativeButton
                  label={busy ? 'Waiting…' : 'Unlock with Face ID'}
                  onPress={unlockByDevice}
                  disabled={busy}
                  loading={busy}
                  fullWidth
                />
                <Txt variant="caption" tone="secondary">
                  Or use your master password.
                </Txt>
              </>
            ) : null}

            <TextInput
              style={inputStyle}
              placeholder="Master password"
              placeholderTextColor={theme.colors.textMuted}
              secureTextEntry
              value={masterPassword}
              onChangeText={setMasterPassword}
              onSubmitEditing={unlock}
              returnKeyType="go"
            />
            <NativeButton
              label={busy ? 'Unlocking…' : 'Unlock'}
              onPress={unlock}
              disabled={busy}
              loading={busy}
              variant={deviceReady ? 'outline' : 'primary'}
              fullWidth
            />

            {showRecoveryUnlock ? (
              <>
                <Txt variant="caption" tone="secondary">
                  Enter your recovery code. Dashes and capitalisation do not matter.
                </Txt>
                <TextInput
                  style={inputStyle}
                  placeholder="XXXX-XXXX-XXXX-XXXX-…"
                  placeholderTextColor={theme.colors.textMuted}
                  autoCapitalize="characters"
                  autoCorrect={false}
                  value={recoveryInput}
                  onChangeText={setRecoveryInput}
                  onSubmitEditing={unlockByRecovery}
                  returnKeyType="go"
                />
                <NativeButton
                  label="Unlock with recovery code"
                  onPress={unlockByRecovery}
                  disabled={busy}
                  fullWidth
                />
              </>
            ) : (
              <Pressable onPress={() => setShowRecoveryUnlock(true)}>
                <Txt variant="caption" tone="secondary">
                  Lost your password and devices? Use a recovery code
                </Txt>
              </Pressable>
            )}
          </View>
        ) : (
          <>
            <View style={{ flexDirection: 'row', gap: spacing[2] }}>
              {(['passwords', 'environment', 'security'] as Tab[]).map((t) => (
                <Pressable
                  key={t}
                  onPress={() => {
                    setTab(t);
                    if (t === 'security') loadHealth();
                  }}
                  style={{
                    paddingVertical: spacing[2],
                    paddingHorizontal: spacing[4],
                    borderRadius: radius.full,
                    backgroundColor: tab === t ? theme.colors.bgSelected : 'transparent',
                    borderWidth: 1,
                    borderColor: tab === t ? theme.colors.borderFocus : theme.colors.border,
                  }}
                >
                  <Txt variant="label" tone={tab === t ? 'accent' : 'secondary'}>
                    {t === 'passwords'
                      ? `Passwords ${items.length}`
                      : t === 'environment'
                        ? `Environment ${secretKeys.length}`
                        : `Security ${status.unlockMethods?.length ?? 0}`}
                  </Txt>
                </Pressable>
              ))}
            </View>

            {tab === 'passwords' ? (
              <>
                {/* Each entity has its own password set — switcher mirrors the
                    Environment tab. */}
                <ScrollView
                  horizontal
                  showsHorizontalScrollIndicator={false}
                  contentContainerStyle={{ gap: spacing[2], paddingVertical: spacing[1] }}
                >
                  {[
                    { scope: GLOBAL_SCOPE, label: 'Personal', itemCount: 0 },
                    ...itemScopeList.filter((s) => s.scope !== GLOBAL_SCOPE),
                  ].map((s) => (
                    <Pressable
                      key={s.scope}
                      onPress={() => switchItemScope(s.scope)}
                      style={{
                        paddingVertical: spacing[2],
                        paddingHorizontal: spacing[3],
                        borderRadius: radius.full,
                        backgroundColor:
                          itemScope === s.scope ? theme.colors.bgSelected : 'transparent',
                        borderWidth: 1,
                        borderColor:
                          itemScope === s.scope ? theme.colors.borderFocus : theme.colors.border,
                      }}
                    >
                      <Txt variant="label" tone={itemScope === s.scope ? 'accent' : 'secondary'}>
                        {s.label}
                      </Txt>
                    </Pressable>
                  ))}
                </ScrollView>

                <View style={{ flexDirection: 'row', gap: spacing[2] }}>
                  <TextInput
                    style={[inputStyle, { flex: 1 }]}
                    placeholder="entity id…"
                    placeholderTextColor={theme.colors.textMuted}
                    autoCapitalize="none"
                    autoCorrect={false}
                    value={newItemEntity}
                    onChangeText={setNewItemEntity}
                    onSubmitEditing={addItemEntity}
                  />
                  <NativeButton
                    label="Add entity"
                    variant="outline"
                    size="sm"
                    onPress={addItemEntity}
                  />
                </View>

                <TextInput
                  style={inputStyle}
                  placeholder="Search items…"
                  placeholderTextColor={theme.colors.textMuted}
                  value={search}
                  onChangeText={setSearch}
                />

                {composing ? (
                  <View style={cardStyle}>
                    <Txt variant="heading">New login</Txt>
                    <TextInput
                      style={inputStyle}
                      placeholder="Name"
                      placeholderTextColor={theme.colors.textMuted}
                      value={draftName}
                      onChangeText={setDraftName}
                    />
                    <TextInput
                      style={inputStyle}
                      placeholder="Username"
                      placeholderTextColor={theme.colors.textMuted}
                      autoCapitalize="none"
                      value={draftUsername}
                      onChangeText={setDraftUsername}
                    />
                    <TextInput
                      style={inputStyle}
                      placeholder="Password"
                      placeholderTextColor={theme.colors.textMuted}
                      autoCapitalize="none"
                      value={draftPassword}
                      onChangeText={setDraftPassword}
                    />
                    <View style={{ flexDirection: 'row', gap: spacing[2] }}>
                      <NativeButton
                        label="Generate"
                        variant="outline"
                        size="sm"
                        onPress={generatePassword}
                      />
                      <NativeButton
                        label="Save"
                        size="sm"
                        onPress={saveDraft}
                        disabled={busy}
                        loading={busy}
                      />
                      <NativeButton
                        label="Cancel"
                        variant="ghost"
                        size="sm"
                        onPress={() => setComposing(false)}
                      />
                    </View>
                    <Txt variant="caption" tone="secondary">
                      Saving into {itemScope === GLOBAL_SCOPE ? 'Personal' : itemScope}.
                    </Txt>
                  </View>
                ) : (
                  <NativeButton label="New item" onPress={() => setComposing(true)} fullWidth />
                )}

                {visibleItems.length === 0 ? (
                  <Txt tone="secondary">
                    {search
                      ? 'Nothing matches that search.'
                      : itemScope === GLOBAL_SCOPE
                        ? 'No personal items yet.'
                        : 'This entity has no passwords yet.'}
                  </Txt>
                ) : (
                  visibleItems.map((item) => (
                    <View key={item.id} style={cardStyle}>
                      <Pressable onPress={() => toggleItem(item.id)}>
                        <Txt variant="body" weight="medium">
                          {item.name}
                        </Txt>
                        <Txt variant="caption" tone="muted">
                          {KIND_LABELS[item.kind]}
                          {item.sharedFrom ? ` · shared by ${item.sharedFrom}` : ''}
                        </Txt>
                      </Pressable>

                      {openId === item.id && openSecret ? (
                        <View style={{ gap: spacing[2] }}>
                          <Txt variant="caption" tone="muted">
                            Username
                          </Txt>
                          {/* Selectable so the value can be long-pressed to copy
                              without pulling in a native clipboard module. */}
                          <Txt variant="mono" selectable>
                            {openSecret.username || '—'}
                          </Txt>

                          <Txt variant="caption" tone="muted">
                            Password
                          </Txt>
                          <Txt variant="mono" selectable>
                            {revealed ? openSecret.password || '—' : '••••••••••••'}
                          </Txt>

                          {openTotp ? (
                            <>
                              <Txt variant="caption" tone="muted">
                                Two-factor code
                              </Txt>
                              <View
                                style={{
                                  flexDirection: 'row',
                                  alignItems: 'baseline',
                                  gap: spacing[3],
                                }}
                              >
                                <Txt
                                  variant="mono"
                                  selectable
                                  style={{
                                    fontSize: 22,
                                    letterSpacing: 3,
                                    color: theme.colors.accent,
                                  }}
                                >
                                  {openTotp.code.slice(0, 3)} {openTotp.code.slice(3)}
                                </Txt>
                                <Txt variant="caption" tone="secondary">
                                  {openTotp.secondsRemaining}s
                                </Txt>
                              </View>
                            </>
                          ) : null}

                          <View style={{ flexDirection: 'row', gap: spacing[2] }}>
                            <NativeButton
                              label={revealed ? 'Hide' : 'Reveal'}
                              variant="outline"
                              size="sm"
                              onPress={() => setRevealed(!revealed)}
                            />
                            <NativeButton
                              label="Delete"
                              variant="danger"
                              size="sm"
                              onPress={() => removeItem(item)}
                            />
                          </View>

                          {openSecret.passwordHistory && openSecret.passwordHistory.length > 0 ? (
                            <>
                              <Txt variant="caption" tone="muted">
                                Previous passwords
                              </Txt>
                              {openSecret.passwordHistory.slice(0, 5).map((entry, i) => (
                                <Txt key={i} variant="mono" tone="secondary" selectable>
                                  {revealed ? entry.password : '••••••••'} ·{' '}
                                  {new Date(entry.changedAt).toLocaleDateString()}
                                </Txt>
                              ))}
                            </>
                          ) : null}
                        </View>
                      ) : null}
                    </View>
                  ))
                )}
              </>
            ) : tab === 'environment' ? (
              <>
                <SectionHeader title="Environment variables" />

                {/* Scope switcher: global defaults, then one space per entity. */}
                <ScrollView
                  horizontal
                  showsHorizontalScrollIndicator={false}
                  contentContainerStyle={{ gap: spacing[2], paddingVertical: spacing[1] }}
                >
                  {[
                    { scope: GLOBAL_SCOPE, label: 'Global', keyCount: 0 },
                    ...scopes.filter((s) => s.scope !== GLOBAL_SCOPE),
                  ].map((s) => (
                    <Pressable
                      key={s.scope}
                      onPress={() => switchScope(s.scope)}
                      style={{
                        paddingVertical: spacing[2],
                        paddingHorizontal: spacing[3],
                        borderRadius: radius.full,
                        backgroundColor:
                          scope === s.scope ? theme.colors.bgSelected : 'transparent',
                        borderWidth: 1,
                        borderColor:
                          scope === s.scope ? theme.colors.borderFocus : theme.colors.border,
                      }}
                    >
                      <Txt variant="label" tone={scope === s.scope ? 'accent' : 'secondary'}>
                        {s.label}
                      </Txt>
                    </Pressable>
                  ))}
                </ScrollView>

                <View style={{ flexDirection: 'row', gap: spacing[2] }}>
                  <TextInput
                    style={[inputStyle, { flex: 1 }]}
                    placeholder="entity id…"
                    placeholderTextColor={theme.colors.textMuted}
                    autoCapitalize="none"
                    autoCorrect={false}
                    value={newScopeEntity}
                    onChangeText={setNewScopeEntity}
                    onSubmitEditing={addEntityScope}
                  />
                  <NativeButton
                    label="Add entity"
                    variant="outline"
                    size="sm"
                    onPress={addEntityScope}
                  />
                </View>

                <Txt variant="caption" tone="secondary">
                  {scope === GLOBAL_SCOPE
                    ? 'Account-wide defaults. Every entity inherits these unless it sets its own.'
                    : 'Only this entity sees these, and they shadow the global value of the same name.'}
                </Txt>

                <View style={cardStyle}>
                  <TextInput
                    style={inputStyle}
                    placeholder="KEY_NAME"
                    placeholderTextColor={theme.colors.textMuted}
                    autoCapitalize="characters"
                    autoCorrect={false}
                    value={newKey}
                    onChangeText={setNewKey}
                  />
                  <TextInput
                    style={inputStyle}
                    placeholder="value"
                    placeholderTextColor={theme.colors.textMuted}
                    autoCapitalize="none"
                    autoCorrect={false}
                    value={newValue}
                    onChangeText={setNewValue}
                  />
                  <NativeButton
                    label="Set"
                    onPress={saveSecret}
                    disabled={busy}
                    loading={busy}
                    fullWidth
                  />
                </View>

                {secretKeys.length === 0 ? (
                  <Txt tone="secondary">
                    {scope === GLOBAL_SCOPE
                      ? 'No environment variables stored yet.'
                      : 'Nothing set for this entity yet — it inherits everything from Global.'}
                  </Txt>
                ) : (
                  secretKeys.map((key) => (
                    <View key={key} style={cardStyle}>
                      <Txt variant="mono" weight="medium">
                        {key}
                      </Txt>
                      <Txt variant="mono" tone="secondary" selectable>
                        {revealedSecrets[key] !== undefined
                          ? revealedSecrets[key]
                          : '••••••••••••'}
                      </Txt>
                      <View style={{ flexDirection: 'row', gap: spacing[2] }}>
                        <NativeButton
                          label={revealedSecrets[key] !== undefined ? 'Hide' : 'Reveal'}
                          variant="outline"
                          size="sm"
                          onPress={() => revealSecret(key)}
                        />
                        <NativeButton
                          label="Delete"
                          variant="danger"
                          size="sm"
                          onPress={() => removeSecret(key)}
                        />
                      </View>
                    </View>
                  ))
                )}
              </>
            ) : (
              <>
                {health ? (
                  <View style={cardStyle}>
                    <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[3] }}>
                      <View
                        style={{
                          width: 52,
                          height: 52,
                          borderRadius: 26,
                          alignItems: 'center',
                          justifyContent: 'center',
                          borderWidth: 3,
                          borderColor:
                            health.score >= 80
                              ? theme.colors.success
                              : health.score >= 50
                                ? theme.colors.warning
                                : theme.colors.danger,
                        }}
                      >
                        <Txt weight="bold">{health.score}</Txt>
                      </View>
                      <View style={{ flex: 1 }}>
                        <Txt variant="heading">Password health</Txt>
                        <Txt variant="caption" tone="secondary">
                          {health.totalWithPasswords === 0
                            ? 'No passwords to check yet.'
                            : health.weak.length + health.reused.length + health.stale.length === 0
                              ? `All ${health.totalWithPasswords} look healthy.`
                              : `${health.weak.length} weak · ${health.reused.length} reused · ${health.stale.length} old`}
                        </Txt>
                      </View>
                    </View>

                    <NativeButton
                      label={breachRunning ? 'Checking breaches…' : 'Check for breaches'}
                      variant="outline"
                      size="sm"
                      onPress={runBreachCheck}
                      disabled={breachRunning}
                      loading={breachRunning}
                      fullWidth
                    />
                    {breach ? (
                      <Txt variant="caption" tone={breach.breached.length > 0 ? 'danger' : 'secondary'}>
                        {breach.breached.length > 0
                          ? `${breach.breached.length} of ${breach.checked} passwords are in known breaches — change these.`
                          : `Checked ${breach.checked} passwords — none found in known breaches.`}
                        {breach.unreachable ? ' (some checks could not reach the service)' : ''}
                      </Txt>
                    ) : (
                      <Txt variant="caption" tone="secondary">
                        Only a 5-character hash prefix per password leaves your device.
                      </Txt>
                    )}
                  </View>
                ) : null}

                <SectionHeader title="Ways in" />
                <Txt variant="caption" tone="secondary">
                  Each one wraps the same key independently. Adding or removing one never
                  re-encrypts your data, and the last remaining one cannot be removed.
                </Txt>

                {(status.unlockMethods ?? []).map((method: UnlockMethod) => (
                  <View key={method.id} style={cardStyle}>
                    <Txt weight="medium">{method.label}</Txt>
                    <Txt variant="caption" tone="secondary">
                      {UNLOCK_KIND_LABELS[method.kind]} · added{' '}
                      {new Date(method.createdAt).toLocaleDateString()}
                      {method.lastUsedAt
                        ? ` · last used ${new Date(method.lastUsedAt).toLocaleDateString()}`
                        : ' · never used'}
                    </Txt>
                  </View>
                ))}

                <View style={cardStyle}>
                  <Txt weight="medium">This device</Txt>
                  {!deviceSupported ? (
                    <Txt variant="caption" tone="secondary">
                      Device unlock is only available on iOS and Android.
                    </Txt>
                  ) : deviceReady ? (
                    <>
                      <Txt variant="caption" tone="secondary">
                        Face ID or Touch ID opens your vault here. The key is held in this
                        device&apos;s keychain and never leaves it once enrolled.
                      </Txt>
                      <NativeButton
                        label="Remove this device"
                        variant="danger"
                        onPress={confirmRevokeDevice}
                        disabled={busy}
                        fullWidth
                      />
                    </>
                  ) : (
                    <>
                      <Txt variant="caption" tone="secondary">
                        Add this device so you can open your vault with Face ID or Touch ID
                        instead of typing your master password.
                      </Txt>
                      <NativeButton
                        label={busy ? 'Adding…' : 'Add this device'}
                        onPress={enrollThisDevice}
                        disabled={busy}
                        loading={busy}
                        fullWidth
                      />
                    </>
                  )}
                </View>

                <View style={cardStyle}>
                  <Txt weight="medium">Recovery code</Txt>
                  <Txt variant="caption" tone="secondary">
                    Opens your vault if you lose your password and every device. Generating a new
                    one does not invalidate the old ones — remove those above if you need to.
                  </Txt>
                  <NativeButton
                    label="Generate a new recovery code"
                    variant="outline"
                    onPress={mintRecoveryCode}
                    disabled={busy}
                    fullWidth
                  />
                </View>
              </>
            )}
          </>
        )}
      </View>
    </Screen>
  );
}
