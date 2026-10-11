import React from 'react';
import { View } from 'react-native';

import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';
import type { MissionStatus } from '@/api';

import { Txt } from './Txt';

type Tone = 'running' | 'idle' | 'warning' | 'danger' | 'done' | 'neutral';

const statusToTone: Record<MissionStatus, Tone> = {
  running: 'running',
  queued: 'idle',
  blocked: 'warning',
  review: 'warning',
  done: 'done',
  failed: 'danger',
  cancelled: 'neutral',
};

const statusLabel: Record<MissionStatus, string> = {
  running: 'Running',
  queued: 'Queued',
  blocked: 'Blocked',
  review: 'In review',
  done: 'Done',
  failed: 'Failed',
  cancelled: 'Cancelled',
};

interface Props {
  tone?: Tone;
  status?: MissionStatus;
  label?: string;
  dot?: boolean;
}

export function StatusPill({ tone, status, label, dot = true }: Props) {
  const theme = useTheme();
  const resolvedTone: Tone = tone ?? (status ? statusToTone[status] : 'neutral');
  const resolvedLabel = label ?? (status ? statusLabel[status] : 'Status');

  const color = {
    running: theme.colors.success,
    idle: theme.colors.textMuted,
    warning: theme.colors.warning,
    danger: theme.colors.danger,
    done: theme.colors.accent,
    neutral: theme.colors.textSecondary,
  }[resolvedTone];

  return (
    <View
      style={{
        flexDirection: 'row',
        alignItems: 'center',
        gap: spacing[1] + 2,
        alignSelf: 'flex-start',
        paddingHorizontal: spacing[2] + 2,
        paddingVertical: 4,
        borderRadius: radius.full,
        backgroundColor: color + '22',
        borderWidth: 1,
        borderColor: color + '44',
      }}
    >
      {dot ? (
        <View
          style={{
            width: 6,
            height: 6,
            borderRadius: 3,
            backgroundColor: color,
          }}
        />
      ) : null}
      <Txt variant="caption" style={{ color, fontSize: 11 }} weight="semibold">
        {resolvedLabel}
      </Txt>
    </View>
  );
}
