import { useFocusEffect, useRouter } from 'expo-router';
import React, { useCallback, useEffect, useState } from 'react';
import {
  Alert,
  KeyboardAvoidingView,
  Modal,
  Platform,
  Pressable,
  ScrollView,
  Switch,
  TextInput,
  View,
} from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import {
  clearAuthToken,
  DEFAULT_GATEWAY_URL,
  getAuthToken,
  getGatewayUrl,
  setAuthToken,
  setGatewayUrl,
} from '@/api';
import { api } from '@/api';
import { auth } from '@/api/auth';
import { APIError, isDemoGateway } from '@/api/client';
import { IconButton, NativeButton, SectionHeader, Txt, useAiConsentGate } from '@/components';
import { setAiConsent } from '@/lib/aiConsent';
import { registerDevice } from '@/lib/companion';
import { haptics } from '@/lib/haptics';
import { isLocalNetworkUrl, openExternal, plainHttpNote, PRIVACY_URL, SUPPORT_URL } from '@/lib/links';
import { getBoolPref, PREF_NOTIF_ON, PREF_PUSH_ON, setBoolPref } from '@/lib/prefs';
import { forgetDevice } from '@/lib/vaultDeviceKey';
import {
  ensureNotificationPermission,
  scheduleHeartbeat,
  sendTestNotification,
} from '@/lib/notifications';
import { useAppStore } from '@/store/appStore';
import { useThemeControls } from '@/theme/ThemeProvider';
import type { ThemePreference } from '@/theme/ThemeProvider';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

