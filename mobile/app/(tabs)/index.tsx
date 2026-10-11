import { useRouter } from 'expo-router';
import React, { useCallback, useEffect, useRef, useState } from 'react';
import {
  Keyboard,
  KeyboardAvoidingView,
  Platform,
  Pressable,
  ScrollView,
  TextInput,
  View,
} from 'react-native';
import Animated, { FadeIn } from 'react-native-reanimated';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import { api, CHAT_HISTORY_TURNS } from '@/api';
import type { ChatHistoryTurn, FeedMessage, QuickAction } from '@/api';
import {
  ApprovalCard,
  DemoNotice,
  MessageBubble,
  QuickActionChip,
  Screen,
  SectionHeader,
  Txt,
  useAiConsentGate,
} from '@/components';
import { EntityPickerSheet } from '@/components/companion/EntityPickerSheet';
import { TalkStrip } from '@/components/companion/TalkStrip';
import { haptics } from '@/lib/haptics';
import { getBoolPref, PREF_VOICE_MUTED, setBoolPref } from '@/lib/prefs';
import { useResponsive } from '@/lib/useResponsive';
import { useVoiceTalk } from '@/lib/useVoiceTalk';
import { useAppStore } from '@/store/appStore';
import { useTheme } from '@/theme/ThemeProvider';
import { entityColor, radius, spacing } from '@/theme/tokens';

/**
 * The conversation so far, as `CaptureChatMessage`s for the next turn: real
 * user/assistant exchanges only — no system rows, no streaming placeholder, and
 * no failure notices (those are the app talking, not the model).
 */
function historyFrom(feed: FeedMessage[]): ChatHistoryTurn[] {
  return feed
    .filter(
      (m): m is FeedMessage & { role: 'user' | 'assistant' } =>
        (m.role === 'user' || m.role === 'assistant') && !m.pending && !m.error,
    )
    .map((m) => ({ role: m.role, content: m.text.trim() }))
    .filter((t) => t.content.length > 0)
    .slice(-CHAT_HISTORY_TURNS);
}

/**
 * Chat — the one place to talk AND type to your world. The composer carries
 * an entity context ("talking about…"), a mic that opens a live conversation
 * (speech streams to the gateway, the reply streams back and is spoken
 * aloud), and the chat feed with approvals and operator events.
 *
 * Each turn sends what the desktop sends: the picked entity as real scope
 * (`entityId` + `scope`), the gateway session id from the last turn, the recent
 * history, and the local timezone offset.
 */
