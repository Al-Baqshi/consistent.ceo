import { Tabs } from 'expo-router';
import React from 'react';
import { View } from 'react-native';

import { GatewayBanner } from '@/components/GatewayBanner';
import { BottomTabs } from '@/components/nav/BottomTabs';
import { Sidebar } from '@/components/nav/Sidebar';
import { TABS } from '@/components/nav/tabs.config';
import { useResponsive } from '@/lib/useResponsive';
import { useTheme } from '@/theme/ThemeProvider';

export default function TabsLayout() {
  const theme = useTheme();
  const { isWide } = useResponsive();

  return (
    <View style={{ flex: 1, flexDirection: 'row', backgroundColor: theme.colors.bg }}>
      {isWide ? <Sidebar /> : null}
      <View style={{ flex: 1 }}>
        <Tabs
          tabBar={isWide ? () => null : (props) => <BottomTabs {...(props as any)} />}
          screenOptions={{
            headerShown: false,
            sceneStyle: { backgroundColor: theme.colors.bg },
          }}
        >
          {/* Titles come from tabs.config.ts — the one list both the bottom bar
              and the iPad sidebar read — so a tab is never named two things.
              Route file names stay as they are; only the words changed. */}
          {TABS.map((t) => (
            <Tabs.Screen key={t.name} name={t.name} options={{ title: t.label }} />
          ))}
        </Tabs>
        {/* Floating "computer unreachable" pill — renders null while the
            configured gateway answers (and always in demo mode). */}
        <GatewayBanner />
      </View>
    </View>
  );
}
