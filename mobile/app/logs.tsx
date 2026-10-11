import { useRouter } from 'expo-router';
import React, { useEffect, useState } from 'react';
import { Platform, Pressable, ScrollView, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import { IconButton, Txt } from '@/components';
import { haptics } from '@/lib/haptics';
import { clearLogs, getLogs, subscribeLogs, type LogEntry, type LogTag } from '@/lib/logger';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

/** Logs — inspect what the app, calls to your computer, and workers are doing. */

const FILTERS: { key: LogTag | 'all' | 'error'; label: string }[] = [
  { key: 'all', label: 'All' },
  { key: 'api', label: 'Computer' },
  { key: 'system', label: 'System' },
  { key: 'worker', label: 'Workers' },
  { key: 'error', label: 'Errors' },
];

export default function LogsScreen() {
  const theme = useTheme();
  const router = useRouter();
  const insets = useSafeAreaInsets();

  const [entries, setEntries] = useState<LogEntry[]>(getLogs());
  const [filter, setFilter] = useState<LogTag | 'all' | 'error'>('all');

  useEffect(() => subscribeLogs(() => setEntries([...getLogs()])), []);

  const filtered = entries.filter((e) => {
    if (filter === 'all') return true;
    if (filter === 'error') return e.level === 'error' || e.level === 'warn';
    return e.tag === filter;
  });

  const levelColor = {
    info: theme.colors.textMuted,
    warn: theme.colors.warning,
    error: theme.colors.danger,
  } as const;

  const mono = Platform.select({ ios: 'Menlo', android: 'monospace', default: 'monospace' });

  return (
    <View style={{ flex: 1, backgroundColor: theme.colors.bg, paddingTop: insets.top }}>
      <View style={{ flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', paddingHorizontal: spacing[5], paddingVertical: spacing[3] }}>
        <View>
          <Txt variant="title">Logs</Txt>
          <Txt variant="caption" tone="muted">
            {filtered.length} entries · in-memory only
          </Txt>
        </View>
        <View style={{ flexDirection: 'row', gap: spacing[2] }}>
          <IconButton
            icon="⌫"
            filled
            onPress={() => {
              haptics.warning();
              clearLogs();
            }}
            label="Clear logs"
          />
          <IconButton icon="✕" filled onPress={() => router.back()} label="Close" />
        </View>
      </View>

      {/* Filters */}
      <ScrollView
        horizontal
        showsHorizontalScrollIndicator={false}
        style={{ flexGrow: 0 }}
        contentContainerStyle={{ gap: spacing[2], paddingHorizontal: spacing[5], paddingBottom: spacing[3] }}
      >
        {FILTERS.map((f) => {
          const active = filter === f.key;
          return (
            <Pressable
              key={f.key}
              onPress={() => {
                haptics.tap();
                setFilter(f.key);
              }}
              style={{
                paddingHorizontal: spacing[3],
                paddingVertical: spacing[2],
                borderRadius: radius.full,
                backgroundColor: active ? theme.colors.bgSelected : theme.colors.bgCard,
                borderWidth: 1,
                borderColor: active ? theme.colors.borderFocus : theme.colors.border,
              }}
            >
              <Txt variant="label" tone={active ? 'accent' : 'secondary'}>
                {f.label}
              </Txt>
            </Pressable>
          );
        })}
      </ScrollView>

      <ScrollView
        style={{ flex: 1 }}
        contentContainerStyle={{ paddingHorizontal: spacing[4], paddingBottom: insets.bottom + spacing[8], gap: 2 }}
        showsVerticalScrollIndicator={false}
      >
        {filtered.length === 0 ? (
          <View style={{ alignItems: 'center', paddingVertical: spacing[10] }}>
            <Txt tone="muted">Nothing here yet.</Txt>
          </View>
        ) : (
          filtered.map((e) => (
            <View
              key={e.id}
              style={{
                flexDirection: 'row',
                gap: spacing[2],
                paddingVertical: spacing[2],
                paddingHorizontal: spacing[2],
                borderRadius: radius.sm,
                backgroundColor: e.level === 'error' ? theme.colors.danger + '14' : 'transparent',
              }}
            >
              <Txt style={{ fontFamily: mono, fontSize: 11, color: theme.colors.textMuted, minWidth: 56 }}>
                {new Date(e.at).toLocaleTimeString(undefined, { hour12: false })}
              </Txt>
              <Txt style={{ fontFamily: mono, fontSize: 11, color: levelColor[e.level], minWidth: 52 }} weight="bold">
                {e.tag.toUpperCase()}
              </Txt>
              <Txt style={{ fontFamily: mono, fontSize: 11, color: theme.colors.textSecondary, flex: 1, lineHeight: 16 }}>
                {e.text}
              </Txt>
            </View>
          ))
        )}
      </ScrollView>
    </View>
  );
}
