import { useLocalSearchParams, useRouter } from 'expo-router';
import React, { useEffect, useState } from 'react';
import {
  ActivityIndicator,
  Alert,
  KeyboardAvoidingView,
  Modal,
  Platform,
  Pressable,
  ScrollView,
  TextInput,
  View,
} from 'react-native';
import Animated, { FadeInDown } from 'react-native-reanimated';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import { api, chatFailureText } from '@/api';
import type { EntityDetail } from '@/api';
import {
  DemoNotice,
  EntityNode,
  IconButton,
  MissionRow,
  NativeButton,
  StatusPill,
  Txt,
  useAiConsentGate,
} from '@/components';
import { haptics } from '@/lib/haptics';
import { useTheme } from '@/theme/ThemeProvider';
import { entityColor, radius, spacing } from '@/theme/tokens';

type TabKey = 'overview' | 'files' | 'notes' | 'missions' | 'memory' | 'related' | 'activity';
const TABS: { key: TabKey; label: string }[] = [
  { key: 'overview', label: 'Overview' },
  { key: 'files', label: 'Files' },
  { key: 'notes', label: 'Notes' },
  { key: 'missions', label: 'Missions' },
  { key: 'memory', label: 'Memory' },
  { key: 'related', label: 'Related' },
  { key: 'activity', label: 'Activity' },
];

