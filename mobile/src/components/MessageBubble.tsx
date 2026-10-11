import { LinearGradient } from 'expo-linear-gradient';
import React from 'react';
import { Pressable, useWindowDimensions, View } from 'react-native';
import Animated, { FadeInDown } from 'react-native-reanimated';

import { alpha } from '@/lib/color';
import { useTheme } from '@/theme/ThemeProvider';
import type { Theme } from '@/theme/tokens';
import { radius, spacing } from '@/theme/tokens';
import type { FeedMessage, FeedVariant } from '@/api';

import { ActionRequestCard } from './ActionRequestCard';
import { ApprovalCard } from './ApprovalCard';
import { QuickActionChip } from './QuickActionChip';
import { Txt } from './Txt';

interface Props {
  message: FeedMessage;
  onFollowUp?: (text: string) => void;
  onApprove?: (id: string) => void;
  onReject?: (id: string) => void;
  /** Tap on an operator-feed row that links somewhere (approval/task/entity). */
  onOpenLink?: (link: NonNullable<FeedMessage['link']>) => void;
}

/**
 * Glyph and colour for each operator-feed presentation. Things that need a
 * person (approval, input, review, overdue) are loud; reports are quiet.
 */
function systemStyle(variant: FeedVariant | undefined, theme: Theme): { glyph: string; color: string; loud: boolean } {
  const c = theme.colors;
  switch (variant) {
    case 'approval':
      return { glyph: '⚠', color: c.accent, loud: true };
    case 'input':
      return { glyph: '?', color: c.accent, loud: true };
    case 'review':
      return { glyph: '◎', color: c.warning, loud: true };
    case 'overdue':
      return { glyph: '◷', color: c.danger, loud: true };
    case 'failure':
      return { glyph: '✕', color: c.danger, loud: false };
    case 'success':
      return { glyph: '✓', color: c.success, loud: false };
    case 'digest':
      return { glyph: '✶', color: c.accent, loud: false };
    case 'heartbeat':
      return { glyph: '◈', color: c.success, loud: false };
    default:
      return { glyph: '✦', color: c.accentAlt, loud: false };
  }
}

