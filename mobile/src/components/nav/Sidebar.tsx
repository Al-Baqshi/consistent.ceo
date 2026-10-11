import { useRouter, usePathname } from 'expo-router';
import React from 'react';
import { Pressable, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import { haptics } from '@/lib/haptics';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

import { Txt } from '../Txt';
import { TABS } from './tabs.config';

export const SIDEBAR_WIDTH = 232;

/** Persistent left navigation rail for iPad / wide layouts. */
export function Sidebar() {
  const theme = useTheme();
  const router = useRouter();
  const pathname = usePathname();
  const insets = useSafeAreaInsets();

  const isActive = (href: string) =>
    href === '/' ? pathname === '/' : pathname.startsWith(href);

  return (
    <View
      style={{
        width: SIDEBAR_WIDTH,
        backgroundColor: theme.colors.bgSurface,
        borderRightWidth: 1,
        borderRightColor: theme.colors.border,
        paddingTop: insets.top + spacing[4],
        paddingBottom: insets.bottom + spacing[4],
        paddingHorizontal: spacing[3],
      }}
    >
      {/* Brand */}
      <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[3], paddingHorizontal: spacing[2], marginBottom: spacing[6] }}>
        <View
          style={{
            width: 34,
            height: 34,
            borderRadius: radius.md,
            backgroundColor: theme.colors.accent,
            alignItems: 'center',
            justifyContent: 'center',
          }}
        >
          <Txt style={{ color: theme.colors.textInverse, fontSize: 18 }} weight="bold">
            ✷
          </Txt>
        </View>
        <View style={{ flex: 1 }}>
          <Txt variant="label" weight="bold" numberOfLines={1}>
            Consistent CEO
          </Txt>
        </View>
      </View>

      {/* Nav items */}
      <View style={{ gap: spacing[1] }}>
        {TABS.map((t) => {
          const active = isActive(t.href);
          return (
            <Pressable
              key={t.name}
              onPress={() => {
                haptics.tap();
                router.navigate(t.href as never);
              }}
              style={{
                flexDirection: 'row',
                alignItems: 'center',
                gap: spacing[3],
                paddingVertical: spacing[3],
                paddingHorizontal: spacing[3],
                borderRadius: radius.lg,
                backgroundColor: active ? theme.colors.bgSelected : 'transparent',
                borderWidth: 1,
                borderColor: active ? theme.colors.borderFocus : 'transparent',
              }}
            >
              <Txt style={{ fontSize: 18, color: active ? theme.colors.accent : theme.colors.textMuted, width: 24 }}>
                {t.icon}
              </Txt>
              <Txt variant="body" tone={active ? 'accent' : 'secondary'} weight={active ? 'semibold' : 'normal'}>
                {t.label}
              </Txt>
            </Pressable>
          );
        })}
      </View>

      <View style={{ flex: 1 }} />

      {/* Settings */}
      <Pressable
        onPress={() => {
          haptics.tap();
          router.push('/settings');
        }}
        style={{
          flexDirection: 'row',
          alignItems: 'center',
          gap: spacing[3],
          paddingVertical: spacing[3],
          paddingHorizontal: spacing[3],
          borderRadius: radius.lg,
        }}
      >
        <Txt style={{ fontSize: 18, color: theme.colors.textMuted, width: 24 }}>⛭</Txt>
        <Txt variant="body" tone="secondary">
          Settings
        </Txt>
      </Pressable>
    </View>
  );
}