export default function SettingsScreen() {
  const theme = useTheme();
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const { preference, setPreference } = useThemeControls();

  const user = useAppStore((s) => s.user);
  const userLoading = useAppStore((s) => s.userLoading);
  const loadUser = useAppStore((s) => s.loadUser);

  const [gateway, setGateway] = useState(DEFAULT_GATEWAY_URL);
  const [token, setTokenValue] = useState('');
  const [hasToken, setHasToken] = useState(false);
  const [notifOn, setNotifOn] = useState(false);
  const [pushOn, setPushOn] = useState(false);
  const [pushStatus, setPushStatus] = useState<string | null>(null);
  const [connecting, setConnecting] = useState(false);
  const [connected, setConnected] = useState<string | null>(null);
  // Why a connect attempt failed — too long for the button label it used to
  // be crammed into.
  const [connectHelp, setConnectHelp] = useState<string | null>(null);
  // The address actually saved, for the "not encrypted" note — not the draft
  // being typed.
  const [savedGateway, setSavedGateway] = useState(DEFAULT_GATEWAY_URL);
  const [deleteOpen, setDeleteOpen] = useState(false);
  const consent = useAiConsentGate();

  useEffect(() => {
    // Hydrate persisted toggles so they reflect reality instead of resetting
    // to off every time the sheet opens.
    getBoolPref(PREF_NOTIF_ON).then(setNotifOn);
    getBoolPref(PREF_PUSH_ON).then(setPushOn);
    loadUser();
  }, [loadUser]);

  // Re-read on every focus, not just mount — coming back from the pairing
  // scanner (or login) must show the URL/session it just stored.
  useFocusEffect(
    useCallback(() => {
      getGatewayUrl().then((url) => {
        setGateway(url);
        setSavedGateway(url);
      });
      getAuthToken().then((t) => setHasToken(!!t));
    }, []),
  );

  const connect = async () => {
    setConnecting(true);
    setConnectHelp(null);
    await setGatewayUrl(gateway.trim());
    setSavedGateway(gateway.trim());
    const health = await api.connectGateway(gateway.trim());
    // A failed probe used to still say "Connected", with an invented latency.
    // Report what the round-trip actually did.
    if (health.gatewayConnected && health.signedOut) {
      setConnected('Reached your computer — sign in to finish connecting.');
      haptics.warning();
    } else if (health.gatewayConnected) {
      setConnected(health.latencyMs != null ? `Connected · ${health.latencyMs}ms` : 'Connected');
      haptics.success();
    } else {
      // Sample data stands in only on the built-in demo address; a real one
      // that didn't answer is just unreachable.
      setConnected('Couldn’t connect — try again');
      setConnectHelp(
        health.demo
          ? 'Nothing answered at that address, so the app keeps showing sample data until you pair.'
          : isLocalNetworkUrl(gateway)
            ? 'Check that Consistent CEO is open on your computer and this phone is on the same Wi-Fi. If iOS asked to find devices on your local network, allow it (Settings → Privacy & Security → Local Network), then try again.'
            : 'Check that Consistent CEO is open on your computer and the address is right.',
      );
      haptics.warning();
    }
    setConnecting(false);
  };

  const saveToken = async () => {
    if (!token.trim()) return;
    await setAuthToken(token.trim());
    setHasToken(true);
    setTokenValue('');
    haptics.success();
  };

  const removeToken = async () => {
    await clearAuthToken();
    setHasToken(false);
    haptics.warning();
  };

  const toggleNotif = async (value: boolean) => {
    haptics.tap();
    if (value) {
      const granted = await ensureNotificationPermission();
      setNotifOn(granted);
      await setBoolPref(PREF_NOTIF_ON, granted);
      if (granted) await scheduleHeartbeat('Consistent CEO', 'Heartbeat · your workers are running.', 4);
    } else {
      setNotifOn(false);
      await setBoolPref(PREF_NOTIF_ON, false);
    }
  };

  // Push is the only path that reaches this phone with the app closed, and the
  // only one that involves a third party — so it is explicitly opt-in, and
  // turning it off re-registers the device as poll-only (token cleared server
  // side) rather than just forgetting locally.
  const togglePush = async (value: boolean) => {
    haptics.tap();
    const previous = pushOn;
    // Optimistic flip; reverted with an explanation if registration fails.
    setPushOn(value);
    setPushStatus(null);
    const result = await registerDevice({ push: value });
    if (!result.ok) {
      setPushOn(previous);
      setPushStatus('Couldn’t reach your computer — check its address and the connection above, then try again.');
      haptics.warning();
      return;
    }
    if (value && !result.pushToken) {
      setPushStatus(
        result.reason === 'permission-denied'
          ? 'Notification permission is denied — enable it for Consistent CEO in iOS Settings, then toggle again.'
          : 'Push isn’t available on this device, so alerts arrive only while the app is open.',
      );
    }
    setPushOn(value);
    await setBoolPref(PREF_PUSH_ON, value);
    haptics.success();
  };

  // Revoking is immediate; allowing again shows the disclosure first, so a
  // yes from here is as informed as one from the first chat message.
  const toggleAiConsent = async (value: boolean) => {
    haptics.tap();
    if (value) await consent.ensure('settings');
    else await setAiConsent(false);
  };

  /**
   * Delete account (App Review 5.1.1(v)). Always visible, so it can be found
   * without signing in first; what it can do depends on where the account is.
   */
  const startDeleteAccount = async () => {
    haptics.tap();
    if (await isDemoGateway()) {
      Alert.alert(
        'Delete account',
        'Your Consistent CEO account lives on your computer, not on this phone, and this phone isn’t paired yet. Pair it and sign in to delete your account here, or delete it from Consistent CEO on your computer.',
        [
          { text: 'OK', style: 'cancel' },
          { text: 'Pair', onPress: () => router.push('/pair') },
        ],
      );
      return;
    }
    if (!user) {
      Alert.alert(
        'Delete account',
        'Your account lives on your computer. Sign in to it here first, then come back to delete it.',
        [
          { text: 'Cancel', style: 'cancel' },
          { text: 'Sign in', onPress: () => router.push('/login') },
        ],
      );
      return;
    }
    setDeleteOpen(true);
  };

  const onAccountDeleted = async () => {
    setDeleteOpen(false);
    // The vault this phone was enrolled in is gone with the account; drop the
    // local Face ID key that unlocked it, and every trace of the session.
    await forgetDevice();
    await clearAuthToken();
    useAppStore.setState({ user: null });
    setHasToken(false);
    haptics.success();
    Alert.alert('Account deleted', 'Your account and its sign-in data were deleted from your computer.', [
      { text: 'OK', onPress: () => router.replace('/(tabs)') },
    ]);
  };

  const httpNote = plainHttpNote(savedGateway);

  // Text-presentation glyphs only — emoji glyphs can fail on some devices.
  const themeOptions: { key: ThemePreference; label: string; icon: string }[] = [
    { key: 'system', label: 'System', icon: '◐' },
    { key: 'dark', label: 'Dark', icon: '●' },
    { key: 'light', label: 'Light', icon: '○' },
  ];

  return (
    <View style={{ flex: 1, backgroundColor: theme.colors.bg, paddingTop: insets.top }}>
      <View style={{ flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', paddingHorizontal: spacing[5], paddingVertical: spacing[3] }}>
        <Txt variant="title">Settings</Txt>
        <IconButton icon="✕" filled onPress={() => router.back()} label="Close" />
      </View>

      <ScrollView contentContainerStyle={{ paddingHorizontal: spacing[5], paddingBottom: insets.bottom + spacing[12] }} showsVerticalScrollIndicator={false}>
        {/* Account */}
        <SectionHeader title="Account & session" />
        <Card>
          <Row
            label="Signed in as"
            value={user?.username ?? (userLoading ? '…' : 'Not signed in')}
          />
          <Divider />
          <Row label="Workspace" value={user?.workspaceId ?? '—'} />
          <Divider />
          <Pressable onPress={() => { haptics.tap(); router.push('/login'); }} style={{ paddingVertical: spacing[3] }}>
            <Txt tone="accent" weight="semibold">
              {user ? 'Switch account →' : 'Sign in / create account →'}
            </Txt>
          </Pressable>
          <Divider />
          <Pressable onPress={() => { haptics.tap(); router.push('/authenticator'); }} style={{ paddingVertical: spacing[3] }}>
            <Txt tone="accent" weight="semibold">
              Authenticator — approve device sign-ins →
            </Txt>
          </Pressable>
          <Divider />
          <Pressable onPress={() => { haptics.tap(); router.push('/vault'); }} style={{ paddingVertical: spacing[3] }}>
            <Txt tone="accent" weight="semibold">
              Vault — passwords, API keys, secrets →
            </Txt>
          </Pressable>
          <Divider />
          <Pressable
            onPress={() => void startDeleteAccount()}
            accessibilityRole="button"
            style={{ paddingVertical: spacing[3] }}
          >
            <Txt tone="danger" weight="semibold">
              Delete account
            </Txt>
            <Txt variant="caption" tone="muted">
              Permanently deletes your account from your computer
            </Txt>
          </Pressable>
        </Card>

        {/* Backend */}
        <SectionHeader title="Connect to your computer" caption="Pair this phone with Consistent CEO on your computer" />
        <Card>
          {/* QR pairing first — the URL field below stays as the escape hatch. */}
          <View style={{ marginTop: spacing[2] }}>
            <NativeButton
              label="Scan to connect"
              icon="▣"
              variant="outline"
              fullWidth
              onPress={() => {
                haptics.tap();
                router.push('/pair');
              }}
            />
          </View>
          <Txt variant="caption" tone="muted" style={{ marginTop: spacing[2], marginBottom: spacing[3], textAlign: 'center' }}>
            Point the camera at the QR code on your computer — or type the URL below.
          </Txt>
          <Txt variant="caption" tone="muted" style={{ marginBottom: spacing[2] }}>
            Computer address (URL)
          </Txt>
          <TextInput
            value={gateway}
            onChangeText={setGateway}
            autoCapitalize="none"
            autoCorrect={false}
            placeholder={DEFAULT_GATEWAY_URL}
            placeholderTextColor={theme.colors.textMuted}
            style={{ color: theme.colors.textPrimary, fontSize: 15, backgroundColor: theme.colors.bgSurface, borderRadius: radius.md, paddingHorizontal: spacing[3], paddingVertical: spacing[3], borderWidth: 1, borderColor: theme.colors.border }}
          />
          <View style={{ marginTop: spacing[3] }}>
            <NativeButton label={connected ?? 'Connect'} icon={connected ? '✓' : '⇄'} loading={connecting} onPress={connect} fullWidth />
          </View>
          {connectHelp ? (
            <Txt variant="caption" tone="danger" style={{ marginTop: spacing[3], lineHeight: 17 }}>
              {connectHelp}
            </Txt>
          ) : null}
          {httpNote ? (
            <Txt variant="caption" tone="muted" style={{ marginTop: spacing[3], marginBottom: spacing[1], lineHeight: 17 }}>
              {httpNote}
            </Txt>
          ) : null}
        </Card>

        {/* Secure token */}
        <SectionHeader
          title="Secure sign-in token"
          caption={Platform.OS === 'ios' ? 'Stored in the iOS Keychain' : 'Stored in this device’s secure storage'}
        />
        <Card>
          <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[2], marginBottom: spacing[2] }}>
            <View style={{ width: 8, height: 8, borderRadius: 4, backgroundColor: hasToken ? theme.colors.success : theme.colors.textMuted }} />
            <Txt variant="label" tone={hasToken ? 'success' : 'muted'}>
              {hasToken ? 'Token stored securely' : 'No token stored'}
            </Txt>
          </View>
          <TextInput
            value={token}
            onChangeText={setTokenValue}
            placeholder="Paste pairing token…"
            placeholderTextColor={theme.colors.textMuted}
            secureTextEntry
            autoCapitalize="none"
            style={{ color: theme.colors.textPrimary, fontSize: 15, backgroundColor: theme.colors.bgSurface, borderRadius: radius.md, paddingHorizontal: spacing[3], paddingVertical: spacing[3], borderWidth: 1, borderColor: theme.colors.border }}
          />
          <View style={{ flexDirection: 'row', gap: spacing[2], marginTop: spacing[3] }}>
            {hasToken ? <NativeButton label="Remove" variant="ghost" size="sm" style={{ flex: 1 }} onPress={removeToken} /> : null}
            <NativeButton label="Save token" size="sm" style={{ flex: 1 }} onPress={saveToken} disabled={!token.trim()} haptic="success" />
          </View>
        </Card>

        {/* Theme */}
        <SectionHeader title="Theme" />
        <View style={{ flexDirection: 'row', gap: spacing[2] }}>
          {themeOptions.map((opt) => {
            const active = preference === opt.key;
            return (
              <Pressable
                key={opt.key}
                onPress={() => {
                  haptics.tap();
                  setPreference(opt.key);
                }}
                style={{
                  flex: 1,
                  alignItems: 'center',
                  gap: 6,
                  paddingVertical: spacing[4],
                  borderRadius: radius.lg,
                  backgroundColor: active ? theme.colors.bgSelected : theme.colors.bgCard,
                  borderWidth: 1,
                  borderColor: active ? theme.colors.borderFocus : theme.colors.border,
                }}
              >
                <Txt style={{ fontSize: 20, color: active ? theme.colors.accent : theme.colors.textMuted }}>
                  {opt.icon}
                </Txt>
                <Txt variant="label" tone={active ? 'accent' : 'secondary'}>
                  {opt.label}
                </Txt>
              </Pressable>
            );
          })}
        </View>

        {/* Notifications */}
        <SectionHeader title="Notifications" />
        <Card>
          <ToggleRow label="Heartbeats & reminders" caption="Local notifications from workers, with sound" value={notifOn} onChange={toggleNotif} />
          <Divider />
          <ToggleRow
            label="Push when the app is closed"
            caption="Sends notification text via Expo's push service. Off = this phone only gets alerts while the app is open."
            value={pushOn}
            onChange={togglePush}
          />
          {pushStatus ? (
            <Txt variant="caption" tone="danger" style={{ paddingBottom: spacing[3] }}>
              {pushStatus}
            </Txt>
          ) : null}
          <Divider />
          <View style={{ paddingVertical: spacing[3] }}>
            <NativeButton
              label="Send test notification (with sound)"
              icon="◈"
              variant="outline"
              size="sm"
              fullWidth
              onPress={async () => {
                const ok = await ensureNotificationPermission();
                if (ok) {
                  await sendTestNotification();
                  haptics.success();
                } else {
                  haptics.warning();
                }
              }}
            />
            <Txt variant="caption" tone="muted" style={{ marginTop: spacing[2], textAlign: 'center' }}>
              Arrives in ~3s — background the app to hear the sound
            </Txt>
          </View>
        </Card>

        {/* Privacy */}
        <SectionHeader title="Privacy" />
        <Card>
          <ToggleRow
            label="AI data sharing"
            caption={
              consent.granted
                ? 'Allowed — chat and voice go to your computer, which sends them to the AI provider you set up there.'
                : 'Not allowed — chat and voice ask before anything is sent.'
            }
            value={consent.granted}
            onChange={(v) => void toggleAiConsent(v)}
          />
          <Divider />
          <LinkRow label="Privacy policy" onPress={() => void openExternal(PRIVACY_URL)} />
          <Divider />
          <LinkRow label="Help & support" onPress={() => void openExternal(SUPPORT_URL)} />
        </Card>

        <View style={{ alignItems: 'center', paddingTop: spacing[8] }}>
          <Txt variant="caption" tone="muted">
            Consistent CEO · mobile
          </Txt>
          <Txt variant="caption" tone="muted">
            local-first AI workspace
          </Txt>
        </View>
      </ScrollView>

      {consent.sheet}
      <DeleteAccountSheet
        visible={deleteOpen}
        username={user?.username ?? ''}
        onCancel={() => setDeleteOpen(false)}
        onDeleted={() => void onAccountDeleted()}
      />
    </View>
  );
}

