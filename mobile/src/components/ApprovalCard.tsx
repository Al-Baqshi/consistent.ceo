import React from 'react';
import { View } from 'react-native';

import { haptics } from '@/lib/haptics';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';
import type { Approval } from '@/api';

import { NativeButton } from './NativeButton';
import { StatusPill } from './StatusPill';
import { Txt } from './Txt';

interface Props {
  approval: Approval;
  onApprove?: (id: string) => void;
  onReject?: (id: string) => void;
  compact?: boolean;
}

const riskTone = {
  low: 'idle',
  medium: 'warning',
  high: 'danger',
} as const;

const riskLabel = { low: 'Low risk', medium: 'Medium risk', high: 'High risk' };

export function ApprovalCard({ approval, onApprove, onReject, compact }: Props) {
  const theme = useTheme();
  const resolved = approval.status !== 'pending';

  return (
    <View
      style={{
        borderRadius: radius.xl,
        borderWidth: 1,
        borderColor:
          approval.risk === 'high' ? theme.colors.danger + '55' : theme.colors.border,
        backgroundColor: theme.colors.bgCard,
        padding: spacing[4],
        gap: spacing[3],
        overflow: 'hidden',
      }}
    >
      {/* Accent edge for high-risk items */}
      {approval.risk === 'high' ? (
        <View
          style={{
            position: 'absolute',
            left: 0,
            top: 0,
            bottom: 0,
            width: 3,
            backgroundColor: theme.colors.danger,
          }}
        />
      ) : null}

      <View style={{ flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', gap: spacing[2] }}>
        <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[2] }}>
          <Txt tone="accent" weight="bold">
            ⚠
          </Txt>
          {/* A risk rating only when the source gave one. The gateway's
              approvals carry none, and inventing "High risk" on the trust
              screen is worse than saying nothing. */}
          {approval.risk ? (
            <StatusPill tone={riskTone[approval.risk]} label={riskLabel[approval.risk]} />
          ) : (
            <Txt variant="caption" tone="accent" weight="semibold">
              Needs your approval
            </Txt>
          )}
        </View>
        <Txt variant="caption" tone="muted" numberOfLines={1} style={{ flexShrink: 1 }}>
          {approval.requestedBy}
        </Txt>
      </View>

      <View style={{ gap: 4 }}>
        <Txt variant="heading">{approval.title}</Txt>
        {!compact && approval.detail ? (
          <Txt variant="body" tone="secondary" style={{ lineHeight: 21 }}>
            {approval.detail}
          </Txt>
        ) : null}
      </View>

      {resolved ? (
        <View
          style={{
            paddingVertical: spacing[2],
            borderRadius: radius.md,
            backgroundColor:
              approval.status === 'approved'
                ? theme.colors.success + '22'
                : theme.colors.danger + '22',
            alignItems: 'center',
          }}
        >
          <Txt
            variant="label"
            style={{
              color: approval.status === 'approved' ? theme.colors.success : theme.colors.danger,
            }}
          >
            {approval.status === 'approved' ? '✓ Approved' : '✕ Rejected'}
          </Txt>
        </View>
      ) : (
        <View style={{ flexDirection: 'row', gap: spacing[2] }}>
          <NativeButton
            label="Reject"
            variant="ghost"
            size="sm"
            style={{ flex: 1 }}
            haptic="none"
            onPress={() => {
              haptics.warning();
              onReject?.(approval.id);
            }}
          />
          <NativeButton
            label="Approve"
            variant="primary"
            size="sm"
            icon="✓"
            style={{ flex: 1.4 }}
            haptic="none"
            onPress={() => {
              haptics.success();
              onApprove?.(approval.id);
            }}
          />
        </View>
      )}
    </View>
  );
}
