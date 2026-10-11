import { useRouter } from 'expo-router';
import { StatusBar } from 'expo-status-bar';
import React, { useCallback, useEffect, useRef, useState } from 'react';
import {
  ActivityIndicator,
  KeyboardAvoidingView,
  Platform,
  Pressable,
  ScrollView,
  TextInput,
  View,
} from 'react-native';
import Animated, { FadeIn, FadeInDown } from 'react-native-reanimated';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import { AiConsentBody, NeedsComputer, Txt, useAiConsentGate } from '@/components';
import { alpha } from '@/lib/color';
import { haptics } from '@/lib/haptics';
import { useDemoMode } from '@/lib/useDemoMode';
import { VoiceSession } from '@/lib/voiceSession';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

/**
 * Voice — a live, full-duplex session with Consistent CEO. The gateway injects
 * your entity catalog server-side, so "how's the coffee business going?"
 * lands on the right entity without any picker. Interrupt any time; the
 * session notices and yields, exactly like a human conversation.
 *
 * Nothing connects until two things are true: a computer is paired (demo mode
 * shows what Voice is and how to pair instead of a connection error), and the
 * operator has agreed to AI data sharing — asked inline here, because this
 * screen is itself a modal and a second one can't reliably stack on it.
 *
 * Named "Voice" so it is not a second thing called "Companion" next to the
 * Chat tab. Every colour comes from the theme, so it follows light mode like
 * the rest of the app.
 *
 * Runs in TEXT modality today; the wire protocol already carries audio both
 * ways, so the PersonaPlex/native-audio upgrade swaps the transport, not the
 * screen.
 */

type Status = 'connecting' | 'live' | 'error' | 'closed';

interface Turn {
  id: string;
  role: 'user' | 'assistant';
  text: string;
  /** True while deltas are still streaming into this turn. */
  streaming?: boolean;
}

let turnId = 0;
const nextTurnId = () => `turn-${++turnId}`;

