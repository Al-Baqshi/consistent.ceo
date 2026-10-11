import { useLocalSearchParams, useRouter } from 'expo-router';
import React, { useCallback, useEffect, useRef, useState } from 'react';
import { ActivityIndicator, Image, RefreshControl, ScrollView, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import { api } from '@/api';
import type { RunArtifact, TaskDetail, TaskEvent, TaskEvidence } from '@/api';
import { IconButton, NativeButton, StatusPill, Txt } from '@/components';
import { haptics } from '@/lib/haptics';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

/**
 * One task, end to end: what it was, where it got to, what the agent reported,
 * what changed on disk, and what the run left behind.
 *
 * Live-only by design. Everything here is assembled from endpoints with no
 * fixture fallback (`getTaskDetail`, the task-filtered operator feed, the
 * run's artifact listing), so an unreachable Mac gets an honest empty state
 * rather than a plausible-looking task nobody ever ran.
 *
 * Polling mirrors `ActionRequestCard`: a task that is still queued, running or
 * in review is re-fetched every few seconds while the screen is mounted, and
 * the timer stops the moment it lands somewhere terminal.
 */

const TASK_POLL_MS = 5_000;
const IMAGE_NAME = /\.(png|jpe?g|gif|webp)$/i;

/** `TaskOutcome` (gateway `state.rs`, snake_case) → what to call it here. */
const OUTCOME_LABELS: Record<string, string> = {
  completed: 'Completed',
  needs_review: 'Needs review',
  blocked_by_policy: 'Blocked by policy',
  needs_human_approval: 'Needs your approval',
  needs_stronger_model: 'Needs a stronger model',
  retryable_failure: 'Retryable failure',
  failed: 'Failed',
};

type Phase =
  | 'loading'
  | 'ready'
  | 'missing' // the gateway answered 404
  | 'unreachable'; // the gateway never answered

/** An artifact plus the authenticated source an `<Image>` can actually load. */
interface ArtifactView extends RunArtifact {
  source: { uri: string; headers: Record<string, string> };
}

function timeAgo(iso: string): string {
  const m = Math.round((Date.now() - new Date(iso).getTime()) / 60000);
  if (m < 1) return 'just now';
  if (m < 60) return `${m}m ago`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.round(h / 24)}d ago`;
}

export default function TaskScreen() {
  const theme = useTheme();
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const { id } = useLocalSearchParams<{ id: string }>();

  const [task, setTask] = useState<TaskDetail | null>(null);
  const [phase, setPhase] = useState<Phase>('loading');
  const [events, setEvents] = useState<TaskEvent[]>([]);
  const [runId, setRunId] = useState<string | null>(null);
  const [artifacts, setArtifacts] = useState<ArtifactView[]>([]);
  const [refreshing, setRefreshing] = useState(false);

  // A poll tick can land after the screen is gone; nothing writes to a
  // component that has already left.
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  // Artifact rows are rebuilt only when the run's file list actually changes.
  // A poll tick that resolves the same names again would hand every `<Image>`
  // a fresh source object (and re-read the token) for a picture already on
  // screen — visible as a flicker every five seconds.
  const artifactKey = useRef('');

  const load = useCallback(async () => {
    if (!id) return;
    let detail: TaskDetail | null;
    try {
      detail = await api.getTaskDetail(id);
    } catch {
      // Network-level: the Mac didn't answer. Keep whatever is already on
      // screen (a poll tick that missed shouldn't blank a loaded task) and
      // only claim an outage when there was nothing to show in the first place.
      if (mounted.current) setPhase((p) => (p === 'ready' ? p : 'unreachable'));
      return;
    }
    if (!mounted.current) return;
    if (!detail) {
      setPhase('missing');
      return;
    }
    setTask(detail);
    setPhase('ready');

    const [feed, run] = await Promise.all([
      api.listTaskEvents(id).catch(() => [] as TaskEvent[]),
      api.getTaskRunId(detail).catch(() => null),
    ]);
    if (!mounted.current) return;
    setEvents(feed);
    setRunId(run);

    if (!run) {
      artifactKey.current = '';
      setArtifacts([]);
      return;
    }
    const files = await api.listRunArtifacts(run).catch(() => [] as RunArtifact[]);
    const key = [run, ...files.map((f) => f.name)].join('|');
    if (!mounted.current || key === artifactKey.current) return;
    const withSources = await Promise.all(
      files.map(async (f) => ({ ...f, source: await api.runArtifactSource(run, f.name) })),
    );
    if (!mounted.current) return;
    artifactKey.current = key;
    setArtifacts(withSources);
  }, [id]);

  useEffect(() => {
    void load();
  }, [load]);

  // Keep following the work while it is still moving; stop dead on a terminal
  // status, the same rule the inline approval card follows.
  const status = task?.status;
  const polling = status === 'queued' || status === 'running' || status === 'review';
  useEffect(() => {
    if (!polling) return;
    const timer = setInterval(() => void load(), TASK_POLL_MS);
    return () => clearInterval(timer);
  }, [polling, load]);

  const onRefresh = async () => {
    setRefreshing(true);
    await load();
    if (mounted.current) setRefreshing(false);
  };

  if (phase !== 'ready' || !task) {
    return (
      <View style={{ flex: 1, backgroundColor: theme.colors.bg, paddingTop: insets.top }}>
        <View style={{ flexDirection: 'row', paddingHorizontal: spacing[4], paddingVertical: spacing[2] }}>
          <IconButton icon="‹" glyphSize={26} filled onPress={() => router.back()} label="Back" />
        </View>
        <View style={{ flex: 1, alignItems: 'center', justifyContent: 'center', gap: spacing[4], paddingHorizontal: spacing[6] }}>
          {phase === 'loading' ? <ActivityIndicator color={theme.colors.accent} /> : null}
          {phase === 'missing' ? (
            <>
              <Txt style={{ fontSize: 34 }} tone="muted">
                ➤
              </Txt>
              <Txt variant="heading" center>
                No such task
              </Txt>
              <Txt variant="body" tone="secondary" center style={{ lineHeight: 22 }}>
                Your computer has nothing under “{id}”. It may have been cancelled, or it belongs to
                another workspace.
              </Txt>
              <NativeButton label="Go back" variant="outline" onPress={() => router.back()} />
            </>
          ) : null}
          {phase === 'unreachable' ? (
            <>
              <Txt style={{ fontSize: 34 }} tone="muted">
                ⌁
              </Txt>
              <Txt variant="heading" center>
                Can’t reach your computer
              </Txt>
              <Txt variant="body" tone="secondary" center style={{ lineHeight: 22 }}>
                Consistent CEO on your computer didn’t answer, so there is nothing real to show for
                this task. Check that your computer is awake and online, then try again.
              </Txt>
              <View style={{ flexDirection: 'row', gap: spacing[2] }}>
                <NativeButton
                  label="Connection settings"
                  variant="ghost"
                  size="sm"
                  onPress={() => router.push('/settings')}
                />
                <NativeButton
                  label="Try again"
                  icon="↻"
                  size="sm"
                  onPress={() => {
                    setPhase('loading');
                    void load();
                  }}
                />
              </View>
            </>
          ) : null}
        </View>
      </View>
    );
  }

  const outcome = task.outcome ? (OUTCOME_LABELS[task.outcome] ?? task.outcome) : null;
  const entityId = task.entityId;

  return (
    <View style={{ flex: 1, backgroundColor: theme.colors.bg }}>
      <View style={{ flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', paddingTop: insets.top + spacing[2], paddingHorizontal: spacing[4] }}>
        <IconButton icon="‹" glyphSize={26} filled onPress={() => router.back()} label="Back" />
        {entityId ? (
          <IconButton
            icon="⬡"
            filled
            label="Open entity"
            onPress={() => {
              haptics.tap();
              router.push(`/entity/${encodeURIComponent(entityId)}`);
            }}
          />
        ) : null}
      </View>

      <ScrollView
        style={{ flex: 1 }}
        contentContainerStyle={{ padding: spacing[5], paddingBottom: insets.bottom + spacing[12], gap: spacing[4] }}
        showsVerticalScrollIndicator={false}
        refreshControl={
          <RefreshControl
            refreshing={refreshing}
            onRefresh={onRefresh}
            tintColor={theme.colors.accent}
            colors={[theme.colors.accent]}
          />
        }
      >
        {/* Heading — status wording comes from StatusPill, the one place a
            task status is turned into words (the approval card renders through
            it too, so both surfaces stay in step). */}
        <View style={{ gap: spacing[3] }}>
          <Txt variant="title" style={{ lineHeight: 34 }}>
            {task.title}
          </Txt>
          <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[2], flexWrap: 'wrap' }}>
            <StatusPill status={task.status} />
            {outcome ? <StatusPill tone="neutral" label={outcome} dot={false} /> : null}
            {polling ? (
              <Txt variant="caption" tone="muted">
                live · updating
              </Txt>
            ) : null}
          </View>
          <Txt variant="caption" tone="muted">
            {[
              task.entityName,
              task.agentName,
              task.dept,
              task.updatedAt ? `updated ${timeAgo(task.updatedAt)}` : undefined,
            ]
              .filter(Boolean)
              .join(' · ')}
          </Txt>
        </View>

        <Section title="Report">
          {task.result ? (
            <Card>
              <Txt variant="body" style={{ lineHeight: 22 }}>
                {task.result}
              </Txt>
            </Card>
          ) : (
            <Empty
              text={
                polling
                  ? 'The agent hasn’t reported back yet — this updates itself.'
                  : 'This task finished without writing a report.'
              }
            />
          )}
        </Section>

        <Section title="What changed on disk">
          <EvidenceBlock evidence={task.evidence} />
        </Section>

        <Section title="Artifacts" note={runId ? `run ${runId.slice(0, 8)}` : undefined}>
          <ArtifactsBlock artifacts={artifacts} hasRun={!!runId} />
        </Section>

        <Section title="Timeline">
          <Timeline events={events} />
        </Section>
      </ScrollView>
    </View>
  );
}

function Section({ title, note, children }: { title: string; note?: string; children: React.ReactNode }) {
  return (
    <View style={{ gap: spacing[2] }}>
      <View style={{ flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between' }}>
        <Txt variant="caption" tone="muted" weight="bold" style={{ letterSpacing: 1, textTransform: 'uppercase' }}>
          {title}
        </Txt>
        {note ? (
          <Txt variant="caption" tone="muted">
            {note}
          </Txt>
        ) : null}
      </View>
      {children}
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

function Empty({ text }: { text: string }) {
  return (
    <View style={{ paddingVertical: spacing[4] }}>
      <Txt variant="body" tone="muted" style={{ lineHeight: 21 }}>
        {text}
      </Txt>
    </View>
  );
}

/**
 * The trust signal: what the run actually did to the working directory, as
 * measured by the gateway rather than claimed by the agent.
 *
 * A non-git directory is called out explicitly — such a pack can show that
 * something changed but never that nothing did, so its zeroes mean "unknown",
 * not "untouched".
 */
function EvidenceBlock({ evidence }: { evidence?: TaskEvidence }) {
  const theme = useTheme();

  if (!evidence) {
    return <Empty text="No evidence was collected for this task — it predates evidence collection, or no run touched a directory." />;
  }
  if (!evidence.isGit) {
    return (
      <Card>
        <Txt variant="body" tone="secondary" style={{ lineHeight: 22 }}>
          The working directory isn’t a git repository, so file-level evidence wasn’t available.
          Nothing here can confirm — or rule out — a change on disk.
        </Txt>
      </Card>
    );
  }

  const nothing =
    evidence.commits === 0 && evidence.filesChanged === 0 && evidence.dirtyDelta.length === 0;
  const plural = (n: number, one: string) => `${n} ${one}${n === 1 ? '' : 's'}`;

  return (
    <Card>
      <Txt variant="heading" style={{ fontSize: 16 }}>
        {nothing
          ? 'Nothing changed on disk'
          : `${plural(evidence.commits, 'commit')} · ${plural(evidence.filesChanged, 'file')}`}
      </Txt>
      {!nothing ? (
        <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[3] }}>
          <Txt variant="label" style={{ color: theme.colors.success }}>
            +{evidence.insertions}
          </Txt>
          <Txt variant="label" style={{ color: theme.colors.danger }}>
            −{evidence.deletions}
          </Txt>
        </View>
      ) : (
        <Txt variant="body" tone="secondary" style={{ lineHeight: 22 }}>
          The run left no commits and no diff in its working directory.
        </Txt>
      )}
      {evidence.dirtyDelta.length > 0 ? (
        <View style={{ gap: 4, marginTop: spacing[1] }}>
          <Txt variant="caption" tone="muted">
            Uncommitted after the run
          </Txt>
          {evidence.dirtyDelta.map((path) => (
            <Txt key={path} variant="mono" tone="secondary" numberOfLines={1}>
              {path}
            </Txt>
          ))}
        </View>
      ) : null}
    </Card>
  );
}

/**
 * Files the run left behind. Screenshots render inline; anything else (and
 * any image whose bytes don't load) falls back to a name + size row, which is
 * all this screen can honestly say about it.
 */
function ArtifactsBlock({ artifacts, hasRun }: { artifacts: ArtifactView[]; hasRun: boolean }) {
  const theme = useTheme();
  const [failed, setFailed] = useState<string[]>([]);

  if (!hasRun) {
    return <Empty text="No agent run is linked to this task, so there are no files to show." />;
  }
  if (artifacts.length === 0) {
    return <Empty text="The run produced no files." />;
  }

  return (
    <View style={{ gap: spacing[3] }}>
      {artifacts.map((a) =>
        IMAGE_NAME.test(a.name) && !failed.includes(a.name) ? (
          <View key={a.name} style={{ gap: spacing[1] }}>
            <Image
              source={a.source}
              style={{ width: '100%', height: 220, borderRadius: radius.lg, borderWidth: 1, borderColor: theme.colors.border, backgroundColor: theme.colors.bgCard }}
              resizeMode="cover"
              accessibilityLabel={a.name}
              onError={() => setFailed((prev) => [...prev, a.name])}
            />
            <Txt variant="caption" tone="muted" numberOfLines={1}>
              {a.name} · {a.size}
            </Txt>
          </View>
        ) : (
          <View
            key={a.name}
            style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[3], backgroundColor: theme.colors.bgCard, borderRadius: radius.lg, borderWidth: 1, borderColor: theme.colors.border, padding: spacing[3] }}
          >
            <View style={{ width: 38, height: 38, borderRadius: radius.md, backgroundColor: theme.colors.bgSurface, alignItems: 'center', justifyContent: 'center' }}>
              <Txt tone="accent">▢</Txt>
            </View>
            <View style={{ flex: 1 }}>
              <Txt variant="label" numberOfLines={1}>
                {a.name}
              </Txt>
              <Txt variant="caption" tone="muted">
                {failed.includes(a.name) ? `${a.size} · couldn’t be displayed` : a.size}
              </Txt>
            </View>
          </View>
        ),
      )}
    </View>
  );
}

/** Feed events for this task, newest first — the same rows Control shows. */
function Timeline({ events }: { events: TaskEvent[] }) {
  const theme = useTheme();
  if (events.length === 0) {
    return <Empty text="Nothing has been recorded against this task yet." />;
  }
  return (
    <View style={{ gap: 0 }}>
      {events.map((e, i) => (
        <View key={e.id} style={{ flexDirection: 'row', gap: spacing[3] }}>
          <View style={{ alignItems: 'center', width: 16 }}>
            <View style={{ width: 8, height: 8, borderRadius: 4, backgroundColor: theme.colors.accent, marginTop: 6 }} />
            {i < events.length - 1 ? <View style={{ flex: 1, width: 1.5, backgroundColor: theme.colors.border }} /> : null}
          </View>
          <View style={{ flex: 1, paddingBottom: spacing[4] }}>
            <Txt variant="body">{e.title}</Txt>
            {e.detail ? (
              <Txt variant="caption" tone="secondary" style={{ lineHeight: 19, marginTop: 2 }}>
                {e.detail}
              </Txt>
            ) : null}
            <Txt variant="caption" tone="muted" style={{ marginTop: 2 }}>
              {timeAgo(e.at)}
            </Txt>
          </View>
        </View>
      ))}
    </View>
  );
}
