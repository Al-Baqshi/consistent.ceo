/**
 * The approvals inbox — everything waiting on a decision from you, in one list.
 *
 * Two kinds arrive here and they are deliberately not separated into sections:
 *
 * 1. **Sign-in requests** (`GET /api/auth/approvals`) — a device asking to sign
 *    in as you. These carry a code that has to be read off the requesting device
 *    and matched, because approving without checking it is the entire attack
 *    this flow exists to stop.
 * 2. **Action approvals** (`GET /api/approvals`) — the policy gate stopping an
 *    agent before it does something outward-facing.
 *
 * They interleave by age, newest first. Splitting them by type would sort by a
 * property nobody is deciding on: what matters is which of these has been
 * waiting longest, not which subsystem raised it.
 *
 * The platform's own health, workers and devices moved to `/system` — none of it
 * is time-sensitive, and it was burying the things that are.
 */
import { useRouter } from 'expo-router';
import React, { useEffect, useMemo, useState } from 'react';
import { Alert, Pressable, View } from 'react-native';

import type { Approval, Session } from '@/api';
import {
  ApprovalCard,
  DemoNotice,
  IconButton,
  NativeButton,
  Screen,
  SectionHeader,
  Txt,
} from '@/components';
import { haptics } from '@/lib/haptics';
import { useAppStore } from '@/store/appStore';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

/** One entry in the merged feed, tagged so the renderer can branch. */
type InboxItem =
  | { kind: 'sign-in'; at: number; session: Session }
  | { kind: 'action'; at: number; approval: Approval };

/** Epoch millis, or 0 when a timestamp is missing or unparseable — an undated
 *  item sorts last rather than crashing the comparator or jumping to the top. */
function at(iso?: string): number {
  if (!iso) return 0;
  const t = Date.parse(iso);
  return Number.isNaN(t) ? 0 : t;
}