function timeAgo(iso: string): string {
  const m = Math.round((Date.now() - new Date(iso).getTime()) / 60000);
  if (m < 60) return `${m}m ago`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.round(h / 24)}d ago`;
}

export default function EntityScreen() {
  const theme = useTheme();
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const { id } = useLocalSearchParams<{ id: string }>();

  const [detail, setDetail] = useState<EntityDetail | null>(null);
  const [missing, setMissing] = useState(false);
  const [tab, setTab] = useState<TabKey>('overview');
  const [qa, setQa] = useState<{ q: string; a: string; loading?: boolean; error?: boolean }[]>([]);
  const [asking, setAsking] = useState(false);
  // One gateway session per entity screen, so follow-up questions here keep
  // their context the way the Chat tab does.
  const [askSession, setAskSession] = useState<string | undefined>(undefined);
  const [ask, setAsk] = useState('');
  // "Not now" on the AI consent sheet: the question stays in the composer.
  const [consentBlocked, setConsentBlocked] = useState(false);
  const consent = useAiConsentGate();
  const [addNoteOpen, setAddNoteOpen] = useState(false);

  const [unreachable, setUnreachable] = useState(false);

  const load = React.useCallback(() => {
    if (!id) return;
    setMissing(false);
    setUnreachable(false);
    api
      .getEntity(id)
      .then((d) => {
        setDetail(d);
        // `null` means the gateway answered 404 — the entity is genuinely not
        // there. Say that instead of spinning forever or showing a stand-in.
        setMissing(d === null);
      })
      .catch(() => setUnreachable(true));
  }, [id]);

  useEffect(() => {
    load();
  }, [load]);

  /**
   * Ask about this entity — a real entity-scoped chat turn (`entityId` +
   * `scope: entity`), streamed. It used to prefix "About X:" onto a workspace
   * turn over a 2.5s POST: real answers timed out, the rejection went
   * unhandled and the spinner never stopped.
   */
  const onAsk = async (raw?: string) => {
    const q = (raw ?? ask).trim();
    if (!q || !detail || asking) return;
    // Nothing goes to an AI before the operator agreed (App Review 5.1.2(i)).
    if (!(await consent.ensure('chat'))) {
      setConsentBlocked(true);
      return;
    }
    setConsentBlocked(false);
    setAsk('');
    setAsking(true);
    haptics.tap();
    const idx = qa.length;
    const history = qa
      .filter((item) => !item.loading && !item.error && item.a)
      .flatMap((item) => [
        { role: 'user' as const, content: item.q },
        { role: 'assistant' as const, content: item.a },
      ]);
    setQa((prev) => [...prev, { q, a: '', loading: true }]);
    const update = (patch: { a: string; loading?: boolean; error?: boolean }) =>
      setQa((prev) => prev.map((item, i) => (i === idx ? { q, ...patch } : item)));
    try {
      for await (const chunk of api.sendMessageStream(q, {
        entityId: detail.id,
        sessionId: askSession,
        history,
      })) {
        if (chunk.sessionId) setAskSession(chunk.sessionId);
        if (chunk.done) {
          update({ a: chunk.message?.text ?? chunk.delta, error: chunk.message?.error });
        } else {
          update({ a: chunk.delta, loading: true });
        }
      }
    } catch (e) {
      update({ a: chatFailureText(e), error: true });
      haptics.error();
    } finally {
      setAsking(false);
    }
  };

  if (!detail) {
    return (
      <View style={{ flex: 1, backgroundColor: theme.colors.bg, alignItems: 'center', justifyContent: 'center', gap: spacing[4], paddingHorizontal: spacing[6] }}>
        {unreachable ? (
          <>
            <Txt style={{ fontSize: 34 }} tone="muted">
              ⌁
            </Txt>
            <Txt variant="heading" center>
              Can’t reach your computer
            </Txt>
            <Txt variant="body" tone="secondary" center>
              Consistent CEO on your computer didn’t answer. Check that it’s awake and online,
              then try again.
            </Txt>
            <View style={{ flexDirection: 'row', gap: spacing[2] }}>
              <NativeButton label="Go back" variant="ghost" onPress={() => router.back()} />
              <NativeButton label="Try again" variant="outline" onPress={load} />
            </View>
          </>
        ) : missing ? (
          <>
            <Txt style={{ fontSize: 34 }} tone="muted">
              ⬡
            </Txt>
            <Txt variant="heading" center>
              No such entity
            </Txt>
            <Txt variant="body" tone="secondary" center>
              Nothing exists under “{id}” in this workspace. It may have been
              deleted, or it belongs to another workspace.
            </Txt>
            <NativeButton label="Go back" variant="outline" onPress={() => router.back()} />
          </>
        ) : (
          <ActivityIndicator color={theme.colors.accent} />
        )}
      </View>
    );
  }

  const accent = entityColor(detail);

  return (
    <View style={{ flex: 1, backgroundColor: theme.colors.bg }}>
      {/* Hero */}
      <View style={{ paddingTop: insets.top + spacing[2] }}>
        {/* ambient glow */}
        <View
          pointerEvents="none"
          style={{ position: 'absolute', top: -80, left: -40, right: -40, height: 260, backgroundColor: accent, opacity: 0.1, borderRadius: 300 }}
        />
        <View style={{ flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', paddingHorizontal: spacing[4] }}>
          <IconButton icon="‹" glyphSize={26} filled onPress={() => router.back()} label="Back" />
          <IconButton icon="⋯" filled onPress={haptics.tap} label="More" />
        </View>

        <View style={{ alignItems: 'center', paddingHorizontal: spacing[5], paddingTop: spacing[3], gap: spacing[2] }}>
          <EntityNode type={detail.type} color={accent} size={92} />
          <Txt variant="title" center style={{ marginTop: spacing[2] }}>
            {detail.name}
          </Txt>
          <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[2] }}>
            <StatusPill
              tone="neutral"
              label={detail.personal ? 'Personal' : detail.type[0].toUpperCase() + detail.type.slice(1)}
              dot={false}
            />
            {detail.stage ? <StatusPill tone="running" label={detail.stage} /> : null}
          </View>
          {detail.summary ? (
            <Txt variant="body" tone="secondary" center style={{ lineHeight: 22, marginTop: 2 }}>
              {detail.summary}
            </Txt>
          ) : null}
          {/* Sample data is labelled, never passed off as this workspace's. */}
          {!detail.live ? <DemoNotice style={{ marginTop: spacing[2], alignSelf: 'stretch' }} /> : null}
        </View>

        {/* Stat row */}
        <View style={{ flexDirection: 'row', justifyContent: 'center', gap: spacing[6], paddingVertical: spacing[4] }}>
          <Stat label="Files" value={detail.stats.files} />
          <Stat label="Notes" value={detail.stats.notes} />
          <Stat label="Missions" value={detail.stats.missions} />
          <Stat label="Memories" value={detail.stats.memories} />
        </View>

        {/* Tabs */}
        <ScrollView horizontal showsHorizontalScrollIndicator={false} contentContainerStyle={{ gap: spacing[2], paddingHorizontal: spacing[5], paddingBottom: spacing[3] }}>
          {TABS.map((t) => {
            const active = tab === t.key;
            return (
              <Pressable
                key={t.key}
                onPress={() => {
                  haptics.tap();
                  setTab(t.key);
                }}
                style={{
                  paddingHorizontal: spacing[3],
                  paddingVertical: spacing[2],
                  borderRadius: radius.full,
                  backgroundColor: active ? theme.colors.bgSelected : 'transparent',
                  borderWidth: 1,
                  borderColor: active ? theme.colors.borderFocus : theme.colors.border,
                }}
              >
                <Txt variant="label" tone={active ? 'accent' : 'secondary'}>
                  {t.label}
                </Txt>
              </Pressable>
            );
          })}
        </ScrollView>
      </View>

      <KeyboardAvoidingView style={{ flex: 1 }} behavior={Platform.OS === 'ios' ? 'padding' : undefined} keyboardVerticalOffset={insets.top}>
        <ScrollView style={{ flex: 1 }} contentContainerStyle={{ padding: spacing[5], paddingBottom: spacing[12], gap: spacing[3] }} showsVerticalScrollIndicator={false}>
          {tab === 'overview' ? <Overview detail={detail} onSummarize={() => onAsk('Give me a short summary and what needs attention.')} /> : null}
          {tab === 'files' ? <FilesTab detail={detail} /> : null}
          {tab === 'notes' ? <NotesTab detail={detail} onAdd={() => setAddNoteOpen(true)} onOpenEntity={(eid) => router.push(`/entity/${eid}`)} /> : null}
          {tab === 'missions' ? <MissionsTab detail={detail} /> : null}
          {tab === 'memory' ? <MemoryTab detail={detail} /> : null}
          {tab === 'related' ? <RelatedTab detail={detail} onOpen={(eid) => router.push(`/entity/${eid}`)} /> : null}
          {tab === 'activity' ? <ActivityTab detail={detail} /> : null}

          {/* Inline Q&A */}
          {qa.length > 0 ? (
            <View style={{ gap: spacing[3], marginTop: spacing[4] }}>
              <Txt variant="caption" tone="muted" weight="bold" style={{ letterSpacing: 1, textTransform: 'uppercase' }}>
                Asked about {detail.name}
              </Txt>
              {qa.map((item, i) => (
                <Animated.View key={i} entering={FadeInDown} style={{ gap: spacing[2] }}>
                  <Txt variant="body" tone="secondary" style={{ fontStyle: 'italic' }}>
                    “{item.q}”
                  </Txt>
                  <View
                    style={{
                      backgroundColor: theme.colors.bgCard,
                      borderRadius: radius.lg,
                      borderWidth: 1,
                      borderColor: item.error ? theme.colors.danger : theme.colors.border,
                      padding: spacing[4],
                    }}
                  >
                    {item.loading && !item.a ? (
                      <ActivityIndicator color={theme.colors.accent} />
                    ) : (
                      <Txt variant="body" tone={item.error ? 'secondary' : 'primary'} style={{ lineHeight: 22 }}>
                        {item.a}
                        {item.loading ? (
                          <Txt tone="accent" weight="bold">
                            {' ▊'}
                          </Txt>
                        ) : null}
                      </Txt>
                    )}
                  </View>
                </Animated.View>
              ))}
            </View>
          ) : null}
        </ScrollView>

        {consentBlocked && !consent.granted ? (
          <Pressable
            onPress={() => {
              haptics.tap();
              void consent.ensure('chat').then((ok) => {
                if (ok) setConsentBlocked(false);
              });
            }}
            accessibilityRole="button"
            style={{ paddingHorizontal: spacing[5], paddingTop: spacing[2], flexDirection: 'row', gap: spacing[2] }}
          >
            <Txt variant="caption" tone="secondary" style={{ flex: 1, lineHeight: 17 }}>
              Not sent. Asking needs your OK to send your question to the AI set up on your computer.
            </Txt>
            <Txt variant="caption" tone="accent" weight="semibold">
              Review
            </Txt>
          </Pressable>
        ) : null}

        {/* Ask composer */}
        <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[2], paddingHorizontal: spacing[5], paddingBottom: Math.max(insets.bottom, spacing[3]), paddingTop: spacing[2] }}>
          <View
            style={{
              flex: 1,
              flexDirection: 'row',
              alignItems: 'center',
              backgroundColor: theme.colors.bgCard,
              borderRadius: radius.xl,
              borderWidth: 1,
              borderColor: theme.colors.border,
              paddingHorizontal: spacing[4],
              minHeight: 50,
            }}
          >
            <TextInput
              value={ask}
              onChangeText={setAsk}
              placeholder={`Ask about ${detail.name}…`}
              placeholderTextColor={theme.colors.textMuted}
              style={{ flex: 1, color: theme.colors.textPrimary, fontSize: 15 }}
              onSubmitEditing={() => onAsk()}
              returnKeyType="send"
            />
          </View>
          <Pressable
            onPress={() => onAsk()}
            disabled={asking || ask.trim().length === 0}
            accessibilityRole="button"
            accessibilityLabel={`Ask about ${detail.name}`}
            style={{
              width: 50,
              height: 50,
              borderRadius: 25,
              backgroundColor: theme.colors.accent,
              alignItems: 'center',
              justifyContent: 'center',
              opacity: asking || ask.trim().length === 0 ? 0.5 : 1,
            }}
          >
            <Txt style={{ color: theme.colors.textInverse, fontSize: 20 }} weight="bold">
              ↑
            </Txt>
          </Pressable>
        </View>
      </KeyboardAvoidingView>

      {consent.sheet}

      <AddNoteModal
        open={addNoteOpen}
        entityName={detail.name}
        onClose={() => setAddNoteOpen(false)}
        onSave={async (title, body) => {
          setAddNoteOpen(false);
          // The note goes to the gateway first. Only a write that landed is
          // allowed to change what the screen shows.
          const saved = await api.saveEntityNote(detail.id, title, body);
          if (!saved) {
            haptics.error();
            Alert.alert('Not saved', "That note didn't reach your computer, so nothing was written.");
            return;
          }
          haptics.success();
          const fresh = await api.getEntity(detail.id);
          if (fresh) setDetail(fresh);
        }}
      />
    </View>
  );
}

function Stat({ label, value }: { label: string; value: number }) {
  return (
    <View style={{ alignItems: 'center' }}>
      <Txt variant="heading" tone="accent">
        {value}
      </Txt>
      <Txt variant="caption" tone="muted">
        {label}
      </Txt>
    </View>
  );
}

function Card({ children }: { children: React.ReactNode }) {
  const theme = useTheme();
  return (
    <View style={{ backgroundColor: theme.colors.bgCard, borderRadius: radius.lg, borderWidth: 1, borderColor: theme.colors.border, padding: spacing[4], gap: spacing[2] }}>
      {children}
    </View>
  );
}

/** Copy + tone for the Knowledge card — covers every `knowledgeIndex.status` plus its absence. */
function knowledgeStatusCopy(
  k: EntityDetail['knowledgeIndex'],
): { line: string; tone: 'secondary' | 'muted' | 'danger'; sub?: string } {
  if (!k) return { line: 'Not indexed yet', tone: 'muted' };
  if (k.status === 'indexing') return { line: 'Indexing the source…', tone: 'secondary' };
  if (k.status === 'ready') {
    const embedded = k.embeddedCount > 0 ? ` · ${k.embeddedCount} embedded` : '';
    return {
      line: `Indexed ${k.docCount} docs${embedded}`,
      tone: 'secondary',
      sub: k.indexedAt ? timeAgo(k.indexedAt) : undefined,
    };
  }
  if (k.status === 'failed') return { line: 'Indexing failed', tone: 'danger', sub: k.error };
  // 'skipped'
  return { line: 'Not indexed', tone: 'muted', sub: k.error };
}

function Overview({ detail, onSummarize }: { detail: EntityDetail; onSummarize: () => void }) {
  const theme = useTheme();
  const knowledge = knowledgeStatusCopy(detail.knowledgeIndex);
  const showSetup = Boolean(detail.readiness || detail.stack || detail.repoPath);
  return (
    <View style={{ gap: spacing[3] }}>
      {detail.whatItIs || detail.profile ? (
        <Card>
          <Txt variant="caption" tone="muted" weight="bold" style={{ letterSpacing: 1, textTransform: 'uppercase' }}>
            What it is
          </Txt>
          <Txt variant="body" tone="secondary" style={{ lineHeight: 22 }}>
            {detail.whatItIs ?? detail.profile}
          </Txt>
        </Card>
      ) : null}
      {detail.primaryGoal || detail.goals ? (
        <Card>
          <Txt variant="caption" tone="muted" weight="bold" style={{ letterSpacing: 1, textTransform: 'uppercase' }}>
            Goal
          </Txt>
          <Txt variant="body" tone="secondary" style={{ lineHeight: 22 }}>
            {detail.primaryGoal ?? detail.goals}
          </Txt>
        </Card>
      ) : null}
      {detail.successCriteria ? (
        <Card>
          <Txt variant="caption" tone="muted" weight="bold" style={{ letterSpacing: 1, textTransform: 'uppercase' }}>
            Success looks like
          </Txt>
          <Txt variant="body" tone="secondary" style={{ lineHeight: 22 }}>
            {detail.successCriteria}
          </Txt>
        </Card>
      ) : null}
      {detail.activeContext ? (
        <Card>
          <Txt variant="caption" tone="muted" weight="bold" style={{ letterSpacing: 1, textTransform: 'uppercase' }}>
            Right now
          </Txt>
          <Txt variant="body" tone="secondary" style={{ lineHeight: 22 }}>
            {detail.activeContext}
          </Txt>
        </Card>
      ) : null}
      {detail.operatingRules ? (
        <Card>
          <Txt variant="caption" tone="muted" weight="bold" style={{ letterSpacing: 1, textTransform: 'uppercase' }}>
            Operating rules
          </Txt>
          <Txt variant="body" tone="secondary" style={{ lineHeight: 22 }}>
            {detail.operatingRules}
          </Txt>
        </Card>
      ) : null}
      {showSetup ? (
        <Card>
          <Txt variant="caption" tone="muted" weight="bold" style={{ letterSpacing: 1, textTransform: 'uppercase' }}>
            Setup
          </Txt>
          {detail.readiness ? (
            <>
              <Txt variant="body" tone="secondary">
                {detail.readiness.label} · {detail.readiness.percent}%
              </Txt>
              <View style={{ height: 5, borderRadius: 3, backgroundColor: theme.colors.bgSurface, overflow: 'hidden' }}>
                <View
                  style={{
                    height: '100%',
                    width: `${Math.max(0, Math.min(100, detail.readiness.percent))}%`,
                    backgroundColor: theme.colors.accent,
                    borderRadius: 3,
                  }}
                />
              </View>
              {detail.readiness.nextQuestion ? (
                <Txt variant="caption" tone="muted">
                  Next: {detail.readiness.nextQuestion}
                </Txt>
              ) : null}
            </>
          ) : null}
          {detail.stack ? (
            <Txt variant="caption" tone="muted">
              {detail.stack}
            </Txt>
          ) : null}
          {detail.repoPath ? (
            <Txt variant="caption" tone="muted">
              {detail.repoPath}
            </Txt>
          ) : null}
        </Card>
      ) : null}
      <Card>
        <Txt variant="caption" tone="muted" weight="bold" style={{ letterSpacing: 1, textTransform: 'uppercase' }}>
          Knowledge
        </Txt>
        <Txt variant="body" tone={knowledge.tone} style={{ lineHeight: 22 }}>
          {knowledge.line}
        </Txt>
        {knowledge.sub ? (
          <Txt variant="caption" tone="muted">
            {knowledge.sub}
          </Txt>
        ) : null}
      </Card>
      <NativeButton label="Summarize & connect with AI" icon="✦" variant="outline" onPress={onSummarize} />
    </View>
  );
}

function FilesTab({ detail }: { detail: EntityDetail }) {
  const theme = useTheme();
  const icon = { doc: '▢', image: '▦', audio: '◈', link: '↗', code: '⟨⟩' } as const;
  if (detail.files.length === 0) {
    // No "Add file" affordance: uploading needs multipart + a document picker
    // the app doesn't bundle, and a button that does nothing is worse than none.
    return <Empty icon="▢" text="No files uploaded to this entity" />;
  }
  return (
    <View style={{ gap: spacing[2] }}>
      {detail.files.map((f) => (
        <View key={f.id} style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[3], backgroundColor: theme.colors.bgCard, borderRadius: radius.lg, borderWidth: 1, borderColor: theme.colors.border, padding: spacing[3] }}>
          <View style={{ width: 38, height: 38, borderRadius: radius.md, backgroundColor: theme.colors.bgSurface, alignItems: 'center', justifyContent: 'center' }}>
            <Txt tone="accent">{icon[f.kind]}</Txt>
          </View>
          <View style={{ flex: 1 }}>
            <Txt variant="label">{f.name}</Txt>
            <Txt variant="caption" tone="muted">
              {f.size} · {timeAgo(f.updatedAt)}
            </Txt>
          </View>
        </View>
      ))}
    </View>
  );
}

function NotesTab({ detail, onAdd, onOpenEntity }: { detail: EntityDetail; onAdd: () => void; onOpenEntity: (id: string) => void }) {
  const theme = useTheme();
  if (detail.notes.length === 0) {
    return (
      <View style={{ gap: spacing[3] }}>
        <Empty icon="✎" text="No notes or captures filed here yet" />
        <NativeButton label="Add note" icon="＋" variant="ghost" onPress={onAdd} />
      </View>
    );
  }
  return (
    <View style={{ gap: spacing[2] }}>
      {detail.notes.map((n) => (
        <View key={n.id} style={{ backgroundColor: theme.colors.bgCard, borderRadius: radius.lg, borderWidth: 1, borderColor: theme.colors.border, padding: spacing[4], gap: spacing[2] }}>
          <Txt variant="heading" style={{ fontSize: 16 }}>
            {n.title}
          </Txt>
          <Txt variant="body" tone="secondary" style={{ lineHeight: 22 }}>
            {n.body}
          </Txt>
          {n.backlinks.length > 0 ? (
            <View style={{ flexDirection: 'row', flexWrap: 'wrap', gap: spacing[2], marginTop: spacing[1] }}>
              {n.backlinks.map((b) => (
                <Pressable
                  key={b}
                  onPress={() => onOpenEntity(b)}
                  style={{ flexDirection: 'row', alignItems: 'center', gap: 4, paddingHorizontal: spacing[2], paddingVertical: 4, borderRadius: radius.sm, backgroundColor: theme.colors.bgSelected }}
                >
                  <Txt tone="accent" style={{ fontSize: 11 }}>
                    ⧉
                  </Txt>
                  <Txt variant="caption" tone="accent">
                    {b}
                  </Txt>
                </Pressable>
              ))}
            </View>
          ) : null}
          <Txt variant="caption" tone="muted">
            {n.createdAt ? timeAgo(n.createdAt) : 'From this entity’s profile'}
          </Txt>
        </View>
      ))}
      <NativeButton label="Add note" icon="＋" variant="ghost" onPress={onAdd} />
    </View>
  );
}

function MissionsTab({ detail }: { detail: EntityDetail }) {
  const router = useRouter();
  if (detail.missions.length === 0) {
    return <Empty icon="➤" text="No missions yet for this entity" />;
  }
  return (
    <View style={{ gap: spacing[2] }}>
      {detail.missions.map((m) => (
        <MissionRow
          key={m.id}
          mission={m}
          onPress={() => {
            haptics.tap();
            // A live mission opens its own task; sample ones have nothing behind them.
            if (detail.live) router.push(`/task/${encodeURIComponent(m.id)}`);
          }}
        />
      ))}
    </View>
  );
}

function MemoryTab({ detail }: { detail: EntityDetail }) {
  const theme = useTheme();
  if (detail.memories.length === 0) {
    return <Empty icon="✦" text="Nothing recalled about this entity yet" />;
  }
  return (
    <View style={{ gap: spacing[2] }}>
      {detail.memories.map((m) => (
        <View key={m.id} style={{ flexDirection: 'row', gap: spacing[3], backgroundColor: theme.colors.bgCard, borderRadius: radius.lg, borderWidth: 1, borderColor: theme.colors.border, padding: spacing[4] }}>
          <Txt tone="accent">✦</Txt>
          <View style={{ flex: 1 }}>
            <Txt variant="body" style={{ lineHeight: 21 }}>
              {m.text}
            </Txt>
            {/* The memory-preview endpoint returns bare strings — no
                timestamps — so name the store rather than invent an age. */}
            <Txt variant="caption" tone="muted" style={{ marginTop: 4 }}>
              {m.at ? timeAgo(m.at) : m.scope === 'long' ? 'Long-term memory' : 'Short-term memory'}
            </Txt>
          </View>
        </View>
      ))}
    </View>
  );
}

function RelatedTab({ detail, onOpen }: { detail: EntityDetail; onOpen: (id: string) => void }) {
  const theme = useTheme();
  if (detail.related.length === 0) return <Empty icon="⧉" text="No related entities" />;
  return (
    <View style={{ gap: spacing[2] }}>
      {detail.related.map((e) => (
        <Pressable
          key={e.id}
          onPress={() => {
            haptics.press();
            onOpen(e.id);
          }}
          style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[3], backgroundColor: theme.colors.bgCard, borderRadius: radius.lg, borderWidth: 1, borderColor: theme.colors.border, padding: spacing[3] }}
        >
          <EntityNode type={e.type} color={entityColor(e)} size={40} />
          <View style={{ flex: 1 }}>
            <Txt variant="label">{e.name}</Txt>
            <Txt variant="caption" tone="muted" numberOfLines={1}>
              {e.summary}
            </Txt>
          </View>
          <Txt tone="accent">›</Txt>
        </Pressable>
      ))}
    </View>
  );
}

function ActivityTab({ detail }: { detail: EntityDetail }) {
  const theme = useTheme();
  if (detail.activity.length === 0) {
    return <Empty icon="≣" text="Nothing has happened here yet" />;
  }
  return (
    <View style={{ gap: 0 }}>
      {detail.activity.map((a, i) => (
        <View key={a.id} style={{ flexDirection: 'row', gap: spacing[3] }}>
          <View style={{ alignItems: 'center', width: 16 }}>
            <View style={{ width: 8, height: 8, borderRadius: 4, backgroundColor: theme.colors.accent, marginTop: 6 }} />
            {i < detail.activity.length - 1 ? <View style={{ flex: 1, width: 1.5, backgroundColor: theme.colors.border }} /> : null}
          </View>
          <View style={{ flex: 1, paddingBottom: spacing[4] }}>
            <Txt variant="body">
              <Txt variant="body" weight="semibold" tone="accent">
                {a.actor}
              </Txt>{' '}
              {a.action}
            </Txt>
            <Txt variant="caption" tone="muted">
              {timeAgo(a.at)}
            </Txt>
          </View>
        </View>
      ))}
    </View>
  );
}

function Empty({ icon, text }: { icon: string; text: string }) {
  return (
    <View style={{ alignItems: 'center', paddingVertical: spacing[10], gap: spacing[2] }}>
      <Txt tone="muted" style={{ fontSize: 28 }}>
        {icon}
      </Txt>
      <Txt tone="muted">{text}</Txt>
    </View>
  );
}

function AddNoteModal({
  open,
  entityName,
  onClose,
  onSave,
}: {
  open: boolean;
  entityName: string;
  onClose: () => void;
  onSave: (title: string, body: string) => void;
}) {
  const theme = useTheme();
  const insets = useSafeAreaInsets();
  const [title, setTitle] = useState('');
  const [body, setBody] = useState('');

  useEffect(() => {
    if (open) {
      setTitle('');
      setBody('');
    }
  }, [open]);

  return (
    <Modal transparent visible={open} animationType="slide" onRequestClose={onClose}>
      <View style={{ flex: 1, backgroundColor: theme.colors.bgOverlay, justifyContent: 'flex-end' }}>
        <KeyboardAvoidingView behavior={Platform.OS === 'ios' ? 'padding' : undefined}>
          <View style={{ backgroundColor: theme.colors.bgSurface, borderTopLeftRadius: radius['2xl'], borderTopRightRadius: radius['2xl'], padding: spacing[5], paddingBottom: insets.bottom + spacing[5], gap: spacing[3] }}>
            <View style={{ flexDirection: 'row', justifyContent: 'space-between', alignItems: 'center' }}>
              <Txt variant="heading">New note · {entityName}</Txt>
              <IconButton icon="✕" onPress={onClose} />
            </View>
            <TextInput
              value={title}
              onChangeText={setTitle}
              placeholder="Title"
              placeholderTextColor={theme.colors.textMuted}
              style={{ color: theme.colors.textPrimary, fontSize: 17, fontWeight: '600', paddingVertical: spacing[2], borderBottomWidth: 1, borderBottomColor: theme.colors.border }}
            />
            <TextInput
              value={body}
              onChangeText={setBody}
              placeholder="Write something…"
              placeholderTextColor={theme.colors.textMuted}
              multiline
              style={{ color: theme.colors.textPrimary, fontSize: 16, minHeight: 100, textAlignVertical: 'top', lineHeight: 22 }}
            />
            <NativeButton label="Save note" icon="✓" fullWidth disabled={!title.trim() && !body.trim()} onPress={() => onSave(title.trim() || 'Untitled', body.trim())} haptic="success" />
          </View>
        </KeyboardAvoidingView>
      </View>
    </Modal>
  );
}
