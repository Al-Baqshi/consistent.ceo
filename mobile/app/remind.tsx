import { useLocalSearchParams, useRouter } from 'expo-router';
import React, { useEffect, useRef, useState } from 'react';
import { ActivityIndicator, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import { NativeButton, Txt } from '@/components';
import { haptics } from '@/lib/haptics';
import { ensureNotificationPermission } from '@/lib/notifications';
import { addReminderText, type ReminderResult } from '@/lib/reminders';
import { friendlyTime } from '@/lib/when';
import { useTheme } from '@/theme/ThemeProvider';
import { spacing } from '@/theme/tokens';

/**
 * `consistentworld://remind?text=send%20ABT%20the%20photo%20tomorrow%209am`
 *
 * One-step capture from outside the app: an iOS Shortcut ("Hey Siri, remind
 * me…" → Dictate Text → Open URL), the share sheet, a widget, or any link.
 * The sentence is saved immediately (offline too) and the screen confirms
 * what was understood.
 */
export default function RemindLink() {
  const { text } = useLocalSearchParams<{ text?: string }>();
  const router = useRouter();
  const theme = useTheme();
  const insets = useSafeAreaInsets();
  const [result, setResult] = useState<ReminderResult | null>(null);
  const started = useRef(false);

  useEffect(() => {
    if (started.current) return;
    started.current = true;
    const sentence = Array.isArray(text) ? text.join(' ') : (text ?? '');
    void (async () => {
      await ensureNotificationPermission();
      const r = await addReminderText(sentence);
      setResult(r);
      if (r.ok) haptics.success();
      else haptics.error();
    })();
  }, [text]);

  const close = () => router.replace('/(tabs)/review');

  return (
    <View
      style={{
        flex: 1,
        backgroundColor: theme.colors.bg,
        paddingTop: insets.top + spacing[6],
        paddingHorizontal: spacing[5],
        gap: spacing[4],
      }}
    >
      {!result ? (
        <ActivityIndicator color={theme.colors.textMuted} />
      ) : result.ok && result.item ? (
        <>
          <Txt variant="title">Got it</Txt>
          <Txt variant="body">{result.item.what}</Txt>
          <Txt variant="caption" tone="muted">
            {friendlyTime(result.item.fireAt)}
            {result.queued ? ' · saved on this phone, syncs when your computer is back' : ''}
          </Txt>
          <Txt variant="caption" tone="muted">
            I’ll keep following up until you mark it done.
          </Txt>
        </>
      ) : (
        <>
          <Txt variant="title">Couldn’t save that</Txt>
          <Txt variant="body">{result.error ?? 'Something went wrong.'}</Txt>
        </>
      )}
      {result ? <NativeButton label="Done" onPress={close} /> : null}
    </View>
  );
}
