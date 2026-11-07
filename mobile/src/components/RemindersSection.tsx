import * as Notifications from 'expo-notifications';
import React, { useEffect, useState } from 'react';
import { Platform, Pressable, TextInput, View } from 'react-native';

import type { ReminderView } from '@/api';
import { haptics } from '@/lib/haptics';
import { ensureNotificationPermission } from '@/lib/notifications';
import {
  actOnReminder,
  addReminder,
  delegateReminder,
  isLocalReminderId,
  type QuickReminderAction,
  syncReminders,
  useReminders,
} from '@/lib/reminders';
import { friendlyTime } from '@/lib/when';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

import { DemoNotice } from './DemoNotice';
import { SectionHeader } from './SectionHeader';
import { StatusPill } from './StatusPill';
import { SwipeableRow, type SwipeAction } from './SwipeableRow';
import { Txt } from './Txt';

/**
 * Open reminders, top of Today: what, where, when it nudges next. Swipe for
 * Done / Snooze 1h / Tomorrow / Drop. The list is the phone's own copy, so it
 * still works — and still rings — while the computer is away.
 */
export function RemindersSection() {
  const theme = useTheme();
  const items = useReminders((s) => s.items);
  const loaded = useReminders((s) => s.loaded);
  const live = useReminders((s) => s.live);
  const offline = useReminders((s) => s.offline);
  const pending = useReminders((s) => s.pending);
  const [what, setWhat] = useState('');
  const [when, setWhen] = useState('');
  const [adding, setAdding] = useState(false);
  const [note, setNote] = useState<{ text: string; error?: boolean } | null>(null);
  const [, setTick] = useState(0);
  const [canRing, setCanRing] = useState(true);

  useEffect(() => {
    void syncReminders();
    if (Platform.OS !== 'web') {
      void Notifications.getPermissionsAsync()
        .then((p) => setCanRing(p.granted))
        .catch(() => undefined);
    }
    // Keep "in 5 min" honest while the screen stays open.
    const timer = setInterval(() => setTick((t) => t + 1), 30_000);
    return () => clearInterval(timer);
  }, []);

  const open = items
    .filter((r) => r.state === 'scheduled' || r.state === 'due' || r.state === 'parked' || r.state === 'delegated')
    .sort(byNextNudge);

  const add = async () => {
    const text = what.trim();
    if (!text || adding) return;
    setAdding(true);
    setNote(null);
    // Local nudges need permission, not a push token.
    setCanRing(await ensureNotificationPermission());
    const result = await addReminder({ what: text, when: when.trim() || undefined });
    setAdding(false);
    if (!result.ok) {
      setNote({ text: result.error ?? 'That reminder didn’t save.', error: true });
      haptics.error();
      return;
    }
    setWhat('');
    setWhen('');
    setNote({
      text: result.queued
        ? `Saved on this phone — ${friendlyTime(result.item?.fireAt)}. It reaches your computer when it’s back.`
        : `Reminder set — ${friendlyTime(result.item?.fireAt)}.`,
    });
    haptics.success();
  };

  const act = async (id: string, action: QuickReminderAction) => {
    setNote(null);
    const result = await actOnReminder(id, action);
    if (result.ok) haptics.success();
    else {
      setNote({ text: result.error ?? 'That didn’t go through.', error: true });
      haptics.error();
    }
  };

  const doIt = async (id: string) => {
    setNote(null);
    const result = await delegateReminder(id);
    if (result.ok) {
      haptics.success();
      setNote({ text: 'Handed to your computer — approve it there or in Approvals.' });
    } else {
      setNote({ text: result.error ?? 'That didn’t go through.', error: true });
      haptics.error();
    }
  };

  const actions = (r: ReminderView): SwipeAction[] => [
    { key: 'drop', label: 'Drop', icon: '✕', color: theme.colors.danger, onPress: () => void act(r.id, 'drop') },
    ...(!offline && r.state !== 'delegated'
      ? [{ key: 'delegate', label: 'Do it', icon: '⚡', color: theme.colors.accentBronze, onPress: () => void doIt(r.id) }]
      : []),
    { key: 'tomorrow', label: 'Tomorrow', icon: '☀', color: theme.colors.accentBronze, onPress: () => void act(r.id, 'tomorrow') },
    { key: 'snooze', label: 'Snooze 1h', icon: '☾', color: theme.colors.info, onPress: () => void act(r.id, 'snooze') },
    { key: 'done', label: 'Done', icon: '✓', color: theme.colors.success, onPress: () => void act(r.id, 'done') },
  ];

  const caption = offline
    ? pending > 0
      ? `Computer unreachable · ${pending} change${pending === 1 ? '' : 's'} waiting to sync`
      : 'Computer unreachable · showing what this phone knows'
    : 'Swipe for done / snooze / tomorrow / drop';

  const inputStyle = {
    color: theme.colors.textPrimary,
    fontSize: 15,
    backgroundColor: theme.colors.bgSurface,
    borderRadius: radius.md,
    paddingHorizontal: spacing[3],
    paddingVertical: spacing[2] + 2,
    borderWidth: 1,
    borderColor: theme.colors.border,
  } as const;

  return (
    <View>
      <SectionHeader title="Reminders" caption={caption} style={{ marginTop: 0 }} />

      <View
        style={{
          backgroundColor: theme.colors.bgCard,
          borderRadius: radius.lg,
          borderWidth: 1,
          borderColor: theme.colors.border,
          padding: spacing[3],
          gap: spacing[2],
          marginBottom: spacing[3],
        }}
      >
        <TextInput
          value={what}
          onChangeText={setWhat}
          placeholder="Remind me to…"
          placeholderTextColor={theme.colors.textMuted}
          returnKeyType="next"
          style={inputStyle}
        />
        <View style={{ flexDirection: 'row', gap: spacing[2] }}>
          <TextInput
            value={when}
            onChangeText={setWhen}
            placeholder="When? (tomorrow 9am, in 2 hours)"
            placeholderTextColor={theme.colors.textMuted}
            returnKeyType="done"
            onSubmitEditing={() => void add()}
            style={[inputStyle, { flex: 1 }]}
          />
          <Pressable
            onPress={() => void add()}
            disabled={!what.trim() || adding}
            accessibilityRole="button"
            accessibilityLabel="Add reminder"
            style={{
              paddingHorizontal: spacing[4],
              borderRadius: radius.md,
              justifyContent: 'center',
              backgroundColor: what.trim() ? theme.colors.accent : theme.colors.bgCardHover,
            }}
          >
            <Txt
              weight="semibold"
              style={{ color: what.trim() ? theme.colors.textInverse : theme.colors.textMuted }}
            >
              {adding ? '…' : 'Add'}
            </Txt>
          </Pressable>
        </View>
        {note ? (
          <Txt variant="caption" tone={note.error ? 'danger' : 'muted'} style={{ lineHeight: 17 }}>
            {note.text}
          </Txt>
        ) : null}
      </View>

      {!canRing && open.length > 0 ? (
        <Pressable
          onPress={async () => {
            haptics.tap();
            const granted = await ensureNotificationPermission();
            setCanRing(granted);
            if (granted) void syncReminders();
          }}
          accessibilityRole="button"
          style={{ paddingBottom: spacing[3] }}
        >
          <Txt variant="caption" tone="danger" weight="semibold">
            Notifications are off, so these can’t ring. Turn them on →
          </Txt>
        </Pressable>
      ) : null}

      {loaded && !live && open.some((r) => !isLocalReminderId(r.id)) ? (
        <DemoNotice style={{ marginBottom: spacing[2] }} />
      ) : null}

      {open.length > 0 ? (
        open.map((r) => (
          <SwipeableRow key={r.id} rightActions={actions(r)} actionWidth={68}>
            <ReminderRow reminder={r} />
          </SwipeableRow>
        ))
      ) : (
        <Txt tone="muted" style={{ paddingVertical: spacing[3] }}>
          {loaded ? 'Nothing to remind you about. Add one above, or just tell Chat.' : 'Loading reminders…'}
        </Txt>
      )}
    </View>
  );
}

