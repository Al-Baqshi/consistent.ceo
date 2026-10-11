import { Stack } from 'expo-router';
import * as SplashScreen from 'expo-splash-screen';
import * as SystemUI from 'expo-system-ui';
import { StatusBar } from 'expo-status-bar';
import React, { useEffect } from 'react';
import { GestureHandlerRootView } from 'react-native-gesture-handler';
import { SafeAreaProvider } from 'react-native-safe-area-context';

import { startCompanion } from '@/lib/companion';
import { ThemeProvider, useTheme } from '@/theme/ThemeProvider';

SplashScreen.preventAutoHideAsync().catch(() => {});

function RootStack() {
  const theme = useTheme();

  useEffect(() => {
    SystemUI.setBackgroundColorAsync(theme.colors.bg).catch(() => {});
    SplashScreen.hideAsync().catch(() => {});
  }, [theme.colors.bg]);

  // Registers this device with the gateway and watches the operator feed for
  // anything worth interrupting for, while the app is foregrounded.
  useEffect(() => startCompanion(), []);

  return (
    <>
      <StatusBar style={theme.mode === 'dark' ? 'light' : 'dark'} />
      <Stack
        screenOptions={{
          headerShown: false,
          contentStyle: { backgroundColor: theme.colors.bg },
          animation: 'slide_from_right',
        }}
      >
        <Stack.Screen name="(tabs)" options={{ animation: 'fade' }} />
        <Stack.Screen
          name="entity/[id]"
          options={{ animation: 'slide_from_bottom', presentation: 'card' }}
        />
        <Stack.Screen
          name="task/[id]"
          options={{ animation: 'slide_from_bottom', presentation: 'card' }}
        />
        <Stack.Screen name="canvas" options={{ animation: 'fade' }} />
        <Stack.Screen name="companion" options={{ presentation: 'modal', animation: 'slide_from_bottom' }} />
        <Stack.Screen name="settings" options={{ presentation: 'modal', animation: 'slide_from_bottom' }} />
        <Stack.Screen name="pair" options={{ presentation: 'modal', animation: 'slide_from_bottom' }} />
        <Stack.Screen name="profile" options={{ presentation: 'modal', animation: 'slide_from_bottom' }} />
        <Stack.Screen name="logs" options={{ presentation: 'modal', animation: 'slide_from_bottom' }} />
        <Stack.Screen name="skills" options={{ presentation: 'modal', animation: 'slide_from_bottom' }} />
        <Stack.Screen name="auth/[session]" options={{ presentation: 'modal', animation: 'slide_from_bottom' }} />
      </Stack>
    </>
  );
}

export default function RootLayout() {
  return (
    <GestureHandlerRootView style={{ flex: 1 }}>
      <SafeAreaProvider>
        <ThemeProvider>
          <RootStack />
        </ThemeProvider>
      </SafeAreaProvider>
    </GestureHandlerRootView>
  );
}
