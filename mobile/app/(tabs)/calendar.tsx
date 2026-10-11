import { useRouter } from 'expo-router';
import React, { useEffect, useMemo, useState } from 'react';
import { Pressable, View } from 'react-native';
import Animated, { FadeInDown } from 'react-native-reanimated';

import { api } from '@/api';
import type { ScheduleItem } from '@/api';
import { showingSampleData } from '@/api/client';
import { DemoNotice, IconButton, Screen, SectionHeader, Txt } from '@/components';
import { alpha } from '@/lib/color';
import { haptics } from '@/lib/haptics';
import { useTheme } from '@/theme/ThemeProvider';
import type { Theme } from '@/theme/tokens';
import { radius, spacing } from '@/theme/tokens';

/**
 * Calendar — a month view of dated work: tasks with a deadline (`due_at`)
 * and cron-scheduled jobs (gateway `/api/schedule`, at their next run). Days
 * with work get dots; tap a day to see its items, tap an item to open it.
 */

const WEEKDAYS = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];

/** Kind colours from the theme, so light mode gets its own tuned values. */
function kindColor(kind: ScheduleItem['kind'], theme: Theme): string {
  if (kind === 'mission') return theme.colors.accent;
  if (kind === 'review') return theme.colors.success;
  return theme.colors.info;
}

const kindLabel: Record<ScheduleItem['kind'], string> = {
  mission: 'Due',
  scheduled: 'Scheduled',
  review: 'Review',
};

function ymd(d: Date): string {
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
}

/** Build the 6-week grid (Monday-first) for the month containing `anchor`. */
function monthGrid(anchor: Date): (Date | null)[][] {
  const first = new Date(anchor.getFullYear(), anchor.getMonth(), 1);
  const startWeekday = (first.getDay() + 6) % 7; // Monday = 0
  const daysInMonth = new Date(anchor.getFullYear(), anchor.getMonth() + 1, 0).getDate();
  const cells: (Date | null)[] = [
    ...Array.from({ length: startWeekday }, () => null),
    ...Array.from({ length: daysInMonth }, (_, i) => new Date(anchor.getFullYear(), anchor.getMonth(), i + 1)),
  ];
  while (cells.length % 7 !== 0) cells.push(null);
  const weeks: (Date | null)[][] = [];
  for (let i = 0; i < cells.length; i += 7) weeks.push(cells.slice(i, i + 7));
  return weeks;
}

