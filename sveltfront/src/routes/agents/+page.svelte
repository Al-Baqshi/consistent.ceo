<script lang="ts">
  /** API shape: group from /api/agent-groups */
  type ApiAgentGroup = {
    id: string;
    name: string;
    icon: string;
    description: string;
  };

  /** API shape: agent from /api/agents */
  type AgentRole = {
    id: string;
    groupId: string;
    name: string;
    role: string;
    kind: string;
    preferredProvider: string;
    preferredModel: string;
    mission: string;
    profile: string;
  };

  type SubAgent = {
    name: string;
    status: 'Idle' | 'Running';
    type: string;
    task: string;
    progress: number;
    color: string;
  };

  /** API shape: director (group + heartbeat) */
  type DirectorView = {
    id: string;
    name: string;
    icon: string;
    description: string;
    heartbeatEnabled: boolean;
    autoTaskKinds: string[];
    modelProvider?: string;
    modelName?: string;
    temperature?: number;
    configurable: boolean;
  };

  /** A model this install can actually offer as a director's muscle. */
  type MuscleOption = {
    provider: string;
    needsKey: boolean;
    cost: string;
    note: string;
  };

  type AgentGroup = {
    id: string;
    name: string;
    icon: string;
    status: 'Running' | 'Idle';
    description: string;
    agentCount: number;
    queueCount: number;
    expanded: boolean;
    heartbeatEnabled: boolean;
    modelProvider?: string;
    modelName?: string;
    configurable: boolean;
    tags?: string[];
    subAgents?: SubAgent[];
  };

  type ChatTab = 'chat' | 'history' | 'files';

  type ChatMessage = {
    id: string;
    text: string;
    time: string;
    isAgent: boolean;
  };

  type HistoryEntry = {
    id: string;
    title: string;
    date: string;
    messages: number;
    preview: string;
  };

  type FileEntry = {
    name: string;
    ext: string;
    size: string;
    date: string;
  };

  type ApiMessagePair = {
    role: string;
    content: string;
  };

  /** API shape: task (only the fields needed to tell which directors are actively working) */
  type ApiTask = {
    id: string;
    status: string;
    assigned_director_id?: string;
  };

  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import { api, apiErrorStatus, fetchAllPages, listSessions } from '$lib/api';
  import Seo from '$lib/components/Seo.svelte';

  let groups = $state<AgentGroup[]>([]);
  let allTasks = $state<ApiTask[]>([]);
  let showDirectorModal = $state(false);
  let showAgentModal = $state(false);
  let savingDirector = $state(false);
  let savingAgent = $state(false);
  let formError = $state<string | null>(null);

  let muscles = $state<MuscleOption[]>([]);
  let muscleBusy = $state<Record<string, boolean>>({});
  let muscleError = $state('');

  async function loadMuscles() {
    muscles = await api<MuscleOption[]>('/api/llm/muscles').catch(() => [] as MuscleOption[]);
  }

  /**
   * Assign which model a director thinks with. Writes the Surreal director row
   * that `director_runner` reads, so the change takes effect on the next beat.
   */
  async function setMuscle(group: AgentGroup, provider: string, select?: HTMLSelectElement) {
    muscleBusy[group.id] = true;
    muscleError = '';
    try {
      // "default" clears both halves. Clearing only the provider left the old
      // model alias pinned, and an alias resolves its own provider — so the
      // director kept thinking with the previous muscle.
      const body = provider ? { modelProvider: provider } : { modelProvider: '', modelName: '' };
      await api(`/api/directors/${encodeURIComponent(group.id)}`, {
        method: 'PATCH',
        body: JSON.stringify(body),
        silent: true
      });
      const g = groups.find((x) => x.id === group.id);
      if (g) {
        g.modelProvider = provider || undefined;
        if (!provider) g.modelName = undefined;
      }
    } catch (err) {
      muscleError = err instanceof Error ? err.message : String(err);
      // The select is one-way bound; put it back on what is actually saved.
      if (select) select.value = group.modelProvider ?? '';
    } finally {
      muscleBusy[group.id] = false;
    }
  }

  let directorName = $state('');
  let directorId = $state('');
  let directorIcon = $state('🧭');
  let directorDescription = $state('');

  let agentName = $state('');
  let agentRole = $state('');
  let agentGroupId = $state('');
  let agentKind = $state('general');
  let agentProvider = $state('default');
  let agentModel = $state('default');
  let agentMission = $state('');
  let agentProfile = $state('');

  async function loadGroups() {
    try {
      const [loadedGroups, loadedAgents, directors] = await Promise.all([
        api<ApiAgentGroup[]>('/api/agent-groups'),
        api<AgentRole[]>('/api/agents'),
        api<DirectorView[]>('/api/directors').catch(() => [] as DirectorView[])
      ]);
      const groupsList = loadedGroups ?? [];
      const agentsList = loadedAgents ?? [];
      const directorsList = directors ?? [];
      groups = groupsList.map((group) => {
        const groupAgents = agentsList.filter((a) => a.groupId === group.id);
        const director = directorsList.find((d) => d.id === group.id);
        const subAgents: SubAgent[] = groupAgents.map((a) => ({
          name: a.name,
          status: 'Idle' as const,
          type: a.kind,
          task: '',
          progress: 0,
          color: '#eed9ae'
        }));
        return {
          ...group,
          status: 'Idle' as const,
          description: group.description,
          agentCount: groupAgents.length,
          queueCount: 0,
          expanded: false,
          heartbeatEnabled: director?.heartbeatEnabled ?? false,
          modelProvider: director?.modelProvider,
          modelName: director?.modelName,
          configurable: director?.configurable ?? false,
          tags: [],
          subAgents
        };
      });
    } catch (err) {
      console.error('Failed to load agent groups/agents', err);
    }
  }

  /** Only in-progress tasks matter here (which directors are working), and
   *  all of them — a bare `/api/tasks` is the oldest 50 in the workspace. */
  async function loadTasks() {
    try {
      allTasks = await fetchAllPages<ApiTask>('/api/tasks?status=in-progress');
    } catch (err) {
      console.error('Failed to load tasks', err);
    }
  }

  onMount(async () => {
    await Promise.all([loadGroups(), loadMuscles(), loadTasks()]);
    const createParam = new URL(window.location.href).searchParams.get('create');
    if (createParam === 'director') {
      showDirectorModal = true;
    } else if (createParam === 'agent') {
      showAgentModal = true;
    }
  });

  async function toggleHeartbeat(e: MouseEvent, group: AgentGroup) {
    e.stopPropagation();
    const next = !group.heartbeatEnabled;
    try {
      await api<DirectorView>('/api/directors/' + group.id + '/heartbeat', {
        method: 'POST',
        body: JSON.stringify({ enabled: next })
      });
      const g = groups.find((x) => x.id === group.id);
      if (g) g.heartbeatEnabled = next;
    } catch (err) {
      console.error('Failed to set heartbeat', err);
    }
  }

  const totalAgents = $derived(groups.reduce((s, g) => s + g.agentCount, 0));
  const activeDirectorIds = $derived(new Set(
    allTasks.filter((t) => t.status === 'in-progress' && t.assigned_director_id).map((t) => t.assigned_director_id as string)
  ));
  const runningCount = $derived(activeDirectorIds.size);
  const idleCount = $derived(groups.reduce((s, g) => s + (g.subAgents?.filter(a => a.status === 'Idle').length ?? 0), 0));
  const directorCount = $derived(groups.length);
  const heartbeatOnCount = $derived(groups.filter(g => g.heartbeatEnabled).length);
  function toggle(id: string) {
    const g = groups.find(g => g.id === id);
    // Only expand groups that have sub-agents to show
    if (g && g.subAgents && g.subAgents.length > 0) g.expanded = !g.expanded;
  }

  function getStatusColor(status: string) {
    return status === 'Idle' ? '#eed9ae' : '#7a8163';
  }

  // Chat panel state
  let chatOpen = $state(false);
  let chatGroup = $state<AgentGroup | null>(null);
  let chatTab = $state<ChatTab>('chat');
  let chatInput = $state('');
  let chatSending = $state(false);
  let chatError = $state('');

  let chatMessages = $state<ChatMessage[]>([]);
  /** Stable, unique keys for the list. Ids should be unique, but a duplicate
   *  (older gateways derived them from the session + role) must not crash
   *  the keyed each block — later copies get a suffix. */
  const keyedChatMessages = $derived.by(() => {
    const seen = new Map<string, number>();
    return chatMessages.map((msg, index) => {
      const base = msg.id || `msg-${index}`;
      const n = seen.get(base) ?? 0;
      seen.set(base, n + 1);
      return { msg, key: n === 0 ? base : `${base}#${n}` };
    });
  });

  let historyEntries = $state<HistoryEntry[]>([]);
  let fileEntries = $state<FileEntry[]>([]);

  async function loadChatMessages(groupId: string) {
    try {
      const list = await api<ChatMessage[]>('/api/agents/' + groupId + '/messages');
      chatMessages = list ?? [];
    } catch (err) {
      console.error('Failed to load messages', err);
      chatMessages = [];
    }
  }

  async function loadHistory(groupId: string) {
    try {
      // Filter server-side (the unfiltered list is capped and mixes every
      // agent), then read each session's messages in parallel.
      const sessionsRes = await listSessions({ agent_id: groupId, limit: 200 });
      const groupSessions = sessionsRes.items ?? [];
      historyEntries = await Promise.all(
        groupSessions.map(async (s): Promise<HistoryEntry> => {
          const base = {
            id: s.id,
            title: s.peer_id || s.id,
            date: new Date(s.last_active).toLocaleString()
          };
          try {
            const msgList =
              (await api<ApiMessagePair[]>(`/api/sessions/${encodeURIComponent(s.id)}/messages`, {
                silent: true
              })) ?? [];
            const lastUserMsg = [...msgList].reverse().find((m) => m.role === 'user');
            return { ...base, messages: msgList.length, preview: lastUserMsg?.content?.slice(0, 120) ?? '' };
          } catch {
            return { ...base, messages: 0, preview: '' };
          }
        })
      );
    } catch (err) {
      console.error('Failed to load history', err);
      historyEntries = [];
    }
  }

  function openChat(e: MouseEvent, group: AgentGroup) {
    e.stopPropagation();
    chatGroup = group;
    chatTab = 'chat';
    chatOpen = true;
    chatMessages = [];
    historyEntries = [];
    fileEntries = [];
    if (group?.id) {
      loadChatMessages(group.id);
      loadHistory(group.id);
    }
  }

  function openConsole(e: MouseEvent, groupId: string) {
    e.stopPropagation();
    goto(`/console?directorId=${encodeURIComponent(groupId)}`);
  }

  function closeChat() {
    chatOpen = false;
    chatGroup = null;
  }

  async function sendMessage() {
    const text = chatInput.trim();
    if (!text || !chatGroup || chatSending) return;
    // Clear first so a second Enter can't post the same note twice.
    chatSending = true;
    chatError = '';
    chatInput = '';
    try {
      const msg = await api<ChatMessage>('/api/agents/' + chatGroup.id + '/send', {
        method: 'POST',
        body: JSON.stringify({ content: text }),
        silent: true
      });
      if (msg) chatMessages = [...chatMessages, msg];
    } catch (err) {
      chatInput = text;
      chatError = err instanceof Error && err.message ? err.message : 'Could not save that note.';
    } finally {
      chatSending = false;
    }
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      void sendMessage();
    }
  }

  function extColor(ext: string) {
    const map: Record<string, string> = {
      py: '#eed9ae', ts: '#c98a4b', tsx: '#c98a4b', js: '#e0a55e',
      md: '#a3a3a3', json: '#f472b6', txt: '#7d75688a0', sh: '#7a8163',
    };
    return map[ext.toLowerCase()] ?? '#7d75688a0';
  }

  function slugify(value: string) {
    return value
      .trim()
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, '-')
      .replace(/^-+|-+$/g, '');
  }

  async function createDirector() {
    if (!directorName.trim()) {
      formError = 'Director name is required.';
      return;
    }
    if (savingDirector) return;
    savingDirector = true;
    formError = null;
    try {
      // A name with no ASCII letters or digits ("Ведущий", "営業") slugifies to
      // nothing. Omit the id then and let the gateway mint one, rather than
      // sending "" and getting a bare 400.
      const id = directorId.trim() || slugify(directorName);
      const payload = {
        ...(id ? { id } : {}),
        name: directorName.trim(),
        icon: directorIcon.trim() || '🧭',
        description: directorDescription.trim() || 'User-created director'
      };
      await api('/api/agent-groups', {
        method: 'POST',
        body: JSON.stringify(payload),
        silent: true
      });
      showDirectorModal = false;
      directorName = '';
      directorId = '';
      directorIcon = '🧭';
      directorDescription = '';
      await loadGroups();
    } catch (err) {
      formError =
        apiErrorStatus(err) === 409
          ? 'That ID already exists. Choose a different name or ID.'
          : err instanceof Error && err.message
            ? err.message
            : 'Failed to create director';
    } finally {
      savingDirector = false;
    }
  }

  async function createAgent() {
    if (!agentName.trim() || !agentRole.trim() || !agentGroupId) {
      formError = 'Agent name, role, and director are required.';
      return;
    }
    if (savingAgent) return;
    savingAgent = true;
    formError = null;
    try {
      await api('/api/agents', {
        method: 'POST',
        body: JSON.stringify({
          group_id: agentGroupId,
          name: agentName.trim(),
          role: agentRole.trim(),
          kind: agentKind.trim() || 'general',
          preferred_provider: agentProvider.trim() || 'default',
          preferred_model: agentModel.trim() || 'default',
          mission: agentMission.trim(),
          profile: agentProfile.trim()
        }),
        silent: true
      });
      showAgentModal = false;
      agentName = '';
      agentRole = '';
      agentGroupId = '';
      agentKind = 'general';
      agentProvider = 'default';
      agentModel = 'default';
      agentMission = '';
      agentProfile = '';
      await loadGroups();
    } catch (err: any) {
      formError = err?.message ?? 'Failed to create agent';
    } finally {
      savingAgent = false;
    }
  }
