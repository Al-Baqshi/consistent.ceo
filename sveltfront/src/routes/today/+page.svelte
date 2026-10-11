<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import {
    getDailyReview,
    completeTask,
    snoozeTask,
    listPendingWorkspaceQuestions,
    setEntityWorkspace,
    type DailyReview,
    type PendingWorkspaceQuestion,
    type TaskLite,
    type StreakSummary
  } from '$lib/api';
  import Seo from '$lib/components/Seo.svelte';

  const EMPTY_STREAK: StreakSummary = {
    currentStreakDays: 0,
    completedToday: 0,
    completedLast7: 0,
    completedLast30: 0
  };

  let review = $state<DailyReview | null>(null);
  /** Entities with queued work and nowhere anybody chose for it to happen. */
  let workspaceQuestions = $state<PendingWorkspaceQuestion[]>([]);
  let loading = $state(false);
  let loadedOnce = $state(false);
  let error = $state<string | null>(null);
  let actionError = $state<string | null>(null);
  /** Task ids with an in-flight mutation, so the row can't be double-fired. */
  let pending = $state<Record<string, boolean>>({});
  let timer: ReturnType<typeof setInterval> | undefined;

  // Every list is read defensively: a gateway that omits one array (older build,
  // partial failure) must still render the rest of the day.
  const streak = $derived(review?.streak ?? EMPTY_STREAK);
  const focus = $derived(review?.suggestedFocus ?? []);
  const overdue = $derived(review?.overdue ?? []);
  const dueToday = $derived(review?.dueToday ?? []);
  const approvals = $derived(review?.pendingApprovals ?? []);
  const missions = $derived(review?.missionsAwaitingInput ?? []);
  const highlights = $derived(review?.heartbeatHighlights ?? []);
  const upcoming = $derived(review?.upcomingHeartbeats ?? []);

  const openCount = $derived(
    overdue.length + dueToday.length + approvals.length + missions.length + workspaceQuestions.length
  );
  const allClear = $derived(loadedOnce && !error && openCount === 0 && focus.length === 0);

  const today = $derived(
    new Date().toLocaleDateString(undefined, {
      weekday: 'long',
      month: 'long',
      day: 'numeric'
    })
  );

  async function load(opts: { silent?: boolean } = {}) {
    if (!opts.silent) loading = true;
    try {
      review = await getDailyReview();
      error = null;
      loadedOnce = true;
      // A gateway too old to know this endpoint must not blank the day.
      try {
        workspaceQuestions = await listPendingWorkspaceQuestions();
      } catch {
        workspaceQuestions = [];
      }
    } catch (err) {
      // A background poll failing should not blank a screen that already works.
      if (!loadedOnce) {
        error = err instanceof Error ? err.message : 'Could not load your day.';
      }
    } finally {
      loading = false;
    }
  }

  onMount(() => {
    void load();
    timer = setInterval(() => void load({ silent: true }), 30000);
  });
  onDestroy(() => {
    if (timer) clearInterval(timer);
  });

  function setPending(id: string, on: boolean) {
    const next = { ...pending };
    if (on) next[id] = true;
    else delete next[id];
    pending = next;
  }

  /** Drop a task from every list it can appear in (focus overlaps the others). */
  function withoutTask(current: DailyReview, id: string): DailyReview {
    return {
      ...current,
      overdue: (current.overdue ?? []).filter((t) => t.id !== id),
      dueToday: (current.dueToday ?? []).filter((t) => t.id !== id),
      suggestedFocus: (current.suggestedFocus ?? []).filter((t) => t.id !== id)
    };
  }

  function describeError(err: unknown, fallback: string): string {
    const msg = err instanceof Error ? err.message : '';
    return msg ? `${fallback} — ${msg}` : fallback;
  }

  async function markDone(task: TaskLite) {
    if (!review || pending[task.id]) return;
    const snapshot = review;
    const s = snapshot.streak ?? EMPTY_STREAK;
    actionError = null;
    setPending(task.id, true);
    // Optimistic: the row leaves and the streak moves in the same frame. The
    // refresh below replaces both with the server's truth.
    review = {
      ...withoutTask(snapshot, task.id),
      streak: {
        currentStreakDays: Math.max(s.currentStreakDays ?? 0, 1),
        completedToday: (s.completedToday ?? 0) + 1,
        completedLast7: (s.completedLast7 ?? 0) + 1,
        completedLast30: (s.completedLast30 ?? 0) + 1
      }
    };
    try {
      await completeTask(task.id);
      await load({ silent: true });
    } catch (err) {
      review = snapshot;
      actionError = describeError(err, `Could not complete “${task.title}”`);
    } finally {
      setPending(task.id, false);
    }
  }

  async function snooze(task: TaskLite) {
    if (!review || pending[task.id]) return;
    const snapshot = review;
    actionError = null;
    setPending(task.id, true);
    review = withoutTask(snapshot, task.id);
    try {
      await snoozeTask(task.id, 24);
      await load({ silent: true });
    } catch (err) {
      review = snapshot;
      actionError = describeError(err, `Could not snooze “${task.title}”`);
    } finally {
      setPending(task.id, false);
    }
  }

  /** "2 days overdue", never "51h". Colour is never the only overdue signal. */
  function overdueLabel(hours?: number | null): string {
    const h = Math.max(0, Math.floor(hours ?? 0));
    if (h < 1) return 'Just missed';
    if (h < 24) return h === 1 ? '1 hour overdue' : `${h} hours overdue`;
    const days = Math.floor(h / 24);
    if (days < 14) return days === 1 ? '1 day overdue' : `${days} days overdue`;
    const weeks = Math.floor(days / 7);
    return `${weeks} weeks overdue`;
  }

  function formatDue(iso?: string | null): string {
    if (!iso) return 'No deadline';
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    const sameDay = d.toDateString() === new Date().toDateString();
    return sameDay
      ? `Due ${d.toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' })}`
      : `Due ${d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' })}`;
  }

  function relativeTime(iso?: string | null): string {
    if (!iso) return '';
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    const mins = Math.round((Date.now() - d.getTime()) / 60000);
    if (mins < 1) return 'just now';
    if (mins < 60) return `${mins}m ago`;
    const hours = Math.round(mins / 60);
    if (hours < 24) return `${hours}h ago`;
    const days = Math.round(hours / 24);
    return days === 1 ? 'yesterday' : `${days}d ago`;
  }

  /** Answer "notes only" straight from here — it is the one choice needing no path. */
  async function keepInNotes(row: PendingWorkspaceQuestion) {
    if (pending[row.entityId]) return;
    actionError = null;
    setPending(row.entityId, true);
    const snapshot = workspaceQuestions;
    workspaceQuestions = snapshot.filter((q) => q.entityId !== row.entityId);
    try {
      await setEntityWorkspace(row.entityId, { choice: 'vault_only' });
      await load({ silent: true });
    } catch (err) {
      workspaceQuestions = snapshot;
      actionError = describeError(err, `Could not save that for ${row.entityName}`);
    } finally {
      setPending(row.entityId, false);
    }
  }

  function streakHeadline(s: StreakSummary): string {
    const days = s.currentStreakDays ?? 0;
    if (days <= 0) return 'Start your streak today';
    return days === 1 ? '1 day streak' : `${days} day streak`;
  }

  function streakSupport(s: StreakSummary): string {
    const days = s.currentStreakDays ?? 0;
    const done = s.completedToday ?? 0;
    if (days <= 0 && done <= 0) return 'Finish one thing today and day one is on the board.';
    if (done > 0) {
      return done === 1
        ? 'One finished today. The streak is safe.'
        : `${done} finished today. The streak is safe.`;
    }
    return 'Nothing finished yet today — one task keeps it alive.';
  }
