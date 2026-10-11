import { useRouter } from 'expo-router';
import React from 'react';
import { Pressable, type StyleProp, type ViewStyle } from 'react-native';

import { haptics } from '@/lib/haptics';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

import { Txt } from './Txt';

/**
 * The one way the app says it is showing sample data.
 *
 * Shown only in demo mode — nothing paired yet, so fixtures stand in. It
 * replaces the "Live / Mock" and "Offline · mock data" pills, which were
 * developer vocabulary, and it never appears for a paired computer that is
 * briefly unreachable (that is the unreachable banner's job, and that data is
 * the operator's own).
 */
export function DemoNotice({ style }: { style?: StyleProp<ViewStyle> }) {
  const theme = useTheme();
  const router = useRouter();
  return (
    <Pressable
      onPress={() => {
        haptics.tap();
        router.push('/pair');
      }}
      accessibilityRole="button"
      accessibilityLabel="Showing sample data. Pair with your computer to see yours."
      style={({ pressed }) => [
        {
          flexDirection: 'row',
          alignItems: 'center',
          gap: spacing[2],
          paddingHorizontal: spacing[3],
          paddingVertical: spacing[2],
          borderRadius: radius.lg,
          backgroundColor: theme.colors.bgSelected,
          borderWidth: 1,
          borderColor: theme.colors.borderFocus,
          opacity: pressed ? 0.75 : 1,
        },
        style,
      ]}
    >
      <Txt tone="accent" style={{ fontSize: 13 }}>
        ◌
      </Txt>
      <Txt variant="caption" tone="secondary" style={{ flex: 1, lineHeight: 17 }}>
        Showing sample data — pair with your computer to see yours.
      </Txt>
      <Txt variant="caption" tone="accent" weight="semibold">
        Pair
      </Txt>
    </Pressable>
  );
}