export default function VoiceScreen() {
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const theme = useTheme();
  const c = theme.colors;

  const [status, setStatus] = useState<Status>('connecting');
  const [errorMsg, setErrorMsg] = useState<string | null>(null);
  const [turns, setTurns] = useState<Turn[]>([]);
  const [draft, setDraft] = useState('');
  const sessionRef = useRef<VoiceSession | null>(null);
  const scrollRef = useRef<ScrollView>(null);
  const demo = useDemoMode();
  const consent = useAiConsentGate();
  const [declined, setDeclined] = useState(false);
  const ready = demo === false && consent.granted;

  const appendDelta = useCallback((delta: string) => {
    setTurns((prev) => {
      const last = prev[prev.length - 1];
      if (last && last.role === 'assistant' && last.streaming) {
        return [...prev.slice(0, -1), { ...last, text: last.text + delta }];
      }
      return [...prev, { id: nextTurnId(), role: 'assistant', text: delta, streaming: true }];
    });
  }, []);

  const finishTurn = useCallback(() => {
    setTurns((prev) => {
      const last = prev[prev.length - 1];
      if (last && last.streaming) {
        return [...prev.slice(0, -1), { ...last, streaming: false }];
      }
      return prev;
    });
  }, []);

  const connect = useCallback(async () => {
    setStatus('connecting');
    setErrorMsg(null);
    try {
      const session = await VoiceSession.connect({
        onEvent: (e) => {
          switch (e.type) {
            case 'setupComplete':
              setStatus('live');
              break;
            case 'text':
              appendDelta(e.delta);
              break;
            case 'turnComplete':
              finishTurn();
              haptics.tap();
              break;
            case 'interrupted':
              finishTurn();
              break;
            case 'error':
              setErrorMsg(e.message);
              setStatus('error');
              break;
          }
        },
        onClose: () => setStatus((s) => (s === 'error' ? s : 'closed')),
      });
      sessionRef.current = session;
      // The upstream connects lazily on the first frame; consider us live
      // once the socket is open — setupComplete confirms it.
      setStatus('live');
    } catch {
      setErrorMsg('Couldn’t start a voice session with your computer');
      setStatus('error');
    }
  }, [appendDelta, finishTurn]);

  useEffect(() => {
    if (!ready) return;
    connect();
    return () => sessionRef.current?.close();
  }, [connect, ready]);

  useEffect(() => {
    scrollRef.current?.scrollToEnd({ animated: true });
  }, [turns]);

  const send = useCallback(() => {
    const text = draft.trim();
    const session = sessionRef.current;
    if (!text || !session?.open) return;
    haptics.press();
    setTurns((prev) => [...prev, { id: nextTurnId(), role: 'user', text }]);
    session.sendText(text);
    setDraft('');
  }, [draft]);

  const statusLabel = demo
    ? 'Not paired with a computer'
    : !ready
      ? 'Waiting for your OK'
      : status === 'live'
      ? 'Live · entity-aware'
      : status === 'connecting'
        ? 'Connecting…'
        : status === 'closed'
          ? 'Session ended'
          : (errorMsg ?? 'Error');
  const statusColor = !ready
    ? c.textMuted
    : status === 'live'
      ? c.success
      : status === 'connecting'
        ? c.warning
        : c.danger;

  return (
    <View style={{ flex: 1, backgroundColor: c.bg }}>
      <StatusBar style={theme.mode === 'dark' ? 'light' : 'dark'} />

      {/* Header */}
      <View
        style={{
          paddingTop: insets.top + spacing[2],
          paddingHorizontal: spacing[4],
          paddingBottom: spacing[3],
          flexDirection: 'row',
          alignItems: 'center',
          gap: spacing[3],
        }}
      >
        <Pressable
          onPress={() => router.back()}
          hitSlop={8}
          accessibilityLabel="Back"
          style={{
            width: 38,
            height: 38,
            borderRadius: 19,
            alignItems: 'center',
            justifyContent: 'center',
            backgroundColor: c.bgCard,
            borderWidth: 1,
            borderColor: c.border,
          }}
        >
          <Txt tone="primary" style={{ fontSize: 16 }}>←</Txt>
        </Pressable>
        <View style={{ flex: 1 }}>
          <Txt variant="heading">Voice</Txt>
          <View style={{ flexDirection: 'row', alignItems: 'center', gap: 6 }}>
            <View style={{ width: 7, height: 7, borderRadius: 4, backgroundColor: statusColor }} />
            <Txt variant="caption" tone="secondary" numberOfLines={1}>
              {statusLabel}
            </Txt>
          </View>
        </View>
        {ready && (status === 'error' || status === 'closed') ? (
          <Pressable
            onPress={connect}
            style={{
              paddingHorizontal: spacing[3],
              height: 38,
              borderRadius: radius.full,
              alignItems: 'center',
              justifyContent: 'center',
              backgroundColor: c.bgSelected,
              borderWidth: 1,
              borderColor: c.borderFocus,
            }}
          >
            <Txt variant="label" tone="accent">
              Reconnect
            </Txt>
          </Pressable>
        ) : null}
      </View>

      <KeyboardAvoidingView
        style={{ flex: 1 }}
        behavior={Platform.OS === 'ios' ? 'padding' : undefined}
        keyboardVerticalOffset={0}
      >
        {/* Transcript */}
        <ScrollView
          ref={scrollRef}
          style={{ flex: 1 }}
          contentContainerStyle={{ padding: spacing[4], gap: spacing[3], paddingBottom: spacing[6] }}
        >
          {demo ? (
            <NeedsComputer
              icon="◉"
              title="Talk to your world"
              body="Voice conversations run through Consistent CEO on your computer, which knows your businesses and projects, so you can just name them. Pair this phone to talk."
              style={{ marginTop: spacing[4] }}
            />
          ) : demo === false && consent.loaded && !consent.granted ? (
            declined ? (
              <View style={{ alignItems: 'center', paddingTop: spacing[8], gap: spacing[3] }}>
                <Txt tone="secondary" center style={{ lineHeight: 21 }}>
                  Voice needs your OK to send what you say to the AI set up on your computer.
                </Txt>
                <Pressable onPress={() => setDeclined(false)} hitSlop={8}>
                  <Txt variant="label" tone="accent">
                    Review
                  </Txt>
                </Pressable>
              </View>
            ) : (
              <AiConsentBody
                kind="voice"
                onAnswer={(allowed) => {
                  if (allowed) void consent.answerInline(true);
                  else setDeclined(true);
                }}
              />
            )
          ) : turns.length === 0 ? (
            <Animated.View entering={FadeIn} style={{ alignItems: 'center', paddingTop: spacing[8], gap: spacing[2] }}>
              {ready && status === 'connecting' ? <ActivityIndicator color={c.accent} /> : null}
              <Txt tone="secondary" style={{ textAlign: 'center', lineHeight: 20 }}>
                Talk to Consistent CEO about anything.{'\n'}It knows your entities — just name them.
              </Txt>
            </Animated.View>
          ) : null}
          {turns.map((t) => (
            <Animated.View
              key={t.id}
              entering={FadeInDown.duration(200)}
              style={{
                alignSelf: t.role === 'user' ? 'flex-end' : 'flex-start',
                maxWidth: '86%',
                borderRadius: radius.xl,
                backgroundColor: t.role === 'user' ? alpha(c.accent, 0.18) : c.assistantBubble,
                borderWidth: 1,
                borderColor: t.role === 'user' ? alpha(c.accent, 0.4) : c.assistantBorder,
                paddingHorizontal: spacing[4],
                paddingVertical: spacing[3],
              }}
            >
              <Txt tone="primary" style={{ fontSize: 15, lineHeight: 21 }}>
                {t.text}
                {t.streaming ? ' ▍' : ''}
              </Txt>
            </Animated.View>
          ))}
        </ScrollView>

        {/* Composer */}
        <View
          style={{
            flexDirection: 'row',
            alignItems: 'center',
            gap: spacing[2],
            paddingHorizontal: spacing[4],
            paddingBottom: insets.bottom + spacing[3],
            paddingTop: spacing[2],
          }}
        >
          <View
            style={{
              flex: 1,
              flexDirection: 'row',
              alignItems: 'center',
              backgroundColor: c.bgCard,
              borderRadius: radius.full,
              borderWidth: 1,
              borderColor: c.border,
              paddingHorizontal: spacing[4],
              height: 48,
            }}
          >
            <TextInput
              value={draft}
              onChangeText={setDraft}
              onSubmitEditing={send}
              placeholder={!ready ? 'Voice isn’t connected' : status === 'live' ? 'Say something…' : 'Connecting…'}
              placeholderTextColor={c.textMuted}
              editable={ready && status === 'live'}
              returnKeyType="send"
              style={{ flex: 1, color: c.textPrimary, fontSize: 15 }}
            />
          </View>
          <Pressable
            onPress={send}
            disabled={!ready || status !== 'live' || draft.trim() === ''}
            accessibilityRole="button"
            accessibilityLabel="Send"
            style={{
              width: 48,
              height: 48,
              borderRadius: 24,
              alignItems: 'center',
              justifyContent: 'center',
              backgroundColor: status === 'live' && draft.trim() ? c.accent : c.bgCardHover,
            }}
          >
            <Txt
              style={{ color: status === 'live' && draft.trim() ? c.textInverse : c.textMuted, fontSize: 17 }}
              weight="bold"
            >
              ↑
            </Txt>
          </Pressable>
        </View>
      </KeyboardAvoidingView>
    </View>
  );
}