export default function CalendarScreen() {
  const theme = useTheme();
  const router = useRouter();

  const [items, setItems] = useState<ScheduleItem[]>([]);
  const [anchor, setAnchor] = useState(() => new Date());
  const [selected, setSelected] = useState(() => ymd(new Date()));
  const [refreshing, setRefreshing] = useState(false);
  const [loadFailed, setLoadFailed] = useState(false);
  const [sample, setSample] = useState(false);

  const load = async () => {
    try {
      setItems(await api.listSchedule());
      setSample(await showingSampleData());
      setLoadFailed(false);
    } catch {
      // A paired computer that didn't answer: keep what is on screen and say
      // so, rather than an unhandled rejection and a silently empty month.
      setLoadFailed(true);
    }
  };

  useEffect(() => {
    load();
  }, []);

  const byDay = useMemo(() => {
    const map = new Map<string, ScheduleItem[]>();
    for (const item of items) {
      const key = ymd(new Date(item.at));
      map.set(key, [...(map.get(key) ?? []), item]);
    }
    for (const list of map.values()) {
      list.sort((a, b) => +new Date(a.at) - +new Date(b.at));
    }
    return map;
  }, [items]);

  const weeks = useMemo(() => monthGrid(anchor), [anchor]);
  const todayKey = ymd(new Date());
  const dayItems = byDay.get(selected) ?? [];

  const monthLabel = anchor.toLocaleDateString(undefined, { month: 'long', year: 'numeric' });
  const selectedLabel = new Date(`${selected}T12:00:00`).toLocaleDateString(undefined, {
    weekday: 'long',
    month: 'long',
    day: 'numeric',
  });

  const shiftMonth = (delta: number) => {
    haptics.tap();
    setAnchor((a) => new Date(a.getFullYear(), a.getMonth() + delta, 1));
  };

  const goToday = () => {
    haptics.tap();
    const now = new Date();
    setAnchor(now);
    setSelected(ymd(now));
  };

  return (
    <Screen
      scroll
      onRefresh={async () => {
        setRefreshing(true);
        await load();
        setRefreshing(false);
      }}
      refreshing={refreshing}
      title="Calendar"
      subtitle="Deadlines and scheduled work"
      headerRight={
        <Pressable
          onPress={goToday}
          style={{
            paddingHorizontal: spacing[3],
            paddingVertical: spacing[2],
            borderRadius: radius.full,
            backgroundColor: theme.colors.bgCard,
            borderWidth: 1,
            borderColor: theme.colors.border,
          }}
        >
          <Txt variant="label" tone="accent">
            Today
          </Txt>
        </Pressable>
      }
    >
      {sample && items.length > 0 ? <DemoNotice style={{ marginBottom: spacing[3] }} /> : null}

      {/* Month header */}
      <View
        style={{
          backgroundColor: theme.colors.bgCard,
          borderRadius: radius.xl,
          borderWidth: 1,
          borderColor: theme.colors.border,
          padding: spacing[3],
          gap: spacing[2],
        }}
      >
        <View style={{ flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', paddingHorizontal: spacing[1] }}>
          <IconButton icon="‹" glyphSize={22} onPress={() => shiftMonth(-1)} label="Previous month" />
          <Txt variant="heading" style={{ fontSize: 17 }}>
            {monthLabel}
          </Txt>
          <IconButton icon="›" glyphSize={22} onPress={() => shiftMonth(1)} label="Next month" />
        </View>

        {/* Weekday header */}
        <View style={{ flexDirection: 'row' }}>
          {WEEKDAYS.map((d) => (
            <View key={d} style={{ flex: 1, alignItems: 'center' }}>
              <Txt variant="caption" tone="muted" weight="semibold" style={{ fontSize: 10, letterSpacing: 0.5 }}>
                {d.toUpperCase()}
              </Txt>
            </View>
          ))}
        </View>

        {/* Day grid */}
        {weeks.map((week, wi) => (
          <View key={wi} style={{ flexDirection: 'row' }}>
            {week.map((day, di) => {
              if (!day) return <View key={di} style={{ flex: 1, height: 46 }} />;
              const key = ymd(day);
              const isSelected = key === selected;
              const isToday = key === todayKey;
              const dots = (byDay.get(key) ?? []).slice(0, 3);
              return (
                <Pressable
                  key={di}
                  onPress={() => {
                    haptics.tap();
                    setSelected(key);
                  }}
                  style={{ flex: 1, height: 46, alignItems: 'center', justifyContent: 'center' }}
                >
                  <View
                    style={{
                      width: 34,
                      height: 34,
                      borderRadius: 17,
                      alignItems: 'center',
                      justifyContent: 'center',
                      backgroundColor: isSelected ? theme.colors.accent : isToday ? theme.colors.bgSelected : 'transparent',
                      borderWidth: isToday && !isSelected ? 1 : 0,
                      borderColor: theme.colors.borderFocus,
                    }}
                  >
                    <Txt
                      variant="label"
                      style={{
                        color: isSelected
                          ? theme.colors.textInverse
                          : isToday
                            ? theme.colors.accent
                            : theme.colors.textPrimary,
                      }}
                      weight={isSelected || isToday ? 'bold' : 'medium'}
                    >
                      {day.getDate()}
                    </Txt>
                  </View>
                  <View style={{ flexDirection: 'row', gap: 3, height: 5, marginTop: 2 }}>
                    {dots.map((item, i) => (
                      <View
                        key={i}
                        style={{
                          width: 4,
                          height: 4,
                          borderRadius: 2,
                          backgroundColor: isSelected ? theme.colors.accent : kindColor(item.kind, theme),
                        }}
                      />
                    ))}
                  </View>
                </Pressable>
              );
            })}
          </View>
        ))}
      </View>

      {loadFailed ? (
        <Txt variant="caption" tone="danger" style={{ marginTop: spacing[2] }}>
          Couldn’t reach your computer — showing what was loaded last.
        </Txt>
      ) : null}

      {/* Selected day */}
      <SectionHeader title={selectedLabel} caption={dayItems.length === 0 ? 'Nothing scheduled' : `${dayItems.length} item${dayItems.length > 1 ? 's' : ''}`} />
      {dayItems.length > 0 ? (
        <View style={{ gap: spacing[2] }}>
          {dayItems.map((item, i) => {
            const time = new Date(item.at).toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' });
            const color = kindColor(item.kind, theme);
            const target = item.taskId
              ? `/task/${encodeURIComponent(item.taskId)}`
              : item.entityId
                ? `/entity/${encodeURIComponent(item.entityId)}`
                : null;
            return (
              <Animated.View key={item.id} entering={FadeInDown.delay(i * 40).duration(220)}>
                <Pressable
                  disabled={!target}
                  onPress={() => {
                    if (!target) return;
                    haptics.tap();
                    router.push(target);
                  }}
                  style={{
                    flexDirection: 'row',
                    alignItems: 'center',
                    gap: spacing[3],
                    backgroundColor: theme.colors.bgCard,
                    borderRadius: radius.lg,
                    borderWidth: 1,
                    borderColor: theme.colors.border,
                    padding: spacing[3],
                  }}
                >
                  <View style={{ alignItems: 'center', minWidth: 62 }}>
                    <Txt variant="label" weight="bold" style={{ fontVariant: ['tabular-nums'] }}>
                      {time}
                    </Txt>
                    <View
                      style={{
                        marginTop: 4,
                        paddingHorizontal: 6,
                        paddingVertical: 1,
                        borderRadius: radius.sm,
                        backgroundColor: alpha(color, 0.16),
                      }}
                    >
                      <Txt variant="caption" style={{ color, fontSize: 9 }} weight="bold">
                        {kindLabel[item.kind].toUpperCase()}
                      </Txt>
                    </View>
                  </View>
                  <View style={{ width: 1, alignSelf: 'stretch', backgroundColor: theme.colors.border }} />
                  <View style={{ flex: 1 }}>
                    <Txt variant="label" style={{ fontSize: 15 }} numberOfLines={2}>
                      {item.title}
                    </Txt>
                    <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[2], marginTop: 2, flexWrap: 'wrap' }}>
                      {item.entityName ? (
                        <View style={{ flexDirection: 'row', alignItems: 'center', gap: 4 }}>
                          <View style={{ width: 6, height: 6, borderRadius: 3, backgroundColor: theme.colors.accent }} />
                          <Txt variant="caption" tone="secondary">
                            {item.entityName}
                          </Txt>
                        </View>
                      ) : null}
                      {item.agentName ? (
                        <Txt variant="caption" tone="muted">
                          · {item.agentName}
                        </Txt>
                      ) : null}
                    </View>
                  </View>
                  {target ? <Txt tone="accent">›</Txt> : null}
                </Pressable>
              </Animated.View>
            );
          })}
        </View>
      ) : (
        <View style={{ alignItems: 'center', paddingVertical: spacing[8], gap: spacing[2] }}>
          <Txt tone="muted" style={{ fontSize: 26 }}>
            ▦
          </Txt>
          <Txt tone="muted" center>
            A clear day. Ask in Chat to schedule something for it.
          </Txt>
        </View>
      )}
    </Screen>
  );
}
