import React from 'react';
import { Pressable, View } from 'react-native';
import Animated, {
  useAnimatedStyle,
  useSharedValue,
  withTiming,
} from 'react-native-reanimated';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import { haptics } from '@/lib/haptics';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

import { GlassPanel } from '../GlassPanel';
import { Txt } from '../Txt';
import { TABS } from './tabs.config';

/**
 * Minimal structural shape of React Navigation's `BottomTabBarProps` — typed
 * locally because React Navigation ships bundled inside expo-router rather
 * than as a top-level package. The real props are assignable to this.
 */
interface TabBarProps {
  state: { index: number; routes: { key: string; name: string }[] };
  navigation: {
    navigate: (name: string) => void;
    // `emit` is a broad generic in React Navigation; we only read `defaultPrevented`.
    emit: (event: any) => { defaultPrevented?: boolean };
  };
}

/** Custom floating bottom tab bar for phones — glassy, haptic, animated. */
export function BottomTabs({ state, navigation }: TabBarProps) {
  const theme = useTheme();
  const insets = useSafeAreaInsets();

  return (
    <View
      style={{
        position: 'absolute',
        left: spacing[4],
        right: spacing[4],
        bottom: Math.max(insets.bottom, spacing[3]),
      }}
    >
      <GlassPanel radius="2xl" style={{ paddingHorizontal: spacing[2], paddingVertical: spacing[2] }}>
        <View style={{ flexDirection: 'row', justifyContent: 'space-around' }}>
          {state.routes.map((route, index) => {
            const def = TABS.find((t) => t.name === route.name);
            if (!def) return null;
            const focused = state.index === index;
            return (
              <TabButton
                key={route.key}
                label={def.label}
                icon={def.icon}
                focused={focused}
                onPress={() => {
                  const event = navigation.emit({
                    type: 'tabPress',
                    target: route.key,
                    canPreventDefault: true,
                  });
                  if (!focused && !event.defaultPrevented) {
                    haptics.tap();
                    navigation.navigate(route.name);
                  }
                }}
              />
            );
          })}
        </View>
      </GlassPanel>
    </View>
  );
}

function TabButton({
  label,
  icon,
  focused,
  onPress,
}: {
  label: string;
  icon: string;
  focused: boolean;
  onPress: () => void;
}) {
  const theme = useTheme();
  const scale = useSharedValue(1);
  const animStyle = useAnimatedStyle(() => ({ transform: [{ scale: scale.value }] }));

  return (
    <Pressable
      onPress={onPress}
      onPressIn={() => (scale.value = withTiming(0.94, { duration: 110 }))}
      onPressOut={() => (scale.value = withTiming(1, { duration: 140 }))}
      style={{ flex: 1, alignItems: 'center' }}
      accessibilityRole="button"
      accessibilityState={{ selected: focused }}
      accessibilityLabel={label}
    >
      <Animated.View
        style={[
          animStyle,
          {
            alignItems: 'center',
            gap: 3,
            paddingVertical: spacing[2],
            paddingHorizontal: spacing[1],
            borderRadius: radius.lg,
            backgroundColor: focused ? theme.colors.bgSelected : 'transparent',
            minWidth: 64,
          },
        ]}
      >
        <Txt style={{ fontSize: 18, color: focused ? theme.colors.accent : theme.colors.textMuted }}>
          {icon}
        </Txt>
        <Txt
          variant="caption"
          numberOfLines={1}
          allowFontScaling={false}
          style={{ fontSize: 10, color: focused ? theme.colors.accent : theme.colors.textMuted }}
          weight={focused ? 'semibold' : 'medium'}
        >
          {label}
        </Txt>
      </Animated.View>
    </Pressable>
  );
}
