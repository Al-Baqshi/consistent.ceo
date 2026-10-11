import { useRouter } from 'expo-router';
import React from 'react';
import { View, type StyleProp, type ViewStyle } from 'react-native';

import { useTheme } from '@/theme/ThemeProvider';
import { radius, sky, spacing } from '@/theme/tokens';

import { NativeButton } from './NativeButton';
import { Txt } from './Txt';

/**
 * The demo-mode state for a screen that has no sample data to stand in: the
 * Vault, Canvas, Voice and Authenticator only make sense against the operator's
 * own computer. It says what the screen does and how to get there, instead of
 * a connection error that reads as "this app is broken".
 */
export function NeedsComputer({
  icon,
  title,
  body,
  onSky,
  style,
}: {
  icon: string;
  title: string;
  body: string;
  /** Render for the always-dark sky screens (Canvas), not the theme. */
  onSky?: boolean;
  style?: StyleProp<ViewStyle>;
}) {
  const theme = useTheme();
  const router = useRouter();
  return (
    <View
      style={[
        {
          backgroundColor: onSky ? sky.overlayStrong : theme.colors.bgCard,
          borderRadius: radius.xl,
          borderWidth: 1,
          borderColor: onSky ? sky.overlayBorder : theme.colors.border,
          padding: spacing[5],
          gap: spacing[3],
          alignItems: 'center',
        },
        style,
      ]}
    >
      <Txt tone="accent" style={{ fontSize: 34 }}>
        {icon}
      </Txt>
      <Txt variant="heading" center style={onSky ? { color: sky.text } : undefined}>
        {title}
      </Txt>
      <Txt
        variant="body"
        tone="secondary"
        center
        style={[{ lineHeight: 21 }, onSky ? { color: sky.textSecondary } : null]}
      >
        {body}
      </Txt>
      <NativeButton label="Pair with your computer" icon="◉" fullWidth onPress={() => router.push('/pair')} />
    </View>
  );
}
