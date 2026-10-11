/**
 * Platform status: health, workers, trusted devices, diagnostics.
 *
 * These used to sit under the Control tab, above and below the approvals that
 * were the reason anyone opened it. Approvals are time-sensitive and everything
 * here is not, so the tab became the approvals inbox and this became the place
 * you go when you want to know how the platform itself is doing.
 */
import { useRouter } from 'expo-router';
import React, { useEffect, useState } from 'react';
import { Pressable, View } from 'react-native';

import { DemoNotice, IconButton, Screen, SectionHeader, StatusPill, Txt } from '@/components';
import { haptics } from '@/lib/haptics';
import { useAppStore } from '@/store/appStore';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

export default function SystemScreen() {
  const theme = useTheme();
  const router = useRouter();

  const health = useAppStore((s) => s.health);
  const workers = useAppStore((s) => s.workers);
  const devices = useAppStore((s) => s.devices);
  const loadControl = useAppStore((s) => s.loadControl);

  const [refreshing, setRefreshing] = useState(false);

  useEffect(() => {
    if (!health) loadControl();
  }, [health, loadControl]);

  const onRefresh = async () => {
    setRefreshing(true);
    await loadControl();
    setRefreshing(false);
  };

  return (
    <Screen
      scroll
      onRefresh={onRefresh}
      refreshing={refreshing}
      title="Platform"
      subtitle="How the system itself is doing"
    >
      {health?.demo && !health.live ? <DemoNotice style={{ marginBottom: spacing[3] }} /> : null}

      {/* System health — every figure here comes from the gateway or shows an
          em-dash. There is no host CPU/memory telemetry to report, so none is
          shown; see `SystemHealth` in src/api/types.ts. */}
      {health ? (
        <View style={{ gap: spacing[3] }}>
          <View style={{ flexDirection: 'row', gap: spacing[3] }}>
            <HealthTile
              label="Connection"
              value={health.gatewayConnected ? 'Connected' : 'Offline'}
              sub={`${health.latencyMs != null ? `${health.latencyMs}ms · ` : ''}${health.gatewayUrl.replace(/^https?:\/\//, '')}`}
              ok={health.gatewayConnected}
            />
            {/* The gateway runs inside the desktop app, so an authenticated
                answer from it IS the pairing. */}
            <HealthTile
              label="Computer"
              value={
                health.demo && !health.live
                  ? 'Not paired'
                  : health.desktopPaired
                    ? 'Connected'
                    : health.signedOut
                      ? 'Sign-in needed'
                      : 'Unreachable'
              }
              sub={
                health.desktopPaired
                  ? (health.desktopName ?? 'Consistent CEO')
                  : health.signedOut
                    ? 'Reachable, but this phone is signed out'
                    : health.demo && !health.live
                      ? 'Pair to see your own data'
                      : 'Check that it’s awake and online'
              }
              ok={health.desktopPaired}
            />
          </View>
          <View style={{ flexDirection: 'row', gap: spacing[3] }}>
            <CountTile label="Workers" value={health.activeWorkers} />
            <CountTile label="Queued" value={health.queuedMissions} />
            <CountTile
              label="LLM calls"
              value={health.llmRequests}
              sub={health.llmCostUsd != null ? `$${health.llmCostUsd.toFixed(2)}` : undefined}
            />
          </View>
          {/* No "Pause platform" button: the computer has no platform-wide
              pause, and a switch that only flipped a flag on this phone was a
              control that did nothing (App Review 2.1). */}
        </View>
      ) : null}

      {/* Workers */}
      <SectionHeader title="Workers" />
      <View style={{ gap: spacing[2] }}>
        {workers.map((w) => (
          <View key={w.id} style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[3], backgroundColor: theme.colors.bgCard, borderRadius: radius.lg, borderWidth: 1, borderColor: theme.colors.border, padding: spacing[3] }}>
            <View style={{ flex: 1 }}>
              <Txt variant="label">{w.name}</Txt>
              <Txt variant="caption" tone="muted" numberOfLines={1}>
                {w.role}
              </Txt>
            </View>
            {/* Live agents carry no run state (the roster says who exists, not
                what they're doing), so no pill rather than a blanket "Idle". */}
            {w.status ? (
              <StatusPill
                tone={w.status === 'running' ? 'running' : w.status === 'paused' ? 'warning' : w.status === 'error' ? 'danger' : 'idle'}
                label={w.status[0].toUpperCase() + w.status.slice(1)}
              />
            ) : null}
          </View>
        ))}
      </View>

      {/* Trusted devices */}
      <SectionHeader title="Trusted devices" action={<IconButton icon="⛭" onPress={() => router.push('/settings')} />} />
      <View style={{ gap: spacing[2] }}>
        {devices.map((d) => (
          <View key={d.id} style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[3], backgroundColor: theme.colors.bgCard, borderRadius: radius.lg, borderWidth: 1, borderColor: theme.colors.border, padding: spacing[3] }}>
            <View style={{ width: 34, height: 34, borderRadius: radius.md, backgroundColor: theme.colors.bgSurface, alignItems: 'center', justifyContent: 'center' }}>
              <Txt tone="accent">{d.platform.includes('mac') || d.platform.includes('desktop') ? '▣' : d.platform.includes('iPad') ? '▭' : '▢'}</Txt>
            </View>
            <View style={{ flex: 1 }}>
              <Txt variant="label">
                {d.name}
                {d.current ? <Txt variant="caption" tone="accent"> · this device</Txt> : null}
              </Txt>
              <Txt variant="caption" tone="muted">
                {d.platform}
              </Txt>
            </View>
            <StatusPill tone={d.trusted ? 'running' : 'idle'} label={d.trusted ? 'Trusted' : 'Pending'} />
          </View>
        ))}
      </View>

      {/* Logs */}
      <SectionHeader title="Diagnostics" />
      <Pressable
        onPress={() => {
          haptics.tap();
          router.push('/logs');
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
            ≣
          </Txt>
        </View>
        <View style={{ flex: 1 }}>
          <Txt variant="label" style={{ fontSize: 15 }}>
            View logs
          </Txt>
          <Txt variant="caption" tone="muted">
            Calls to your computer, workers, and system events on this phone
          </Txt>
        </View>
        <Txt tone="muted">›</Txt>
      </Pressable>
    </Screen>
  );
}

