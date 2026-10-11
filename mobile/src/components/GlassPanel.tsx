import { BlurView } from 'expo-blur';
import React from 'react';
import { Platform, StyleProp, View, ViewStyle } from 'react-native';

import { useTheme } from '@/theme/ThemeProvider';
import { radius } from '@/theme/tokens';

interface Props {
  children: React.ReactNode;
  style?: StyleProp<ViewStyle>;
  intensity?: number;
  /** Turn off the border (e.g. when nesting). */
  bordered?: boolean;
  radius?: keyof typeof radius;
}

/**
 * Frosted surface. Uses a real BlurView on iOS; on Android/web falls back to
 * a solid translucent card (blur there is expensive / inconsistent).
 */
export function GlassPanel({
  children,
  style,
  intensity = 30,
  bordered = true,
  radius: r = 'xl',
}: Props) {
  const theme = useTheme();
  const rounded = radius[r];
  const border = bordered
    ? { borderWidth: 1, borderColor: theme.colors.border }
    : null;

  if (Platform.OS === 'ios') {
    return (
      <BlurView
        intensity={intensity}
        tint={theme.mode === 'dark' ? 'dark' : 'light'}
        style={[
          { borderRadius: rounded, overflow: 'hidden' },
          border,
          style,
        ]}
      >
        <View style={{ backgroundColor: theme.colors.glass }}>{children}</View>
      </BlurView>
    );
  }

  return (
    <View
      style={[
        {
          borderRadius: rounded,
          overflow: 'hidden',
          backgroundColor: theme.colors.glassStrong,
        },
        border,
        style,
      ]}
    >
      {children}
    </View>
  );
}
