import { LinearGradient } from 'expo-linear-gradient';
import React from 'react';
import { ActivityIndicator, Pressable, StyleProp, View, ViewStyle } from 'react-native';
import Animated, {
  useAnimatedStyle,
  useSharedValue,
  withTiming,
} from 'react-native-reanimated';

import { haptics } from '@/lib/haptics';
import { useTheme } from '@/theme/ThemeProvider';
import { HIT_SLOP, MIN_TOUCH, radius, spacing } from '@/theme/tokens';

import { Txt } from './Txt';

type Variant = 'primary' | 'ghost' | 'outline' | 'danger';
type Size = 'sm' | 'md' | 'lg';

interface Props {
  label: string;
  onPress?: () => void;
  variant?: Variant;
  size?: Size;
  icon?: string;
  disabled?: boolean;
  loading?: boolean;
  fullWidth?: boolean;
  style?: StyleProp<ViewStyle>;
  haptic?: 'tap' | 'press' | 'success' | 'none';
}

const heights: Record<Size, number> = { sm: 40, md: 48, lg: 56 };

export function NativeButton({
  label,
  onPress,
  variant = 'primary',
  size = 'md',
  icon,
  disabled,
  loading,
  fullWidth,
  style,
  haptic = 'press',
}: Props) {
  const theme = useTheme();
  const scale = useSharedValue(1);
  const height = heights[size];

  const animStyle = useAnimatedStyle(() => ({ transform: [{ scale: scale.value }] }));

  const handlePress = () => {
    if (disabled || loading) return;
    if (haptic !== 'none') haptics[haptic]();
    onPress?.();
  };

  const isPrimary = variant === 'primary';
  const isDanger = variant === 'danger';
  const textTone = isPrimary || isDanger ? 'inverse' : variant === 'outline' ? 'accent' : 'primary';

  const body = (
    <View
      style={{
        flexDirection: 'row',
        alignItems: 'center',
        justifyContent: 'center',
        gap: spacing[2],
        height,
        paddingHorizontal: size === 'sm' ? spacing[4] : spacing[5],
      }}
    >
      {loading ? (
        <ActivityIndicator color={isPrimary || isDanger ? theme.colors.textInverse : theme.colors.accent} />
      ) : (
        <>
          {icon ? (
            <Txt variant="body" tone={textTone} weight="bold">
              {icon}
            </Txt>
          ) : null}
          <Txt variant="label" tone={textTone} weight="semibold" style={{ fontSize: size === 'lg' ? 16 : 15 }}>
            {label}
          </Txt>
        </>
      )}
    </View>
  );

  return (
    <Animated.View style={[animStyle, fullWidth && { alignSelf: 'stretch' }, style]}>
      <Pressable
        onPress={handlePress}
        onPressIn={() => (scale.value = withTiming(0.97, { duration: 110 }))}
        onPressOut={() => (scale.value = withTiming(1, { duration: 140 }))}
        disabled={disabled || loading}
        hitSlop={HIT_SLOP}
        style={{ opacity: disabled ? 0.5 : 1, minHeight: MIN_TOUCH }}
      >
        {isPrimary ? (
          <LinearGradient
            colors={theme.gradients.primaryButton}
            start={{ x: 0, y: 0 }}
            end={{ x: 0, y: 1 }}
            style={{ borderRadius: radius.lg, overflow: 'hidden' }}
          >
            {body}
          </LinearGradient>
        ) : (
          <View
            style={{
              borderRadius: radius.lg,
              overflow: 'hidden',
              backgroundColor: isDanger
                ? theme.colors.danger
                : variant === 'ghost'
                  ? theme.colors.bgCard
                  : 'transparent',
              borderWidth: variant === 'outline' ? 1.5 : 0,
              borderColor: theme.colors.borderFocus,
            }}
          >
            {body}
          </View>
        )}
      </Pressable>
    </Animated.View>
  );
}
