import React from 'react';
import { Pressable, View } from 'react-native';

import { useTheme } from '@/theme/ThemeProvider';
import { entityAccent, radius, spacing } from '@/theme/tokens';
import type { Mission } from '@/api';

import { StatusPill } from './StatusPill';
import { Txt } from './Txt';

interface Props {
  mission: Mission;
  onPress?: () => void;
}

export function MissionRow({ mission, onPress }: Props) {
  const theme = useTheme();
  const accent = mission.status === 'running' ? theme.colors.success : theme.colors.accent;

  return (
    <Pressable
      onPress={onPress}
      style={{
        backgroundColor: theme.colors.bgCard,
        borderRadius: radius.lg,
        borderWidth: 1,
        borderColor: theme.colors.border,
        padding: spacing[4],
        gap: spacing[3],
      }}
    >
      <View style={{ flexDirection: 'row', alignItems: 'flex-start', justifyContent: 'space-between', gap: spacing[3] }}>
        <View style={{ flex: 1, gap: 4 }}>
          <Txt variant="heading" numberOfLines={1} style={{ fontSize: 16 }}>
            {mission.title}
          </Txt>
          <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[2], flexWrap: 'wrap' }}>
            {mission.entityName ? (
              <View style={{ flexDirection: 'row', alignItems: 'center', gap: 4 }}>
                <View
                  style={{
                    width: 6,
                    height: 6,
                    borderRadius: 3,
                    backgroundColor: entityAccent.project,
                  }}
                />
                <Txt variant="caption" tone="secondary">
                  {mission.entityName}
                </Txt>
              </View>
            ) : null}
            {mission.agentName ? (
              <Txt variant="caption" tone="muted">
                · {mission.agentName}
              </Txt>
            ) : null}
          </View>
        </View>
        <StatusPill status={mission.status} />
      </View>

      {/* Progress bar */}
      <View
        style={{
          height: 5,
          borderRadius: 3,
          backgroundColor: theme.colors.bgSurface,
          overflow: 'hidden',
        }}
      >
        <View
          style={{
            height: '100%',
            width: `${Math.round(mission.progress * 100)}%`,
            backgroundColor: accent,
            borderRadius: 3,
          }}
        />
      </View>
    </Pressable>
  );
}