function byNextNudge(a: ReminderView, b: ReminderView): number {
  // Parked reminders (no next nudge) sink to the bottom.
  const ta = a.fireAt ? Date.parse(a.fireAt) : Number.POSITIVE_INFINITY;
  const tb = b.fireAt ? Date.parse(b.fireAt) : Number.POSITIVE_INFINITY;
  return ta - tb;
}

function ReminderRow({ reminder: r }: { reminder: ReminderView }) {
  const theme = useTheme();
  const local = isLocalReminderId(r.id);
  const when =
    r.state === 'parked'
      ? 'parked — no more nudges'
      : r.state === 'delegated'
        ? 'being done on your computer'
        : friendlyTime(r.fireAt);
  return (
    <View
      style={{
        flexDirection: 'row',
        alignItems: 'center',
        gap: spacing[3],
        backgroundColor: theme.colors.bgCard,
        borderRadius: radius.lg,
        borderWidth: 1,
        borderColor: r.state === 'due' ? theme.colors.borderFocus : theme.colors.border,
        paddingHorizontal: spacing[4],
        paddingVertical: spacing[3],
      }}
    >
      <View style={{ flex: 1, gap: 2 }}>
        <Txt variant="body" weight="semibold" numberOfLines={2}>
          {r.what}
        </Txt>
        <Txt variant="caption" tone="muted" numberOfLines={1}>
          {[r.entityName, when, local ? 'on this phone' : null].filter(Boolean).join(' · ')}
        </Txt>
      </View>
      {r.state === 'due' ? <StatusPill tone="warning" label="Due" /> : null}
      {r.state === 'parked' ? <StatusPill tone="neutral" label="Parked" /> : null}
      {r.state === 'delegated' ? <StatusPill tone="running" label="On your computer" /> : null}
    </View>
  );
}
