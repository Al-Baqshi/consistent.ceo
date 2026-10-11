import { useRouter } from 'expo-router';
import React, { useCallback, useEffect, useState } from 'react';
import { Platform, RefreshControl, ScrollView, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import { auth, type PendingApproval } from '@/api/auth';
import { IconButton, NativeButton, NeedsComputer, Txt } from '@/components';
import { haptics } from '@/lib/haptics';
import { useDemoMode } from '@/lib/useDemoMode';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

/**
 * The phone acting as an authenticator: lists login-approval requests from the
 * user's other devices and lets them approve/deny after confirming the code.
 */
export default function AuthenticatorScreen() {
  const theme = useTheme();
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const [items, setItems] = useState<PendingApproval[]>([]);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  // Sign-in requests come from the operator's computer; with nothing paired
  // there is nothing to poll, and nothing to approve.
  const demo = useDemoMode();

  const load = useCallback(async () => {
    try {
      setItems(await auth.pendingApprovals());
    } catch {
      // ignore transient errors; the poll will retry
    }
  }, []);

  useEffect(() => {
    if (demo !== false) return;
    void load();
    const timer = setInterval(load, 4000);
    return () => clearInterval(timer);
  }, [load, demo]);

  const decide = async (item: PendingApproval, approve: boolean) => {
    setBusyId(item.requestId);
    if (approve) haptics.success();
    else haptics.warning();
    try {
      if (approve) await auth.approve(item.requestId, item.code, `${Platform.OS} authenticator`);
      else await auth.deny(item.requestId);
      setItems((prev) => prev.filter((i) => i.requestId !== item.requestId));
    } catch {
      // keep it in the list so the user can retry
    } finally {
      setBusyId(null);
    }
  };

  return (
    <View style={{ flex: 1, backgroundColor: theme.colors.bg, paddingTop: insets.top }}>
      <View style={{ flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', paddingHorizontal: spacing[5], paddingVertical: spacing[3] }}>
        <Txt variant="title">Authenticator</Txt>
        <IconButton icon="✕" filled onPress={() => router.back()} label="Close" />
      </View>

      <ScrollView
        contentContainerStyle={{ padding: spacing[5], gap: spacing[4] }}
        refreshControl={
          <RefreshControl
            refreshing={refreshing}
            onRefresh={async () => {
              setRefreshing(true);
              await load();
              setRefreshing(false);
            }}
            tintColor={theme.colors.accent}
          />
        }
      >
        <Txt variant="body" tone="secondary">
          Approve sign-ins on your other devices. Only approve a request whose code matches the one shown there.
        </Txt>

        {demo ? (
          <NeedsComputer
            icon="⚿"
            title="Approve sign-ins from your phone"
            body="When you sign in to Consistent CEO on another device, your computer can ask this phone to confirm it's you. Pair this phone with your computer to turn that on."
          />
        ) : items.length === 0 ? (
          <View style={{ alignItems: 'center', paddingVertical: spacing[8], gap: spacing[2] }}>
            <Txt style={{ fontSize: 40 }}>⚿</Txt>
            <Txt variant="body" tone="muted" center>
              No pending sign-in requests.
            </Txt>
          </View>
        ) : (
          items.map((item) => (
            <View
              key={item.requestId}
              style={{ backgroundColor: theme.colors.bgCard, borderRadius: radius.lg, borderWidth: 1, borderColor: theme.colors.border, padding: spacing[4], gap: spacing[3] }}
            >
              <View style={{ gap: spacing[1] }}>
                <Txt variant="caption" tone="accent" weight="bold" style={{ letterSpacing: 2, textTransform: 'uppercase' }}>
                  Sign-in request
                </Txt>
                <Txt variant="body" tone="secondary">
                  {item.context ?? 'A device wants to sign in'}
                </Txt>
              </View>

              <View style={{ alignItems: 'center', gap: spacing[1] }}>
                <Txt variant="caption" tone="muted">
                  Confirm this code matches
                </Txt>
                <Txt weight="bold" style={{ fontSize: 34, letterSpacing: 8, fontVariant: ['tabular-nums'], color: theme.colors.textPrimary }}>
                  {item.code}
                </Txt>
              </View>

              <View style={{ gap: spacing[2] }}>
                <NativeButton label="Approve" icon="✓" fullWidth loading={busyId === item.requestId} onPress={() => decide(item, true)} haptic="none" />
                <NativeButton label="Deny" variant="ghost" fullWidth onPress={() => decide(item, false)} haptic="none" />
              </View>
            </View>
          ))
        )}
      </ScrollView>
    </View>
  );
}
