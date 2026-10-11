import AsyncStorage from '@react-native-async-storage/async-storage';
import React, {
  createContext,
  useContext,
  useEffect,
  useMemo,
  useState,
} from 'react';
import { useColorScheme } from 'react-native';

import { buildTheme, Theme, ThemeMode } from './tokens';

export type ThemePreference = 'system' | 'dark' | 'light';

const STORAGE_KEY = 'cw.themePreference';

interface ThemeContextValue {
  theme: Theme;
  mode: ThemeMode;
  preference: ThemePreference;
  setPreference: (pref: ThemePreference) => void;
}

const ThemeContext = createContext<ThemeContextValue | null>(null);

export function ThemeProvider({ children }: { children: React.ReactNode }) {
  const systemScheme = useColorScheme();
  const [preference, setPreferenceState] = useState<ThemePreference>('system');

  useEffect(() => {
    AsyncStorage.getItem(STORAGE_KEY).then((stored) => {
      if (stored === 'dark' || stored === 'light' || stored === 'system') {
        setPreferenceState(stored);
      }
    });
  }, []);

  const setPreference = (pref: ThemePreference) => {
    setPreferenceState(pref);
    AsyncStorage.setItem(STORAGE_KEY, pref).catch(() => {});
  };

  const mode: ThemeMode =
    preference === 'system'
      ? systemScheme === 'light'
        ? 'light'
        : 'dark'
      : preference;

  const theme = useMemo(() => buildTheme(mode), [mode]);

  const value = useMemo(
    () => ({ theme, mode, preference, setPreference }),
    [theme, mode, preference],
  );

  return <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>;
}

export function useTheme(): Theme {
  const ctx = useContext(ThemeContext);
  if (!ctx) throw new Error('useTheme must be used within ThemeProvider');
  return ctx.theme;
}

export function useThemeControls(): Omit<ThemeContextValue, 'theme'> {
  const ctx = useContext(ThemeContext);
  if (!ctx) throw new Error('useThemeControls must be used within ThemeProvider');
  const { theme, ...rest } = ctx;
  return rest;
}
