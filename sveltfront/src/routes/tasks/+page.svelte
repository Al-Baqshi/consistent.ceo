<script lang="ts">
  import { onMount } from 'svelte';
  import { page } from '$app/stores';
  import { goto } from '$app/navigation';
  import { api, fetchAllPages, type PaginatedResponse, listApprovals, patchApproval } from '$lib/api';
  import TaskKanban from '$lib/components/TaskKanban.svelte';
  import { feedKindLabel, feedKindTone, type FeedTone } from '$lib/feedKinds';
  import Seo from '$lib/components/Seo.svelte';

  type TaskStatus = 'backlog' | 'queued' | 'in-progress' | 'failed' | 'completed' | 'cancelled';

  type Task = {
    id: string;
    title: string;
    dept: string;
    roles: string[];
    status: TaskStatus;
    tries: string;
    progress: number;
    priority?: string;
    entity_id?: string;
    entity_name?: string;
    mission_id?: string;
    requires_human_approval?: boolean;
    result?: string;
    task_kind?: string;
    assigned_director_id?: string;
    assigned_agent_id?: string;
    outcome?: string;
    created_at?: string;
  };

  type TaskKindRecommendation = {
    taskKind: string;
    directorId?: string;
    provider?: string;
    model?: string;
    mode?: string;
    generationTier?: string;
    reviewTier?: string;
    fallbackTiers?: string[];
  };

  type ActionEvent = {
    id: string;
    timestamp: string;
    capability: string;
    executionLevel: string;
    outcome: string;
    reason?: string;
    target?: string;
  };

  type OperatorFeedEvent = {
    id: string;
    timestamp: string;
    kind: string;
    taskId?: string;
    missionId?: string;
    title: string;
    detail?: string;
    approvalId?: string;
  };

  type ApprovalRow = {
    id: string;
    createdAt: string;
    entityId: string;
    action: string;
    status: 'pending' | 'approved' | 'rejected' | 'expired';
    taskId?: string;
    detail?: string;
  };

  type EntityRow = { id: string; name: string };

  function formatTimestamp(value?: string): string {
    if (!value) return '—';
    const d = new Date(value);
    if (Number.isNaN(d.getTime())) return value;
    return d.toLocaleString();
  }

  let tasks = $state<Task[]>([]);
  let entityNames = $state<Record<string, string>>({});
  let pendingApprovals = $state<ApprovalRow[]>([]);
  let approvalsLoading = $state(false);
  let operatorFeed = $state<OperatorFeedEvent[]>([]);
  let operatorFeedLoading = $state(false);
  let selectedTask = $state<Task | null>(null);
  let recommendations = $state<TaskKindRecommendation[]>([]);
  let selectedTaskActions = $state<ActionEvent[]>([]);
  let selectedTaskActionsLoading = $state(false);

  const filter = $derived($page.url.searchParams.get('filter'));
  const requestedTaskId = $derived($page.url.searchParams.get('task_id'));
  const entityFilterId = $derived($page.url.searchParams.get('entity_id'));
  const missionFilterId = $derived($page.url.searchParams.get('mission_id'));
  let handledRequestedTaskId = $state<string | null>(null);
  const filteredTasks = $derived(
    tasks.filter((t) => {
      if (filter === 'decision-required') {
        const isDecisionRequired = t.requires_human_approval || t.outcome === 'needshumanapproval';
        if (!isDecisionRequired || t.status === 'completed' || t.status === 'failed') return false;
      }
      if (entityFilterId && t.entity_id !== entityFilterId) return false;
      if (missionFilterId && t.mission_id !== missionFilterId) return false;
      return true;
    })
  );
  const isDecisionRequiredView = $derived(filter === 'decision-required');
  const isScopedView = $derived(Boolean(entityFilterId || missionFilterId || isDecisionRequiredView));

  async function loadTasks() {
    try {
      // Every page, not the default first 50: the board is filtered client-side
      // by entity/mission, so a truncated list would hide newer work entirely.
      const [list, recs, entitiesRes] = await Promise.all([
        fetchAllPages<Task>('/api/tasks'),
        api<TaskKindRecommendation[]>('/api/routing/task-kinds').catch(() => []),
        fetchAllPages<EntityRow>('/api/entities').catch(() => [] as EntityRow[])
      ]);
      const nameMap = Object.fromEntries((entitiesRes ?? []).map((e) => [e.id, e.name]));
      entityNames = nameMap;
      tasks = (list ?? []).map((task) => ({
        ...task,
        entity_name: task.entity_id ? nameMap[task.entity_id] ?? task.entity_id : undefined
      }));
      recommendations = recs ?? [];
      if (requestedTaskId && requestedTaskId !== handledRequestedTaskId) {
        const target = tasks.find((t) => t.id === requestedTaskId);
        if (target) {
          handledRequestedTaskId = requestedTaskId;
          await openDetail(target);
        }
      }
    } catch (err) {
      console.error('Failed to load tasks', err);
    }
  }

  async function loadApprovals() {
    approvalsLoading = true;
    try {
      const list = await listApprovals({ status: 'pending' });
      pendingApprovals = (list ?? []).map((a) => ({
        id: a.id,
        createdAt: a.createdAt,
        entityId: a.entityId,
        action: a.action,
        status: a.status,
        taskId: a.taskId,
        detail: a.detail,
      }));
    } catch (err) {
      console.error('Failed to load approvals', err);
      pendingApprovals = [];
    } finally {
      approvalsLoading = false;
    }
  }

  async function resolveApproval(id: string, status: 'approved' | 'rejected') {
    try {
      await patchApproval(id, status);
      await loadApprovals();
      await loadTasks();
      await loadOperatorFeed();
    } catch (err) {
      console.error('Approval update failed', err);
    }
  }

  async function loadOperatorFeed() {
    operatorFeedLoading = true;
    try {
      const res = await api<PaginatedResponse<OperatorFeedEvent>>('/api/operator-feed?limit=24');
      operatorFeed = res.items ?? [];
    } catch (err) {
      console.error('Failed to load operator feed', err);
    } finally {
      operatorFeedLoading = false;
    }
  }

  /** Dot colour per tone; labels and tones come from `$lib/feedKinds`. */
  const FEED_DOT_CLASS: Record<FeedTone, string> = {
    alert: 'feed-dot-blocked',
    warn: 'feed-dot-approval',
    good: 'feed-dot-completed',
    info: 'feed-dot-created'
  };

  function feedKindClass(kind: string): string {
    return FEED_DOT_CLASS[feedKindTone(kind)];
  }

  function formatFeedTime(iso: string): string {
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    return d.toLocaleString(undefined, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
  }

  function onFeedRowClick(taskId?: string, missionId?: string) {
    if (taskId) {
      const t = tasks.find((x) => x.id === taskId);
      if (t) {
        openDetail(t);
        return;
      }
    }
    if (missionId) {
      goto(`/missions/${missionId}`);
    }
  }

  onMount(() => {
    loadTasks();
    loadApprovals();
    loadOperatorFeed();
    const id = setInterval(() => {
      loadOperatorFeed();
    }, 12000);
    return () => clearInterval(id);
  });

  async function afterKanbanPatch() {
    await loadTasks();
    await loadOperatorFeed();
  }

  async function requeue(taskId: string) {
    try {
      await api('/api/tasks/' + taskId + '/requeue', { method: 'POST' });
      await loadTasks();
    } catch (err) {
      console.error('Requeue failed', err);
    }
  }

  async function openDetail(task: Task) {
    selectedTask = task;
    selectedTaskActionsLoading = true;
    try {
      selectedTaskActions = await api<ActionEvent[]>(
        `/api/actions?task_id=${encodeURIComponent(task.id)}&limit=20`
      );
    } catch (err) {
      selectedTaskActions = [];
      console.error('Failed to load action events for task', err);
    } finally {
      selectedTaskActionsLoading = false;
    }
  }

  function closeDetail() {
    selectedTask = null;
    selectedTaskActions = [];
  }

  const selectedTaskRoute = $derived(
    selectedTask?.task_kind
      ? recommendations.find((item) => item.taskKind === selectedTask?.task_kind)
      : null
  );

  // ─── Helpers ─────────────────────────────────────────────────────────────────
  const STATUS_LABEL: Record<TaskStatus, string> = {
    backlog: 'backlog',
    queued: 'queued',
    'in-progress': 'in progress',
    failed: 'failed',
    completed: 'completed',
    cancelled: 'cancelled'
  };

  function outcomeLabel(outcome?: string) {
    switch ((outcome ?? '').toLowerCase()) {
      case 'needshumanapproval': return 'needs approval';
      case 'blockedbypolicy': return 'blocked by policy';
      case 'needsstrongermodel': return 'needs stronger model';
      case 'needsreview': return 'needs review';
      case 'retryablefailure': return 'retryable failure';
      case 'failed': return 'failed';
      case 'completed': return 'completed';
      default: return outcome ?? 'n/a';
    }
  }
</script>

<Seo title="Tasks" noindex />

<div class="tasks-page">
  <!-- ── Page Header ── -->
  <div class="page-header">
    <div class="header-left">
      <h1 class="header-title">Tasks</h1>
      <span class="task-count">{filteredTasks.length}{isDecisionRequiredView ? ' decision-required' : ''} tasks</span>
      {#if entityFilterId}
        <span class="task-filter-pill">{entityNames[entityFilterId] ?? `entity ${entityFilterId.slice(0, 8)}…`}</span>
      {/if}
      {#if missionFilterId}
        <span class="task-filter-pill">mission {missionFilterId.slice(0, 8)}…</span>
      {/if}
      {#if isScopedView}
        <a href="/tasks" class="filter-clear">Show all</a>
      {/if}
    </div>
  </div>

  {#if filteredTasks.length === 0}
    <section class="task-empty-guide" aria-label="Task onboarding">
      <h3>No tasks in this view</h3>
      <p>Capture from Home or create from an entity to start the task -> activity -> heartbeat loop.</p>
      <div class="task-empty-actions">
        <a class="filter-clear" href="/">Open Home capture</a>
        <a class="filter-clear" href="/entities?create=1">Create entity</a>
      </div>
    </section>
  {/if}

  <details class="tasks-extras">
    <summary>Approvals &amp; activity</summary>
  <!-- Pending approvals queue -->
  <section class="approvals-queue" aria-label="Pending approvals">
    <div class="operator-feed-head">
      <span class="operator-feed-title">Approvals</span>
      {#if approvalsLoading}
        <span class="operator-feed-loading">Loading…</span>
      {/if}
    </div>
    {#if pendingApprovals.length === 0 && !approvalsLoading}
      <p class="operator-feed-empty">No pending approval requests.</p>
    {:else}
      <ul class="approvals-list">
        {#each pendingApprovals as a (a.id)}
          <li class="approval-row">
            <div class="approval-main">
              <span class="approval-action">{a.action}</span>
              <span class="approval-meta">entity {a.entityId.slice(0, 8)}…</span>
              {#if a.detail}
                <span class="approval-detail">{a.detail}</span>
              {/if}
            </div>
            <div class="approval-actions">
              <button type="button" class="btn-secondary text-sm" onclick={() => resolveApproval(a.id, 'approved')}>Approve</button>
              <button type="button" class="btn-ghost text-sm" onclick={() => resolveApproval(a.id, 'rejected')}>Reject</button>
            </div>
          </li>
        {/each}
      </ul>
    {/if}
  </section>

  <!-- Operator activity feed -->
  <section class="operator-feed" aria-label="Task activity">
    <div class="operator-feed-head">
      <span class="operator-feed-title">Activity</span>
      {#if operatorFeedLoading}
        <span class="operator-feed-loading">Updating…</span>
      {/if}
    </div>
    <ul class="operator-feed-list" role="list">
      {#if operatorFeed.length === 0 && !operatorFeedLoading}
        <li class="operator-feed-empty-item">
          <p class="operator-feed-empty">No events yet. Create tasks or run the orchestrator to see activity.</p>
        </li>
      {:else}
        {#each operatorFeed.slice(0, 12) as ev (ev.id)}
          <li class="operator-feed-item">
            <button
              type="button"
              class="operator-feed-row"
              onclick={() => onFeedRowClick(ev.taskId, ev.missionId)}
            >
              <div class="feed-row-top">
                <span class="feed-dot {feedKindClass(ev.kind)}" aria-hidden="true"></span>
                <span class="feed-kind">{feedKindLabel(ev.kind)}</span>
                <span class="feed-time">{formatFeedTime(ev.timestamp)}</span>
              </div>
              <span class="feed-title">{ev.title}</span>
            </button>
          </li>
        {/each}
      {/if}
    </ul>
  </section>
  </details>

  <!-- ── Kanban Board (shared with Dashboard) ── -->
  <TaskKanban
    tasks={filteredTasks}
    selectedTaskId={selectedTask?.id ?? null}
    onSelectTask={openDetail}
    onAfterPatch={afterKanbanPatch}
    onRequeue={requeue}
  />

  <!-- Task detail pane (last run result, etc.) -->
  {#if selectedTask}
    <div class="detail-backdrop" onclick={closeDetail} role="button" tabindex="0" onkeydown={(e) => { if (e.key === 'Escape' || e.key === 'Enter' || e.key === ' ') closeDetail(); }}></div>
    <aside class="detail-pane" role="region" aria-label="Task details" tabindex="-1">
      <div class="detail-header">
        <h3 class="detail-title">{selectedTask.title}</h3>
        <button type="button" class="detail-close" onclick={closeDetail} aria-label="Close">×</button>
      </div>
      <div class="detail-body">
        <div class="detail-meta">
          <span class="status-badge status-{selectedTask.status}">{STATUS_LABEL[selectedTask.status]}</span>
          <span>{selectedTask.dept}</span>
          {#if selectedTask.requires_human_approval || selectedTask.outcome === 'needshumanapproval'}
            <span class="badge-approval">Decision required</span>
          {/if}
          {#if selectedTask.outcome && selectedTask.outcome !== 'completed' && selectedTask.outcome !== 'needshumanapproval'}
            <span class="badge-approval" style="background: rgba(138, 90, 46, 0.18); color: #eed9ae; border-color: rgba(138, 90, 46, 0.45);">{outcomeLabel(selectedTask.outcome)}</span>
          {/if}
          {#if selectedTask.entity_id}
            <span class="detail-entity">Entity: <code>{selectedTask.entity_name ?? selectedTask.entity_id}</code></span>
          {/if}
        </div>
        {#if selectedTask.roles?.length}
          <div class="detail-roles">
            {#each selectedTask.roles as role}<span class="role-tag">{role}</span>{/each}
          </div>
        {/if}
        <div class="detail-section">
          <h4 class="detail-section-title">Decision trail</h4>
          <div class="detail-meta">
            <span>Kind: <code>{selectedTask.task_kind ?? 'generic'}</code></span>
            <span>Created: <code>{formatTimestamp(selectedTask.created_at)}</code></span>
            <span>Outcome: <code>{outcomeLabel(selectedTask.outcome)}</code></span>
            <span>Director: <code>{selectedTask.assigned_director_id ?? 'unassigned'}</code></span>
            <span>Agent: <code>{selectedTask.assigned_agent_id ?? 'unassigned'}</code></span>
          </div>
          {#if selectedTaskRoute}
            <div class="detail-meta">
              <span>Route provider/model: <code>{selectedTaskRoute.provider ?? 'default'} / {selectedTaskRoute.model ?? 'default'}</code></span>
              <span>Generation tier: <code>{selectedTaskRoute.generationTier ?? 'default'}</code></span>
              <span>Review tier: <code>{selectedTaskRoute.reviewTier ?? 'not set'}</code></span>
              <span>Escalation: <code>{selectedTaskRoute.fallbackTiers?.length ? selectedTaskRoute.fallbackTiers.join(' -> ') : 'none configured'}</code></span>
            </div>
          {/if}
          {#if selectedTaskActionsLoading}
            <p class="detail-empty">Loading action events…</p>
          {:else if selectedTaskActions.length > 0}
            <pre class="detail-result">{selectedTaskActions.map((event) => `${new Date(event.timestamp).toLocaleString()} | ${event.outcome} | ${event.capability} | ${event.executionLevel}${event.target ? ` | target=${event.target}` : ''}${event.reason ? ` | ${event.reason}` : ''}`).join('\n')}</pre>
          {:else}
            <p class="detail-empty">No action events recorded for this task yet.</p>
          {/if}
        </div>
        <div class="detail-section">
          <h4 class="detail-section-title">Last run result</h4>
          {#if selectedTask.result}
            <pre class="detail-result">{selectedTask.result}</pre>
          {:else}
            <p class="detail-empty">No result yet. Run the task (heartbeat on) to populate.</p>
          {/if}
        </div>
      </div>
    </aside>
  {/if}
</div>

<style>
  .tasks-page {
    padding: var(--page-padding);
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .tasks-extras {
    flex-shrink: 0;
    border: 1px solid var(--border);
    border-radius: var(--radius-lg);
    background: var(--bg-secondary);
  }
  .task-empty-guide {
    border: 1px dashed var(--border);
    border-radius: var(--radius-lg);
    background: var(--bg-card);
    padding: var(--space-3);
    display: grid;
    gap: 8px;
  }
  .task-empty-guide h3 {
    margin: 0;
    font-size: 14px;
    color: var(--text-primary);
  }
  .task-empty-guide p {
    margin: 0;
    font-size: 12px;
    color: var(--text-secondary);
  }
  .task-empty-actions {
    display: grid;
    grid-template-columns: 1fr;
    gap: 8px;
  }
  .tasks-extras > summary {
    list-style: none;
    cursor: pointer;
    user-select: none;
    padding: var(--space-2) var(--space-3);
    font-size: var(--text-xs);
    font-weight: var(--font-semibold);
    color: var(--text-secondary);
    min-height: 44px;
    display: flex;
    align-items: center;
  }
  .tasks-extras > summary::-webkit-details-marker {
    display: none;
  }
  .tasks-extras[open] > summary {
    border-bottom: 1px solid var(--border);
  }

  /* ── Header ── */
  .page-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    flex-shrink: 0;
    flex-wrap: wrap;
    gap: var(--space-2);
  }

  .header-actions {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }

  .header-left {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }

  .header-title {
    margin: 0;
    font-size: 20px;
    font-weight: 700;
    color: var(--text-primary);
  }

  .task-count {
    font-size: var(--text-xs);
    color: var(--text-muted);
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: var(--radius-full);
    padding: 1px 8px;
  }

  .task-filter-pill {
    font-size: 11px;
    color: var(--text-secondary);
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 2px 8px;
    background: var(--bg-card);
  }

  .btn-primary {
    height: var(--btn-height-sm);
    padding: 0 var(--space-3);
    background: var(--accent-green);
    color: var(--color-text-inverse);
    border: none;
    border-radius: var(--radius-md);
    font-size: var(--text-sm);
    font-weight: var(--font-semibold);
    transition: opacity var(--transition-base);
  }
  .btn-primary:hover { opacity: 0.85; }

  .btn-ghost {
    height: var(--btn-height-sm);
    padding: 0 var(--space-2);
    background: transparent;
    color: var(--text-muted);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    font-size: var(--text-xs);
    cursor: pointer;
    min-width: 44px;
    min-height: 44px;
  }
  .btn-ghost:hover {
    color: var(--text-primary);
    border-color: var(--text-muted);
  }

  /* ── Approvals queue ── */
  .approvals-queue {
    flex-shrink: 0;
    background: transparent;
    border: 0;
    padding: var(--space-2) var(--space-3);
    margin-bottom: 0;
  }
  .approvals-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .approval-row {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-2);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
  }
  @media (min-width: 640px) {
    .approval-row {
      flex-direction: row;
      align-items: center;
      justify-content: space-between;
    }
  }
  .approval-main {
    display: flex;
    flex-direction: column;
    gap: 4px;
    min-width: 0;
  }
  .approval-action {
    font-weight: var(--font-semibold);
    font-size: var(--text-sm);
  }
  .approval-meta,
  .approval-detail {
    font-size: var(--text-2xs);
    color: var(--text-muted);
    word-break: break-word;
  }
  .approval-actions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
  }
  .approval-actions button {
    min-height: 44px;
    min-width: 44px;
  }

  /* ── Operator feed ── */
  .operator-feed {
    flex-shrink: 0;
    background: transparent;
    border: 0;
    border-top: 1px solid var(--border-subtle);
    border-radius: 0;
    padding: var(--space-1) var(--space-2);
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    min-height: 0;
  }

  .operator-feed-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
  }

  .operator-feed-title {
    font-size: var(--text-2xs);
    font-weight: var(--font-bold);
    letter-spacing: 0.06em;
    color: var(--text-secondary);
    text-transform: uppercase;
  }

  .operator-feed-loading {
    font-size: var(--text-2xs);
    color: var(--text-muted);
  }

  .operator-feed-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
    max-height: min(22vh, 210px);
    overflow-x: hidden;
    overflow-y: auto;
    -webkit-overflow-scrolling: touch;
    scrollbar-width: thin;
  }

  .operator-feed-empty-item {
    list-style: none;
    margin: 0;
    padding: 0;
  }

  .operator-feed-empty {
    margin: 0;
    font-size: var(--text-xs);
    color: var(--text-muted);
  }

  .operator-feed-item {
    list-style: none;
    margin: 0;
    padding: 0;
    min-width: 0;
  }

  .operator-feed-row {
    display: flex;
    flex-direction: column;
    align-items: stretch;
    gap: 2px;
    width: 100%;
    min-width: 0;
    padding: 5px 8px;
    background: var(--bg-card);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-sm);
    cursor: pointer;
    text-align: left;
    font: inherit;
    color: inherit;
    transition: border-color var(--transition-base);
  }

  .feed-row-top {
    display: flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
  }

  .operator-feed-row:hover {
    border-color: var(--accent-purple);
  }

  .feed-dot {
    width: 8px;
    height: 8px;
    border-radius: var(--radius-full);
    flex-shrink: 0;
  }

  .feed-dot-created { background: var(--accent-blue); }
  .feed-dot-completed { background: var(--accent-green); }
  .feed-dot-blocked { background: var(--accent-red); }
  .feed-dot-approval { background: var(--accent-orange); }

  .feed-kind {
    font-size: var(--text-2xs);
    font-weight: var(--font-semibold);
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--text-muted);
    flex-shrink: 0;
  }

  .feed-title {
    font-size: var(--text-2xs);
    color: var(--text-primary);
    line-height: 1.35;
    word-break: break-word;
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }

  .feed-time {
    font-size: var(--text-2xs);
    color: var(--text-muted);
    flex-shrink: 0;
    margin-left: auto;
  }

  @media (min-width: 900px) {
    .tasks-extras {
      display: grid;
      grid-template-columns: 1fr 1fr;
      gap: var(--space-2);
      padding: var(--space-2);
    }
    .tasks-extras > summary {
      grid-column: 1 / -1;
    }
    .tasks-extras .approvals-queue {
      padding: 0;
    }
    .tasks-extras .operator-feed {
      border-top: 0;
      border-left: 1px solid var(--border-subtle);
      padding: 0 0 0 var(--space-2);
    }
  }

  .filter-clear {
    font-size: var(--text-xs);
    color: var(--accent-purple);
    text-decoration: none;
  }
  .filter-clear:hover { text-decoration: underline; }

  /* ── Task detail pane ── */
  .detail-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.35);
    z-index: 40;
  }

  .detail-pane {
    position: fixed;
    top: 0;
    right: 0;
    width: min(400px, 100vw);
    height: 100vh;
    background: var(--bg-secondary);
    border-left: 1px solid var(--border);
    z-index: 41;
    display: flex;
    flex-direction: column;
    box-shadow: -4px 0 20px rgba(0, 0, 0, 0.15);
  }

  .detail-header {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: var(--space-2);
    padding: var(--space-3);
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }

  .detail-title {
    font-size: var(--text-sm);
    font-weight: var(--font-semibold);
    color: var(--text-primary);
    margin: 0;
    flex: 1;
    min-width: 0;
  }

  .detail-close {
    background: none;
    border: none;
    font-size: 1.25rem;
    color: var(--text-muted);
    cursor: pointer;
    padding: 0 4px;
    line-height: 1;
  }
  .detail-close:hover { color: var(--text-primary); }

  .detail-body {
    padding: var(--space-3);
    overflow-y: auto;
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }

  .detail-meta {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--text-xs);
    color: var(--text-secondary);
  }

  .badge-approval {
    background: var(--color-orange-dim);
    color: var(--accent-orange);
    padding: 2px 8px;
    border-radius: var(--tag-radius);
    font-size: var(--text-2xs);
    font-weight: var(--font-semibold);
  }

  .detail-entity code {
    font-size: var(--text-2xs);
    background: var(--bg-card);
    padding: 1px 6px;
    border-radius: var(--radius-sm);
  }

  .detail-roles { display: flex; flex-wrap: wrap; gap: 3px; }

  .detail-section-title {
    font-size: var(--text-xs);
    font-weight: var(--font-bold);
    color: var(--text-secondary);
    margin: 0 0 var(--space-2);
    letter-spacing: 0.04em;
  }

  .detail-result {
    font-size: var(--text-xs);
    color: var(--text-primary);
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: var(--space-3);
    margin: 0;
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 60vh;
    overflow-y: auto;
  }

  .detail-empty {
    font-size: var(--text-xs);
    color: var(--text-muted);
    margin: 0;
  }

</style>
