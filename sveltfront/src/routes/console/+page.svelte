<script lang="ts">
  import { onMount } from 'svelte';
  import { page } from '$app/stores';
  import { goto } from '$app/navigation';
  import { api, fetchAllPages } from '$lib/api';
  import Seo from '$lib/components/Seo.svelte';

  type DirectorView = {
    id: string;
    name: string;
    icon: string;
    description: string;
    heartbeatEnabled: boolean;
    autoTaskKinds: string[];
  };

  type ApiAgent = {
    id: string;
    groupId: string;
    name: string;
    role: string;
  };

  type Entity = {
    id: string;
    name: string;
    dept: string;
    stage?: string;
    profile?: string;
  };

  type CommandResponse = {
    task_id: string;
    director_id: string;
    agent_id?: string | null;
    entity_id?: string | null;
  };

  let directors = $state<DirectorView[]>([]);
  let agents = $state<ApiAgent[]>([]);
  let entities = $state<Entity[]>([]);

  let selectedDirectorId = $state<string>('');
  let selectedAgentId = $state<string>('auto');
  let selectedEntityId = $state<string>('');
  let mode = $state<string>('auto');
  let prompt = $state<string>('');

  let isSubmitting = $state(false);
  let errorMessage = $state<string | null>(null);
  let lastCommand: CommandResponse | null = $state(null);

  const agentOptions = $derived(
    selectedDirectorId
      ? agents.filter((a) => a.groupId === selectedDirectorId)
      : []
  );

  // An agent belongs to one director. Switching director must drop an agent
  // picked under the old one, or the command is rejected with a 400.
  $effect(() => {
    if (selectedAgentId !== 'auto' && !agentOptions.some((a) => a.id === selectedAgentId)) {
      selectedAgentId = 'auto';
    }
  });

  const selectedDirector = $derived(
    directors.find((d) => d.id === selectedDirectorId)
  );

  const selectedEntity = $derived(
    entities.find((e) => e.id === selectedEntityId)
  );

  async function load() {
    try {
      const [dirs, ags, entsRes] = await Promise.all([
        api<DirectorView[]>('/api/directors').catch(() => [] as DirectorView[]),
        api<ApiAgent[]>('/api/agents').catch(() => [] as ApiAgent[]),
        fetchAllPages<Entity>('/api/entities').catch(() => [] as Entity[])
      ]);
      directors = dirs ?? [];
      agents = ags ?? [];
      entities = (entsRes ?? []).map((e) => ({
        id: e.id,
        name: e.name,
        dept: (e as any).dept ?? (e as any).department ?? '',
        stage: (e as any).stage,
        profile: (e as any).profile
      }));
      const urlDirector = $page.url.searchParams.get('directorId');
      if (urlDirector && directors.some((d) => d.id === urlDirector)) {
        selectedDirectorId = urlDirector;
      } else if (!selectedDirectorId && directors.length > 0) {
        selectedDirectorId = directors[0].id;
      }
    } catch (err) {
      console.error('Failed to load console data', err);
      errorMessage = 'Failed to load directors, agents, or entities.';
    }
  }

  onMount(load);

  async function submitCommand() {
    if (isSubmitting) return;
    errorMessage = null;
    lastCommand = null;
    if (!selectedDirectorId) {
      errorMessage = 'Please choose a director.';
      return;
    }
    if (!prompt.trim()) {
      errorMessage = 'Please enter a prompt.';
      return;
    }
    isSubmitting = true;
    try {
      const body: any = {
        director_id: selectedDirectorId,
        agent_id: selectedAgentId === 'auto' ? null : selectedAgentId,
        entity_id: selectedEntityId || null,
        prompt: prompt.trim(),
        mode: mode || null
      };
      const resp = await api<CommandResponse>('/api/commands', {
        method: 'POST',
        body: JSON.stringify(body),
        silent: true
      });
      lastCommand = resp;
      prompt = '';
    } catch (err: any) {
      console.error('Command failed', err);
      errorMessage = err?.message ?? 'Command failed';
    } finally {
      isSubmitting = false;
    }
  }
</script>

<Seo title="Console" noindex />

