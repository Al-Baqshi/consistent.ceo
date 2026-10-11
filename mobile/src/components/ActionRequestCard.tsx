import React, { useEffect, useState } from 'react';
import { View } from 'react-native';

import { api } from '@/api';
import type { Approval } from '@/api';
import { haptics } from '@/lib/haptics';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

import { NativeButton } from './NativeButton';
import { StatusPill } from './StatusPill';
import { Txt } from './Txt';

/**
 * Inline approval card for an `action_request` the assistant queued mid-chat.
 * Slim variant of `ApprovalCard`: fetches the approval by id, offers
 * Approve / Deny, and after approval follows the linked task —
 * awaiting → queued → running → done (or blocked). Denied shows "Cancelled".
 * Polling runs only while the card is mounted and stops on terminal states.
 */

const TASK_POLL_MS = 5_000;

/** What the card shows right now, in the order the flow moves through. */
type Phase =
  | 'loading' // fetching the approval row
  | 'awaiting' // pending, buttons live
  | 'deciding' // PATCH in flight
  | 'queued' // approved; the task hasn't started yet
  | 'running'
  | 'done'
  | 'blocked' // the task failed
  | 'cancelled' // denied
  | 'missing'; // approval not found (or gateway unreachable)

const PHASE_PILL: Record<Exclude<Phase, 'loading'>, { tone: 'running' | 'idle' | 'warning' | 'danger' | 'done'; label: string }> = {
  awaiting: { tone: 'warning', label: 'Awaiting approval' },
  deciding: { tone: 'warning', label: 'Sending…' },
  queued: { tone: 'idle', label: 'Approved · queued' },
  running: { tone: 'running', label: 'Running' },
  done: { tone: 'done', label: 'Done' },
  blocked: { tone: 'danger', label: 'Blocked' },
  cancelled: { tone: 'idle', label: 'Cancelled' },
  missing: { tone: 'idle', label: 'Not found' },
};

interface Props {
  approvalId: string;
  /** The stream's description line, e.g. "Waiting for your approval: <title>". */
  description: string;
}

export function ActionRequestCard({ approvalId, description }: Props) {
  const theme = useTheme();
  const [approval, setApproval] = useState<Approval | null>(null);
  const [phase, setPhase] = useState<Phase>('loading');
  const [error, setError] = useState<string | null>(null);

  // Fetch the approval this card was created for.
  useEffect(() => {
    let alive = true;
    api
      .getApproval(approvalId)
      .then((a) => {
        if (!alive) return;
        if (!a) {
          setPhase('missing');
          return;
        }
        setApproval(a);
        setPhase(a.status === 'pending' ? 'awaiting' : a.status === 'approved' ? 'queued' : 'cancelled');
      })
      .catch(() => {
        if (alive) setPhase('missing');
      });
    return () => {
      alive = false;
    };
  }, [approvalId]);

  // After approval, follow the linked task until it lands somewhere terminal.
  const taskId = approval?.taskId;
  const polling = (phase === 'queued' || phase === 'running') && !!taskId;
  useEffect(() => {
    if (!polling || !taskId) return;
    let alive = true;
    const check = async () => {
      try {
        const task = await api.getTask(taskId);
        if (!alive || !task) return;
        if (task.status === 'running' || task.status === 'review') setPhase('running');
        else if (task.status === 'done') setPhase('done');
        else if (task.status === 'failed' || task.status === 'blocked') setPhase('blocked');
        else if (task.status === 'cancelled') setPhase('cancelled');
        // queued / backlog: stay where we are, keep polling.
      } catch {
        // Gateway unreachable — the next tick tries again.
      }
    };
    void check();
    const timer = setInterval(() => void check(), TASK_POLL_MS);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, [polling, taskId]);

  const decide = async (approve: boolean) => {
    setError(null);
    setPhase('deciding');
    try {
      const updated = approve
        ? await api.approveAction(approvalId)
        : await api.rejectAction(approvalId);
      setApproval((prev) => ({ ...(prev ?? updated), ...updated, taskId: updated.taskId ?? prev?.taskId }));
      setPhase(approve ? 'queued' : 'cancelled');
    } catch {
      setError("That didn't reach your computer — try again.");
      setPhase('awaiting');
    }
  };

  // Before the approval row arrives (or when it never does), the stream's own
  // description is the honest thing to show.
  const title = approval?.title ?? description;
  const terminal = phase === 'done' || phase === 'blocked' || phase === 'cancelled';

  return (
    <View
      style={{
        borderRadius: radius.xl,
        borderWidth: 1,
        borderColor: phase === 'awaiting' ? theme.colors.borderFocus : theme.colors.border,
        backgroundColor: theme.colors.bgCard,
        padding: spacing[4],
        gap: spacing[3],
      }}
    >
      <View style={{ flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', gap: spacing[2] }}>
        <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[2], flex: 1 }}>
          <Txt tone="accent" weight="bold">
            ⚠
          </Txt>
          {phase === 'loading' ? (
            <StatusPill tone="idle" label="Loading…" />
          ) : (
            <StatusPill tone={PHASE_PILL[phase].tone} label={PHASE_PILL[phase].label} />
          )}
        </View>
        {approval ? (
          <Txt variant="caption" tone="muted">
            {approval.requestedBy}
          </Txt>
        ) : null}
      </View>

      <View style={{ gap: 4 }}>
        <Txt variant="heading" style={{ fontSize: 16 }}>
          {title}
        </Txt>
        {approval?.detail ? (
          <Txt variant="body" tone="secondary" style={{ lineHeight: 21 }}>
            {approval.detail}
          </Txt>
        ) : null}
        {phase === 'missing' ? (
          <Txt variant="caption" tone="muted">
            This approval couldn’t be loaded — it may have been handled elsewhere. Check the
            Control tab.
          </Txt>
        ) : null}
      </View>

      {error ? (
        <Txt variant="caption" style={{ color: theme.colors.danger }}>
          {error}
        </Txt>
      ) : null}

      {phase === 'awaiting' || phase === 'deciding' ? (
        <View style={{ flexDirection: 'row', gap: spacing[2] }}>
          <NativeButton
            label="Deny"
            variant="ghost"
            size="sm"
            style={{ flex: 1 }}
            haptic="none"
            disabled={phase === 'deciding'}
            onPress={() => {
              haptics.warning();
              void decide(false);
            }}
          />
          <NativeButton
            label="Approve"
            variant="primary"
            size="sm"
            icon="✓"
            style={{ flex: 1.4 }}
            haptic="none"
            loading={phase === 'deciding'}
            onPress={() => {
              haptics.success();
              void decide(true);
            }}
          />
        </View>
      ) : null}

      {terminal ? (
        <View
          style={{
            paddingVertical: spacing[2],
            borderRadius: radius.md,
            backgroundColor:
              phase === 'done'
                ? theme.colors.success + '22'
                : phase === 'blocked'
                  ? theme.colors.danger + '22'
                  : theme.colors.bgCardHover,
            alignItems: 'center',
          }}
        >
          <Txt
            variant="label"
            style={{
              color:
                phase === 'done'
                  ? theme.colors.success
                  : phase === 'blocked'
                    ? theme.colors.danger
                    : theme.colors.textMuted,
            }}
          >
            {phase === 'done' ? '✓ Done' : phase === 'blocked' ? '✕ Blocked — the task failed' : 'Cancelled'}
          </Txt>
        </View>
      ) : null}
    </View>
  );
}
