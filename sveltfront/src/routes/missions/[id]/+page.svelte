<script lang="ts">
  import { feedKindLabel } from '$lib/feedKinds';
  import { invalidateAll } from '$app/navigation';
  import { applyMissionAction, cancelMission } from '$lib/api';
  import { deriveMissionExecutionSummary } from '$lib/missionExecution';
  import { describeMissionFeedEvent, humanizeAutorunBlockers, parseFeedDetail } from '$lib/missionFeed';
  import { deriveCurrentMissionState } from '$lib/missionState';
  import { api } from '$lib/api';
  import Seo from '$lib/components/Seo.svelte';
  type Task = {
    id: string;
    title: string;
    status: 'backlog' | 'queued' | 'in-progress' | 'failed' | 'completed' | 'cancelled';
    progress: number;
    task_kind?: string;
    priority?: string;
    assigned_director_id?: string;
    assigned_agent_id?: string;
    result?: string;
  };

  type Mission = {
    id: string;
    entity_id: string;
    title: string;
    summary: string;
    status: 'planned' | 'active' | 'blocked' | 'user-input-needed' | 'completed' | 'cancelled';
    priority: string;
    goal: string;
    owner?: string;
    created_by?: string;
    source?: string;
    created_at: string;
    updated_at: string;
    last_reviewed_at?: string;
    final_report?: string;
  };

  type MissionEvent = {
    id: string;
    timestamp: string;
    kind:
      | 'task_created'
      | 'task_completed'
      | 'task_blocked'
      | 'approval_needed'
      | 'mission_created'
      | 'mission_stalled'
      | 'mission_missing_next_task'
      | 'mission_review_due'
      | 'mission_followup_suppressed'
      | 'mission_recommendation_accepted'
      | 'mission_recommendation_dismissed'
      | 'mission_snoozed'
      | 'mission_user_input_needed';
    taskId?: string;
    missionId?: string;
    title: string;
    detail?: string;
  };

  type MissionMetrics = {
    recommendationEmitted: number;
    recommendationAccepted: number;
    recommendationDismissed: number;
    duplicateSuppressed: number;
    staleUnresolved?: number;
    byBranch?: Record<string, Record<string, number>>;
  };
  type DirectorView = {
    id: string;
    name: string;
    heartbeatEnabled: boolean;
  };
  let { data } = $props<{
    data: {
      mission: Mission | null;
      tasks: Task[];
      events: MissionEvent[];
      entityActivity: MissionEvent[];
      directors: DirectorView[];
      metrics: MissionMetrics | null;
      error: string | null;
    };
  }>();
  const mission = $derived(data.mission);
  const tasks = $derived((data.tasks ?? []) as Task[]);
  const events = $derived((data.events ?? []) as MissionEvent[]);
  const entityActivity = $derived((data.entityActivity ?? []) as MissionEvent[]);
  const directors = $derived((data.directors ?? []) as DirectorView[]);
  const error = $derived(data.error);
  const metrics = $derived(data.metrics);
  const activeHeartbeatWorkers = $derived(directors.filter((d: DirectorView) => d.heartbeatEnabled));

  type EngineStatus = {
    missionId: string;
    attached: boolean;
    taskId?: string;
    cwd?: string;
    nextBeatAt?: string;
    lastSummary?: string;
    sessionId?: string;
    beatCount: number;
    costUsd: number;
    paused: boolean;
    waitingOnUsageUntil?: string;
  };

  let engine = $state<EngineStatus | null>(null);
  let engineBusy = $state(false);
  let engineError = $state('');
  let engineCwd = $state('');
  let engineHours = $state(4);
  let engineBudget = $state(0);

  async function loadEngine() {
    if (!mission) return;
    try {
      engine = await api<EngineStatus>(`/api/missions/${mission.id}/engine`);
    } catch (err) {
      engine = null;
    }
  }

  async function engineCall(path: string, method: string, body?: unknown) {
    if (!mission) return;
    engineBusy = true;
    engineError = '';
    try {
      const res = await api<EngineStatus>(`/api/missions/${mission.id}/engine${path}`, {
        method,
        ...(body ? { body: JSON.stringify(body) } : {})
      });
      engine = res ?? null;
    } catch (err) {
      engineError = err instanceof Error ? err.message : String(err);
    } finally {
      engineBusy = false;
      if (method === 'DELETE') await loadEngine();
    }
  }

  const attachEngine = () =>
    engineCall('', 'POST', {
      cwd: engineCwd.trim() || undefined,
      beatIntervalMinutes: Math.max(1, Math.round(engineHours * 60)),
      budgetUsd: engineBudget > 0 ? engineBudget : undefined
    });
  const detachEngine = () => engineCall('', 'DELETE');
  const beatNow = () => engineCall('/beat-now', 'POST');

  $effect(() => {
    if (mission?.id) void loadEngine();
  });

  const queued = $derived(tasks.filter((t: Task) => t.status === 'queued' || t.status === 'backlog'));
  const inProgress = $derived(tasks.filter((t: Task) => t.status === 'in-progress'));
  const blocked = $derived(tasks.filter((t: Task) => t.status === 'failed'));
  const completed = $derived(tasks.filter((t: Task) => t.status === 'completed'));
  const nextTask = $derived(queued[0] ?? inProgress[0] ?? null);
  const executionSummary = $derived(deriveMissionExecutionSummary(tasks, events));
  const allBlocked = $derived(tasks.length > 0 && blocked.length === tasks.length);
  const allCompleted = $derived(tasks.length > 0 && completed.length === tasks.length);
  let actionBusy = $state(false);
  let actionError = $state<string | null>(null);

  async function runAction(action: 'review_now' | 'snooze_review' | 'accept_proposed_next_step' | 'dismiss_no_action' | 'mark_user_input_needed', hours?: number) {
    if (!mission) return;
    actionBusy = true;
    actionError = null;
    try {
      await applyMissionAction(mission.id, { action, hours });
      await invalidateAll();
    } catch (err) {
      actionError = err instanceof Error ? err.message : 'Mission action failed';
    } finally {
      actionBusy = false;
    }
  }

  const missionActive = $derived(
    mission != null && mission.status !== 'completed' && mission.status !== 'cancelled'
  );

  async function handleCancelMission() {
    if (!mission || !missionActive) return;
    if (typeof window !== 'undefined' && !window.confirm('Cancel this mission and stop all its work?')) {
      return;
    }
    actionBusy = true;
    actionError = null;
    try {
      await cancelMission(mission.id);
      await invalidateAll();
    } catch (err) {
      actionError = err instanceof Error ? err.message : 'Could not cancel the mission';
    } finally {
      actionBusy = false;
    }
  }

  type EventMeta = {
    reason?: string;
    reasonRaw?: string;
    branch?: string;
    policy?: string;
    counts?: string;
    taskKind?: string;
    action?: string;
    reviewDue?: string;
    snoozedUntil?: string;
    raw?: string;
  };

  /** Shared with every other feed surface — see `$lib/feedKinds`. */
  const kindLabel = (kind: MissionEvent['kind']): string => feedKindLabel(kind);

  function branchLabel(branch?: string): string | undefined {
    if (!branch) return undefined;
    if (branch === 'bootstrap') return 'Bootstrap';
    if (branch === 'unblock') return 'Unblock';
    if (branch === 'next_phase') return 'Next phase';
    if (branch === 'user_input') return 'Needs input';
    return branch;
  }

  function parseEventMeta(event: MissionEvent): EventMeta {
    const detail = event.detail ?? '';
    if (!detail) return {};
    const pairs = parseFeedDetail(detail);
    const total = pairs.observed_total ?? pairs.total;
    const blockedCount = pairs.observed_blocked ?? pairs.blocked;
    const completedCount = pairs.observed_completed ?? pairs.completed;
    const policy = (() => {
      const flags = [];
      if (pairs.policy_proposal_enabled) flags.push(`proposal:${pairs.policy_proposal_enabled}`);
      if (pairs.policy_auto_create) flags.push(`auto-create:${pairs.policy_auto_create}`);
      return flags.length ? flags.join(' · ') : undefined;
    })();
    return {
      reason: pairs.reason === 'autorun_blocked' ? humanizeAutorunBlockers(pairs.blockers) ?? 'auto-run blocked' : pairs.reason?.replaceAll('_', ' '),
      reasonRaw: pairs.reason,
      branch: branchLabel(pairs.branch),
      policy,
      counts: total ? `total:${total}${blockedCount ? ` blocked:${blockedCount}` : ''}${completedCount ? ` completed:${completedCount}` : ''}` : undefined,
      taskKind: pairs.task_kind,
      action: pairs.action ?? pairs.review_action ?? pairs.decision,
      reviewDue: pairs.review_due,
      snoozedUntil: pairs.snoozed_until,
      raw: detail
    };
  }

  const latestEvent = $derived(events[0] ?? null);
  const latestEventSummary = $derived(latestEvent ? describeMissionFeedEvent(latestEvent) : null);
  const latestReviewEvent = $derived(
    events.find((e: MissionEvent) => e.kind === 'mission_review_due' || e.kind === 'mission_snoozed') ?? null
  );
  const nextStepSuggestionEvent = $derived(
    events.find((e: MissionEvent) =>
      e.kind === 'mission_missing_next_task' ||
      e.kind === 'mission_stalled' ||
      e.kind === 'mission_user_input_needed'
    ) ?? null
  );
  const nextStepSuggestionMeta = $derived(nextStepSuggestionEvent ? parseEventMeta(nextStepSuggestionEvent) : null);
  const latestSuppressionReason = $derived(latestEvent ? parseEventMeta(latestEvent).reasonRaw : undefined);
  const recommendationSummary = $derived(metrics ? {
    emitted: metrics.recommendationEmitted,
    accepted: metrics.recommendationAccepted,
    dismissed: metrics.recommendationDismissed,
    suppressed: metrics.duplicateSuppressed,
    acceptanceRate: metrics.recommendationEmitted > 0 ? Math.round((metrics.recommendationAccepted / metrics.recommendationEmitted) * 100) : 0,
    dismissalRate: metrics.recommendationEmitted > 0 ? Math.round((metrics.recommendationDismissed / metrics.recommendationEmitted) * 100) : 0
  } : null);

  const nextActionStatus = $derived(
    deriveCurrentMissionState({
      missionStatus: mission?.status,
      hasNextTask: !!nextTask,
      nextTaskTitle: nextTask?.title,
      allBlocked,
      allCompleted,
      latestEventKind: latestEvent?.kind,
      latestSuppressionReason
    })
  );
  const progressRate = $derived(
    tasks.length > 0 ? Math.round((completed.length / tasks.length) * 100) : 0
  );