<div class="console-page">
  <div class="page-header">
    <div class="header-left">
      <h1 class="header-title">Console</h1>
      <span class="header-subtitle">Turn a prompt into tasks under a director</span>
    </div>
  </div>

  <div class="console-layout">
    <section class="console-main">
      <div class="controls-grid">
        <div class="field">
          <label for="entity-select">Entity</label>
          <select id="entity-select" bind:value={selectedEntityId}>
            <option value="">None</option>
            {#each entities as e}
              <option value={e.id}>{e.name}</option>
            {/each}
          </select>
          <button class="field-add-btn" type="button" onclick={() => goto('/entities?create=1')}>+ Create entity</button>
        </div>

        <div class="field">
          <label for="director-select">Director<span class="required">*</span></label>
          <select id="director-select" bind:value={selectedDirectorId}>
            <option value="">Select director…</option>
            {#each directors as d}
              <option value={d.id}>{d.icon} {d.name}</option>
            {/each}
          </select>
          <button class="field-add-btn" type="button" onclick={() => goto('/agents?create=director')}>+ Create director</button>
        </div>

        <div class="field">
          <label for="agent-select">Agent</label>
          <select id="agent-select" bind:value={selectedAgentId} disabled={!selectedDirectorId}>
            <option value="auto">Automatic (director decides)</option>
            {#each agentOptions as a}
              <option value={a.id}>{a.name} — {a.role}</option>
            {/each}
          </select>
          <button class="field-add-btn" type="button" onclick={() => goto('/agents?create=agent')}>+ Create agent</button>
        </div>

        <div class="field">
          <label for="mode-select">Mode</label>
          <select id="mode-select" bind:value={mode}>
            <option value="auto">Execute automatically</option>
            <option value="decision-required">Decision required</option>
            <option value="draft-only">Draft only (no auto-run)</option>
          </select>
        </div>
      </div>

      <div class="prompt-card">
        <label for="prompt">Prompt</label>
        <textarea
          id="prompt"
          rows="5"
          bind:value={prompt}
          placeholder="Describe what you want this director/agent to do…"
        ></textarea>
        <div class="prompt-actions">
          {#if errorMessage}
            <span class="error-text">{errorMessage}</span>
          {/if}
          <button class="btn-primary" onclick={submitCommand} disabled={isSubmitting}>
            {isSubmitting ? 'Sending…' : 'Send command'}
          </button>
        </div>
      </div>

      {#if lastCommand}
        <div class="result-card">
          <div class="result-header">
            <span class="result-title">Command created</span>
          </div>
          <p class="result-body">
            Task <code>{lastCommand.task_id}</code> was created for director
            <code>{lastCommand.director_id}</code>
            {#if lastCommand.agent_id}
              with agent <code>{lastCommand.agent_id}</code>
            {/if}
            {#if lastCommand.entity_id}
              and entity <code>{lastCommand.entity_id}</code>
            {/if}
            .
          </p>
          <div class="result-links">
            <a href="/tasks" class="link">Open Tasks board</a>
          </div>
        </div>
      {/if}
    </section>

    <aside class="console-sidebar">
      <div class="sidebar-section">
        <div class="sidebar-title">Director</div>
        {#if selectedDirector}
          <p class="sidebar-line">
            <span class="mono">{selectedDirector.icon} {selectedDirector.name}</span>
          </p>
          <p class="sidebar-text">{selectedDirector.description}</p>
        {:else}
          <p class="sidebar-muted">Choose a director to route this command.</p>
        {/if}
      </div>

      <div class="sidebar-section">
        <div class="sidebar-title">Entity</div>
        {#if selectedEntity}
          <p class="sidebar-line">
            <span class="mono">{selectedEntity.name}</span>
            {#if selectedEntity.stage}
              <span class="badge">{selectedEntity.stage}</span>
            {/if}
          </p>
          {#if selectedEntity.profile}
            <p class="sidebar-text clamp">{selectedEntity.profile}</p>
          {:else}
            <p class="sidebar-muted">No profile text for this entity yet.</p>
          {/if}
        {:else}
          <p class="sidebar-muted">Optional but recommended: link the command to an entity.</p>
        {/if}
      </div>

      <div class="sidebar-section">
        <div class="sidebar-title">Mode</div>
        <p class="sidebar-text">
          {#if mode === 'decision-required'}
            Tasks will be created in the <strong>Decision required</strong> lane and won’t auto-run.
          {:else if mode === 'draft-only'}
            Tasks will stay in <strong>Backlog</strong> for manual review before execution.
          {:else}
            Tasks will be <strong>Queued</strong> for automatic execution when the director heartbeat is on.
          {/if}
        </p>
      </div>
    </aside>
  </div>
</div>

<style>
  .console-page {
    padding: var(--page-padding);
    height: calc(100vh - var(--topbar-height));
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    overflow: hidden;
  }

  .page-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    flex-shrink: 0;
  }

  .header-left {
    display: flex;
    align-items: baseline;
    flex-wrap: wrap;
    gap: var(--space-2);
  }

  .header-title {
    margin: 0;
    font-size: 20px;
    font-weight: 700;
    color: var(--text-primary);
  }

  .header-subtitle {
    font-size: 13px;
    color: var(--text-muted);
  }

  .console-layout {
    display: grid;
    grid-template-columns: minmax(0, 2.1fr) minmax(260px, 0.9fr);
    gap: var(--space-3);
    flex: 1;
    min-height: 0;
  }

  @media (max-width: 900px) {
    .console-layout {
      grid-template-columns: minmax(0, 1fr);
    }
  }

  .console-main {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    min-width: 0;
  }

  .controls-grid {
    display: grid;
    grid-template-columns: repeat(4, minmax(0, 1fr));
    gap: var(--space-2);
  }

  @media (max-width: 900px) {
    .controls-grid {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
  }

  @media (max-width: 560px) {
    .controls-grid {
      grid-template-columns: minmax(0, 1fr);
    }
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .field-add-btn {
    align-self: flex-start;
    min-height: 32px;
    border: 1px dashed var(--accent-cyan);
    border-radius: var(--radius-md);
    background: transparent;
    color: var(--accent-cyan);
    font-size: var(--text-2xs);
    font-weight: var(--font-semibold);
    padding: 0 var(--space-2);
  }

  .field-add-btn:hover {
    background: rgba(201, 138, 75, 0.08);
  }

  .field label {
    font-size: var(--text-2xs);
    color: var(--text-secondary);
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }

  .required {
    color: var(--accent-red);
    margin-left: 2px;
  }

  select {
    height: var(--btn-height-sm);
    border-radius: var(--radius-md);
    border: 1px solid var(--border);
    background: var(--bg-card);
    color: var(--text-primary);
    font-size: var(--text-xs);
    padding: 0 var(--space-2);
  }

  .prompt-card {
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: var(--radius-lg);
    padding: var(--space-3);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  .prompt-card label {
    font-size: var(--text-xs);
    font-weight: var(--font-semibold);
    color: var(--text-secondary);
  }

  textarea {
    width: 100%;
    border-radius: var(--radius-md);
    border: 1px solid var(--border);
    background: var(--bg-card);
    color: var(--text-primary);
    font-size: var(--text-sm);
    padding: var(--space-2);
    resize: vertical;
    min-height: 120px;
  }

  .prompt-actions {
    display: flex;
    justify-content: flex-end;
    align-items: center;
    gap: var(--space-2);
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
  }

  .btn-primary:disabled {
    opacity: 0.6;
    cursor: default;
  }

  .error-text {
    font-size: var(--text-xs);
    color: var(--accent-red);
    margin-right: auto;
  }

  .result-card {
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: var(--radius-lg);
    padding: var(--space-3);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  .result-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }

  .result-title {
    font-size: var(--text-xs);
    font-weight: var(--font-semibold);
    color: var(--text-secondary);
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }

  .result-body {
    font-size: var(--text-xs);
    color: var(--text-secondary);
  }

  .result-body code {
    font-size: var(--text-2xs);
    background: var(--bg-card);
    padding: 1px 6px;
    border-radius: var(--radius-sm);
  }

  .result-links {
    display: flex;
    justify-content: flex-start;
  }

  .link {
    font-size: var(--text-xs);
    color: var(--accent-purple);
    text-decoration: none;
  }

  .link:hover {
    text-decoration: underline;
  }

  .console-sidebar {
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: var(--radius-lg);
    padding: var(--space-3);
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    min-height: 0;
  }

  .sidebar-section {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .sidebar-title {
    font-size: var(--text-xs);
    font-weight: var(--font-semibold);
    color: var(--text-primary);
  }

  .sidebar-line {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--text-xs);
    color: var(--text-secondary);
  }

  .sidebar-text {
    font-size: var(--text-xs);
    color: var(--text-secondary);
  }

  .sidebar-muted {
    font-size: var(--text-xs);
    color: var(--text-muted);
  }

  .mono {
    font-family: var(--font-mono);
  }

  .badge {
    font-size: var(--text-2xs);
    padding: 1px 6px;
    border-radius: var(--radius-full);
    background: var(--bg-card);
    border: 1px solid var(--border);
  }

  .clamp {
    line-clamp: 5;
    display: -webkit-box;
    -webkit-line-clamp: 5;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
</style>

