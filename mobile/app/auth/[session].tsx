import { useLocalSearchParams, useRouter } from 'expo-router';
import React, { useEffect, useState } from 'react';
import { ActivityIndicator, View } from 'react-native';
import Animated, { FadeIn, FadeInDown } from 'react-native-reanimated';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import { api } from '@/api';
import type { Session } from '@/api';
import { IconButton, NativeButton, Txt } from '@/components';
import { EntityNode } from '@/components';
import { haptics } from '@/lib/haptics';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

function shortWhen(iso?: string): string {
  if (!iso) return '—';
  const mins = Math.round((Date.now() - new Date(iso).getTime()) / 60_000);
  if (!Number.isFinite(mins)) return '—';
  if (mins < 1) return 'Just now';
  if (mins < 60) return `${mins}m ago`;
  return `${Math.round(mins / 60)}h ago`;
}

/**
 * One sign-in request, in full. Everything shown comes from
 * `GET /api/auth/approvals` — the device label, the code, when it was asked.
 * The previous version hardcoded "Studio Mac · Chrome / Local network / Just
 * now" regardless of who was actually knocking, which is precisely the detail a
 * person uses to decide whether to approve.
 */
export default function AuthSessionScreen() {
  const theme = useTheme();
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const { session } = useLocalSearchParams<{ session: string }>();
  const [request, setRequest] = useState<Session | null>(null);
  const [loading, setLoading] = useState(true);
  const [result, setResult] = useState<'approved' | 'rejected' | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let cancelled = false;
    api.listPendingSessions().then((list) => {
      if (cancelled) return;
      setRequest(list.find((s) => s.id === session) ?? null);
      setLoading(false);
    });
    return () => {
      cancelled = true;
    };
  }, [session]);

  const decide = async (approve: boolean) => {
    if (!request) return;
    setBusy(true);
    setError(null);
    approve ? haptics.success() : haptics.warning();
    try {
      await api.authenticateSession(request.id, approve, request.code);
      setResult(approve ? 'approved' : 'rejected');
      setTimeout(() => router.back(), 1200);
    } catch {
      haptics.error();
      setError(
        "That didn't reach your computer. The request is still waiting — check you're signed in and try again.",
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <View style={{ flex: 1, backgroundColor: theme.colors.bg, paddingTop: insets.top }}>
      <View style={{ flexDirection: 'row', justifyContent: 'flex-end', paddingHorizontal: spacing[5], paddingVertical: spacing[3] }}>
        <IconButton icon="✕" filled onPress={() => router.back()} label="Close" />
      </View>

      <View style={{ flex: 1, alignItems: 'center', justifyContent: 'center', paddingHorizontal: spacing[6], gap: spacing[5] }}>
        {result ? (
          <Animated.View entering={FadeIn} style={{ alignItems: 'center', gap: spacing[4] }}>
            <View
              style={{
                width: 96,
                height: 96,
                borderRadius: 48,
                alignItems: 'center',
                justifyContent: 'center',
                backgroundColor: result === 'approved' ? theme.colors.success + '22' : theme.colors.danger + '22',
              }}
            >
              <Txt style={{ fontSize: 44, color: result === 'approved' ? theme.colors.success : theme.colors.danger }}>
                {result === 'approved' ? '✓' : '✕'}
              </Txt>
            </View>
            <Txt variant="title" center>
              {result === 'approved' ? 'Device approved' : 'Request denied'}
            </Txt>
            <Txt variant="body" tone="secondary" center>
              {result === 'approved'
                ? 'The desktop session is now trusted and connected.'
                : 'The session request was rejected.'}
            </Txt>
          </Animated.View>
        ) : loading ? (
          <ActivityIndicator color={theme.colors.accent} />
        ) : !request ? (
          <Animated.View entering={FadeIn} style={{ alignItems: 'center', gap: spacing[3] }}>
            <Txt style={{ fontSize: 40 }} tone="muted">
              ⚿
            </Txt>
            <Txt variant="title" center>
              No longer waiting
            </Txt>
            <Txt variant="body" tone="secondary" center style={{ lineHeight: 22 }}>
              This sign-in request isn’t pending any more — it may have expired,
              been handled elsewhere, or this phone isn’t signed in to your
              computer.
            </Txt>
            <NativeButton label="Close" variant="outline" onPress={() => router.back()} />
          </Animated.View>
        ) : (
          <>
            <Animated.View entering={FadeInDown.duration(220)} style={{ alignItems: 'center', gap: spacing[2] }}>
              <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[4] }}>
                <EntityNode type="person" size={64} />
                <View style={{ width: 40, alignItems: 'center' }}>
                  <Txt tone="accent" style={{ fontSize: 22 }}>
                    ⇄
                  </Txt>
                </View>
                <View style={{ width: 64, height: 64, borderRadius: radius.lg, backgroundColor: theme.colors.bgCard, borderWidth: 1, borderColor: theme.colors.border, alignItems: 'center', justifyContent: 'center' }}>
                  <Txt tone="accent" style={{ fontSize: 26 }}>▣</Txt>
                </View>
              </View>
            </Animated.View>

            <View style={{ alignItems: 'center', gap: spacing[2] }}>
              <Txt variant="caption" tone="accent" weight="bold" style={{ letterSpacing: 2, textTransform: 'uppercase' }}>
                Authentication request
              </Txt>
              <Txt variant="title" center>
                Approve this session?
              </Txt>
              <Txt variant="body" tone="secondary" center style={{ lineHeight: 22 }}>
                A device wants to sign in to your Consistent CEO workspace. Only
                approve if the code below matches the one it is showing.
              </Txt>
            </View>

            {request.code ? (
              <View style={{ alignItems: 'center', gap: spacing[1] }}>
                <Txt variant="caption" tone="muted">
                  Confirm this code matches
                </Txt>
                <Txt weight="bold" style={{ fontSize: 34, letterSpacing: 8, fontVariant: ['tabular-nums'], color: theme.colors.textPrimary }}>
                  {request.code}
                </Txt>
              </View>
            ) : null}

            <View style={{ width: '100%', backgroundColor: theme.colors.bgCard, borderRadius: radius.lg, borderWidth: 1, borderColor: theme.colors.border, padding: spacing[4], gap: spacing[2] }}>
              <Detail label="Requested by" value={request.device} />
              <Detail label="Request" value={request.id} />
              <Detail label="Asked" value={shortWhen(request.requestedAt)} />
              <Detail label="Expires" value={request.expiresAt ? new Date(request.expiresAt).toLocaleTimeString() : '—'} />
            </View>

            {error ? (
              <Txt variant="caption" center style={{ color: theme.colors.danger }}>
                {error}
              </Txt>
            ) : null}

            <View style={{ width: '100%', gap: spacing[2] }}>
              <NativeButton label="Approve sign-in" icon="⚿" fullWidth loading={busy} onPress={() => decide(true)} haptic="none" />
              <NativeButton label="Deny" variant="ghost" fullWidth onPress={() => decide(false)} haptic="none" />
            </View>
          </>
        )}
      </View>
    </View>
  );
}

function Detail({ label, value }: { label: string; value: string }) {
  return (
    <View style={{ flexDirection: 'row', justifyContent: 'space-between' }}>
      <Txt variant="body" tone="muted">
        {label}
      </Txt>
      <Txt variant="body" weight="semibold">
        {value}
      </Txt>
    </View>
  );
}
