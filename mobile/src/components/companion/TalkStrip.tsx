import React, { useEffect, useState } from 'react';
import { Pressable, View } from 'react-native';
import Animated, {
  cancelAnimation,
  Easing,
  FadeInDown,
  useAnimatedStyle,
  useSharedValue,
  withRepeat,
  withTiming,
  type SharedValue,
} from 'react-native-reanimated';

import type { VoiceTalkState } from '@/lib/useVoiceTalk';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

import { Txt } from '../Txt';

interface Props {
  state: VoiceTalkState;
  errorMessage: string | null;
  muted: boolean;
  onToggleMute: () => void;
  /** ✓ — the user finished this utterance; commit it for a reply. */
  onDone: () => void;
  /** ✕ — end the whole conversation and close the strip. */
  onClose: () => void;
}

const STATE_LABEL: Record<VoiceTalkState, string> = {
  idle: 'Starting…',
  connecting: 'Connecting…',
  listening: 'Listening…',
  thinking: 'Thinking…',
  speaking: 'Speaking…',
  error: 'Voice error',
};

/**
 * Replaces the composer while talking to the companion. Live conversation,
 * not a recorder: audio streams to the gateway while you speak, the reply
 * streams back and is read aloud.
 */
export function TalkStrip({ state, errorMessage, muted, onToggleMute, onDone, onClose }: Props) {
  const theme = useTheme();
  const [secs, setSecs] = useState(0);

  useEffect(() => {
    if (state !== 'listening') return;
    setSecs(0);
    const id = setInterval(() => setSecs((s) => s + 1), 1000);
    return () => clearInterval(id);
  }, [state]);

  const mmss = `${Math.floor(secs / 60)}:${String(secs % 60).padStart(2, '0')}`;
  const listening = state === 'listening';

  return (
    <Animated.View entering={FadeInDown.duration(200)} style={{ gap: spacing[1] }}>
      <View
        style={{
          flexDirection: 'row',
          alignItems: 'center',
          gap: spacing[3],
          backgroundColor: theme.colors.bgCard,
          borderRadius: radius.xl,
          borderWidth: 1,
          borderColor: state === 'error' ? theme.colors.danger : theme.colors.borderFocus,
          paddingHorizontal: spacing[3],
          height: 56,
        }}
      >
        <Pressable
          onPress={onClose}
          hitSlop={8}
          accessibilityLabel="End the conversation"
          style={{ width: 36, height: 36, borderRadius: 18, alignItems: 'center', justifyContent: 'center', backgroundColor: theme.colors.bgCardHover }}
        >
          <Txt style={{ color: theme.colors.textSecondary, fontSize: 15 }}>✕</Txt>
        </Pressable>

        <View
          style={{
            width: 8,
            height: 8,
            borderRadius: 4,
            backgroundColor:
              state === 'error'
                ? theme.colors.danger
                : listening
                  ? theme.colors.success
                  : theme.colors.accent,
          }}
        />
        <Txt variant="label" style={{ minWidth: 76 }} tone={state === 'error' ? 'danger' : 'primary'}>
          {STATE_LABEL[state]}
        </Txt>
        {listening ? (
          <Txt variant="caption" tone="muted" style={{ fontVariant: ['tabular-nums'] }}>
            {mmss}
          </Txt>
        ) : null}

        <View style={{ flex: 1 }}>
          {listening || state === 'speaking' ? <WaveBars color={theme.colors.accent} /> : null}
        </View>

        <Pressable
          onPress={onToggleMute}
          hitSlop={8}
          accessibilityLabel={muted ? 'Unmute spoken replies' : 'Mute spoken replies'}
          style={{ width: 36, height: 36, borderRadius: 18, alignItems: 'center', justifyContent: 'center', backgroundColor: theme.colors.bgCardHover }}
        >
          <Txt style={{ color: muted ? theme.colors.textMuted : theme.colors.accent, fontSize: 15 }}>
            {muted ? '🔇' : '🔊'}
          </Txt>
        </Pressable>

        <Pressable
          onPress={onDone}
          disabled={!listening}
          hitSlop={8}
          accessibilityLabel="Done speaking — get a reply"
          style={{
            width: 40,
            height: 40,
            borderRadius: 20,
            alignItems: 'center',
            justifyContent: 'center',
            backgroundColor: listening ? theme.colors.accent : theme.colors.bgCardHover,
          }}
        >
          <Txt
            style={{ color: listening ? theme.colors.textInverse : theme.colors.textMuted, fontSize: 17 }}
            weight="bold"
          >
            ✓
          </Txt>
        </Pressable>
      </View>

      {errorMessage ? (
        <Txt variant="caption" tone="danger" style={{ paddingHorizontal: spacing[2] }}>
          {errorMessage}
        </Txt>
      ) : null}
    </Animated.View>
  );
}

/** Animated waveform bars. */
function WaveBars({ color }: { color: string }) {
  const t = useSharedValue(0);

  useEffect(() => {
    t.value = withRepeat(withTiming(1, { duration: 900, easing: Easing.linear }), -1, false);
    return () => cancelAnimation(t);
  }, [t]);

  return (
    <View style={{ flexDirection: 'row', alignItems: 'center', gap: 3, height: 30, overflow: 'hidden' }}>
      {Array.from({ length: 20 }, (_, i) => (
        <Bar key={i} t={t} phase={i * 0.55} color={color} />
      ))}
    </View>
  );
}

function Bar({ t, phase, color }: { t: SharedValue<number>; phase: number; color: string }) {
  const style = useAnimatedStyle(() => {
    const h = 4 + 22 * Math.abs(Math.sin(t.value * Math.PI * 2 + phase));
    return { height: h };
  });
  return <Animated.View style={[{ width: 3, borderRadius: 2, backgroundColor: color }, style]} />;
}