export default function ChatScreen() {
  const theme = useTheme();
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const { isWide } = useResponsive();

  const feed = useAppStore((s) => s.feed);
  const quickActions = useAppStore((s) => s.quickActions);
  const feedLive = useAppStore((s) => s.feedLive);
  const needsAttention = useAppStore((s) => s.needsAttention);
  const loadFeed = useAppStore((s) => s.loadFeed);
  const pushMessage = useAppStore((s) => s.pushMessage);
  const replaceMessage = useAppStore((s) => s.replaceMessage);
  const approve = useAppStore((s) => s.approve);
  const reject = useAppStore((s) => s.reject);
  const entities = useAppStore((s) => s.entities);
  const loadEntities = useAppStore((s) => s.loadEntities);
  const setChatSessionId = useAppStore((s) => s.setChatSessionId);

  const [text, setText] = useState('');
  const [sending, setSending] = useState(false);
  const [talking, setTalking] = useState(false);
  const [voiceMuted, setVoiceMuted] = useState(false);
  const [pickerOpen, setPickerOpen] = useState(false);
  const [entityId, setEntityId] = useState<string | undefined>(undefined);
  const [keyboardUp, setKeyboardUp] = useState(false);
  // Set when the operator chose "Not now" on the AI consent sheet: nothing was
  // sent, and the composer says why instead of silently swallowing the text.
  const [consentBlocked, setConsentBlocked] = useState(false);
  const consent = useAiConsentGate();
  const scrollRef = useRef<ScrollView>(null);
  const inputRef = useRef<TextInput>(null);

  const contextEntity = entityId ? entities.find((e) => e.id === entityId) : undefined;

  useEffect(() => {
    if (feed.length === 0) loadFeed();
    if (entities.length === 0) loadEntities();
  }, [feed.length, entities.length, loadFeed, loadEntities]);

  useEffect(() => {
    getBoolPref(PREF_VOICE_MUTED).then(setVoiceMuted);
  }, []);

  useEffect(() => {
    const showSub = Keyboard.addListener('keyboardWillShow', () => setKeyboardUp(true));
    const hideSub = Keyboard.addListener('keyboardWillHide', () => setKeyboardUp(false));
    return () => {
      showSub.remove();
      hideSub.remove();
    };
  }, []);

  const scrollToEnd = useCallback(() => {
    requestAnimationFrame(() => scrollRef.current?.scrollToEnd({ animated: true }));
  }, []);

  const send = useCallback(
    async (raw: string) => {
      const body = raw.trim();
      if (!body || sending) return;
      // Nothing leaves the phone for an AI until the operator has agreed to
      // where it goes (App Review 5.1.2(i)). "Not now" keeps the draft.
      if (!(await consent.ensure('chat'))) {
        setConsentBlocked(true);
        // A tapped chip or quick action has no draft of its own; keep its text
        // so a later "Continue" doesn't need it retyped.
        setText((t) => (t.trim() ? t : body));
        return;
      }
      setConsentBlocked(false);
      setSending(true);
      setText('');
      const at = new Date().toISOString();
      // Read the store directly: history and session must reflect the turn
      // that just finished, not the render this callback was created in.
      const { feed: before, chatSessionId } = useAppStore.getState();
      const history = historyFrom(before);
      pushMessage({ id: `u-${Date.now()}`, role: 'user', text: body, at, meta: contextEntity?.name });
      scrollToEnd();

      const pendingId = `p-${Date.now()}`;
      pushMessage({ id: pendingId, role: 'assistant', text: '', at, pending: true });
      try {
        const stream = api.sendMessageStream(body, {
          entityId: contextEntity?.id,
          sessionId: chatSessionId,
          history,
        });
        for await (const chunk of stream) {
          if (chunk.sessionId && chunk.sessionId !== useAppStore.getState().chatSessionId) {
            setChatSessionId(chunk.sessionId);
          }
          if (chunk.done && chunk.message) {
            replaceMessage(pendingId, chunk.message);
          } else {
            replaceMessage(pendingId, {
              id: pendingId,
              role: 'assistant',
              text: chunk.delta,
              at,
              pending: true,
            });
          }
          scrollToEnd();
        }
        haptics.tap();
      } finally {
        setSending(false);
      }
    },
    [pushMessage, replaceMessage, scrollToEnd, sending, contextEntity, setChatSessionId, consent.ensure],
  );

  // One live voice turn maps onto two feed bubbles (user transcript +
  // streaming assistant reply), created lazily as deltas arrive.
  const voiceTurn = useRef<{ userId: string | null; assistantId: string | null; assistantText: string }>({
    userId: null,
    assistantId: null,
    assistantText: '',
  });

  const showUserTranscript = useCallback(
    (transcript: string) => {
      const at = new Date().toISOString();
      const display = transcript.trim() || '…';
      const meta = contextEntity ? `Voice · ${contextEntity.name}` : 'Voice';
      if (!voiceTurn.current.userId) {
        voiceTurn.current.userId = `uv-${Date.now()}`;
        pushMessage({ id: voiceTurn.current.userId, role: 'user', text: display, at, meta });
      } else {
        replaceMessage(voiceTurn.current.userId, {
          id: voiceTurn.current.userId,
          role: 'user',
          text: display,
          at,
          meta,
        });
      }
      scrollToEnd();
    },
    [contextEntity, pushMessage, replaceMessage, scrollToEnd],
  );

  const voice = useVoiceTalk({
    systemInstruction: contextEntity
      ? `The operator is currently talking about ${contextEntity.name}.`
      : undefined,
    muted: voiceMuted,
    onUserTranscript: showUserTranscript,
    onAssistantDelta: (reply) => {
      const at = new Date().toISOString();
      // Keep bubble order sane even if the reply starts before the transcript.
      if (!voiceTurn.current.userId) showUserTranscript('');
      voiceTurn.current.assistantText = reply;
      if (!voiceTurn.current.assistantId) {
        voiceTurn.current.assistantId = `av-${Date.now()}`;
        pushMessage({ id: voiceTurn.current.assistantId, role: 'assistant', text: reply, at, pending: true });
      } else {
        replaceMessage(voiceTurn.current.assistantId, {
          id: voiceTurn.current.assistantId,
          role: 'assistant',
          text: reply,
          at,
          pending: true,
        });
      }
      scrollToEnd();
    },
    onAssistantDone: (reply) => {
      const { assistantId } = voiceTurn.current;
      if (assistantId) {
        replaceMessage(assistantId, {
          id: assistantId,
          role: 'assistant',
          text: reply || voiceTurn.current.assistantText,
          at: new Date().toISOString(),
        });
      }
      voiceTurn.current = { userId: null, assistantId: null, assistantText: '' };
      scrollToEnd();
      haptics.tap();
    },
    onError: () => haptics.warning(),
  });

  const startTalking = async () => {
    haptics.heavy();
    if (!(await consent.ensure('voice'))) {
      setConsentBlocked(true);
      return;
    }
    setConsentBlocked(false);
    setTalking(true);
    void voice.start();
  };

  const stopTalking = () => {
    // Finalize any in-flight assistant bubble before tearing down.
    const { assistantId, assistantText } = voiceTurn.current;
    if (assistantId) {
      replaceMessage(assistantId, {
        id: assistantId,
        role: 'assistant',
        text: assistantText || '…',
        at: new Date().toISOString(),
      });
    }
    voiceTurn.current = { userId: null, assistantId: null, assistantText: '' };
    voice.stop();
    setTalking(false);
  };

  const toggleVoiceMute = () => {
    haptics.tap();
    setVoiceMuted((m) => {
      void setBoolPref(PREF_VOICE_MUTED, !m);
      return !m;
    });
  };

  const handleQuickAction = (qa: QuickAction) => {
    switch (qa.intent) {
      case 'ask':
        // `payload` is the entity the chip was actually built from. Without one
        // (offline chips) ask an open question rather than naming a project
        // this workspace may not have.
        setText(qa.payload ? `How is ${qa.payload} doing?` : 'What needs me right now?');
        inputRef.current?.focus();
        break;
      case 'capture':
        setPickerOpen(true);
        inputRef.current?.focus();
        break;
      case 'approve':
        scrollRef.current?.scrollTo({ y: 0, animated: true });
        break;
      case 'mission':
        send('Start a mission');
        break;
      case 'summary':
        send('Summarize today');
        break;
      case 'entity':
        router.navigate('/brain');
        break;
      case 'auth':
        // The authenticator lists the real pending sign-ins; there is no
        // fixture session id to deep-link to any more.
        router.push('/authenticator');
        break;
    }
  };

  /** A tapped operator-feed row goes where the operator can act on it. */
  const openLink = (link: NonNullable<FeedMessage['link']>) => {
    haptics.tap();
    if (link.approvalId) router.navigate('/control');
    else if (link.taskId) router.push(`/task/${encodeURIComponent(link.taskId)}`);
    else if (link.entityId) router.push(`/entity/${encodeURIComponent(link.entityId)}`);
  };

  const greeting = (() => {
    const h = new Date().getHours();
    if (h < 12) return 'Good morning';
    if (h < 18) return 'Good afternoon';
    return 'Good evening';
  })();

  const bottomClearance = keyboardUp ? spacing[2] : isWide ? spacing[2] : insets.bottom + 70;

  return (
    <Screen title={greeting} subtitle="Here's what your world is doing">
      <KeyboardAvoidingView
        style={{ flex: 1 }}
        behavior={Platform.OS === 'ios' ? 'padding' : undefined}
        keyboardVerticalOffset={insets.top + 64}
      >
        <ScrollView
          ref={scrollRef}
          style={{ flex: 1 }}
          showsVerticalScrollIndicator={false}
          contentContainerStyle={{ paddingBottom: spacing[4] }}
          onContentSizeChange={scrollToEnd}
          keyboardDismissMode="interactive"
        >
          {needsAttention.length > 0 ? (
            <Animated.View entering={FadeIn}>
              <SectionHeader title="What needs me" caption={`${needsAttention.length} waiting on you`} />
              <View style={{ gap: spacing[3] }}>
                {needsAttention.map((a) => (
                  <ApprovalCard key={a.id} approval={a} compact onApprove={approve} onReject={reject} />
                ))}
              </View>
              <SectionHeader title="Conversation" />
            </Animated.View>
          ) : null}

          {/* The demo conversation is a scripted sample, and says so. Only in
              demo mode — a paired computer that is briefly unreachable keeps
              the real conversation and lets the banner explain. */}
          {!feedLive && feed.length > 0 ? <DemoNotice style={{ marginBottom: spacing[2] }} /> : null}

          {feed.map((m) => (
            <MessageBubble
              key={m.id}
              message={m}
              onFollowUp={send}
              onApprove={approve}
              onReject={reject}
              onOpenLink={openLink}
            />
          ))}
        </ScrollView>

        {/* Quick actions */}
        {!keyboardUp ? (
          <ScrollView
            horizontal
            showsHorizontalScrollIndicator={false}
            style={{ flexGrow: 0 }}
            contentContainerStyle={{ gap: spacing[2], paddingVertical: spacing[2] }}
          >
            {quickActions.map((qa) => (
              <QuickActionChip key={qa.id} label={qa.label} icon={qa.icon} onPress={() => handleQuickAction(qa)} />
            ))}
          </ScrollView>
        ) : (
          <View style={{ height: spacing[2] }} />
        )}

        {/* Composer / live talk */}
        <View style={{ paddingBottom: bottomClearance, gap: spacing[2] }}>
          {consentBlocked && !consent.granted ? (
            <Pressable
              onPress={() => {
                haptics.tap();
                void consent.ensure('chat').then((ok) => {
                  if (ok) setConsentBlocked(false);
                });
              }}
              accessibilityRole="button"
              style={{
                flexDirection: 'row',
                alignItems: 'center',
                gap: spacing[2],
                paddingHorizontal: spacing[3],
                paddingVertical: spacing[2],
                borderRadius: radius.lg,
                backgroundColor: theme.colors.bgCard,
                borderWidth: 1,
                borderColor: theme.colors.border,
              }}
            >
              <Txt variant="caption" tone="secondary" style={{ flex: 1, lineHeight: 17 }}>
                Not sent. Chat and voice need your OK to send your words to the AI set up on your computer.
              </Txt>
              <Txt variant="caption" tone="accent" weight="semibold">
                Review
              </Txt>
            </Pressable>
          ) : null}
          {talking ? (
            <TalkStrip
              state={voice.state}
              errorMessage={voice.errorMessage}
              muted={voiceMuted}
              onToggleMute={toggleVoiceMute}
              onDone={voice.endUtterance}
              onClose={stopTalking}
            />
          ) : (
            <View style={{ flexDirection: 'row', alignItems: 'flex-end', gap: spacing[2] }}>
              <View
                style={{
                  flex: 1,
                  backgroundColor: theme.colors.bgCard,
                  borderRadius: radius.xl,
                  borderWidth: 1,
                  borderColor: theme.colors.border,
                }}
              >
                {/* Entity context pill */}
                <Pressable
                  onPress={() => {
                    haptics.tap();
                    setPickerOpen(true);
                  }}
                  accessibilityRole="button"
                  accessibilityLabel="Choose which entity you're talking about"
                  style={{
                    flexDirection: 'row',
                    alignItems: 'center',
                    gap: 6,
                    alignSelf: 'flex-start',
                    marginLeft: spacing[3],
                    marginTop: spacing[2],
                    paddingHorizontal: spacing[2] + 2,
                    paddingVertical: 4,
                    borderRadius: radius.full,
                    backgroundColor: contextEntity
                      ? theme.colors.bgSelected
                      : theme.colors.bgCardHover,
                  }}
                >
                  <View
                    style={{
                      width: 7,
                      height: 7,
                      borderRadius: 4,
                      backgroundColor: contextEntity ? entityColor(contextEntity) : theme.colors.textMuted,
                    }}
                  />
                  <Txt variant="caption" tone={contextEntity ? 'accent' : 'muted'} weight="semibold">
                    {contextEntity ? contextEntity.name : 'Whole workspace'} ▾
                  </Txt>
                </Pressable>

                <View style={{ flexDirection: 'row', alignItems: 'center', paddingLeft: spacing[4], paddingRight: spacing[2] }}>
                  <TextInput
                    ref={inputRef}
                    value={text}
                    onChangeText={setText}
                    placeholder={contextEntity ? `Ask or capture for ${contextEntity.name}…` : 'Ask anything, or capture a thought…'}
                    placeholderTextColor={theme.colors.textMuted}
                    style={{ flex: 1, color: theme.colors.textPrimary, fontSize: 16, paddingVertical: spacing[3], maxHeight: 110 }}
                    multiline
                    submitBehavior="submit"
                    onSubmitEditing={() => send(text)}
                    returnKeyType="send"
                  />
                  <Pressable
                    onPress={() => send(text)}
                    disabled={text.trim().length === 0}
                    accessibilityLabel="Send"
                    style={{
                      width: 38,
                      height: 38,
                      borderRadius: 19,
                      backgroundColor:
                        text.trim().length > 0 ? theme.colors.accent : theme.colors.bgCardHover,
                      alignItems: 'center',
                      justifyContent: 'center',
                      marginBottom: 6,
                    }}
                  >
                    <Txt
                      style={{
                        color: text.trim().length > 0 ? theme.colors.textInverse : theme.colors.textMuted,
                        fontSize: 17,
                      }}
                      weight="bold"
                    >
                      ↑
                    </Txt>
                  </Pressable>
                </View>
              </View>

              {/* Mic — always available, next to send: type OR talk. */}
              <Pressable
                onPress={() => void startTalking()}
                accessibilityRole="button"
                accessibilityLabel="Talk by voice"
                style={{
                  width: 56,
                  height: 56,
                  borderRadius: 28,
                  alignItems: 'center',
                  justifyContent: 'center',
                  backgroundColor: theme.colors.accent,
                  shadowColor: theme.colors.accent,
                  shadowOpacity: 0.35,
                  shadowRadius: 10,
                  shadowOffset: { width: 0, height: 2 },
                  elevation: 8,
                }}
              >
                <Txt style={{ color: theme.colors.white, fontSize: 22 }} weight="bold">
                  ◉
                </Txt>
              </Pressable>
            </View>
          )}
        </View>
      </KeyboardAvoidingView>

      <EntityPickerSheet
        visible={pickerOpen}
        entities={entities}
        selectedId={entityId}
        onSelect={setEntityId}
        onClose={() => setPickerOpen(false)}
      />
      {consent.sheet}
    </Screen>
  );
}
