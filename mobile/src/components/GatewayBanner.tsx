import { useRouter } from 'expo-router';
import React, { useEffect, useState } from 'react';
import { Pressable, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import { DEFAULT_GATEWAY_URL, gatewayState, getGatewayUrl } from '@/api';
import { haptics } from '@/lib/haptics';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

import { Txt } from './Txt';

/**
 * Slim floating pill shown while a REAL configured gateway is unreachable —
 * "Computer unreachable since HH:MM". Hidden entirely in demo mode (the built-in
 * default URL, where fixtures answer) and whenever the gateway responds.
 *
 * `gatewayState` is a plain mutable object kept fresh by every request the
 * app makes (the companion loop polls every 15s while foregrounded), so the
 * banner just re-reads it on a short interval rather than probing itself.
 */

const CHECK_MS = 5_000;

function sinceLabel(ts: number): string {
  return new Date(ts).toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
}

export function GatewayBanner() {
  const theme = useTheme();
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const [since, setSince] = useState<number | null>(null);

  useEffect(() => {
    let alive = true;
    const check = async () => {
      const url = await getGatewayUrl().catch(() => DEFAULT_GATEWAY_URL);
      if (!alive) return;
      const down =
        url !== DEFAULT_GATEWAY_URL &&
        gatewayState.reachable === false &&
        gatewayState.unreachableSince > 0;
      setSince(down ? gatewayState.unreachableSince : null);
    };
    void check();
    const timer = setInterval(() => void check(), CHECK_MS);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, []);

  if (since == null) return null;

  return (
    <Pressable
      onPress={() => {
        haptics.tap();
        router.push('/settings');
      }}
      accessibilityRole="button"
      accessibilityLabel={`Computer unreachable since ${sinceLabel(since)}. Open connection settings`}
      style={{
        position: 'absolute',
        top: insets.top + spacing[1],
        alignSelf: 'center',
        zIndex: 40,
        flexDirection: 'row',
        alignItems: 'center',
        gap: spacing[2],
        paddingHorizontal: spacing[3],
        paddingVertical: spacing[1] + 2,
        borderRadius: radius.full,
        backgroundColor: theme.colors.bgCard,
        borderWidth: 1,
        borderColor: theme.colors.danger + '66',
        shadowColor: '#000',
        shadowOpacity: 0.18,
        shadowRadius: 8,
        shadowOffset: { width: 0, height: 2 },
        elevation: 4,
      }}
    >
      <View style={{ width: 7, height: 7, borderRadius: 4, backgroundColor: theme.colors.danger }} />
      <Txt variant="caption" weight="semibold" style={{ color: theme.colors.danger }}>
        Computer unreachable since {sinceLabel(since)}
      </Txt>
    </Pressable>
  );
}
