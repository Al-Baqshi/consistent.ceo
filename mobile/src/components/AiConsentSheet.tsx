import React, { useCallback, useEffect, useRef, useState } from 'react';
import { Modal, Pressable, ScrollView, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import { isDemoGateway } from '@/api/client';
import { hasAiConsent, setAiConsent, useAiConsentStore } from '@/lib/aiConsent';
import { openExternal, PRIVACY_URL } from '@/lib/links';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

import { NativeButton } from './NativeButton';
import { Txt } from './Txt';

/** What the operator was about to do when the sheet appeared. */
export type AiConsentKind = 'chat' | 'voice' | 'settings';

interface SheetProps {
  visible: boolean;
  kind: AiConsentKind;
  onAnswer: (allowed: boolean) => void;
}

function Point({ children }: { children: React.ReactNode }) {
  return (
    <View style={{ flexDirection: 'row', gap: spacing[2] }}>
      <Txt tone="accent" style={{ lineHeight: 21 }}>
        •
      </Txt>
      <Txt variant="body" tone="secondary" style={{ flex: 1, lineHeight: 21 }}>
        {children}
      </Txt>
    </View>
  );
}

/**
 * The "where your words go" disclosure and its two answers (App Review
 * 5.1.2(i)). Plain words, no hedging: the phone talks only to the operator's
 * own computer, and that computer sends the conversation to whichever AI
 * provider they set up there.
 *
 * Rendered inside `AiConsentSheet`, or inline by a screen that is itself a
 * modal (Voice), where presenting a second modal while the first one is still
 * animating in is unreliable on iOS.
 */
export function AiConsentBody({ kind, onAnswer }: { kind: AiConsentKind; onAnswer: (allowed: boolean) => void }) {
  const [demo, setDemo] = useState(false);

  useEffect(() => {
    void isDemoGateway().then(setDemo);
  }, []);

  return (
    <View style={{ gap: spacing[3] }}>
      <Txt variant="heading" style={{ fontSize: 20 }} accessibilityRole="header">
        {kind === 'voice' ? 'Your voice goes to AI on your computer' : 'Your messages go to AI on your computer'}
      </Txt>
      <Point>
        What you type in this app, and what you say in a voice conversation, is sent to Consistent CEO on your own
        computer.
      </Point>
      <Point>
        To answer, your computer sends it, with related notes and history from your workspace, to the AI provider
        you set up there: Claude (Anthropic), OpenAI, Google Gemini, or a local model that keeps it on your computer.
      </Point>
      <Point>Voice conversations use the Google Gemini voice service set up on your computer.</Point>
      <Point>Nothing is sent to the makers of Consistent CEO.</Point>
      {demo ? (
        <Txt variant="caption" tone="muted" style={{ lineHeight: 18 }}>
          This phone isn’t paired yet, so for now replies are samples and nothing leaves the phone.
        </Txt>
      ) : null}
      <Pressable
        onPress={() => void openExternal(PRIVACY_URL)}
        accessibilityRole="link"
        hitSlop={8}
        style={{ alignSelf: 'flex-start', paddingVertical: spacing[1] }}
      >
        <Txt variant="label" tone="accent">
          Privacy policy ↗
        </Txt>
      </Pressable>
      <Txt variant="caption" tone="muted" style={{ lineHeight: 18 }}>
        You can change this any time in Settings → Privacy.
      </Txt>
      <View style={{ gap: spacing[2], paddingTop: spacing[2] }}>
        <NativeButton label="Continue" fullWidth haptic="success" onPress={() => onAnswer(true)} />
        <NativeButton label="Not now" variant="ghost" fullWidth haptic="tap" onPress={() => onAnswer(false)} />
      </View>
    </View>
  );
}

/**
 * The disclosure as a one-time bottom sheet. Shown in demo mode too — the
 * reviewer has to see it, and a demo user deserves the same disclosure before
 * they pair a real computer.
 */
export function AiConsentSheet({ visible, kind, onAnswer }: SheetProps) {
  const theme = useTheme();
  const insets = useSafeAreaInsets();

  return (
    <Modal transparent visible={visible} animationType="slide" onRequestClose={() => onAnswer(false)}>
      <View style={{ flex: 1, backgroundColor: theme.colors.bgOverlay, justifyContent: 'flex-end' }}>
        <View
          accessibilityViewIsModal
          style={{
            backgroundColor: theme.colors.bgSurface,
            borderTopLeftRadius: radius['2xl'],
            borderTopRightRadius: radius['2xl'],
            borderWidth: 1,
            borderColor: theme.colors.border,
            paddingTop: spacing[3],
            paddingBottom: insets.bottom + spacing[4],
            maxHeight: '88%',
          }}
        >
          <View style={{ alignItems: 'center', paddingBottom: spacing[2] }}>
            <View style={{ width: 38, height: 5, borderRadius: 3, backgroundColor: theme.colors.border }} />
          </View>
          <ScrollView contentContainerStyle={{ paddingHorizontal: spacing[5] }}>
            {visible ? <AiConsentBody kind={kind} onAnswer={onAnswer} /> : null}
          </ScrollView>
        </View>
      </View>
    </Modal>
  );
}

/**
 * Gate an AI-bound action on consent. `ensure(kind)` resolves true at once when
 * the operator already agreed; otherwise it opens this screen's own sheet and
 * resolves with their answer. Render `sheet` once in the screen — each screen
 * owns its sheet because a root-level RN Modal can't present over a native
 * modal screen (Voice, Settings) on iOS.
 */
export function useAiConsentGate() {
  const granted = useAiConsentStore((s) => s.granted);
  const [request, setRequest] = useState<AiConsentKind | null>(null);
  const resolver = useRef<((allowed: boolean) => void) | null>(null);

  useEffect(() => {
    void hasAiConsent();
  }, []);

  // A screen that unmounts with the sheet open must not leave a caller hanging.
  useEffect(
    () => () => {
      resolver.current?.(false);
      resolver.current = null;
    },
    [],
  );

  const ensure = useCallback(async (kind: AiConsentKind): Promise<boolean> => {
    if (await hasAiConsent()) return true;
    // A second tap while the sheet is up supersedes the first request, which
    // resolves as "not now" rather than hanging.
    resolver.current?.(false);
    return new Promise<boolean>((resolve) => {
      resolver.current = resolve;
      setRequest(kind);
    });
  }, []);

  // The buttons already give haptic feedback.
  const answer = useCallback(async (allowed: boolean) => {
    if (allowed) await setAiConsent(true);
    setRequest(null);
    const resolve = resolver.current;
    resolver.current = null;
    resolve?.(allowed);
  }, []);

  const sheet = <AiConsentSheet visible={request !== null} kind={request ?? 'chat'} onAnswer={(a) => void answer(a)} />;

  /** For an inline `AiConsentBody`: record a yes without a pending request. */
  const answerInline = useCallback(async (allowed: boolean) => {
    if (allowed) await setAiConsent(true);
  }, []);

  return { granted: granted === true, loaded: granted !== null, ensure, sheet, answerInline };
}
