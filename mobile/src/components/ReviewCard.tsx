import React from 'react';
import { Pressable, View } from 'react-native';

import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';
import type { ReviewSection } from '@/api';

import { Txt } from './Txt';

const kindIcon: Record<ReviewSection['kind'], string> = {
  summary: '✦',
  completed: '✓',
  questions: '?',
  missed: '⚠',
  next: '➤',
  updates: '◈',
  heartbeats: '◉',
};

interface Props {
  section: ReviewSection;
  onItemPress?: (entityId?: string) => void;
}

export function ReviewCard({ section, onItemPress }: Props) {
  const theme = useTheme();

  const accent =
    section.kind === 'missed'
      ? theme.colors.danger
      : section.kind === 'completed'
        ? theme.colors.success
        : section.kind === 'questions'
          ? theme.colors.warning
          : theme.colors.accent;

  return (
    <View
      style={{
        backgroundColor: theme.colors.bgCard,
        borderRadius: radius.xl,
        borderWidth: 1,
        borderColor: theme.colors.border,
        padding: spacing[4],
        gap: spacing[3],
      }}
    >
      <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[2] }}>
        <View
          style={{
            width: 26,
            height: 26,
            borderRadius: 8,
            backgroundColor: accent + '22',
            alignItems: 'center',
            justifyContent: 'center',
          }}
        >
          <Txt style={{ color: accent, fontSize: 13 }} weight="bold">
            {kindIcon[section.kind]}
          </Txt>
        </View>
        <Txt variant="heading" style={{ fontSize: 16 }}>
          {section.title}
        </Txt>
      </View>

      <View style={{ gap: spacing[2] }}>
        {section.items.map((item) => (
          <Pressable
            key={item.id}
            disabled={!item.entityId}
            onPress={() => onItemPress?.(item.entityId)}
            style={{
              flexDirection: 'row',
              alignItems: 'flex-start',
              gap: spacing[2],
              paddingVertical: 2,
            }}
          >
            <View
              style={{
                width: 5,
                height: 5,
                borderRadius: 3,
                backgroundColor: theme.colors.textMuted,
                marginTop: 8,
              }}
            />
            <Txt variant="body" tone="secondary" style={{ flex: 1, lineHeight: 21 }}>
              {item.text}
              {item.meta ? (
                <Txt variant="caption" tone="muted">
                  {'  · ' + item.meta}
                </Txt>
              ) : null}
            </Txt>
            {item.entityId ? (
              <Txt tone="accent" style={{ marginTop: 2 }}>
                ›
              </Txt>
            ) : null}
          </Pressable>
        ))}
      </View>
    </View>
  );
}
