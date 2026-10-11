import React from 'react';
import { Pressable, View } from 'react-native';
import { Gesture, GestureDetector } from 'react-native-gesture-handler';
import Animated, {
  runOnJS,
  useAnimatedStyle,
  useSharedValue,
  withSpring,
} from 'react-native-reanimated';

import { haptics } from '@/lib/haptics';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

import { Txt } from './Txt';

export interface SwipeAction {
  key: string;
  label: string;
  icon: string;
  color: string;
  onPress: () => void;
}

interface Props {
  children: React.ReactNode;
  rightActions?: SwipeAction[];
  leftActions?: SwipeAction[];
  actionWidth?: number;
}

/**
 * Lightweight pan-based swipe row (no dependency on a specific RNGH Swipeable
 * API version). Reveals action buttons on left/right and snaps open/closed.
 */
export function SwipeableRow({
  children,
  rightActions = [],
  leftActions = [],
  actionWidth = 76,
}: Props) {
  const theme = useTheme();
  const translateX = useSharedValue(0);
  const startX = useSharedValue(0);

  const rightWidth = rightActions.length * actionWidth;
  const leftWidth = leftActions.length * actionWidth;

  const pan = Gesture.Pan()
    .activeOffsetX([-12, 12])
    .failOffsetY([-16, 16])
    .onBegin(() => {
      startX.value = translateX.value;
    })
    .onUpdate((e) => {
      let next = startX.value + e.translationX;
      // clamp to available action widths
      if (next < -rightWidth) next = -rightWidth - (next + rightWidth) * 0.2;
      if (next > leftWidth) next = leftWidth + (next - leftWidth) * 0.2;
      translateX.value = next;
    })
    .onEnd((e) => {
      const v = e.velocityX;
      if (translateX.value < -rightWidth / 2 || v < -600) {
        translateX.value = withSpring(-rightWidth, { damping: 26, stiffness: 260, overshootClamping: true });
        runOnJS(haptics.tap)();
      } else if (translateX.value > leftWidth / 2 || v > 600) {
        translateX.value = withSpring(leftWidth, { damping: 26, stiffness: 260, overshootClamping: true });
        runOnJS(haptics.tap)();
      } else {
        translateX.value = withSpring(0, { damping: 26, stiffness: 260, overshootClamping: true });
      }
    });

  const rowStyle = useAnimatedStyle(() => ({ transform: [{ translateX: translateX.value }] }));

  const close = () => {
    translateX.value = withSpring(0, { damping: 26, stiffness: 260, overshootClamping: true });
  };

  const renderActions = (actions: SwipeAction[], side: 'left' | 'right') => (
    <View
      style={{
        position: 'absolute',
        top: 0,
        bottom: 0,
        [side]: 0,
        flexDirection: 'row',
      }}
    >
      {actions.map((a) => (
        <Pressable
          key={a.key}
          onPress={() => {
            haptics.press();
            a.onPress();
            close();
          }}
          style={{
            width: actionWidth,
            alignItems: 'center',
            justifyContent: 'center',
            gap: 4,
            backgroundColor: a.color,
            borderRadius: radius.lg,
            marginHorizontal: 3,
          }}
        >
          <Txt style={{ fontSize: 18, color: theme.colors.white }}>{a.icon}</Txt>
          <Txt variant="caption" style={{ color: theme.colors.white, fontSize: 10 }} weight="semibold">
            {a.label}
          </Txt>
        </Pressable>
      ))}
    </View>
  );

  return (
    <View style={{ marginBottom: spacing[2], borderRadius: radius.lg }}>
      {rightActions.length > 0 ? renderActions(rightActions, 'right') : null}
      {leftActions.length > 0 ? renderActions(leftActions, 'left') : null}
      <GestureDetector gesture={pan}>
        <Animated.View style={rowStyle}>{children}</Animated.View>
      </GestureDetector>
    </View>
  );
}
