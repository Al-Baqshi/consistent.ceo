<script lang="ts">
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import { api, fetchAllPages, type PaginatedResponse } from '$lib/api';
  import { feedKindLabel } from '$lib/feedKinds';
  import { clearApiError } from '$lib/errors';
  import { describeMissionFeedEvent } from '$lib/missionFeed';
  import TaskKanban from '$lib/components/TaskKanban.svelte';
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
    requires_human_approval?: boolean;
    assigned_director_id?: string;
    assigned_agent_id?: string;
    task_kind?: string;
    outcome?: string;
    result?: string;
    entity_id?: string;
  };

  type DirectorView = { id: string; name: string; icon: string; description: string; heartbeatEnabled: boolean; autoTaskKinds: string[] };
  type EntityRuleRow = {
    id: string;
    name: string;
    preferred_model_tier?: string | null;
    entity_runtime?: {
      automation_mode?: 'manual' | 'suggest' | 'auto_run';
      heartbeat_mode?: 'off' | 'suggest' | 'execute';
    } | null;
  };
  type OrchestrationRule = {
    id: string;
    match_project?: string;
    match_department?: string;
    match_tag?: string;
    director_id?: string;
    retry_count?: number;
  };
  type MissionPolicy = {
    starterTaskAutoQueueingEnabled: boolean;
    followupTaskProposalEnabled: boolean;
    followupAutoCreateEnabled: boolean;
    defaultReviewCadenceHours: number;
  };
  type ApprovalRequest = {
    id: string;
    entityId: string;
    action: string;
    status: 'pending' | 'approved' | 'rejected' | 'expired';
    detail?: string;
    updatedAt: string;
  };
  type Agent = {
    id: string;
    groupId: string;
    name: string;
    role: string;
    kind: string;
    preferredProvider: string;
    preferredModel: string;
    mission: string;
  };
  type OperatorFeedEvent = {
    id: string;
    timestamp: string;
    kind: string;
    title: string;
    detail?: string;
    taskId?: string;
    missionId?: string;
  };
  let allTasks = $state<Task[]>([]);
  let directors = $state<DirectorView[]>([]);
  let agents = $state<Agent[]>([]);
  let loadFailed = $state(false);
  let operatorFeed = $state<OperatorFeedEvent[]>([]);
  let entities = $state<EntityRuleRow[]>([]);
  let orchestrationRules = $state<OrchestrationRule[]>([]);
  let missionPolicy = $state<MissionPolicy | null>(null);
  let approvals = $state<ApprovalRequest[]>([]);
  let approvalBusyId = $state<string | null>(null);
  let refreshTimer: ReturnType<typeof setInterval> | undefined;
  let expandedDirectors = $state<Record<string, boolean>>({});
  let selectedTask = $state<Task | null>(null);

  const heartbeatOnCount = $derived(directors.filter((d) => d.heartbeatEnabled).length);
  const decisionRequiredCount = $derived(allTasks.filter((t) => (t.requires_human_approval || t.outcome === 'needshumanapproval') && t.status !== 'completed' && t.status !== 'failed').length);

  const inProgressTasks = $derived(allTasks.filter((t) => t.status === 'in-progress'));
  const activeDirectorIds = $derived([...new Set(inProgressTasks.map((t) => t.assigned_director_id).filter(Boolean))] as string[]);
  const activeAgentIds = $derived([...new Set(inProgressTasks.map((t) => t.assigned_agent_id).filter(Boolean))] as string[]);
  const activeDirectors = $derived(directors.filter((d) => activeDirectorIds.includes(d.id)));
  const blockedTaskCount = $derived(allTasks.filter((t) => t.outcome === 'blockedbypolicy').length);
  const strongerModelCount = $derived(allTasks.filter((t) => t.outcome === 'needsstrongermodel').length);
  const escalationCount = $derived(blockedTaskCount + strongerModelCount);
  const pendingApprovals = $derived(approvals.filter((a) => a.status === 'pending'));
  const recentOperatorFeed = $derived(operatorFeed.slice(0, 8));
  const entityRulesPreview = $derived(entities.slice(0, 6));
  const visibleAgents = $derived(
    agents
      .slice()
      .sort((a, b) => {
        const aActive = activeAgentIds.includes(a.id) ? 1 : 0;
        const bActive = activeAgentIds.includes(b.id) ? 1 : 0;
        if (aActive !== bActive) return bActive - aActive;
        return a.name.localeCompare(b.name);
      })
  );

  function agentsForDirector(directorId: string) {
    return agents.filter((a) => a.groupId === directorId);
  }

  function toggleDirectorExpand(id: string) {
    expandedDirectors = { ...expandedDirectors, [id]: !expandedDirectors[id] };
  }

  async function toggleHeartbeat(e: MouseEvent, director: DirectorView) {
    e.stopPropagation();
    const next = !director.heartbeatEnabled;
    try {
      await api<DirectorView>('/api/directors/' + director.id + '/heartbeat', {
        method: 'POST',
        body: JSON.stringify({ enabled: next })
      });
      directors = directors.map((d) => (d.id === director.id ? { ...d, heartbeatEnabled: next } : d));
    } catch (err) {
      console.error('Failed to set heartbeat', err);
    }
  }

  function openCaptureForDirector(directorId: string) {
    goto('/console?directorId=' + encodeURIComponent(directorId));
  }

  async function loadData() {
    try {
      loadFailed = false;
      const [tasksRes, dirs, allAgents, feedRes, allEntitiesRes, rules, policy, approvalsRes] = await Promise.all([
        // The newest 500, not the default first page (the oldest 50). This
        // polls every 5s, so it reads the recent tail rather than everything.
        fetchAllPages<Task>('/api/tasks', { last: 500 }),
        api<DirectorView[]>('/api/directors').catch(() => [] as DirectorView[]),
        api<Agent[]>('/api/agents').catch(() => [] as Agent[]),
        api<PaginatedResponse<OperatorFeedEvent>>('/api/operator-feed?limit=40').then(r => r.items ?? []).catch(() => [] as OperatorFeedEvent[]),
        fetchAllPages<EntityRuleRow>('/api/entities').catch(() => [] as EntityRuleRow[]),
        api<OrchestrationRule[]>('/api/orchestration/rules').catch(() => [] as OrchestrationRule[]),
        api<MissionPolicy>('/api/missions/policy').catch(() => null as MissionPolicy | null),
        api<ApprovalRequest[]>('/api/approvals?status=pending').catch(() => [] as ApprovalRequest[])
      ]);
      allTasks = tasksRes ?? [];
      directors = dirs ?? [];
      agents = allAgents ?? [];
      operatorFeed = feedRes ?? [];
      entities = allEntitiesRes ?? [];
      orchestrationRules = rules ?? [];
      missionPolicy = policy ?? null;
      approvals = approvalsRes ?? [];
      clearApiError();
    } catch (err) {
      loadFailed = true;
      console.error('Failed to load dashboard data', err);
    }
  }

  async function requeueTask(taskId: string) {
    try {
      await api('/api/tasks/' + taskId + '/requeue', { method: 'POST' });
      await loadData();
    } catch (err) {
      console.error('Requeue failed', err);
    }
  }

  async function resolveApproval(id: string, status: 'approved' | 'rejected') {
    approvalBusyId = id;
    try {
      await api<ApprovalRequest>('/api/approvals/' + id, {
        method: 'PATCH',
        body: JSON.stringify({ status })
      });
      await loadData();
    } catch (err) {
      console.error('Approval resolution failed', err);
    } finally {
      approvalBusyId = null;
    }
  }

  function openTaskDetail(task: Task) {
    selectedTask = task;
  }

  function closeTaskDetail() {
    selectedTask = null;
  }

  onMount(() => {
    loadData();
    refreshTimer = setInterval(loadData, 5000);
    return () => {
      if (refreshTimer) clearInterval(refreshTimer);
    };
  });
