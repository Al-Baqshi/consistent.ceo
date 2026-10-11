import { useRouter } from 'expo-router';
import React, { useEffect } from 'react';
import { Pressable, ScrollView, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import { DemoNotice, EntityNode, IconButton, SectionHeader, StatusPill, Txt } from '@/components';
import { haptics } from '@/lib/haptics';
import { openExternal, PRIVACY_URL, SUPPORT_URL } from '@/lib/links';
import { useAppStore } from '@/store/appStore';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

/**
 * Profile — who is signed in, which computer this phone is paired with,
 * trusted devices, and doors to Settings / Logs / session approval.
 */
export default function ProfileScreen() {
  const theme = useTheme();
  const router = useRouter();
  const insets = useSafeAreaInsets();

  const health = useAppStore((s) => s.health);
  const devices = useAppStore((s) => s.devices);
  const loadControl = useAppStore((s) => s.loadControl);
  const user = useAppStore((s) => s.user);
  const userLoading = useAppStore((s) => s.userLoading);
  const loadUser = useAppStore((s) => s.loadUser);
  const signOut = useAppStore((s) => s.signOut);

  useEffect(() => {
    if (!health) loadControl();
  }, [health, loadControl]);

  // Re-check on every open: the token may have been added or revoked since.
  useEffect(() => {
    loadUser();
  }, [loadUser]);

  // Sample data only in demo mode (nothing paired). A paired computer is
  // described by what it actually answered — never "mock data".
  const demoData = !!health?.demo && !health.live;
  const connection: { tone: 'running' | 'warning' | 'danger' | 'idle'; label: string; short: string } =
    !health || demoData
      ? { tone: 'idle', label: 'Not paired with a computer', short: 'Not paired' }
      : health.desktopPaired
        ? {
            tone: 'running',
            label: 'Connected to your computer',
            short: health.latencyMs != null ? `Connected · ${health.latencyMs}ms` : 'Connected',
          }
        : health.signedOut
          ? { tone: 'warning', label: 'Signed out on your computer', short: 'Sign-in needed' }
          : { tone: 'danger', label: 'Can’t reach your computer', short: 'Unreachable' };

  const NavRow = ({
    icon,
    label,
    caption,
    onPress,
  }: {
    icon: string;
    label: string;
    caption: string;
    onPress: () => void;
  }) => (
    <Pressable
      onPress={() => {
        haptics.tap();
        onPress();
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
      <View
        style={{
          width: 38,
          height: 38,
          borderRadius: radius.md,
          alignItems: 'center',
          justifyContent: 'center',
          backgroundColor: theme.colors.bgSelected,
        }}
      >
        <Txt tone="accent" style={{ fontSize: 17 }}>
          {icon}
        </Txt>
      </View>
      <View style={{ flex: 1 }}>
        <Txt variant="label" style={{ fontSize: 15 }}>
          {label}
        </Txt>
        <Txt variant="caption" tone="muted">
          {caption}
        </Txt>
      </View>
      <Txt tone="muted">›</Txt>
    </Pressable>
  );

  return (
    <View style={{ flex: 1, backgroundColor: theme.colors.bg, paddingTop: insets.top }}>
      <View style={{ flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', paddingHorizontal: spacing[5], paddingVertical: spacing[3] }}>
        <Txt variant="title">You</Txt>
        <IconButton icon="✕" filled onPress={() => router.back()} label="Close" />
      </View>

      <ScrollView
        contentContainerStyle={{ paddingHorizontal: spacing[5], paddingBottom: insets.bottom + spacing[12], gap: spacing[3] }}
        showsVerticalScrollIndicator={false}
      >
        {/* Identity */}
        <View
          style={{
            alignItems: 'center',
            backgroundColor: theme.colors.bgCard,
            borderRadius: radius.xl,
            borderWidth: 1,
            borderColor: theme.colors.border,
            padding: spacing[6],
            gap: spacing[2],
          }}
        >
          <EntityNode type="person" size={84} />
          <Txt variant="heading" style={{ fontSize: 20, marginTop: spacing[2] }}>
            {user?.username ?? (userLoading ? '…' : 'Not signed in')}
          </Txt>
          <Txt variant="caption" tone="muted">
            {user
              ? `Workspace · ${user.workspaceId}`
              : userLoading
                ? 'Checking your session…'
                : 'Sign in to connect this phone to your world'}
          </Txt>
          {user?.email ? (
            <Txt variant="caption" tone="muted">
              {user.email}
            </Txt>
          ) : null}
          {user ? (
            <Txt variant="caption" tone="muted" style={{ opacity: 0.7 }}>
              ID {user.userId}
            </Txt>
          ) : null}
          {health ? (
            <View style={{ marginTop: spacing[1] }}>
              <StatusPill tone={connection.tone} label={connection.label} />
            </View>
          ) : null}
          <Pressable
            onPress={async () => {
              haptics.tap();
              if (user) {
                await signOut();
              } else {
                router.push('/login');
              }
            }}
            style={{ marginTop: spacing[2] }}
          >
            <Txt variant="caption" tone={user ? 'danger' : 'accent'} weight="semibold">
              {user ? 'Sign out' : 'Sign in →'}
            </Txt>
          </Pressable>
        </View>

        {demoData ? <DemoNotice /> : null}

        {/* Paired computer */}
        <SectionHeader title="Connected computer" />
        <View
          style={{
            backgroundColor: theme.colors.bgCard,
            borderRadius: radius.lg,
            borderWidth: 1,
            borderColor: theme.colors.border,
            padding: spacing[4],
            gap: spacing[3],
          }}
        >
          <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[3] }}>
            <View style={{ width: 44, height: 44, borderRadius: radius.md, alignItems: 'center', justifyContent: 'center', backgroundColor: theme.colors.bgCardHover }}>
              <Txt tone="accent" style={{ fontSize: 20 }}>
                ▣
              </Txt>
            </View>
            <View style={{ flex: 1 }}>
              <Txt variant="label" style={{ fontSize: 15 }} numberOfLines={1}>
                {health?.desktopName ?? 'Your computer'}
              </Txt>
              <Txt variant="caption" tone="muted" numberOfLines={1}>
                {demoData ? 'Not paired yet' : (health?.gatewayUrl.replace(/^https?:\/\//, '') ?? 'Checking…')}
              </Txt>
            </View>
            {health ? <StatusPill tone={connection.tone} label={connection.short} /> : null}
          </View>
        </View>

        {/* Trusted devices */}
        <SectionHeader title="Trusted devices" />
        <View style={{ gap: spacing[2] }}>
          {devices.map((d) => (
            <View
              key={d.id}
              style={{
                flexDirection: 'row',
                alignItems: 'center',
                gap: spacing[3],
                backgroundColor: theme.colors.bgCard,
                borderRadius: radius.lg,
                borderWidth: 1,
                borderColor: theme.colors.border,
                padding: spacing[3],
              }}
            >
              <View style={{ width: 34, height: 34, borderRadius: radius.md, backgroundColor: theme.colors.bgCardHover, alignItems: 'center', justifyContent: 'center' }}>
                <Txt tone="accent">{d.platform.toLowerCase().includes('mac') || d.platform.toLowerCase().includes('desktop') ? '▣' : d.platform.toLowerCase().includes('ipad') ? '▭' : '▢'}</Txt>
              </View>
              <View style={{ flex: 1 }}>
                <Txt variant="label">
                  {d.name}
                  {d.current ? (
                    <Txt variant="caption" tone="accent">
                      {' '}
                      · this device
                    </Txt>
                  ) : null}
                </Txt>
                <Txt variant="caption" tone="muted">
                  {d.platform}
                </Txt>
              </View>
              <StatusPill tone={d.trusted ? 'running' : 'idle'} label={d.trusted ? 'Trusted' : 'Pending'} />
            </View>
          ))}
        </View>

        {/* Doors */}
        <SectionHeader title="App" />
        <View style={{ gap: spacing[2] }}>
          <NavRow icon="✦" label="Skills" caption="Playbooks your agents load on demand" onPress={() => router.push('/skills')} />
          <NavRow icon="⚷" label="Vault" caption="Passwords, API keys, environment variables" onPress={() => router.push('/vault')} />
          <NavRow icon="⛭" label="Settings" caption="Theme, notifications, connection, privacy, account" onPress={() => router.push('/settings')} />
          <NavRow icon="≣" label="Logs" caption="What the app and workers have been doing" onPress={() => router.push('/logs')} />
          {/* The authenticator lists the real pending requests; `sess-1` was a
              fixture id that opened a request nobody had made. */}
          <NavRow icon="⚿" label="Authenticator" caption="Approve sign-ins on your other devices" onPress={() => router.push('/authenticator')} />
        </View>

        <SectionHeader title="About" />
        <View style={{ gap: spacing[2] }}>
          <NavRow icon="◇" label="Privacy policy" caption="What the app sends, and where" onPress={() => void openExternal(PRIVACY_URL)} />
          <NavRow icon="?" label="Help & support" caption="Guides, questions and contact" onPress={() => void openExternal(SUPPORT_URL)} />
        </View>
      </ScrollView>
    </View>
  );
}