export default function ApprovalsScreen() {
  const theme = useTheme();
  const router = useRouter();

  const health = useAppStore((s) => s.health);
  const sessions = useAppStore((s) => s.sessions);
  const needsAttention = useAppStore((s) => s.needsAttention);
  const loadControl = useAppStore((s) => s.loadControl);
  const loadFeed = useAppStore((s) => s.loadFeed);
  const approve = useAppStore((s) => s.approve);
  const reject = useAppStore((s) => s.reject);
  const authenticate = useAppStore((s) => s.authenticate);

  const [refreshing, setRefreshing] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);

  // `loadFeed` refreshes approvals and operator events; it merges into the
  // chat rather than replacing it, so opening this tab no longer wipes the
  // conversation on the Chat tab.
  useEffect(() => {
    if (!health) loadControl();
    if (needsAttention.length === 0) loadFeed();
  }, [health, needsAttention.length, loadControl, loadFeed]);

  const onRefresh = async () => {
    setRefreshing(true);
    await Promise.all([loadControl(), loadFeed()]);
    setRefreshing(false);
  };

  const items = useMemo<InboxItem[]>(() => {
    const merged: InboxItem[] = [
      ...sessions.map((s) => ({ kind: 'sign-in' as const, at: at(s.requestedAt), session: s })),
      ...needsAttention.map((a) => ({ kind: 'action' as const, at: at(a.createdAt), approval: a })),
    ];
    return merged.sort((x, y) => y.at - x.at);
  }, [sessions, needsAttention]);

  /** A sign-in decision that never reached the gateway must say so out loud. */
  const decideSignIn = async (id: string, ok: boolean) => {
    try {
      await authenticate(id, ok);
      await loadControl();
    } catch (e) {
      haptics.error();
      Alert.alert('Still waiting', e instanceof Error ? e.message : 'Your computer did not answer.');
    }
  };

  return (
    <Screen
      scroll
      onRefresh={onRefresh}
      refreshing={refreshing}
      title="Approvals"
      subtitle={
        items.length > 0
          ? `${items.length} waiting on you`
          : 'Nothing needs a decision'
      }
      headerRight={
        <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[2] }}>
          <IconButton icon="▤" onPress={() => router.push('/system')} label="Platform status" />
        </View>
      }
    >
      {health?.demo && !health.live ? <DemoNotice style={{ marginBottom: spacing[3] }} /> : null}

      {actionError ? (
        <Pressable
          onPress={() => setActionError(null)}
          style={{
            flexDirection: 'row',
            alignItems: 'center',
            gap: spacing[2],
            backgroundColor: theme.colors.bgCard,
            borderRadius: radius.lg,
            borderWidth: 1,
            borderColor: theme.colors.danger,
            padding: spacing[3],
            marginBottom: spacing[2],
          }}
        >
          <Txt style={{ color: theme.colors.danger }}>⚠</Txt>
          <Txt variant="caption" tone="secondary" style={{ flex: 1 }}>
            {actionError}
          </Txt>
          <Txt variant="caption" tone="muted">
            Dismiss
          </Txt>
        </Pressable>
      ) : null}

      {items.length === 0 ? (
        <View style={{ alignItems: 'center', gap: spacing[2], paddingVertical: spacing[8] }}>
          <Txt tone="accent" style={{ fontSize: 34 }}>
            ✓
          </Txt>
          <Txt variant="heading" style={{ fontSize: 17 }}>
            You're all caught up
          </Txt>
          <Txt variant="caption" tone="muted" center>
            Sign-in requests and anything an agent needs your go-ahead for will
            land here, and push you a notification.
          </Txt>
        </View>
      ) : (
        <View style={{ gap: spacing[3] }}>
          {items.map((item) =>
            item.kind === 'sign-in' ? (
              <SignInRequest
                key={`s:${item.session.id}`}
                session={item.session}
                onDecide={decideSignIn}
              />
            ) : (
              <ApprovalCard
                key={`a:${item.approval.id}`}
                approval={item.approval}
                onApprove={approve}
                onReject={reject}
              />
            ),
          )}
        </View>
      )}

      <SectionHeader title="Platform" />
      <Pressable
        onPress={() => {
          haptics.tap();
          router.push('/system');
        }}
        style={{
          flexDirection: 'row',
          alignItems: 'center',
          gap: spacing[3],
          backgroundColor: theme.colors.bgCard,
          borderRadius: radius.lg,
          borderWidth: 1,
          borderColor: theme.colors.border,
          padding: spacing[4],
        }}
      >
        <View style={{ width: 38, height: 38, borderRadius: radius.md, alignItems: 'center', justifyContent: 'center', backgroundColor: theme.colors.bgSelected }}>
          <Txt tone="accent" style={{ fontSize: 17 }}>
            ▤
          </Txt>
        </View>
        <View style={{ flex: 1 }}>
          <Txt variant="label" style={{ fontSize: 15 }}>
            Platform status
          </Txt>
          <Txt variant="caption" tone="muted">
            Health, workers, trusted devices, logs
          </Txt>
        </View>
        <Txt tone="muted">›</Txt>
      </Pressable>
    </Screen>
  );
}

/**
 * A device asking to sign in as you.
 *
 * The code is shown large and monospaced on purpose: the only thing that makes
 * this safe is the user comparing it against the screen that requested it, so it
 * has to be readable at a glance and hard to skim past.
 */
function SignInRequest({
  session,
  onDecide,
}: {
  session: Session;
  onDecide: (id: string, approve: boolean) => void;
}) {
  const theme = useTheme();
  return (
    <View style={{ backgroundColor: theme.colors.bgCard, borderRadius: radius.xl, borderWidth: 1, borderColor: theme.colors.borderFocus, padding: spacing[4], gap: spacing[3] }}>
      <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[2] }}>
        <Txt tone="accent">⚿</Txt>
        <View style={{ flex: 1 }}>
          <Txt variant="heading" style={{ fontSize: 16 }}>
            {session.device}
          </Txt>
          <Txt variant="caption" tone="muted">
            {session.location ?? 'Sign-in request'}
          </Txt>
        </View>
      </View>
      {session.code ? (
        <View style={{ alignItems: 'center', gap: 2 }}>
          <Txt variant="caption" tone="muted">
            Confirm this code matches
          </Txt>
          <Txt weight="bold" style={{ fontSize: 30, letterSpacing: 8, fontVariant: ['tabular-nums'], color: theme.colors.textPrimary }}>
            {session.code}
          </Txt>
        </View>
      ) : null}
      <View style={{ flexDirection: 'row', gap: spacing[2] }}>
        <NativeButton label="Deny" variant="ghost" size="sm" style={{ flex: 1 }} onPress={() => onDecide(session.id, false)} />
        <NativeButton label="Approve sign-in" variant="primary" size="sm" icon="✓" style={{ flex: 1.4 }} onPress={() => onDecide(session.id, true)} haptic="success" />
      </View>
    </View>
  );
}