function timeLabel(iso: string): string {
  const diff = Date.now() - new Date(iso).getTime();
  const m = Math.round(diff / 60000);
  if (m < 1) return 'now';
  if (m < 60) return `${m}m`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h}h`;
  return `${Math.round(h / 24)}d`;
}

export function MessageBubble({ message, onFollowUp, onApprove, onReject, onOpenLink }: Props) {
  const theme = useTheme();
  const { width } = useWindowDimensions();
  // Numeric widths — percentage maxWidth against an auto-width parent is
  // unreliable on the new architecture and can collapse bubbles.
  const bubbleMax = Math.min(width - 80, 560);
  const pillMax = width - 56;
  const isUser = message.role === 'user';
  const isSystem = message.role === 'system';

  if (isSystem) {
    const { glyph, color, loud } = systemStyle(message.variant, theme);
    const link = message.link;
    const tappable = !!link && !!onOpenLink;
    return (
      <Animated.View entering={FadeInDown.duration(200)} style={{ marginVertical: spacing[2] }}>
        <Pressable
          disabled={!tappable}
          onPress={() => link && onOpenLink?.(link)}
          accessibilityRole={tappable ? 'button' : undefined}
          accessibilityLabel={`${message.label ? `${message.label}: ` : ''}${message.text}`}
          style={({ pressed }) => ({
            flexDirection: 'row',
            alignItems: 'flex-start',
            gap: spacing[2],
            alignSelf: 'center',
            maxWidth: pillMax,
            paddingHorizontal: spacing[3],
            paddingVertical: spacing[2],
            borderRadius: loud ? radius.lg : radius.xl,
            backgroundColor: loud ? alpha(color, 0.1) : theme.colors.bgSurface,
            borderWidth: 1,
            borderColor: loud ? alpha(color, 0.35) : theme.colors.borderSubtle,
            opacity: pressed ? 0.7 : 1,
          })}
        >
          <Txt style={{ color, fontSize: 12, lineHeight: 17 }}>{glyph}</Txt>
          <View style={{ flexShrink: 1, gap: 1 }}>
            {message.label ? (
              <Txt variant="caption" weight="semibold" style={{ color, fontSize: 11, letterSpacing: 0.3 }}>
                {message.label}
              </Txt>
            ) : null}
            <Txt variant="caption" tone={loud ? 'secondary' : 'muted'} style={{ lineHeight: 17 }}>
              {message.text}
            </Txt>
          </View>
          {tappable ? (
            <Txt tone="muted" style={{ fontSize: 13, lineHeight: 17 }}>
              ›
            </Txt>
          ) : null}
        </Pressable>
      </Animated.View>
    );
  }

  return (
    <Animated.View
      entering={FadeInDown.duration(200)}
      style={{
        marginVertical: spacing[2],
        alignItems: isUser ? 'flex-end' : 'flex-start',
      }}
    >
      <View style={{ maxWidth: bubbleMax }}>
        {isUser ? (
          <LinearGradient
            colors={[theme.colors.userBubbleFrom, theme.colors.userBubbleTo] as const}
            start={{ x: 0, y: 0 }}
            end={{ x: 0, y: 1 }}
            style={{
              borderRadius: radius.xl,
              borderBottomRightRadius: radius.sm,
              paddingHorizontal: spacing[4],
              paddingVertical: spacing[3],
            }}
          >
            <Txt variant="body" style={{ color: theme.colors.userBubbleText, lineHeight: 22 }}>
              {message.text}
            </Txt>
          </LinearGradient>
        ) : (
          <View
            style={{
              borderRadius: radius.xl,
              borderBottomLeftRadius: radius.sm,
              paddingHorizontal: spacing[4],
              paddingVertical: spacing[3],
              backgroundColor: message.error ? alpha(theme.colors.danger, 0.08) : theme.colors.assistantBubble,
              borderWidth: 1,
              borderColor: message.error ? alpha(theme.colors.danger, 0.4) : theme.colors.assistantBorder,
            }}
          >
            <Txt variant="body" tone={message.error ? 'secondary' : 'primary'} style={{ lineHeight: 22 }}>
              {message.text}
              {message.pending ? (
                <Txt tone="accent" weight="bold">
                  {' ▊'}
                </Txt>
              ) : null}
            </Txt>
          </View>
        )}

        {/* Embedded approval */}
        {message.approval ? (
          <View style={{ marginTop: spacing[2] }}>
            <ApprovalCard approval={message.approval} onApprove={onApprove} onReject={onReject} />
          </View>
        ) : null}

        {/* Approval-gated actions queued during this turn — each card fetches
            its approval and follows the linked task while mounted. */}
        {message.actionRequests?.map((req) => (
          <View key={req.approvalId} style={{ marginTop: spacing[2] }}>
            <ActionRequestCard approvalId={req.approvalId} description={req.description} />
          </View>
        ))}

        {/* Suggested follow-ups */}
        {message.followUps && message.followUps.length > 0 && !message.pending ? (
          <View style={{ flexDirection: 'row', flexWrap: 'wrap', gap: spacing[2], marginTop: spacing[2] }}>
            {message.followUps.map((f) => (
              <QuickActionChip key={f} label={f} icon="→" onPress={() => onFollowUp?.(f)} />
            ))}
          </View>
        ) : null}

        <Txt
          variant="caption"
          tone="muted"
          style={{ marginTop: 4, alignSelf: isUser ? 'flex-end' : 'flex-start', fontSize: 10 }}
        >
          {message.meta ? `${message.meta} · ${timeLabel(message.at)}` : timeLabel(message.at)}
        </Txt>
      </View>
    </Animated.View>
  );
}
