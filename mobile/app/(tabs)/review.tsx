import { useRouter } from 'expo-router';
import React, { useEffect, useState } from 'react';
import { View } from 'react-native';
import Animated, { FadeInDown } from 'react-native-reanimated';

import { api } from '@/api';
import type { ReviewStreak } from '@/api';
import {
  DemoNotice,
  GlassPanel,
  MissionRow,
  ReviewCard,
  Screen,
  SectionHeader,
  SwipeableRow,
  Txt,
} from '@/components';
import type { SwipeAction } from '@/components';
import { haptics } from '@/lib/haptics';
import { useAppStore } from '@/store/appStore';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

/** Open work only — what a Done or Snooze swipe can sensibly act on. */
const ACTIVE = new Set(['running', 'queued', 'blocked', 'review']);

/**
 * Today — the day's review and the work in flight. Named after the desktop's
 * "Today" page so the two surfaces use one word for one thing.
 */
export default function TodayScreen() {
  const theme = useTheme();
  const router = useRouter();
  const review = useAppStore((s) => s.review);
  const loadReview = useAppStore((s) => s.loadReview);
  const missions = useAppStore((s) => s.missions);
  const loadMissions = useAppStore((s) => s.loadMissions);
  const [refreshing, setRefreshing] = useState(false);
  const [dismissed, setDismissed] = useState<string[]>([]);
  const [actionError, setActionError] = useState<string | null>(null);

  useEffect(() => {
    if (!review) loadReview();
    if (missions.length === 0) loadMissions();
  }, [review, loadReview, missions.length, loadMissions]);

  const onRefresh = async () => {
    setRefreshing(true);
    await Promise.all([loadReview(), loadMissions()]);
    setRefreshing(false);
  };

  // Cancelled, failed and finished tasks are not "active". Treating every
  // non-done row as active put cancelled work here with a Done swipe that
  // would have completed it.
  const activeMissions = missions.filter((m) => ACTIVE.has(m.status) && !dismissed.includes(m.id));

  /**
   * Swipe actions write to the gateway. The row disappears immediately, but
   * only stays gone if the write landed — a failed call puts it back and says
   * why, rather than pretending the task was handled.
   */
  const runMissionAction = async (id: string, action: 'snooze' | 'complete') => {
    setActionError(null);
    setDismissed((d) => [...d, id]);
    const result =
      action === 'complete' ? await api.completeTask(id) : await api.snoozeTask(id, 24);
    if (result.ok) {
      haptics.success();
      await loadMissions();
      return;
    }
    setDismissed((d) => d.filter((x) => x !== id));
    setActionError(result.error ?? 'That didn’t go through.');
    haptics.error();
  };

  const missionActions = (id: string): SwipeAction[] => [
    { key: 'snooze', label: 'Snooze', icon: '☾', color: theme.colors.info, onPress: () => void runMissionAction(id, 'snooze') },
    { key: 'complete', label: 'Done', icon: '✓', color: theme.colors.success, onPress: () => void runMissionAction(id, 'complete') },
  ];

  const dateLabel = review
    ? new Date(review.date).toLocaleDateString(undefined, { weekday: 'long', month: 'long', day: 'numeric' })
    : '';

  return (
    <Screen scroll onRefresh={onRefresh} refreshing={refreshing} title="Today" subtitle={dateLabel}>
      {review ? (
        <View style={{ gap: spacing[3] }}>
          {/* Headline */}
          <GlassPanel style={{ padding: spacing[5] }}>
            <Txt variant="caption" tone="accent" weight="bold" style={{ letterSpacing: 1.5, textTransform: 'uppercase', marginBottom: spacing[2] }}>
              Today, in one line
            </Txt>
            <Txt variant="heading" style={{ lineHeight: 26 }}>
              {review.headline}
            </Txt>
          </GlassPanel>
          {!review.live ? <DemoNotice /> : null}

          {review.streak ? <StreakBanner streak={review.streak} /> : null}

          {review.sections.length > 0 ? (
            review.sections.map((section, i) => (
              <Animated.View key={section.id} entering={FadeInDown.delay(i * 40).duration(220)}>
                <ReviewCard section={section} onItemPress={(entityId) => entityId && router.push(`/entity/${entityId}`)} />
              </Animated.View>
            ))
          ) : (
            <View style={{ paddingVertical: spacing[8], alignItems: 'center', gap: spacing[2] }}>
              <Txt variant="heading" tone="accent" style={{ fontSize: 22 }}>
                ✓
              </Txt>
              <Txt tone="muted">Nothing overdue, nothing waiting on you.</Txt>
            </View>
          )}

          {/* Active missions. These moved off the Control tab when it became the
              approvals inbox: they are status, not decisions, and the day's
              review is where you already look to see what is in flight. */}
          <SectionHeader title="Active missions" caption="Swipe for done / snooze" />
          {actionError ? (
            <Txt variant="caption" style={{ color: theme.colors.danger }}>
              {actionError}
            </Txt>
          ) : null}
          {activeMissions.length > 0 ? (
            activeMissions.map((m) => (
              <SwipeableRow key={m.id} rightActions={missionActions(m.id)}>
                <MissionRow
                  mission={m}
                  onPress={() => {
                    haptics.tap();
                    // Sample missions have no task behind them to open.
                    if (review.live) router.push(`/task/${encodeURIComponent(m.id)}`);
                  }}
                />
              </SwipeableRow>
            ))
          ) : (
            <Txt tone="muted" style={{ paddingVertical: spacing[3] }}>
              Nothing running right now.
            </Txt>
          )}
        </View>
      ) : (
        <View style={{ paddingTop: spacing[16], alignItems: 'center' }}>
          <Txt tone="muted">Gathering your day…</Txt>
        </View>
      )}
    </Screen>
  );
}

/**
 * Consistency, stated plainly. Counts come straight from the gateway's
 * `streak` block, which it derives from `completed_at` on real task rows.
 */
function StreakBanner({ streak }: { streak: ReviewStreak }) {
  const theme = useTheme();
  const days = streak.currentStreakDays;
  const caption =
    days === 0
      ? streak.completedToday > 0
        ? 'Streak starts today'
        : 'Finish one thing to start a streak'
      : days === 1
        ? 'Day one — keep it'
        : `${days} days in a row`;

  return (
    <View
      style={{
        flexDirection: 'row',
        alignItems: 'center',
        gap: spacing[3],
        backgroundColor: theme.colors.bgCard,
        borderRadius: radius.xl,
        borderWidth: 1,
        borderColor: days > 0 ? theme.colors.borderFocus : theme.colors.border,
        padding: spacing[4],
      }}
    >
      <View style={{ alignItems: 'center', minWidth: 54 }}>
        <Txt variant="title" tone={days > 0 ? 'accent' : 'muted'}>
          {days}
        </Txt>
        <Txt variant="caption" tone="muted" weight="bold" style={{ letterSpacing: 1, textTransform: 'uppercase' }}>
          {days === 1 ? 'day' : 'days'}
        </Txt>
      </View>
      <View style={{ flex: 1, gap: 2 }}>
        <Txt variant="label">{caption}</Txt>
        <Txt variant="caption" tone="muted">
          {streak.completedToday} done today · {streak.completedLast7} this week · {streak.completedLast30} this month
        </Txt>
      </View>
    </View>
  );
}