</script>

<Seo title="Today" noindex />

{#snippet taskRow(task: TaskLite, variant: 'focus' | 'overdue' | 'due', rank?: number)}
  <li class="task-row" class:is-overdue={variant === 'overdue'} class:is-focus={variant === 'focus'}>
    {#if rank != null}
      <span class="focus-rank" aria-hidden="true">{rank}</span>
    {/if}
    <div class="task-body">
      <div class="task-main">
        <span class="task-title">{task.title}</span>
        <div class="task-meta">
          {#if variant === 'overdue' || (task.overdueHours ?? 0) > 0}
            <span class="tag tag-overdue">
              <span class="tag-glyph" aria-hidden="true">!</span>
              {overdueLabel(task.overdueHours)}
            </span>
          {:else if task.dueAt}
            <span class="tag">{formatDue(task.dueAt)}</span>
          {/if}
          {#if task.priority}
            <span class="tag tag-quiet">{task.priority}</span>
          {/if}
          {#if task.entityId}
            <a class="task-link" href={`/entities/${task.entityId}`}
              >{task.entityName ?? task.entityId.slice(0, 8)}</a
            >
          {/if}
          {#if task.missionId}
            <a class="task-link" href={`/missions/${task.missionId}`}>mission</a>
          {/if}
        </div>
      </div>
      <div class="task-actions">
        <button
          type="button"
          class="btn-done"
          disabled={Boolean(pending[task.id])}
          aria-label={`Mark “${task.title}” as done`}
          onclick={() => void markDone(task)}>Done</button
        >
        <button
          type="button"
          class="btn-snooze"
          disabled={Boolean(pending[task.id])}
          aria-label={`Snooze “${task.title}” for 24 hours`}
          onclick={() => void snooze(task)}>Snooze 24h</button
        >
      </div>
    </div>
  </li>
{/snippet}

<div class="today-page">
  <header class="today-head">
    <div class="today-head-text">
      <h1 class="today-title">Today</h1>
      <p class="today-date">{today}</p>
    </div>
    <button
      type="button"
      class="today-refresh"
      onclick={() => void load()}
      disabled={loading}
      aria-label="Refresh today's review">{loading ? 'Refreshing…' : 'Refresh'}</button
    >
  </header>

  {#if actionError}
    <p class="today-action-error" role="alert">{actionError}</p>
  {/if}

  {#if error}
    <div class="today-error" role="status">
      <p class="today-error-title">Could not load your day.</p>
      <p class="today-error-detail">{error}</p>
      <button type="button" class="today-retry" onclick={() => void load()}>Try again</button>
    </div>
  {:else if !loadedOnce && loading}
    <p class="today-loading" role="status">Pulling together your day…</p>
  {:else if loadedOnce}
    <!-- ── Streak ── -->
    <section class="streak" aria-label="Consistency streak">
      <div class="streak-hero">
        <span class="streak-number" class:dim={(streak.currentStreakDays ?? 0) === 0}
          >{streak.currentStreakDays ?? 0}</span
        >
        <div class="streak-hero-text">
          <h2 class="streak-headline">{streakHeadline(streak)}</h2>
          <p class="streak-support">{streakSupport(streak)}</p>
        </div>
      </div>
      <dl class="streak-stats">
        <div class="streak-stat">
          <dt>Done today</dt>
          <dd>{streak.completedToday ?? 0}</dd>
        </div>
        <div class="streak-stat">
          <dt>Last 7 days</dt>
          <dd>{streak.completedLast7 ?? 0}</dd>
        </div>
        <div class="streak-stat">
          <dt>Last 30 days</dt>
          <dd>{streak.completedLast30 ?? 0}</dd>
        </div>
      </dl>
    </section>

    <!-- ── Workspace decisions ── -->
    {#if workspaceQuestions.length > 0}
      <section class="block block-overdue" aria-labelledby="today-workspace">
        <div class="block-head">
          <h2 class="block-title" id="today-workspace">Where should these live?</h2>
          <span class="block-count count-warning">{workspaceQuestions.length}</span>
        </div>
        <ul class="simple-list" role="list">
          {#each workspaceQuestions as row (row.entityId)}
            <li class="simple-row">
              <div class="simple-main">
                <span class="simple-title">{row.question}</span>
                <span class="simple-meta">
                  {row.entityName} · work is queued and waiting on your answer
                </span>
              </div>
              <div class="workspace-row-actions">
                <button
                  type="button"
                  class="btn-snooze"
                  disabled={Boolean(pending[row.entityId])}
                  aria-label={`Keep ${row.entityName} in notes only`}
                  onclick={() => void keepInNotes(row)}>Notes only</button
                >
                <a class="row-link" href={`/entities/${row.entityId}`}>Choose a folder</a>
              </div>
            </li>
          {/each}
        </ul>
      </section>
    {/if}

    {#if allClear}
      <section class="clear-day" aria-label="Nothing outstanding">
        <h2 class="clear-title">You're clear.</h2>
        <p class="clear-copy">
          Nothing overdue, nothing due, no decisions waiting. This is what a run day looks like.
        </p>
        <div class="clear-actions">
          <a class="clear-link" href="/">Set the next goal in chat</a>
          <a class="clear-link" href="/missions">Review missions</a>
        </div>
      </section>
    {/if}

    <!-- ── Suggested focus ── -->
    {#if focus.length > 0}
      <section class="block block-focus" aria-labelledby="today-focus">
        <div class="block-head">
          <h2 class="block-title" id="today-focus">If you do nothing else</h2>
          <span class="block-count">{focus.length}</span>
        </div>
        <ul class="task-list" role="list">
          {#each focus as task, i (task.id)}
            {@render taskRow(task, 'focus', i + 1)}
          {/each}
        </ul>
      </section>
    {/if}

    <!-- ── Overdue ── -->
    {#if overdue.length > 0}
      <section class="block block-overdue" aria-labelledby="today-overdue">
        <div class="block-head">
          <h2 class="block-title" id="today-overdue">Overdue</h2>
          <span class="block-count count-warning">{overdue.length}</span>
        </div>
        <ul class="task-list" role="list">
          {#each overdue as task (task.id)}
            {@render taskRow(task, 'overdue')}
          {/each}
        </ul>
      </section>
    {/if}

    <!-- ── Due today ── -->
    {#if dueToday.length > 0}
      <section class="block" aria-labelledby="today-due">
        <div class="block-head">
          <h2 class="block-title" id="today-due">Due today</h2>
          <span class="block-count">{dueToday.length}</span>
        </div>
        <ul class="task-list" role="list">
          {#each dueToday as task (task.id)}
            {@render taskRow(task, 'due')}
          {/each}
        </ul>
      </section>
    {/if}

    <!-- ── Approvals ── -->
    {#if approvals.length > 0}
      <section class="block" aria-labelledby="today-approvals">
        <div class="block-head">
          <h2 class="block-title" id="today-approvals">Waiting on your decision</h2>
          <span class="block-count count-warning">{approvals.length}</span>
        </div>
        <ul class="simple-list" role="list">
          {#each approvals as approval (approval.id)}
            <li class="simple-row">
              <div class="simple-main">
                <span class="simple-title">{approval.title}</span>
                <span class="simple-meta">
                  entity {approval.entityId?.slice(0, 8) ?? '—'} · {relativeTime(approval.createdAt)}
                </span>
              </div>
              <a class="row-link" href="/tasks?filter=decision-required">Review</a>
            </li>
          {/each}
        </ul>
      </section>
    {/if}

    <!-- ── Missions awaiting input ── -->
    {#if missions.length > 0}
      <section class="block" aria-labelledby="today-missions">
        <div class="block-head">
          <h2 class="block-title" id="today-missions">Missions awaiting your input</h2>
          <span class="block-count">{missions.length}</span>
        </div>
        <ul class="simple-list" role="list">
          {#each missions as mission (mission.id)}
            <li class="simple-row">
              <div class="simple-main">
                <span class="simple-title">{mission.title}</span>
                <span class="simple-meta">entity {mission.entityId?.slice(0, 8) ?? '—'}</span>
              </div>
              <a class="row-link" href={`/missions/${mission.id}`}>Open</a>
            </li>
          {/each}
        </ul>
      </section>
    {/if}

    <!-- ── Heartbeat highlights ── -->
    {#if upcoming.length > 0}
      <section class="block block-quiet" aria-labelledby="today-upcoming">
        <div class="block-head">
          <h2 class="block-title" id="today-upcoming">Upcoming beats</h2>
          <span class="block-count">{upcoming.length}</span>
        </div>
        <ul class="simple-list" role="list">
          {#each upcoming as beat (beat.id)}
            <li class="simple-row">
              <div class="simple-main">
                <span class="simple-title">{beat.name}</span>
                <span class="simple-meta">{beat.targetName ?? 'Workspace'} · {formatDue(beat.nextFireAt)}</span>
              </div>
              {#if beat.targetId}<a class="row-link" href={`/entities/${beat.targetId}`}>Open</a>{/if}
            </li>
          {/each}
        </ul>
      </section>
    {/if}

    {#if highlights.length > 0}
      <section class="block block-quiet" aria-labelledby="today-highlights">
        <div class="block-head">
          <h2 class="block-title" id="today-highlights">What your entities noticed</h2>
        </div>
        <ul class="highlight-list" role="list">
          {#each highlights as h, i (`${h.entityId ?? 'entity'}-${h.firedAt}-${i}`)}
            <li class="highlight-row">
              <div class="highlight-top">
                {#if h.entityId}
                  <a class="highlight-entity" href={`/entities/${h.entityId}`}
                    >{h.entityName ?? h.entityId.slice(0, 8)}</a
                  >
                {:else}
                  <span class="highlight-entity">{h.entityName ?? 'Workspace'}</span>
                {/if}
                <span class="highlight-time">{relativeTime(h.firedAt)}</span>
              </div>
              <p class="highlight-summary">{h.summary}</p>
            </li>
          {/each}
        </ul>
      </section>
    {/if}
  {/if}
</div>

<style>
  .today-page {
    padding: 16px 14px 40px;
    max-width: 900px;
    margin: 0 auto;
    display: flex;
    flex-direction: column;
    gap: 16px;
  }

  /* ── Header ── */
  .today-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
  }
  .today-title {
    margin: 0;
    font-size: 22px;
    font-weight: 800;
    letter-spacing: -0.01em;
    color: var(--text-primary);
  }
  .today-date {
    margin: 2px 0 0;
    font-size: 12px;
    color: var(--text-muted);
  }
  .today-refresh {
    min-height: 30px;
    padding: 0 12px;
    border: 1px solid var(--border);
    border-radius: 999px;
    background: transparent;
    color: var(--text-secondary);
    font-size: 12px;
    font-weight: 700;
    cursor: pointer;
    flex-shrink: 0;
  }
  .today-refresh:disabled {
    opacity: 0.5;
    cursor: default;
  }

  /* ── Load / error states ── */
  .today-loading {
    margin: 0;
    padding: 24px 0;
    font-size: 13px;
    color: var(--text-muted);
  }
  .today-error {
    border: 1px solid color-mix(in srgb, var(--accent-red) 40%, var(--border));
    background: var(--color-red-dim);
    border-radius: 12px;
    padding: 14px;
  }
  .today-error-title {
    margin: 0;
    font-size: 13px;
    font-weight: 700;
    color: var(--text-primary);
  }
  .today-error-detail {
    margin: 4px 0 10px;
    font-size: 12px;
    color: var(--text-secondary);
    overflow-wrap: anywhere;
  }
  .today-retry {
    min-height: 30px;
    padding: 0 12px;
    border: 1px solid var(--border);
    border-radius: 999px;
    background: transparent;
    color: var(--text-primary);
    font-size: 12px;
    font-weight: 700;
    cursor: pointer;
  }
  .today-action-error {
    margin: 0;
    padding: 10px 12px;
    border: 1px solid color-mix(in srgb, var(--accent-red) 40%, var(--border));
    background: var(--color-red-dim);
    border-radius: 10px;
    font-size: 12px;
    color: var(--text-primary);
    overflow-wrap: anywhere;
  }

  /* ── Streak ── */
  .streak {
    border: 1px solid color-mix(in srgb, var(--accent-cyan) 30%, var(--border));
    border-radius: 16px;
    background: var(--panel-gradient);
    box-shadow: var(--shadow-md);
    padding: 18px;
    display: flex;
    flex-direction: column;
    gap: 16px;
  }
  .streak-hero {
    display: flex;
    align-items: center;
    gap: 16px;
  }
  .streak-number {
    font-size: 54px;
    line-height: 1;
    font-weight: 800;
    letter-spacing: -0.03em;
    background: var(--gradient-text);
    background-clip: text;
    -webkit-background-clip: text;
    -webkit-text-fill-color: transparent;
    flex-shrink: 0;
    font-variant-numeric: tabular-nums;
  }
  .streak-number.dim {
    background: none;
    -webkit-text-fill-color: currentColor;
    color: var(--text-muted);
    opacity: 0.55;
  }
  .streak-hero-text {
    min-width: 0;
  }
  .streak-headline {
    margin: 0;
    font-size: 17px;
    font-weight: 700;
    color: var(--text-primary);
  }
  .streak-support {
    margin: 3px 0 0;
    font-size: 12px;
    color: var(--text-secondary);
  }
  .streak-stats {
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    gap: 8px;
    margin: 0;
    padding-top: 14px;
    border-top: 1px solid var(--border-subtle);
  }
  .streak-stat {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .streak-stat dt {
    font-size: 10px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-muted);
  }
  .streak-stat dd {
    margin: 0;
    font-size: 18px;
    font-weight: 700;
    color: var(--text-primary);
    font-variant-numeric: tabular-nums;
  }

  /* ── Clear day ── */
  .clear-day {
    border: 1px solid color-mix(in srgb, var(--accent-green) 38%, var(--border));
    background: var(--color-green-dim);
    border-radius: 14px;
    padding: 20px;
    text-align: center;
  }
  .clear-title {
    margin: 0;
    font-size: 16px;
    font-weight: 800;
    color: var(--text-primary);
  }
  .clear-copy {
    margin: 6px 0 0;
    font-size: 12px;
    color: var(--text-secondary);
  }
  .clear-actions {
    margin-top: 12px;
    display: flex;
    justify-content: center;
    flex-wrap: wrap;
    gap: 8px;
  }
  .clear-link {
    font-size: 12px;
    font-weight: 700;
    color: var(--accent-cyan);
    text-decoration: none;
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 6px 12px;
  }
  .clear-link:hover {
    background: var(--surface-hover);
  }

  /* ── Section blocks ── */
  .block {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .block-head {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .block-title {
    margin: 0;
    font-size: 11px;
    font-weight: 800;
    text-transform: uppercase;
    letter-spacing: 0.07em;
    color: var(--text-secondary);
  }
  .block-count {
    font-size: 10px;
    font-weight: 700;
    color: var(--text-muted);
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 1px 7px;
    font-variant-numeric: tabular-nums;
  }
  .count-warning {
    color: var(--accent-red);
    border-color: color-mix(in srgb, var(--accent-red) 45%, var(--border));
  }
  .block-focus .block-title {
    font-size: 12px;
    color: var(--text-primary);
  }

  .task-list,
  .simple-list,
  .highlight-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  /* ── Task rows ── */
  .focus-rank {
    flex-shrink: 0;
    width: 20px;
    height: 20px;
    display: flex;
    align-items: center;
    justify-content: center;
    margin-top: 1px;
    border-radius: 999px;
    border: 1px solid color-mix(in srgb, var(--accent-cyan) 45%, var(--border));
    font-size: 11px;
    font-weight: 800;
    color: var(--accent-cyan);
    font-variant-numeric: tabular-nums;
  }
  .task-row {
    min-width: 0;
    display: flex;
    align-items: flex-start;
    gap: 10px;
    padding: 10px 12px;
    border: 1px solid var(--border-subtle);
    border-radius: 10px;
    background: color-mix(in srgb, var(--bg-card) 84%, transparent);
  }
  .task-row.is-focus {
    border-color: color-mix(in srgb, var(--accent-cyan) 32%, var(--border));
    background: var(--bg-card);
  }
  .task-row.is-overdue {
    border-color: color-mix(in srgb, var(--accent-red) 42%, var(--border));
    background: color-mix(in srgb, var(--color-red-dim) 60%, var(--bg-card));
  }
  .task-body {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  @media (min-width: 640px) {
    .task-body {
      flex-direction: row;
      align-items: center;
      justify-content: space-between;
      gap: 12px;
    }
  }
  .task-main {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .task-title {
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
    overflow-wrap: anywhere;
  }
  .task-meta {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 6px;
  }
  .tag {
    font-size: 10px;
    font-weight: 700;
    color: var(--text-muted);
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 1px 7px;
  }
  .tag-quiet {
    text-transform: lowercase;
  }
  .tag-overdue {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    color: var(--accent-red);
    border-color: color-mix(in srgb, var(--accent-red) 45%, var(--border));
  }
  .tag-glyph {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 12px;
    height: 12px;
    border-radius: 999px;
    border: 1px solid currentColor;
    font-size: 8px;
    line-height: 1;
    font-weight: 800;
  }
  .task-link,
  .workspace-row-actions {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }

  .row-link {
    font-size: 10px;
    font-weight: 700;
    color: var(--accent-cyan);
    text-decoration: none;
    border-bottom: 1px solid transparent;
  }
  .task-link:hover,
  .row-link:hover {
    border-bottom-color: currentColor;
  }
  .task-actions {
    display: flex;
    gap: 6px;
    flex-shrink: 0;
  }
  .btn-done,
  .btn-snooze {
    min-height: 32px;
    padding: 0 12px;
    border-radius: 999px;
    font-size: 12px;
    font-weight: 700;
    cursor: pointer;
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
    white-space: nowrap;
  }
  .btn-done {
    border-color: color-mix(in srgb, var(--accent-green) 55%, var(--border));
    background: var(--color-green-dim);
    color: var(--text-primary);
  }
  .btn-done:hover:not(:disabled) {
    background: color-mix(in srgb, var(--accent-green) 30%, transparent);
  }
  .btn-snooze:hover:not(:disabled) {
    color: var(--text-primary);
    background: var(--surface-hover);
  }
  .btn-done:disabled,
  .btn-snooze:disabled {
    opacity: 0.45;
    cursor: default;
  }

  /* ── Simple rows (approvals, missions) ── */
  .simple-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 10px 12px;
    border: 1px solid var(--border-subtle);
    border-radius: 10px;
    background: color-mix(in srgb, var(--bg-card) 84%, transparent);
  }
  .simple-main {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  .simple-title {
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
    overflow-wrap: anywhere;
  }
  .simple-meta {
    font-size: 11px;
    color: var(--text-muted);
  }

  /* ── Heartbeat highlights ── */
  .block-quiet {
    margin-top: 4px;
    padding-top: 12px;
    border-top: 1px solid var(--border-subtle);
    opacity: 0.9;
  }
  .highlight-row {
    display: flex;
    flex-direction: column;
    gap: 3px;
    padding: 8px 0;
  }
  .highlight-top {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .highlight-entity {
    font-size: 10px;
    font-weight: 800;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-secondary);
    text-decoration: none;
  }
  .highlight-time {
    font-size: 10px;
    color: var(--text-muted);
    margin-left: auto;
  }
  .highlight-summary {
    margin: 0;
    font-size: 12px;
    color: var(--text-muted);
    line-height: 1.45;
    overflow-wrap: anywhere;
  }
</style>