</script>

<Seo title="Agents" noindex />

<div class="agents-page">
  <h1 class="agents-title">Agents</h1>
  <div class="agents-redirect-hint">
    <span>Day-to-day heartbeat and the task board live on the</span>
    <a href="/dashboard">Dashboard</a>.
    <span>Use this page for detailed team setup (directors, agents, chat).</span>
  </div>
  <div class="agents-header">
    <div class="agents-stats">
      <span class="stat-item">
        <span class="stat-icon">⊙</span>
        Agents: <strong>{totalAgents}</strong>
      </span>
      <span class="stat-item">
        <span class="status-dot running"></span>
        Running: <strong>{runningCount}</strong>
      </span>
      <span class="stat-item">
        <span class="status-dot idle"></span>
        Idle: <strong>{idleCount}</strong>
      </span>
      <span class="stat-item">
        <span class="icon-person">👤</span>
        Directors: <strong>{directorCount}</strong>
      </span>
      <span class="stat-item heartbeat-meta">
        {#if directorCount === 0}
          Heartbeat: —
        {:else if heartbeatOnCount === 0}
          <span class="heartbeat-off">Heartbeat: off</span>
        {:else}
          <span class="heartbeat-on">Heartbeat: <strong>{heartbeatOnCount}/{directorCount}</strong> on</span>
        {/if}
      </span>
    </div>
    <div class="agents-actions">
      <button class="add-btn director" onclick={() => { formError = null; showDirectorModal = true; }}>+ Director</button>
      <button class="add-btn agent" onclick={() => { formError = null; showAgentModal = true; }}>+ Agent</button>
    </div>
  </div>

  {#if showDirectorModal}
    <div class="modal-backdrop" onclick={(e) => { if (e.target === e.currentTarget) showDirectorModal = false; }} role="button" tabindex="0" onkeydown={(e) => e.key === 'Escape' && (showDirectorModal = false)}>
      <div class="modal" role="dialog" aria-modal="true" tabindex="-1">
        <h3 class="modal-title">Create director</h3>
        <div class="modal-form">
          <label>Name <input type="text" bind:value={directorName} placeholder="Director name" /></label>
          <label>Slug/ID <input type="text" bind:value={directorId} placeholder="optional (auto from name)" /></label>
          <label>Icon <input type="text" bind:value={directorIcon} placeholder="🧭" maxlength="4" /></label>
          <label>Description <textarea rows="2" bind:value={directorDescription} placeholder="Short responsibility description"></textarea></label>
          {#if formError}<div class="form-error">{formError}</div>{/if}
        </div>
        <div class="modal-actions">
          <button class="btn-secondary" onclick={() => showDirectorModal = false}>Cancel</button>
          <button class="add-btn agent" onclick={createDirector} disabled={savingDirector}>
            {savingDirector ? 'Creating…' : 'Create director'}
          </button>
        </div>
      </div>
    </div>
  {/if}

  {#if showAgentModal}
    <div class="modal-backdrop" onclick={(e) => { if (e.target === e.currentTarget) showAgentModal = false; }} role="button" tabindex="0" onkeydown={(e) => e.key === 'Escape' && (showAgentModal = false)}>
      <div class="modal" role="dialog" aria-modal="true" tabindex="-1">
        <h3 class="modal-title">Create agent</h3>
        <div class="modal-form">
          <label>Director
            <select bind:value={agentGroupId}>
              <option value="">Select director…</option>
              {#each groups as group}
                <option value={group.id}>{group.icon} {group.name}</option>
              {/each}
            </select>
          </label>
          <label>Name <input type="text" bind:value={agentName} placeholder="Agent name" /></label>
          <label>Role <input type="text" bind:value={agentRole} placeholder="e.g. Strategy Analyst" /></label>
          <label>Kind <input type="text" bind:value={agentKind} placeholder="general" /></label>
          <label>Provider <input type="text" bind:value={agentProvider} placeholder="default" /></label>
          <label>Model <input type="text" bind:value={agentModel} placeholder="default" /></label>
          <label>Mission <textarea rows="2" bind:value={agentMission} placeholder="Agent mission"></textarea></label>
          {#if formError}<div class="form-error">{formError}</div>{/if}
        </div>
        <div class="modal-actions">
          <button class="btn-secondary" onclick={() => showAgentModal = false}>Cancel</button>
          <button class="add-btn agent" onclick={createAgent} disabled={savingAgent}>
            {savingAgent ? 'Creating…' : 'Create agent'}
          </button>
        </div>
      </div>
    </div>
  {/if}

  <div class="directors-grid">
    {#if muscleError}<p class="muscle-error" role="status">Could not set muscle: {muscleError}</p>{/if}
    {#each groups as group}
      <div
        class="director-card"
        class:expanded={group.expanded}
        class:director-active={group.heartbeatEnabled && activeDirectorIds.has(group.id)}
      >
        <div class="director-card-header">
          <div class="director-title-row">
            <span class="group-icon" aria-hidden="true">{group.icon}</span>
            <span class="group-name">{group.name}</span>
            <div class="director-actions">
              <button type="button" class="icon-action" aria-label="More options" onclick={(e) => e.stopPropagation()}>⋯</button>
              <button type="button" class="icon-action" aria-label="Expand" onclick={(e) => { e.stopPropagation(); toggle(group.id); }}>
                {#if group.subAgents && group.subAgents.length > 0}
                  {group.expanded ? '▼' : '▶'}
                {:else}
                  <span style="opacity:0">▶</span>
                {/if}
              </button>
            </div>
          </div>
          <p class="group-desc">{group.description}</p>
        </div>

        <div class="director-stats">
          <span class="director-stat">
            <span class="status-dot running" aria-hidden="true"></span>
            <strong>{group.agentCount}</strong> Agents
          </span>
          <span class="director-stat">
            <span class="status-dot idle" aria-hidden="true"></span>
            queue <strong>{group.queueCount}</strong>
          </span>
        </div>
        <div class="director-muscle" role="group" aria-label="Model this director thinks with">
          <span class="muscle-label">muscle</span>
          {#if group.configurable}
            <select
              class="muscle-select"
              disabled={muscleBusy[group.id]}
              value={group.modelProvider ?? ''}
              onclick={(e) => e.stopPropagation()}
              onchange={(e) => {
                e.stopPropagation();
                const select = e.currentTarget as HTMLSelectElement;
                void setMuscle(group, select.value, select);
              }}
              title="Which model this director thinks with"
            >
              <option value="">default (global brain)</option>
              {#each muscles as m (m.provider)}
                <option value={m.provider}>{m.provider}{m.needsKey ? '' : ' · no key'}</option>
              {/each}
            </select>
          {:else}
            <span class="muscle-static" title="No director row backs this group yet">
              {group.modelProvider ?? 'default'}
            </span>
          {/if}
        </div>
        <div class="director-toolbar">
          <button
            type="button"
            class="heartbeat-pill"
            class:on={group.heartbeatEnabled}
            class:off={!group.heartbeatEnabled}
            class:active={group.heartbeatEnabled && activeDirectorIds.has(group.id)}
            onclick={(e) => { e.stopPropagation(); toggleHeartbeat(e, group); }}
            title={group.heartbeatEnabled
              ? (activeDirectorIds.has(group.id) ? 'Heartbeat on – currently working – click to turn off' : 'Heartbeat on – click to turn off')
              : 'Heartbeat off – click to turn on'}
          >
            <span class="heartbeat-dot">
              {#if group.heartbeatEnabled && activeDirectorIds.has(group.id)}
                <span class="heartbeat-wave"></span>
              {/if}
            </span>
            <span class="heartbeat-label">{group.heartbeatEnabled ? 'on' : 'off'}</span>
          </button>
          <button type="button" class="icon-action chat-btn" aria-label="Open notes" title="Notes" onclick={(e) => openChat(e, group)}>💬</button>
          <button type="button" class="icon-action" aria-label="Add agent" onclick={(e) => e.stopPropagation()}>+</button>
          <button type="button" class="icon-action" aria-label="Files" onclick={(e) => e.stopPropagation()}>📁</button>
          <button
            type="button"
            class="icon-action"
            aria-label="Open command console for this director"
            onclick={(e) => openConsole(e, group.id)}
          >
            ⌨️
          </button>
        </div>

        {#if group.expanded && group.subAgents}
          <div class="subagents-panel">
            {#if group.tags && group.tags.length > 0}
              <div class="tags-row">
                {#each group.tags as tag}
                  <span class="tag">{tag}</span>
                {/each}
              </div>
            {/if}
            <div class="subagents-list">
              {#each group.subAgents as sub}
                <div class="subagent-card">
                  <div class="subagent-card-header">
                    <span class="sub-dot" style="background: {getStatusColor(sub.status)}"></span>
                    <span class="sub-name">{sub.name}</span>
                    <span class="sub-status" style="color: {getStatusColor(sub.status)}">{sub.status}</span>
                  </div>
                  <div class="subagent-card-meta">
                    <span class="sub-type-badge">{sub.type}</span>
                    {#if sub.task}
                      <span class="sub-task">{sub.task}</span>
                    {/if}
                  </div>
                  <div class="subagent-card-progress">
                    <div class="sub-progress">
                      <div class="sub-progress-fill" style="background: {sub.color}; width: {sub.progress}%"></div>
                    </div>
                    <span class="sub-pct">{sub.progress}%</span>
                  </div>
                </div>
              {/each}
            </div>
          </div>
        {/if}
      </div>
    {/each}
  </div>
</div>

<!-- Chat Panel -->
{#if chatOpen && chatGroup}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="chat-backdrop" onclick={closeChat}></div>
  <div class="chat-panel">
    <!-- Header -->
    <div class="chat-header">
      <div class="chat-agent-info">
        <div class="chat-agent-avatar">
          <span>{chatGroup.icon}</span>
        </div>
        <div class="chat-agent-meta">
          <div class="chat-agent-name">
            {chatGroup.name}
            <span class="chat-status-dot"></span>
          </div>
          <div class="chat-agent-role">{chatGroup.description.split(',')[0]}</div>
        </div>
      </div>
      <button class="chat-close" onclick={closeChat}>✕</button>
    </div>

    <!-- Tabs -->
    <div class="chat-tabs">
      <button
        class="chat-tab"
        class:active={chatTab === 'chat'}
        onclick={() => chatTab = 'chat'}
      >
        <span class="tab-icon">💬</span> Notes
      </button>
      <button
        class="chat-tab"
        class:active={chatTab === 'history'}
        onclick={() => chatTab = 'history'}
      >
        <span class="tab-icon">🕐</span> History
      </button>
      <button
        class="chat-tab"
        class:active={chatTab === 'files'}
        onclick={() => chatTab = 'files'}
      >
        <span class="tab-icon">📁</span> Files
      </button>
    </div>

    <!-- Body -->
    <div class="chat-body">
      {#if chatTab === 'chat'}
        <div class="chat-messages">
          <p class="chat-notes-hint">
            Notes for {chatGroup.name}, kept here for you. Directors don’t read or answer these yet —
            to give {chatGroup.name} work, use the
            <a href={`/console?directorId=${encodeURIComponent(chatGroup.id)}`}>console</a>.
          </p>
          {#if keyedChatMessages.length === 0}
            <div class="tab-empty">
              <span>No notes yet.</span>
            </div>
          {/if}
          {#each keyedChatMessages as { msg, key } (key)}
            <div class="chat-msg" class:agent={msg.isAgent} class:user={!msg.isAgent}>
              {#if msg.isAgent}
                <div class="msg-avatar">{chatGroup.icon}</div>
              {/if}
              <div class="msg-bubble">
                <div class="msg-text">{msg.text}</div>
                <div class="msg-time">{msg.time}</div>
              </div>
            </div>
          {/each}
        </div>

      {:else if chatTab === 'history'}
        <div class="tab-empty-section">
          {#if historyEntries.length === 0}
            <div class="tab-empty">
              <span class="tab-empty-icon">🕐</span>
              <span>No conversation history yet</span>
            </div>
          {:else}
            {#each historyEntries as entry}
              <div class="history-row">
                <div class="history-title">{entry.title}</div>
                {#if entry.preview}
                  <div class="history-preview">{entry.preview}</div>
                {/if}
                <div class="history-meta">{entry.date} · {entry.messages} messages</div>
              </div>
            {/each}
          {/if}
        </div>

      {:else if chatTab === 'files'}
        <div class="tab-empty-section">
          {#if fileEntries.length === 0}
            <div class="tab-empty">
              <span class="tab-empty-icon">📁</span>
              <span>No files shared yet</span>
            </div>
          {:else}
            {#each fileEntries as file}
              <div class="file-row">
                <span class="file-ext-badge" style="background: {extColor(file.ext)}22; color: {extColor(file.ext)}; border-color: {extColor(file.ext)}44">.{file.ext}</span>
                <div class="file-info">
                  <span class="file-name">{file.name}</span>
                  <span class="file-meta">{file.size} · {file.date}</span>
                </div>
              </div>
            {/each}
          {/if}
        </div>
      {/if}
    </div>

    <!-- Input -->
    {#if chatTab === 'chat'}
      {#if chatError}
        <p class="chat-error" role="alert">{chatError}</p>
      {/if}
      <div class="chat-input-bar">
        <input
          class="chat-input"
          type="text"
          placeholder="Add a note for {chatGroup.name}…"
          bind:value={chatInput}
          onkeydown={onKeydown}
          disabled={chatSending}
        />
        <button
          class="chat-send"
          type="button"
          aria-label="Save note"
          onclick={() => void sendMessage()}
          disabled={!chatInput.trim() || chatSending}
        >
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round">
            <line x1="22" y1="2" x2="11" y2="13"></line>
            <polygon points="22 2 15 22 11 13 2 9 22 2"></polygon>
          </svg>
        </button>
      </div>
    {/if}
  </div>
{/if}

<style>
  .director-muscle {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-top: 6px;
  }
  .muscle-label {
    font-size: 0.6875rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    opacity: 0.6;
  }
  .muscle-select {
    font-size: 0.75rem;
    padding: 2px 6px;
    border-radius: 6px;
    border: 1px solid var(--border, #2a2a2a);
    background: var(--bg-card, transparent);
    color: inherit;
    max-width: 190px;
  }
  .muscle-select:disabled {
    opacity: 0.5;
  }
  .muscle-static {
    font-size: 0.75rem;
    opacity: 0.7;
  }
  .muscle-error {
    margin: 6px 0 0;
    font-size: 0.75rem;
    color: #c96a4b;
  }

  .agents-page {
    padding: 16px 20px;
  }

  .agents-title {
    margin: 0 0 12px;
    font-size: 20px;
    font-weight: 700;
    color: var(--text-primary);
  }

  .agents-redirect-hint {
    font-size: 12px;
    color: var(--text-secondary);
    line-height: 1.5;
    margin-bottom: 14px;
    padding: 10px 12px;
    border-radius: 10px;
    border: 1px solid var(--border);
    background: var(--bg-secondary);
  }
  .agents-redirect-hint a {
    color: var(--accent-cyan);
    font-weight: 700;
    margin: 0 4px;
  }

  .agents-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 16px;
    flex-wrap: wrap;
    gap: 8px;
  }

  .agents-stats {
    display: flex;
    align-items: center;
    gap: 16px;
    font-size: 12px;
    color: var(--text-secondary);
    flex-wrap: wrap;
  }

  .stat-item {
    display: flex;
    align-items: center;
    gap: 5px;
  }
  .stat-item strong { color: var(--text-primary); }

  .stat-icon { font-size: 14px; }
  .icon-person { font-size: 12px; }

  .heartbeat-meta { margin-left: 8px; }
  .heartbeat-on { color: var(--accent-green, #7a8163); }
  .heartbeat-off { color: var(--text-muted, #7d7568); }

  .status-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
  }
  .status-dot.running { background: #7a8163; }
  .status-dot.idle { background: #eed9ae; }

  .agents-actions {
    display: flex;
    gap: 6px;
    flex-shrink: 0;
  }

  .add-btn {
    border: 1px solid var(--border);
    border-radius: 5px;
    padding: 5px 12px;
    font-size: 12px;
    font-weight: 600;
  }
  .add-btn.director { background: var(--bg-card); color: var(--text-primary); }
  .add-btn.agent { background: var(--accent-purple); color: #fff; border-color: var(--accent-purple); }

  /* Directors in a horizontal grid – cards side by side, wrap on narrow */
  .directors-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(260px, 1fr));
    gap: 16px;
  }

  @media (min-width: 640px) {
    .directors-grid {
      grid-template-columns: repeat(auto-fill, minmax(280px, 1fr));
      gap: 20px;
    }
  }

  @media (min-width: 1024px) {
    .directors-grid {
      grid-template-columns: repeat(auto-fill, minmax(260px, 1fr));
    }
  }

  .director-card {
    display: flex;
    flex-direction: column;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 10px;
    overflow: hidden;
    min-width: 0;
    transition: border-color 0.2s ease, box-shadow 0.2s ease;
  }

  .director-card.expanded {
    border-color: var(--accent-purple);
    box-shadow: 0 0 0 1px var(--accent-purple);
  }

  .director-card.director-active {
    border-color: rgba(122, 129, 99, 0.5);
    box-shadow: 0 0 0 1px rgba(122, 129, 99, 0.18), 0 0 18px rgba(122, 129, 99, 0.14);
  }

  .director-card-header {
    padding: 14px 14px 8px;
  }

  .director-title-row {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 4px;
  }

  .group-icon { font-size: 20px; flex-shrink: 0; }
  .group-name {
    font-size: 14px;
    font-weight: 600;
    color: var(--text-primary);
    flex: 1;
    min-width: 0;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .director-actions {
    display: flex;
    align-items: center;
    gap: 2px;
    flex-shrink: 0;
  }

  .group-desc {
    font-size: 11px;
    color: var(--text-muted);
    line-height: 1.35;
    margin: 0;
  }

  .director-stats {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 8px 14px;
    font-size: 11px;
    color: var(--text-secondary);
  }

  .director-stat {
    display: flex;
    align-items: center;
    gap: 5px;
  }
  .director-stat strong { color: var(--text-primary); }

  .director-toolbar {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 8px 14px 12px;
    border-top: 1px solid var(--border);
    flex-wrap: wrap;
  }

  .heartbeat-pill {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    padding: 3px 8px;
    border-radius: 999px;
    font-size: 11px;
    border: 1px solid transparent;
    cursor: pointer;
    background: var(--panel-bg, #1e1a15);
    transition: border-color 0.2s ease, box-shadow 0.2s ease, color 0.2s ease;
  }
  .heartbeat-pill.on {
    border-color: var(--accent-green, #7a8163);
    color: var(--accent-green, #7a8163);
    box-shadow: 0 0 0 1px rgba(122, 129, 99, 0.25), 0 0 8px rgba(122, 129, 99, 0.45);
  }
  .heartbeat-pill.on .heartbeat-dot { background: var(--accent-green, #7a8163); }
  .heartbeat-pill.on.active {
    box-shadow: 0 0 0 1px rgba(122, 129, 99, 0.3), 0 0 10px rgba(122, 129, 99, 0.6);
    animation: heartbeatGlowPulse 1.8s ease-in-out infinite;
  }
  .heartbeat-pill.off {
    border-color: var(--text-muted, #7d7568);
    color: var(--text-muted, #7d7568);
  }
  .heartbeat-pill.off .heartbeat-dot { background: var(--text-muted, #7d7568); }
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
    background: var(--accent-green, #7a8163);
    animation: heartbeatWave 1.6s ease-out infinite;
  }
  .heartbeat-label { white-space: nowrap; }

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
    padding: 4px 6px;
    border-radius: 4px;
  }
  .icon-action:hover { background: var(--bg-card-hover); color: var(--text-primary); }
  .chat-btn:hover { color: var(--accent-purple) !important; }

  .subagents-panel {
    border-top: 1px solid var(--border);
    padding: 10px 14px 14px;
    background: rgba(0,0,0,0.2);
    animation: panelOpen 0.18s var(--ease-out);
  }

  @keyframes panelOpen {
    from { opacity: 0; transform: translateY(-6px); }
    to   { opacity: 1; transform: translateY(0); }
  }

  .tags-row { display: flex; gap: 5px; margin-bottom: 8px; flex-wrap: wrap; }

  .tag {
    font-size: 10px;
    padding: 2px 8px;
    border-radius: 3px;
    background: var(--bg-card);
    border: 1px solid var(--border);
    color: var(--text-secondary);
  }

  /* Responsive grid: agents flow horizontally when space allows, wrap to multiple rows */
  .subagents-list {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(200px, 1fr));
    gap: 10px;
  }

  @media (min-width: 900px) {
    .subagents-list {
      grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
      gap: 12px;
    }
  }

  @media (min-width: 1200px) {
    .subagents-list {
      grid-template-columns: repeat(auto-fill, minmax(200px, 1fr));
      gap: 12px;
    }
  }

  .subagent-card {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 10px 12px;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 8px;
    font-size: 11px;
    min-width: 0;
  }

  .subagent-card:hover {
    border-color: var(--text-muted);
    background: var(--bg-card-hover);
  }

  .subagent-card-header {
    display: flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
  }

  .sub-dot { width: 7px; height: 7px; border-radius: 50%; flex-shrink: 0; }
  .sub-name {
    font-weight: 600;
    color: var(--text-primary);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    flex: 1;
    min-width: 0;
  }
  .sub-status { font-weight: 600; font-size: 10px; flex-shrink: 0; }

  .subagent-card-meta {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-wrap: wrap;
    min-width: 0;
  }

  .sub-type-badge {
    background: rgba(242,237,228,0.06);
    border: 1px solid var(--border);
    border-radius: 4px;
    padding: 2px 6px;
    font-size: 10px;
    color: var(--text-secondary);
  }

  .sub-task {
    color: var(--text-muted);
    font-size: 10px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 100%;
  }

  .subagent-card-progress {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 2px;
  }

  .sub-progress {
    flex: 1;
    height: 5px;
    background: var(--border);
    border-radius: 3px;
    overflow: hidden;
    min-width: 0;
  }
  .sub-progress-fill { height: 100%; border-radius: 3px; }
  .sub-pct { font-size: 10px; color: var(--text-secondary); flex-shrink: 0; }

  /* ── Chat Panel ── */
  .chat-backdrop {
    position: fixed;
    inset: 0;
    z-index: 100;
  }

  .chat-panel {
    position: fixed;
    bottom: 24px;
    right: 24px;
    width: 380px;
    max-width: calc(100vw - 32px);
    height: 560px;
    max-height: calc(100vh - 120px);
    background: #16130f;
    border: 1px solid var(--border);
    border-radius: 14px;
    box-shadow: 0 24px 60px rgba(0,0,0,0.7), 0 0 0 1px rgba(99,102,241,0.18);
    display: flex;
    flex-direction: column;
    overflow: hidden;
    z-index: 101;
    animation: slideUp 0.18s ease;
  }

  @keyframes slideUp {
    from { opacity: 0; transform: translateY(16px); }
    to   { opacity: 1; transform: translateY(0); }
  }

  /* Header */
  .chat-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 14px 16px;
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }

  .chat-agent-info { display: flex; align-items: center; gap: 10px; }

  .chat-agent-avatar {
    width: 36px;
    height: 36px;
    border-radius: 10px;
    background: rgba(99,102,241,0.16);
    border: 1px solid rgba(99,102,241,0.34);
    display: flex;
    align-items: center;
    justify-content: center;
    font-size: 18px;
    flex-shrink: 0;
  }

  .chat-agent-meta { display: flex; flex-direction: column; gap: 2px; }

  .chat-agent-name {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 13px;
    font-weight: 700;
    color: var(--text-primary);
  }

  .chat-status-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--accent-orange);
    flex-shrink: 0;
  }

  .chat-agent-role {
    font-size: 11px;
    color: var(--text-muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 240px;
  }

  .chat-close {
    background: none;
    border: none;
    color: var(--text-muted);
    font-size: 14px;
    padding: 4px 6px;
    border-radius: 6px;
    line-height: 1;
    flex-shrink: 0;
  }
  .chat-close:hover { background: var(--bg-card-hover); color: var(--text-primary); }

  /* Tabs */
  .chat-tabs {
    display: flex;
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }

  .chat-tab {
    flex: 1;
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 5px;
    padding: 9px 6px;
    background: none;
    border: none;
    border-bottom: 2px solid transparent;
    font-size: 12px;
    color: var(--text-secondary);
    transition: color 0.15s, border-color 0.15s;
  }
  .chat-tab:hover { color: var(--text-primary); }
  .chat-tab.active {
    color: var(--accent-purple);
    border-bottom-color: var(--accent-purple);
    font-weight: 600;
  }

  .tab-icon { font-size: 13px; }

  /* Body */
  .chat-body {
    flex: 1;
    overflow: hidden;
    display: flex;
    flex-direction: column;
  }

  /* Chat messages */
  .chat-messages {
    flex: 1;
    overflow-y: auto;
    padding: 12px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .chat-notes-hint {
    margin: 0;
    font-size: 12px;
    line-height: 1.5;
    color: var(--text-muted);
  }

  .chat-notes-hint a {
    color: var(--text-secondary);
  }

  .chat-error {
    margin: 0;
    padding: 6px 12px 0;
    font-size: 12px;
    color: var(--color-error, #c96a4b);
  }

  .chat-msg {
    display: flex;
    gap: 8px;
    align-items: flex-end;
  }

  .chat-msg.user { flex-direction: row-reverse; }

  .msg-avatar {
    width: 28px;
    height: 28px;
    border-radius: 8px;
    background: rgba(99,102,241,0.16);
    border: 1px solid rgba(99,102,241,0.3);
    display: flex;
    align-items: center;
    justify-content: center;
    font-size: 14px;
    flex-shrink: 0;
  }

  .msg-bubble {
    max-width: 75%;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 12px 12px 12px 4px;
    padding: 8px 12px;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .chat-msg.user .msg-bubble {
    background: rgba(99,102,241,0.16);
    border-color: rgba(99,102,241,0.34);
    border-radius: 12px 12px 4px 12px;
  }

  .msg-text { font-size: 13px; color: var(--text-primary); line-height: 1.5; }
  .msg-time { font-size: 10px; color: var(--text-muted); }

  /* History / Files empty */
  .tab-empty-section {
    flex: 1;
    overflow-y: auto;
    padding: 12px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .tab-empty {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 8px;
    color: var(--text-muted);
    font-size: 12px;
    height: 100%;
    min-height: 200px;
  }

  .tab-empty-icon { font-size: 28px; opacity: 0.35; }

  /* History rows */
  .history-row {
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 10px 12px;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  .history-title { font-size: 12px; color: var(--text-primary); font-weight: 600; }
  .history-preview { font-size: 11px; color: var(--text-muted); line-height: 1.35; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .history-meta { font-size: 10px; color: var(--text-muted); }

  /* File rows */
  .file-row {
    display: flex;
    align-items: center;
    gap: 10px;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 8px 12px;
  }

  .file-ext-badge {
    font-size: 10px;
    font-weight: 700;
    padding: 2px 6px;
    border-radius: 4px;
    border: 1px solid;
    flex-shrink: 0;
    font-family: monospace;
  }

  .file-info { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
  .file-name { font-size: 12px; color: var(--text-primary); font-weight: 500; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .file-meta { font-size: 10px; color: var(--text-muted); }

  /* Input bar */
  .chat-input-bar {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px 12px;
    border-top: 1px solid var(--border);
    flex-shrink: 0;
    background: #16130f;
  }

  .chat-input {
    flex: 1;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 8px 12px;
    font-size: 13px;
    color: var(--text-primary);
    font-family: var(--font);
    outline: none;
    transition: border-color 0.15s;
  }
  .chat-input::placeholder { color: var(--text-muted); }
  .chat-input:focus { border-color: var(--accent-purple); }

  .chat-send {
    width: 34px;
    height: 34px;
    border-radius: 8px;
    background: var(--accent-purple);
    border: none;
    color: #fff;
    display: flex;
    align-items: center;
    justify-content: center;
    flex-shrink: 0;
    transition: opacity 0.15s;
  }
  .chat-send:hover:not(:disabled) { opacity: 0.85; }
  .chat-send:disabled { opacity: 0.35; cursor: default; }

  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.56);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 120;
    padding: 16px;
  }

  .modal {
    width: min(560px, 100%);
    max-height: 88vh;
    overflow: auto;
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 16px;
  }

  .modal-title {
    margin: 0 0 10px 0;
    font-size: 14px;
    color: var(--text-primary);
  }

  .modal-form {
    display: grid;
    grid-template-columns: 1fr;
    gap: 10px;
  }

  .modal-form label {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: 12px;
    color: var(--text-secondary);
  }

  .modal-form input,
  .modal-form select,
  .modal-form textarea {
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-card);
    color: var(--text-primary);
    font-size: 16px;
    padding: 8px 10px;
  }

  .form-error {
    font-size: 12px;
    color: var(--accent-red);
  }

  .modal-actions {
    margin-top: 12px;
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }

  .btn-secondary {
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 7px 12px;
    background: var(--bg-card);
    color: var(--text-primary);
    font-size: 12px;
  }
</style>