/**
 * Re-authenticate and delete. The password travels only to the operator's own
 * computer, which checks it before deleting anything; a wrong one changes
 * nothing.
 */
function DeleteAccountSheet({
  visible,
  username,
  onCancel,
  onDeleted,
}: {
  visible: boolean;
  username: string;
  onCancel: () => void;
  onDeleted: () => void;
}) {
  const theme = useTheme();
  const insets = useSafeAreaInsets();
  const [password, setPassword] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!visible) {
      setPassword('');
      setError(null);
      setBusy(false);
    }
  }, [visible]);

  const submit = async () => {
    if (!password) {
      setError('Enter your password to confirm.');
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await auth.deleteAccount(password);
      onDeleted();
    } catch (e) {
      haptics.warning();
      if (e instanceof APIError) {
        // The gateway's contract (`delete_account` in auth_api.rs): 403 wrong
        // password, 429 locked out like login, 400 nothing deletable, 401 only
        // from the auth layer (this phone's session is gone).
        setError(
          e.status === 403
            ? 'That password isn’t right. Nothing was deleted.'
            : e.status === 401
              ? 'This phone is signed out. Sign in again, then delete the account.'
              : e.status === 429
                ? 'Too many attempts. Wait a few minutes, then try again.'
                : e.status === 400 && e.detail === 'no account to delete'
                  ? 'This sign-in is your computer’s own owner account (or a device token), so it can’t be deleted from the phone. To remove it, and everything else, delete Consistent CEO and its data folder from your computer.'
                  : e.status === 400 && e.detail
                    ? `Your computer refused: ${e.detail}. Nothing was deleted.`
                    : e.status === 404 || e.status === 405
                      ? 'The Consistent CEO version on your computer can’t delete accounts from the phone yet. Update it, or delete the account there.'
                      : `Your computer couldn’t finish deleting the account (error ${e.status}). Try again in a moment.`,
        );
      } else {
        setError('Can’t reach your computer. Make sure Consistent CEO is open on it, then try again.');
      }
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal transparent visible={visible} animationType="slide" onRequestClose={onCancel}>
      <KeyboardAvoidingView
        behavior={Platform.OS === 'ios' ? 'padding' : undefined}
        style={{ flex: 1, backgroundColor: theme.colors.bgOverlay, justifyContent: 'flex-end' }}
      >
        <View
          accessibilityViewIsModal
          style={{
            backgroundColor: theme.colors.bgSurface,
            borderTopLeftRadius: radius['2xl'],
            borderTopRightRadius: radius['2xl'],
            borderWidth: 1,
            borderColor: theme.colors.border,
            paddingHorizontal: spacing[5],
            paddingTop: spacing[5],
            paddingBottom: insets.bottom + spacing[4],
            gap: spacing[3],
          }}
        >
          <Txt variant="heading" style={{ fontSize: 20 }} accessibilityRole="header">
            Delete your account?
          </Txt>
          <Txt variant="body" tone="secondary" style={{ lineHeight: 21 }}>
            This permanently deletes {username ? `“${username}”` : 'this account'} from Consistent CEO on your
            computer: its password, profile and recovery codes, passkeys, API keys, signed-in sessions and
            devices, and its personal vault. It can’t be undone.
          </Txt>
          <Txt variant="caption" tone="muted" style={{ lineHeight: 17 }}>
            Workspace data on your computer (businesses, projects, missions, chats and files) stays there. To
            remove everything, delete Consistent CEO and its data folder from your computer.
          </Txt>
          <TextInput
            value={password}
            onChangeText={setPassword}
            placeholder="Your password"
            placeholderTextColor={theme.colors.textMuted}
            secureTextEntry
            autoCapitalize="none"
            autoCorrect={false}
            textContentType="password"
            editable={!busy}
            onSubmitEditing={() => void submit()}
            style={{
              color: theme.colors.textPrimary,
              fontSize: 16,
              backgroundColor: theme.colors.bgCard,
              borderRadius: radius.md,
              paddingHorizontal: spacing[3],
              paddingVertical: spacing[3],
              borderWidth: 1,
              borderColor: theme.colors.border,
            }}
          />
          {error ? (
            <Txt variant="caption" tone="danger" style={{ lineHeight: 17 }}>
              {error}
            </Txt>
          ) : null}
          <NativeButton
            label="Delete account"
            variant="danger"
            fullWidth
            loading={busy}
            disabled={!password}
            onPress={() => void submit()}
          />
          <NativeButton label="Cancel" variant="ghost" fullWidth onPress={onCancel} disabled={busy} />
        </View>
      </KeyboardAvoidingView>
    </Modal>
  );
}