</script>

<Seo title="Mission" noindex />

<div class="mission-page">
  <a href="/entities/{mission?.entity_id ?? ''}" class="back-link">← Back to entity</a>
  {#if mission}
    <header class="mission-header">
      <h1>{mission.title}</h1>
      <div class="chips">
        <span>{mission.status}</span>
        <span>priority: {mission.priority}</span>
      </div>
      <div class="actions quick-links">
        <a class="quick-link" href={`/tasks?mission_id=${mission.id}`}>Open mission tasks</a>
        <a class="quick-link" href={`/tasks?entity_id=${mission.entity_id}`}>Open entity tasks</a>
        {#if missionActive}
          <button type="button" class="quick-link cancel-link" disabled={actionBusy} onclick={handleCancelMission}>
            Cancel mission
          </button>
        {/if}
      </div>
      {#if actionError}
        <p class="mission-cancel-error" role="status">{actionError}</p>
      {/if}
      {#if mission.summary}
        <p>{mission.summary}</p>
      {/if}
      {#if mission.goal}
        <p><strong>Goal:</strong> {mission.goal}</p>
      {/if}
    </header>

    {#if mission.final_report}
      <section class="mission-final-report">
        <h2>Final report</h2>
        <pre class="mission-final-report-body">{mission.final_report}</pre>
      </section>
    {/if}

    <section class="mission-engine">
      <h2>Engine</h2>
      {#if engine?.attached}
        <p class="engine-sub">
          Claude Code is working this mission on its own, resuming the same session each beat.
        </p>
        <div class="engine-facts">
          <div><span class="engine-label">Beats run</span><strong>{engine.beatCount}</strong></div>
          <div><span class="engine-label">Spent</span><strong>${engine.costUsd.toFixed(2)}</strong></div>
          <div>
            <span class="engine-label">Next beat</span>
            <strong>{engine.paused ? 'Paused' : (engine.nextBeatAt ? new Date(engine.nextBeatAt).toLocaleString() : 'Due now')}</strong>
          </div>
        </div>
        {#if engine.cwd}<p class="engine-path">Working in <code>{engine.cwd}</code></p>{/if}
        {#if engine.waitingOnUsageUntil}
          <p class="engine-summary">
            ⏳ Your Claude usage window is spent. Work resumes on its own at
            <strong>{new Date(engine.waitingOnUsageUntil).toLocaleString()}</strong> — nothing to do.
          </p>
        {/if}
        {#if engine.lastSummary}
          <p class="engine-summary"><strong>Last beat:</strong> {engine.lastSummary}</p>
        {/if}
        {#if engine.paused}
          <p class="engine-summary">
            Paused — the agent either finished or needs a decision from you. Beat now to continue.
          </p>
        {/if}
        <div class="engine-actions">
          <button type="button" class="quick-link" disabled={engineBusy} onclick={beatNow}>
            {engineBusy ? 'Working…' : 'Beat now'}
          </button>
          <button type="button" class="quick-link cancel-link" disabled={engineBusy} onclick={detachEngine}>
            Detach engine
          </button>
        </div>
      {:else}
        <p class="engine-sub">
          Hand this mission to Claude Code on this machine. It wakes on a cadence, picks up where it
          left off, and tells you when it is blocked or thinks it is done — it never closes the
          mission itself.
        </p>
        <div class="engine-form">
          <label>
            <span class="engine-label">Working directory</span>
            <input type="text" bind:value={engineCwd} placeholder="Defaults to the entity's repo" />
          </label>
          <label>
            <span class="engine-label">Beat every (hours)</span>
            <input type="number" min="0.25" step="0.25" bind:value={engineHours} />
          </label>
          <label>
            <span class="engine-label">Budget cap (USD, 0 = none)</span>
            <input type="number" min="0" step="1" bind:value={engineBudget} />
          </label>
        </div>
        <div class="engine-actions">
          <button type="button" class="quick-link" disabled={engineBusy} onclick={attachEngine}>
            {engineBusy ? 'Attaching…' : 'Attach engine'}
          </button>
        </div>
      {/if}
      {#if engineError}<p class="mission-cancel-error" role="status">{engineError}</p>{/if}
    </section>

    <section class="stats">
      <div>Queued: <strong>{queued.length}</strong></div>
      <div>In progress: <strong>{inProgress.length}</strong></div>
      <div>Blocked: <strong>{blocked.length}</strong></div>
      <div>Done: <strong>{completed.length}</strong></div>
      <div>Progress: <strong>{progressRate}%</strong></div>
      <div>Heartbeat workers: <strong>{activeHeartbeatWorkers.length}</strong></div>
    </section>
    <section class="next-task">
      <h2>Execution state</h2>
      <div class="meta">
        <span class="badge execution-{executionSummary.status}">{executionSummary.label}</span>
        <span class="badge">active {executionSummary.activeCount}</span>
        <span class="badge">queued {executionSummary.queuedCount}</span>
        {#if executionSummary.failedCount > 0}<span class="badge">failed {executionSummary.failedCount}</span>{/if}
      </div>
      <p class="empty">{executionSummary.detail}</p>
      {#if executionSummary.blockingReason}
        <p class="empty">Latest blocking reason: <strong>{executionSummary.blockingReason}</strong></p>
      {/if}
      {#if latestEventSummary}
        <p class="empty emphasis">Latest feed signal: {latestEventSummary}</p>
      {/if}
    </section>
    <section class="next-task">
      <h2>Heartbeat Review</h2>
      {#if activeHeartbeatWorkers.length === 0}
        <p class="empty">Heartbeat is currently off for all workers. Mission review will rely on manual actions.</p>
      {:else}
        <p class="empty">
          Heartbeat is active on {activeHeartbeatWorkers.length} worker{activeHeartbeatWorkers.length === 1 ? '' : 's'}:
          {activeHeartbeatWorkers.map((d: DirectorView) => d.name).join(', ')}.
        </p>
      {/if}
      {#if latestReviewEvent}
        {@const reviewMeta = parseEventMeta(latestReviewEvent)}
        <div class="meta">
          <span class="badge">Latest review: {kindLabel(latestReviewEvent.kind)}</span>
          {#if reviewMeta.reviewDue}<span class="badge">Due: {reviewMeta.reviewDue}</span>{/if}
          {#if reviewMeta.snoozedUntil}<span class="badge">Snoozed until: {reviewMeta.snoozedUntil}</span>{/if}
        </div>
      {:else}
        <p class="empty">No review event yet. Use Review now to trigger a mission review checkpoint.</p>
      {/if}
    </section>
    <section class="next-task">
      <h2>Current State</h2>
      <p class="empty">{nextActionStatus}</p>
      {#if recommendationSummary}
        <p class="empty">
          Recommendations emitted {recommendationSummary.emitted} · accepted {recommendationSummary.accepted} · dismissed {recommendationSummary.dismissed} · duplicate-suppressed {recommendationSummary.suppressed} · stale unresolved {metrics?.staleUnresolved ?? 0}
        </p>
        <p class="empty">
          Acceptance rate {recommendationSummary.acceptanceRate}% · Dismissal rate {recommendationSummary.dismissalRate}%
        </p>
        {#if metrics?.byBranch}
          <div class="meta">
            {#each Object.entries(metrics.byBranch) as [branch, values]}
              {@const branchValues = values as Record<string, number>}
              <span class="badge">
                {branchLabel(branch) ?? branch}: {(branchValues.emitted ?? 0)} emitted / {(branchValues.accepted ?? 0)} accepted / {(branchValues.dismissed ?? 0)} dismissed / {(branchValues.snoozed ?? 0)} snoozed
              </span>
            {/each}
          </div>
        {/if}
      {/if}
    </section>
    <section class="next-task">
      <h2>Suggested Next Step</h2>
      {#if nextStepSuggestionEvent}
        <p class="empty">
          {describeMissionFeedEvent(nextStepSuggestionEvent)
            ?? (nextStepSuggestionMeta?.reason
              ? `Reason: ${nextStepSuggestionMeta.reason}.`
              : 'Mission analysis indicates a next step is needed.')}
          {#if !describeMissionFeedEvent(nextStepSuggestionEvent) && nextStepSuggestionMeta?.branch} Branch: {nextStepSuggestionMeta.branch}.{/if}
        </p>
        {#if nextStepSuggestionEvent.kind === 'mission_user_input_needed'}
          <p class="empty">Mission needs explicit user input before next automation step.</p>
        {/if}
        {#if nextStepSuggestionMeta?.taskKind}
          <p class="empty">Suggested task kind: <strong>{nextStepSuggestionMeta.taskKind}</strong></p>
        {/if}
        <div class="actions">
          <button disabled={actionBusy} onclick={() => runAction('accept_proposed_next_step')}>Accept suggested next step</button>
          <button disabled={actionBusy} onclick={() => runAction('dismiss_no_action')}>Dismiss suggestion</button>
        </div>
      {:else}
        <p class="empty">No open suggestion right now. Mission is either progressing or waiting on review cadence.</p>
      {/if}
    </section>
    <section class="next-task">
      <h2>Review Actions</h2>
      <div class="actions">
        <button disabled={actionBusy} onclick={() => runAction('review_now')}>Review now</button>
        <button disabled={actionBusy} onclick={() => runAction('snooze_review', 24)}>Snooze 24h</button>
        <button disabled={actionBusy} onclick={() => runAction('accept_proposed_next_step')}>Accept next step</button>
        <button disabled={actionBusy} onclick={() => runAction('dismiss_no_action')}>Dismiss</button>
        <button disabled={actionBusy} onclick={() => runAction('mark_user_input_needed')}>Needs user input</button>
      </div>
      {#if actionError}<p class="empty">{actionError}</p>{/if}
      {#if metrics}
        <p class="empty">Reco: {metrics.recommendationEmitted} emitted, {metrics.recommendationAccepted} accepted, {metrics.recommendationDismissed} dismissed, {metrics.duplicateSuppressed} suppressed.</p>
      {/if}
    </section>

    <section class="next-task">
      <h2>Next Task</h2>
      {#if nextTask}
        <div class="next-task-card">
          <div class="row">
            <span class="title">{nextTask.title}</span>
            <span class="badge">{nextTask.status}</span>
          </div>
          <div class="meta">
            <span>{nextTask.progress}%</span>
            {#if nextTask.task_kind}<span>{nextTask.task_kind}</span>{/if}
            {#if nextTask.assigned_director_id}<span>{nextTask.assigned_director_id}</span>{/if}
          </div>
        </div>
      {:else if tasks.length === 0}
        <p class="empty">No linked tasks yet. Create a mission task set to get this mission moving.</p>
      {:else if allBlocked}
        <p class="empty">All mission tasks are blocked. Resolve blockers or queue a recovery task.</p>
      {:else if allCompleted}
        <p class="empty">All mission tasks are complete. Add a review or next-phase task if needed.</p>
      {:else}
        <p class="empty">No clear next task right now. Review mission status and queue the next step.</p>
      {/if}
    </section>

    <section class="tasks">
      <h2>Mission Tasks</h2>
      {#if tasks.length === 0}
        <p class="empty">No linked tasks yet.</p>
      {:else}
        <ul>
          {#each tasks as task}
            <li>
              <div class="row">
                <span class="title">{task.title}</span>
                <span class="badge">{task.status}</span>
              </div>
              <div class="meta">
                <span>{task.progress}%</span>
                {#if task.task_kind}<span>{task.task_kind}</span>{/if}
                {#if task.assigned_director_id}<span>{task.assigned_director_id}</span>{/if}
              </div>
            </li>
          {/each}
        </ul>
      {/if}
    </section>

    <section class="timeline">
      <h2>Mission Timeline</h2>
      {#if events.length === 0}
        <p class="empty">No mission events yet.</p>
      {:else}
        <ul class="events">
          {#each events as event}
            {@const meta = parseEventMeta(event)}
            <li>
              <div class="row">
                <span class="badge">{kindLabel(event.kind)}</span>
                <span class="time">{new Date(event.timestamp).toLocaleString()}</span>
              </div>
              <div class="title">{event.title}</div>
              {#if describeMissionFeedEvent(event)}
                <div class="event-summary">{describeMissionFeedEvent(event)}</div>
              {/if}
              <div class="meta">
                {#if meta.reason}<span class="badge">Reason: {meta.reason}</span>{/if}
                {#if meta.branch}<span class="badge">Branch: {meta.branch}</span>{/if}
                {#if meta.policy}<span class="badge">Policy: {meta.policy}</span>{/if}
                {#if meta.counts}<span class="badge">Counts: {meta.counts}</span>{/if}
                {#if meta.taskKind}<span class="badge">Task kind: {meta.taskKind}</span>{/if}
                {#if meta.action}<span class="badge">Action: {meta.action.replaceAll('_', ' ')}</span>{/if}
                {#if meta.reviewDue}<span class="badge">Review due: {meta.reviewDue}</span>{/if}
                {#if meta.snoozedUntil}<span class="badge">Snoozed until: {meta.snoozedUntil}</span>{/if}
              </div>
              {#if meta.raw}
                <details class="raw-detail">
                  <summary>Raw event detail</summary>
                  <div class="event-detail">{meta.raw}</div>
                </details>
              {/if}
            </li>
          {/each}
        </ul>
      {/if}
    </section>
    <section class="timeline">
      <h2>Related Activity</h2>
      {#if entityActivity.length === 0}
        <p class="empty">No additional entity activity in this window.</p>
      {:else}
        <ul class="events">
          {#each entityActivity.slice(0, 20) as event}
            <li>
              <div class="row">
                <span class="badge">{kindLabel(event.kind)}</span>
                <span class="time">{new Date(event.timestamp).toLocaleString()}</span>
              </div>
              <div class="title">{event.title}</div>
              {#if describeMissionFeedEvent(event)}
                <div class="event-summary">{describeMissionFeedEvent(event)}</div>
              {/if}
              {#if event.detail}<div class="event-detail">{event.detail}</div>{/if}
            </li>
          {/each}
        </ul>
      {/if}
    </section>
  {:else if error}
    <p>{error}</p>
  {:else}
    <p>Loading…</p>
  {/if}
</div>

<style>
  .mission-engine {
    border: 1px solid var(--border, #2a2a2a);
    border-radius: 12px;
    padding: 16px;
    margin-bottom: 16px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .mission-engine h2 {
    margin: 0;
    font-size: 1rem;
  }
  .engine-sub {
    margin: 0;
    font-size: 0.85rem;
    opacity: 0.8;
  }
  .engine-facts {
    display: flex;
    flex-wrap: wrap;
    gap: 20px;
  }
  .engine-facts div {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .engine-label {
    font-size: 0.75rem;
    opacity: 0.7;
  }
  .engine-path,
  .engine-summary {
    margin: 0;
    font-size: 0.8125rem;
    opacity: 0.85;
    overflow-wrap: anywhere;
  }
  .engine-form {
    display: flex;
    flex-wrap: wrap;
    gap: 12px;
  }
  .engine-form label {
    display: flex;
    flex-direction: column;
    gap: 4px;
    flex: 1 1 180px;
  }
  .engine-form input {
    min-height: 34px;
    padding: 0 8px;
    border-radius: 8px;
    border: 1px solid var(--border, #2a2a2a);
    background: transparent;
    color: inherit;
  }
  .engine-actions {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
  }

  .mission-page { max-width: 920px; margin: 0 auto; padding: 16px; }
  .back-link { display: inline-block; margin-bottom: 10px; }
  .mission-header { border: 1px solid var(--border); border-radius: 10px; padding: 14px; }
  .mission-header h1 { margin: 0 0 8px; font-size: 1.1rem; }
  .chips { display: flex; gap: 8px; flex-wrap: wrap; margin-bottom: 8px; }
  .chips span { border: 1px solid var(--border); border-radius: 999px; padding: 2px 8px; font-size: 12px; }
  .stats { margin-top: 12px; display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 8px; }
  .stats div { border: 1px solid var(--border); border-radius: 8px; padding: 10px; font-size: 13px; }
  .tasks { margin-top: 14px; border: 1px solid var(--border); border-radius: 10px; padding: 12px; }
  .next-task { margin-top: 14px; border: 1px solid var(--border); border-radius: 10px; padding: 12px; }
  .actions { display: grid; grid-template-columns: 1fr; gap: 8px; margin-top: 8px; }
  .actions button { min-height: 44px; border: 1px solid var(--border); border-radius: 8px; background: var(--bg-card); color: var(--text-primary); }
  .quick-link { min-height: 44px; border: 1px solid var(--border); border-radius: 8px; background: var(--bg-card); color: var(--text-primary); text-decoration: none; display: inline-flex; align-items: center; justify-content: center; padding: 8px 10px; font-size: 12px; font-weight: 700; }
  .quick-link:hover { border-color: var(--accent-purple); }
  .cancel-link { cursor: pointer; color: var(--accent-red, #c96a4b); border-color: color-mix(in srgb, var(--accent-red, #c96a4b) 40%, var(--border)); }
  .cancel-link:disabled { opacity: 0.5; cursor: default; }
  .mission-cancel-error { color: var(--accent-red, #c96a4b); font-size: 12px; margin: 6px 0 0; }
  .mission-final-report { margin: 12px 0; padding: 12px; border: 1px solid var(--border); border-radius: 8px; background: var(--bg-card); }
  .mission-final-report h2 { margin: 0 0 8px; font-size: 14px; }
  .mission-final-report-body { margin: 0; white-space: pre-wrap; word-break: break-word; font-size: 12px; line-height: 1.5; color: var(--text-secondary); max-height: 360px; overflow: auto; }
  @media (min-width: 720px) { .actions { grid-template-columns: repeat(2, minmax(0, 1fr)); } }
  .next-task-card { border: 1px solid var(--border); border-radius: 8px; padding: 8px; background: rgba(242,237,228,0.02); }
  .empty { color: var(--text-muted); font-size: 13px; margin: 0; }
  .empty.emphasis { color: var(--text-primary); }
  .tasks ul { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 8px; }
  .tasks li { border: 1px solid var(--border); border-radius: 8px; padding: 8px; }
  .timeline { margin-top: 14px; border: 1px solid var(--border); border-radius: 10px; padding: 12px; }
  .events { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 8px; }
  .events li { border: 1px solid var(--border); border-radius: 8px; padding: 8px; }
  .time { font-size: 11px; color: var(--text-muted); }
  .event-summary { margin-top: 6px; font-size: 13px; color: var(--text-primary); line-height: 1.45; }
  .event-detail { margin-top: 4px; font-size: 12px; color: var(--text-secondary); white-space: pre-wrap; }
  .raw-detail { margin-top: 6px; }
  .raw-detail summary { font-size: 11px; color: var(--text-muted); cursor: pointer; }
  .row { display: flex; justify-content: space-between; gap: 8px; }
  .title { font-size: 13px; }
  .badge { font-size: 11px; border: 1px solid var(--border); border-radius: 999px; padding: 1px 8px; }
  .meta { display: flex; gap: 8px; margin-top: 6px; font-size: 11px; color: var(--text-muted); flex-wrap: wrap; }
  .badge.execution-active { border-color: rgba(122, 129, 99, 0.45); color: #7a8163; }
  .badge.execution-queued { border-color: rgba(201, 138, 75, 0.45); color: #eed9ae; }
  .badge.execution-failed { border-color: rgba(201, 106, 75, 0.45); color: #c96a4b; }
  .badge.execution-recently-active { border-color: rgba(224, 165, 94, 0.45); color: #eed9ae; }
  .badge.execution-idle { border-color: var(--border); color: var(--text-muted); }
</style>