</script>

<Seo title="Operations" noindex />

<div class="dashboard">
  <div class="main-panel">
    <div class="section-header" id="operations">
      <h1 class="section-label">Operations</h1>
      <div class="control-stats">
        {#if loadFailed}
          <span class="stale-hint">Data may be stale or gateway unreachable</span>
        {:else}
          <span>
            Active work: <strong>{inProgressTasks.length}</strong>
            <span class="active-badge" class:idle={inProgressTasks.length === 0}>
              {inProgressTasks.length > 0 ? 'active' : 'idle'}
            </span>
          </span>
          {#if directors.length > 0}
            <span class="heartbeat-summary">
              {#if heartbeatOnCount === 0}
                Heartbeat: <strong>OFF</strong>
              {:else}
                Heartbeat: <strong>{heartbeatOnCount}/{directors.length}</strong> on
              {/if}
            </span>
          {/if}
          {#if decisionRequiredCount > 0}
            <a href="/tasks?filter=decision-required" class="decision-required">Approvals: <strong>{decisionRequiredCount}</strong></a>
          {/if}
          {#if blockedTaskCount > 0}
            <span>Blocked: <strong>{blockedTaskCount}</strong></span>
          {/if}
          {#if strongerModelCount > 0}
            <span>Needs capacity: <strong>{strongerModelCount}</strong></span>
          {/if}
        {/if}
      </div>
    </div>

    <div class="dept-grid" id="directors">
      {#each directors as director}
        <div class="dept-card director-card" class:director-active={activeDirectorIds.includes(director.id)}>
          <div class="dept-card-header">
            <div class="dept-info">
              <span class="dept-icon">{director.icon}</span>
              <span class="dept-name">{director.name}</span>
            </div>
            <div class="director-activity">
              {#if activeDirectorIds.includes(director.id)}
                <span class="activity-wave"></span>
                <span class="activity-text">Active</span>
              {:else if director.heartbeatEnabled}
                <span class="activity-text standby">Heartbeat on</span>
              {:else}
                <span class="activity-text idle">Idle</span>
              {/if}
            </div>
          </div>
          <div class="dept-meta">
            <span><strong>{agents.filter((agent) => agent.groupId === director.id).length}</strong> Agents</span>
            <span><strong>{allTasks.filter((task) => task.assigned_director_id === director.id && task.status === 'in-progress').length}</strong> Running</span>
          </div>
          <div class="dept-status">
            <span class="status-dot" class:running={activeDirectorIds.includes(director.id)}></span>
            <span>{director.description}</span>
          </div>
          <div class="director-toolbar">
            <button
              type="button"
              class="heartbeat-pill"
              class:on={director.heartbeatEnabled}
              class:off={!director.heartbeatEnabled}
              class:active={director.heartbeatEnabled && activeDirectorIds.includes(director.id)}
              onclick={(e) => toggleHeartbeat(e, director)}
              title={director.heartbeatEnabled
                ? (activeDirectorIds.includes(director.id) ? 'Heartbeat on — currently working — click to pause pickup' : 'Heartbeat on — click to pause pickup')
                : 'Heartbeat off — click to enable pickup'}
            >
              <span class="heartbeat-dot">
                {#if director.heartbeatEnabled && activeDirectorIds.includes(director.id)}
                  <span class="heartbeat-wave"></span>
                {/if}
              </span>
              <span class="heartbeat-label">{director.heartbeatEnabled ? 'on' : 'off'}</span>
            </button>
            <button
              type="button"
              class="icon-action"
              aria-label="Open Capture for this director"
              onclick={() => openCaptureForDirector(director.id)}
            >
              ⌨️
            </button>
            {#if agentsForDirector(director.id).length > 0}
              <button
                type="button"
                class="icon-action"
                aria-label={expandedDirectors[director.id] ? 'Collapse agents' : 'Expand agents'}
                onclick={() => toggleDirectorExpand(director.id)}
              >
                {expandedDirectors[director.id] ? '▼' : '▶'}
              </button>
            {/if}
          </div>
          {#if expandedDirectors[director.id] && agentsForDirector(director.id).length > 0}
            <div class="subagents-panel">
              {#each agentsForDirector(director.id) as ag}
                <div class="subagent-row">
                  <span class="sub-dot" class:live={activeAgentIds.includes(ag.id)}></span>
                  <span class="sub-name">{ag.name}</span>
                  <span class="sub-role">{ag.role}</span>
                  <span class="sub-state" class:live={activeAgentIds.includes(ag.id)}>
                    {activeAgentIds.includes(ag.id) ? 'live' : 'ready'}
                  </span>
                </div>
              {/each}
            </div>
          {/if}
        </div>
      {/each}
    </div>

    <p class="team-setup-hint">
      Need more setup?
      <a href="/agents">Team setup</a>
      (directors, agents, chat).
    </p>

    <div class="task-section">
      <div class="task-section-header">
        <h2 class="section-label-sm">Active work</h2>
      </div>

      <div class="focus-bar">
        <span class="focus-label">Status:</span>
        <span class="focus-value">{inProgressTasks.length > 0 ? 'Work in progress' : 'Idle'}</span>
        {#if activeDirectors.length > 0}
          <span class="focus-sep">Lead director: {activeDirectors.map((d) => d.name).join(', ')}</span>
        {/if}
      </div>

      <div class="task-board-header">
        <div class="board-title">
          <h2 class="section-label-sm">Task board</h2>
        </div>
      </div>

      <TaskKanban
        tasks={allTasks}
        selectedTaskId={selectedTask?.id ?? null}
        onSelectTask={openTaskDetail}
        onAfterPatch={loadData}
        onRequeue={requeueTask}
      />
    </div>

    <details class="ops-extras">
      <summary>Rules, approvals &amp; activity</summary>
      <div class="ops-extras-body">
        <div class="sidebar-section">
          <div class="sidebar-row">
            <span class="sidebar-label">Approvals</span>
            <span class="sidebar-badge">{pendingApprovals.length} pending</span>
          </div>
          <div class="agent-list">
            {#if pendingApprovals.length === 0}
              <div class="agent-card">
                <div class="agent-role">No pending approvals.</div>
              </div>
            {:else}
              {#each pendingApprovals.slice(0, 8) as approval}
                <div class="agent-card">
                  <div class="agent-card-top">
                    <div>
                      <div class="agent-name">{approval.action}</div>
                      <div class="agent-role">entity: {approval.entityId}</div>
                    </div>
                    <span class="agent-state">pending</span>
                  </div>
                  {#if approval.detail}
                    <div class="agent-meta-line">{approval.detail}</div>
                  {/if}
                  <div class="approval-actions">
                    <button
                      type="button"
                      class="btn-approve"
                      disabled={approvalBusyId === approval.id}
                      onclick={() => resolveApproval(approval.id, 'approved')}
                    >
                      Approve
                    </button>
                    <button
                      type="button"
                      class="btn-reject"
                      disabled={approvalBusyId === approval.id}
                      onclick={() => resolveApproval(approval.id, 'rejected')}
                    >
                      Reject
                    </button>
                  </div>
                </div>
              {/each}
            {/if}
          </div>
        </div>

        <div class="sidebar-section">
          <div class="sidebar-row">
            <span class="sidebar-label">Rules</span>
            <span class="sidebar-badge">{orchestrationRules.length} routing</span>
          </div>
          <div class="agent-list">
            <div class="agent-card">
              <div class="agent-name">Director rules</div>
              <div class="agent-role">Heartbeat and task kinds by worker</div>
              <div class="agent-meta-line compact">
                <span>Heartbeat on: {heartbeatOnCount}/{directors.length}</span>
                <span>Escalations: {escalationCount}</span>
              </div>
            </div>
            {#if missionPolicy}
              <div class="agent-card">
                <div class="agent-name">Mission rules</div>
                <div class="agent-meta-line">Review cadence: {missionPolicy.defaultReviewCadenceHours}h</div>
                <div class="agent-meta-line compact">
                  <span>Suggest next: {missionPolicy.followupTaskProposalEnabled ? 'on' : 'off'}</span>
                  <span>Auto-create: {missionPolicy.followupAutoCreateEnabled ? 'on' : 'off'}</span>
                </div>
              </div>
            {/if}
            <div class="agent-card">
              <div class="agent-name">Entity rules</div>
              <div class="agent-role">Automation + heartbeat mode preview</div>
              {#if entityRulesPreview.length === 0}
                <div class="agent-meta-line">No entities yet.</div>
              {:else}
                {#each entityRulesPreview as entity}
                  <div class="entity-rule-row">
                    <span class="truncate">{entity.name}</span>
                    <span class="entity-rule-meta">
                      {entity.entity_runtime?.automation_mode ?? 'manual'} / {entity.entity_runtime?.heartbeat_mode ?? 'off'}
                    </span>
                  </div>
                {/each}
              {/if}
            </div>
          </div>
        </div>

        <div class="sidebar-section">
          <div class="sidebar-row">
            <span class="sidebar-label">Team</span>
            <span class="sidebar-badge">{visibleAgents.length}</span>
          </div>
          <div class="agent-list">
            {#each visibleAgents as agent}
              <div class="agent-card" class:agent-active={activeAgentIds.includes(agent.id)}>
                <div class="agent-card-top">
                  <div>
                    <div class="agent-name">{agent.name}</div>
                    <div class="agent-role">{agent.role}</div>
                  </div>
                  <span class="agent-state" class:live={activeAgentIds.includes(agent.id)}>
                    {#if activeAgentIds.includes(agent.id)}live{:else}ready{/if}
                  </span>
                </div>
                <div class="agent-meta-line">
                  <span>{directors.find((director) => director.id === agent.groupId)?.name ?? agent.groupId}</span>
                </div>
              </div>
            {/each}
          </div>
        </div>

        <div class="sidebar-section">
          <div class="sidebar-row">
            <span class="sidebar-label">Activity</span>
            <span class="sidebar-badge">{recentOperatorFeed.length} recent</span>
          </div>
          <div class="agent-list">
            {#if recentOperatorFeed.length === 0}
              <div class="agent-card">
                <div class="agent-role">No log events yet.</div>
              </div>
            {:else}
              {#each recentOperatorFeed as event}
                <div class="agent-card">
                  <div class="agent-card-top">
                    <div>
                      <div class="agent-name">{feedKindLabel(event.kind)}</div>
                      <div class="agent-role">{event.title}</div>
                    </div>
                  </div>
                  {#if describeMissionFeedEvent(event)}
                    <div class="agent-meta-line">{describeMissionFeedEvent(event)}</div>
                  {/if}
                  <div class="agent-meta-line compact">
                    <span>{new Date(event.timestamp).toLocaleString()}</span>
                  </div>
                </div>
              {/each}
            {/if}
          </div>
        </div>
      </div>
    </details>
  </div>

  {#if selectedTask}
    <div
      class="detail-backdrop"
      onclick={closeTaskDetail}
      role="button"
      tabindex="0"
      onkeydown={(e) => {
        if (e.key === 'Escape' || e.key === 'Enter' || e.key === ' ') closeTaskDetail();
      }}
    ></div>
    <aside class="detail-pane" role="region" aria-label="Task details">
      <div class="detail-header">
        <h3 class="detail-title">{selectedTask.title}</h3>
        <button type="button" class="detail-close" onclick={closeTaskDetail} aria-label="Close">×</button>
      </div>
      <div class="detail-body">
        <div class="detail-meta">
          <span>{selectedTask.dept}</span>
          <span>{selectedTask.status}</span>
          <span>{selectedTask.task_kind ?? 'generic'}</span>
          {#if selectedTask.entity_id}
            <span>entity: <code>{selectedTask.entity_id}</code></span>
          {/if}
        </div>
        {#if selectedTask.roles?.length}
          <div class="detail-meta">
            {#each selectedTask.roles as role}
              <span class="sidebar-badge">{role}</span>
            {/each}
          </div>
        {/if}
        <div class="agent-meta-line">Outcome: {selectedTask.outcome ?? 'n/a'}</div>
        {#if selectedTask.result}
          <pre class="detail-result">{selectedTask.result}</pre>
        {:else}
          <p class="agent-role">No result yet for this task.</p>
        {/if}
      </div>
    </aside>
  {/if}
</div>

<style>
  .dashboard {
    display: flex;
    flex-direction: column;
  }

  .main-panel {
    padding: 16px 20px;
    min-width: 0;
    max-width: 100%;
  }

  .section-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 12px;
    flex-wrap: wrap;
    gap: 6px;
  }

  .section-label {
    margin: 0;
    font-size: 20px;
    font-weight: 700;
    color: var(--text-primary);
  }

  .control-stats {
    display: flex;
    gap: 16px;
    font-size: 11px;
    color: var(--accent-red);
    flex-wrap: wrap;
  }
  .control-stats strong { color: var(--accent-red); }
  .stale-hint { color: var(--accent-red); font-style: italic; }
  .decision-required {
    color: var(--accent-orange);
    text-decoration: none;
  }
  .decision-required:hover { text-decoration: underline; }
  .active-badge {
    margin-left: 6px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    min-height: 18px;
    padding: 0 6px;
    border-radius: 999px;
    border: 1px solid rgba(122, 129, 99, 0.55);
    background: rgba(22, 101, 52, 0.4);
    color: #7a8163;
    font-size: 10px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.03em;
  }
  .active-badge.idle {
    border-color: rgba(242, 237, 228, 0.4);
    background: rgba(74, 61, 82, 0.35);
    color: #b3aa9b;
  }

  .dept-grid {
    display: grid;
    grid-template-columns: repeat(4, 1fr);
    gap: 8px;
    margin-bottom: 20px;
  }

  @media (max-width: 1100px) {
    .dept-grid { grid-template-columns: repeat(3, 1fr); }
  }
  @media (max-width: 700px) {
    .dept-grid { grid-template-columns: repeat(2, 1fr); }
  }
  @media (max-width: 400px) {
    .dept-grid { grid-template-columns: 1fr; }
  }

  .dept-card {
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 12px;
    border-left: 2px solid var(--dept-color, var(--border));
  }

  .director-card {
    position: relative;
    overflow: hidden;
  }

  .director-card.director-active {
    border-color: rgba(201, 138, 75, 0.45);
    box-shadow: 0 0 0 1px rgba(201, 138, 75, 0.15), 0 0 20px rgba(201, 138, 75, 0.12);
  }

  .director-card.director-active::after {
    content: '';
    position: absolute;
    inset: 0;
    background: linear-gradient(110deg, transparent 20%, rgba(201, 138, 75, 0.14) 45%, transparent 70%);
    transform: translateX(-120%);
    animation: directorSweep 2.8s ease-in-out infinite;
    pointer-events: none;
  }

  .director-toolbar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px;
    margin-top: 10px;
    padding-top: 8px;
    border-top: 1px solid var(--border-subtle);
  }

  .heartbeat-pill {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    padding: 4px 10px;
    border-radius: 999px;
    font-size: 11px;
    font-weight: 600;
    border: 1px solid transparent;
    cursor: pointer;
    background: var(--bg-secondary);
  }
  .heartbeat-pill.on {
    border-color: rgba(122, 129, 99, 0.55);
    color: #7a8163;
    box-shadow: 0 0 0 1px rgba(122, 129, 99, 0.25), 0 0 8px rgba(122, 129, 99, 0.45);
  }
  .heartbeat-pill.on .heartbeat-dot {
    background: #7a8163;
  }
  .heartbeat-pill.on.active {
    box-shadow: 0 0 0 1px rgba(122, 129, 99, 0.3), 0 0 10px rgba(122, 129, 99, 0.6);
    animation: heartbeatGlowPulse 1.8s ease-in-out infinite;
  }
  .heartbeat-pill.off {
    border-color: var(--border);
    color: var(--text-muted);
  }
  .heartbeat-pill.off .heartbeat-dot {
    background: var(--text-muted);
  }
  .heartbeat-dot {
    position: relative;
    width: 6px;
    height: 6px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .heartbeat-wave {
    position: absolute;
    inset: 0;
    border-radius: 50%;
    background: #7a8163;
    animation: heartbeatWave 1.6s ease-out infinite;
  }

  @keyframes heartbeatGlowPulse {
    0%, 100% { box-shadow: 0 0 0 1px rgba(122, 129, 99, 0.3), 0 0 8px rgba(122, 129, 99, 0.45); }
    50% { box-shadow: 0 0 0 1px rgba(122, 129, 99, 0.35), 0 0 14px rgba(122, 129, 99, 0.75); }
  }
  @keyframes heartbeatWave {
    0% { box-shadow: 0 0 0 0 rgba(122, 129, 99, 0.6); opacity: 1; }
    70% { box-shadow: 0 0 0 8px rgba(122, 129, 99, 0); opacity: 0; }
    100% { box-shadow: 0 0 0 0 rgba(122, 129, 99, 0); opacity: 0; }
  }

  .icon-action {
    background: none;
    border: none;
    color: var(--text-muted);
    font-size: 14px;
    padding: 6px 8px;
    border-radius: 6px;
    cursor: pointer;
    min-width: 44px;
    min-height: 44px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
  }
  .icon-action:hover {
    background: var(--bg-card-hover);
    color: var(--text-primary);
  }

  .subagents-panel {
    margin-top: 8px;
    padding: 8px 0 0;
    border-top: 1px dashed var(--border-subtle);
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .subagent-row {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    font-size: 11px;
    color: var(--text-secondary);
  }
  .sub-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--text-muted);
    flex-shrink: 0;
  }
  .sub-dot.live {
    background: #7a8163;
    box-shadow: 0 0 6px rgba(122, 129, 99, 0.5);
  }
  .sub-name {
    font-weight: 600;
    color: var(--text-primary);
  }
  .sub-role {
    opacity: 0.85;
    flex: 1;
    min-width: 0;
  }
  .sub-state {
    font-size: 10px;
    text-transform: uppercase;
    font-weight: 700;
    color: var(--text-muted);
    margin-left: auto;
  }
  .sub-state.live {
    color: #7a8163;
  }

  .team-setup-hint {
    font-size: 12px;
    color: var(--text-secondary);
    margin: 0 0 16px;
  }
  .team-setup-hint a {
    color: var(--accent-cyan);
    font-weight: 700;
  }

  .dept-card-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 6px;
  }

  .dept-info {
    display: flex;
    align-items: center;
    gap: 6px;
  }

  .dept-icon { font-size: 14px; }

  .dept-name {
    font-size: 12px;
    font-weight: 600;
    color: var(--text-primary);
  }

  .director-activity {
    display: flex;
    align-items: center;
    gap: 6px;
  }

  .activity-text {
    font-size: 10px;
    font-weight: 700;
    color: #eed9ae;
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }

  .activity-text.standby { color: var(--accent-orange); }
  .activity-text.idle { color: var(--text-muted); }

  .activity-wave {
    width: 8px;
    height: 8px;
    border-radius: 999px;
    background: #c98a4b;
    box-shadow: 0 0 0 rgba(201, 138, 75, 0.55);
    animation: directorPulse 1.6s infinite;
  }

  .dept-meta {
    display: flex;
    gap: 12px;
    font-size: 11px;
    color: var(--text-secondary);
    margin-bottom: 6px;
  }
  .dept-meta strong { color: var(--text-primary); }

  .dept-status {
    display: flex;
    align-items: center;
    gap: 5px;
    font-size: 11px;
    color: var(--text-secondary);
  }

  .status-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--text-muted);
  }
  .status-dot.running { background: var(--running-green); }

  .task-section { margin-top: 4px; }

  .task-section-header {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-bottom: 8px;
  }

  .section-label-sm {
    margin: 0;
    font-size: 14px;
    font-weight: 600;
    color: var(--text-primary);
  }

  .focus-bar {
    display: flex;
    align-items: center;
    gap: 16px;
    font-size: 12px;
    color: var(--text-secondary);
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 7px 12px;
    margin-bottom: 12px;
    flex-wrap: wrap;
  }

  .focus-label { color: var(--text-muted); }
  .focus-value { color: var(--text-primary); }
  .focus-sep { color: var(--text-secondary); }

  .task-board-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 10px;
  }

  .board-title {
    display: flex;
    align-items: center;
    gap: 6px;
  }

  .ops-extras {
    margin-top: 20px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-secondary);
  }

  .ops-extras > summary {
    list-style: none;
    padding: 10px 14px;
    font-size: 12px;
    font-weight: 600;
    color: var(--text-secondary);
    cursor: pointer;
    user-select: none;
  }

  .ops-extras > summary::-webkit-details-marker {
    display: none;
  }

  .ops-extras[open] > summary {
    border-bottom: 1px solid var(--border);
  }

  .ops-extras-body {
    padding: 12px;
    display: flex;
    flex-direction: column;
    gap: 16px;
  }

  @media (min-width: 900px) {
    .ops-extras-body {
      flex-direction: row;
      align-items: flex-start;
      gap: 24px;
    }

    .ops-extras-body .sidebar-section {
      flex: 1;
      min-width: 0;
    }
  }

  .sidebar-section {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .sidebar-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-size: 11px;
  }

  .sidebar-label { color: var(--text-secondary); }
  .sidebar-badge {
    font-size: 10px;
    color: var(--text-muted);
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 3px;
    padding: 1px 5px;
  }

  .agent-list {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .agent-card {
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 8px;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .agent-card.agent-active {
    border-color: rgba(201, 138, 75, 0.45);
    box-shadow: 0 0 16px rgba(201, 138, 75, 0.1);
  }

  .agent-card-top {
    display: flex;
    justify-content: space-between;
    gap: 8px;
    align-items: flex-start;
  }

  .agent-name {
    font-size: 12px;
    font-weight: 700;
    color: var(--text-primary);
  }

  .agent-role,
  .agent-meta-line {
    font-size: 10px;
    color: var(--text-secondary);
  }

  .agent-meta-line.compact {
    display: flex;
    justify-content: space-between;
    gap: 8px;
    color: var(--text-muted);
  }

  .approval-actions {
    display: flex;
    gap: 8px;
    margin-top: 8px;
  }

  .btn-approve,
  .btn-reject {
    min-height: 36px;
    padding: 6px 10px;
    border-radius: 8px;
    border: 1px solid var(--border);
    background: var(--bg-card-hover);
    color: var(--text-primary);
    font-size: 12px;
    font-weight: 700;
    cursor: pointer;
  }

  .btn-approve {
    border-color: rgba(122, 129, 99, 0.4);
    color: #7a8163;
  }

  .btn-reject {
    border-color: rgba(201, 106, 75, 0.45);
    color: #c96a4b;
  }

  .entity-rule-row {
    display: flex;
    justify-content: space-between;
    gap: 8px;
    font-size: 11px;
    color: var(--text-secondary);
    border-top: 1px dashed var(--border-subtle);
    padding-top: 6px;
    margin-top: 6px;
  }

  .entity-rule-meta {
    color: var(--text-muted);
    white-space: nowrap;
  }

  .agent-state {
    font-size: 10px;
    font-weight: 700;
    text-transform: uppercase;
    color: var(--text-muted);
  }

  .agent-state.live {
    color: #eed9ae;
  }

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
    width: min(420px, 100vw);
    height: 100vh;
    background: var(--bg-secondary);
    border-left: 1px solid var(--border);
    z-index: 41;
    display: flex;
    flex-direction: column;
  }

  .detail-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    padding: 12px;
    border-bottom: 1px solid var(--border);
  }

  .detail-title {
    margin: 0;
    font-size: 14px;
    font-weight: 700;
    color: var(--text-primary);
    min-width: 0;
  }

  .detail-close {
    border: none;
    background: transparent;
    color: var(--text-muted);
    font-size: 20px;
    cursor: pointer;
    min-width: 44px;
    min-height: 44px;
  }

  .detail-body {
    padding: 12px;
    display: flex;
    flex-direction: column;
    gap: 10px;
    overflow-y: auto;
  }

  .detail-meta {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    font-size: 12px;
    color: var(--text-secondary);
  }

  .detail-result {
    margin: 0;
    font-size: 11px;
    line-height: 1.45;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 10px;
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 55vh;
    overflow-y: auto;
  }

  @keyframes directorPulse {
    0% { box-shadow: 0 0 0 0 rgba(201, 138, 75, 0.55); }
    70% { box-shadow: 0 0 0 10px rgba(201, 138, 75, 0); }
    100% { box-shadow: 0 0 0 0 rgba(201, 138, 75, 0); }
  }

  @keyframes directorSweep {
    0% { transform: translateX(-120%); }
    100% { transform: translateX(120%); }
  }
</style>