function LinkRow({ label, onPress }: { label: string; onPress: () => void }) {
  return (
    <Pressable
      onPress={() => {
        haptics.tap();
        onPress();
      }}
      accessibilityRole="link"
      style={{ flexDirection: 'row', justifyContent: 'space-between', alignItems: 'center', paddingVertical: spacing[3] }}
    >
      <Txt variant="body">{label}</Txt>
      <Txt tone="muted">↗</Txt>
    </Pressable>
  );
}

function Card({ children }: { children: React.ReactNode }) {
  const theme = useTheme();
  return (
    <View style={{ backgroundColor: theme.colors.bgCard, borderRadius: radius.lg, borderWidth: 1, borderColor: theme.colors.border, paddingHorizontal: spacing[4], paddingVertical: spacing[2] }}>
      {children}
    </View>
  );
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <View style={{ flexDirection: 'row', justifyContent: 'space-between', alignItems: 'center', paddingVertical: spacing[3] }}>
      <Txt variant="body" tone="secondary">
        {label}
      </Txt>
      <Txt variant="body" weight="semibold">
        {value}
      </Txt>
    </View>
  );
}

function ToggleRow({ label, caption, value, onChange }: { label: string; caption: string; value: boolean; onChange: (v: boolean) => void }) {
  const theme = useTheme();
  return (
    <View style={{ flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', paddingVertical: spacing[3] }}>
      <View style={{ flex: 1, paddingRight: spacing[3] }}>
        <Txt variant="body">{label}</Txt>
        <Txt variant="caption" tone="muted">
          {caption}
        </Txt>
      </View>
      <Switch
        value={value}
        onValueChange={onChange}
        trackColor={{
          true: theme.colors.accent,
          // Off-state must stay visible on light (white-on-white otherwise).
          false: theme.mode === 'dark' ? theme.colors.bgCardHover : 'rgba(24, 23, 28, 0.18)',
        }}
        thumbColor={theme.colors.white}
      />
    </View>
  );
}

function Divider() {
  const theme = useTheme();
  return <View style={{ height: 1, backgroundColor: theme.colors.border }} />;
}