function HealthTile({ label, value, sub, ok }: { label: string; value: string; sub: string; ok: boolean }) {
  const theme = useTheme();
  return (
    <View style={{ flex: 1, backgroundColor: theme.colors.bgCard, borderRadius: radius.lg, borderWidth: 1, borderColor: theme.colors.border, padding: spacing[4], gap: 4 }}>
      <Txt variant="caption" tone="muted" weight="bold" style={{ letterSpacing: 1, textTransform: 'uppercase' }}>
        {label}
      </Txt>
      <View style={{ flexDirection: 'row', alignItems: 'center', gap: 6 }}>
        <View style={{ width: 8, height: 8, borderRadius: 4, backgroundColor: ok ? theme.colors.success : theme.colors.danger }} />
        <Txt variant="body" weight="semibold">
          {value}
        </Txt>
      </View>
      <Txt variant="caption" tone="muted" numberOfLines={1}>
        {sub}
      </Txt>
    </View>
  );
}

/**
 * A count the gateway reported — or an em-dash when it reported nothing.
 * A missing number must never render as `0`, which reads as a measurement.
 */
function CountTile({ label, value, sub }: { label: string; value?: number; sub?: string }) {
  const theme = useTheme();
  const known = value != null && Number.isFinite(value);
  return (
    <View style={{ flex: 1, backgroundColor: theme.colors.bgCard, borderRadius: radius.lg, borderWidth: 1, borderColor: theme.colors.border, padding: spacing[3], alignItems: 'center', justifyContent: 'center', gap: 2 }}>
      <Txt variant="title" tone={known ? 'accent' : 'muted'}>
        {known ? value : '—'}
      </Txt>
      <Txt variant="caption" tone="muted">
        {label}
      </Txt>
      {sub ? (
        <Txt variant="caption" tone="muted">
          {sub}
        </Txt>
      ) : null}
    </View>
  );
}
