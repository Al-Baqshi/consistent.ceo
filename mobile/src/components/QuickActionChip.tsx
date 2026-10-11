import React from 'react';
import { Pressable } from 'react-native';
import Animated, {
  useAnimatedStyle,
  useSharedValue,
  withTiming,
} from 'react-native-reanimated';

import { haptics } from '@/lib/haptics';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

import { Txt } from './Txt';

interface Props {
  label: string;
  icon?: string;
  onPress?: () => void;
  active?: boolean;
}

export function QuickActionChip({ label, icon, onPress, active }: Props) {
  const theme = useTheme();
  const scale = useSharedValue(1);
  const animStyle = useAnimatedStyle(() => ({ transform: [{ scale: scale.value }] }));

  return (
    <Animated.View style={animStyle}>
      <Pressable
        onPress={() => {
          haptics.tap();
          onPress?.();
        }}
        onPressIn={() => (scale.value = withTiming(0.97, { duration: 110 }))}
        onPressOut={() => (scale.value = withTiming(1, { duration: 140 }))}
        style={{
          flexDirection: 'row',
          alignItems: 'center',
          gap: spacing[2],
          paddingHorizontal: spacing[3] + 2,
          paddingVertical: spacing[2] + 2,
          borderRadius: radius.full,
          backgroundColor: active ? theme.colors.bgSelected : theme.colors.bgCard,
          borderWidth: 1,
          borderColor: active ? theme.colors.borderFocus : theme.colors.border,
        }}
      >
        {icon ? (
          <Txt tone="accent" weight="semibold" style={{ fontSize: 13 }}>
            {icon}
          </Txt>
        ) : null}
        <Txt variant="label" tone={active ? 'accent' : 'secondary'}>
          {label}
        </Txt>
      </Pressable>
    </Animated.View>
  );
}
