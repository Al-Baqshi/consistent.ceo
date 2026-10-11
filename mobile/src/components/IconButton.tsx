import React from 'react';
import { Pressable, StyleProp, ViewStyle } from 'react-native';
import Animated, {
  useAnimatedStyle,
  useSharedValue,
  withTiming,
} from 'react-native-reanimated';

import { haptics } from '@/lib/haptics';
import { useTheme } from '@/theme/ThemeProvider';
import { HIT_SLOP, MIN_TOUCH, radius } from '@/theme/tokens';

import { Txt } from './Txt';

interface Props {
  icon: string;
  onPress?: () => void;
  onLongPress?: () => void;
  size?: number;
  glyphSize?: number;
  filled?: boolean;
  accent?: boolean;
  style?: StyleProp<ViewStyle>;
  label?: string; // accessibility
}

export function IconButton({
  icon,
  onPress,
  onLongPress,
  size = MIN_TOUCH,
  glyphSize = 18,
  filled = false,
  accent = false,
  style,
  label,
}: Props) {
  const theme = useTheme();
  const scale = useSharedValue(1);
  const animStyle = useAnimatedStyle(() => ({ transform: [{ scale: scale.value }] }));

  return (
    <Animated.View style={[animStyle, style]}>
      <Pressable
        accessibilityRole="button"
        accessibilityLabel={label ?? icon}
        onPress={() => {
          haptics.tap();
          onPress?.();
        }}
        onLongPress={
          onLongPress
            ? () => {
                haptics.heavy();
                onLongPress();
              }
            : undefined
        }
        onPressIn={() => (scale.value = withTiming(0.94, { duration: 110 }))}
        onPressOut={() => (scale.value = withTiming(1, { duration: 140 }))}
        hitSlop={HIT_SLOP}
        style={{
          width: size,
          height: size,
          borderRadius: radius.md,
          alignItems: 'center',
          justifyContent: 'center',
          backgroundColor: filled ? theme.colors.bgCard : 'transparent',
          borderWidth: filled ? 1 : 0,
          borderColor: theme.colors.border,
        }}
      >
        <Txt
          style={{ fontSize: glyphSize }}
          tone={accent ? 'accent' : 'secondary'}
          weight="semibold"
        >
          {icon}
        </Txt>
      </Pressable>
    </Animated.View>
  );
}
