<script lang="ts">
  import { goto } from '$app/navigation';
  import { onMount } from 'svelte';
  import { api, deleteEntity, fetchAllPages } from '$lib/api';
  import EntityImportModal from '$lib/components/EntityImportModal.svelte';
  import Seo from '$lib/components/Seo.svelte';

  type EntityFile = { name: string; size: string };

  type Relationship = { type: string; target: string; status: string; notes?: string };

  type EntityEnrichment = {
    status: string;
    error?: string;
    draft?: Record<string, string>;
  };

  type EntityReadiness = { percent: number; label: string; nextQuestion?: string; missingFields?: string[] };
  type EntitySetup = { whatItIs?: string; primaryGoal?: string; successCriteria?: string; primaryDomain?: string; primaryEmail?: string; repoPath?: string; backendStack?: string; frontendStack?: string; databaseStack?: string };

  type Entity = {
    id: string;
    name: string;
    status: string;
    isPersonal?: boolean;
    type?: string;
    dept: string;
    fileCount: number;
    path?: string;
    expanded?: boolean;
    entityMeta?: string;
    files?: EntityFile[];
    stage?: string;
    domains?: string[];
    markets?: string[];
    profile?: string;
    goals?: string;
    activeContext?: string;
    operatingRules?: string;
    notes?: string;
    contentPipeline?: string;
    relationships?: Relationship[];
    enrichment?: EntityEnrichment;
    setup?: EntitySetup;
    readiness?: EntityReadiness;
  };

  let entities = $state<Entity[]>([]);
  let showCreateModal = $state(false);
  let createName = $state('');
  let createDescription = $state('');
  let creating = $state(false);
  let createError = $state('');

  // Import flow lives in EntityImportModal; this page only opens it and
  // navigates to whatever entity the commit produced.
  let showImportModal = $state(false);

  let deleteTarget = $state<Entity | null>(null);
  let deleting = $state(false);
  let deleteError = $state('');

  async function confirmDelete() {
    if (!deleteTarget || deleting) return;
    deleting = true;
    deleteError = '';
    try {
      await deleteEntity(deleteTarget.id);
      deleteTarget = null;
      await loadEntities();
    } catch (err) {
      deleteError = err instanceof Error ? err.message : 'Delete failed';
    } finally {
      deleting = false;
    }
  }

  async function importDone(entity: { id?: string }) {
    showImportModal = false;
    await loadEntities();
    if (entity?.id) goto(`/entities/${entity.id}`);
  }

  const totalFiles = $derived(entities.reduce((s, e) => s + (e.fileCount ?? 0), 0));
  const activeCount = $derived(entities.filter(e => e.status === 'Active').length);

  async function loadEntities() {
    try {
      // All pages: the gateway's default page is 50, so entity #51 onward
      // never appeared here.
      const rows = await fetchAllPages<Entity>('/api/entities');
      entities = rows.map(e => ({ ...e, expanded: false }));
    } catch (err) {
      console.error('Failed to load entities', err);
    }
  }

  function openCreate() {
    createError = '';
    showCreateModal = true;
  }

  async function createProject() {
    if (!createName.trim() || creating) return;
    creating = true;
    createError = '';
    try {
      const created = await api<Entity>('/api/entities', {
        method: 'POST',
        body: JSON.stringify({
          name: createName.trim(),
          description: createDescription.trim() || undefined,
        }),
        silent: true
      });
      showCreateModal = false;
      createName = '';
      createDescription = '';
      await loadEntities();
      if (created?.id) goto(`/entities/${created.id}`);
    } catch (err) {
      createError = err instanceof Error && err.message ? err.message : 'Could not create the entity.';
    } finally {
      creating = false;
    }
  }

  function toggle(id: string) {
    const e = entities.find(e => e.id === id);
    if (e) e.expanded = !e.expanded;
  }

  function getStatusColor(status: string) {
    return status === 'Active' ? '#7a8163' : '#e0a55e';
  }

  onMount(loadEntities);
  onMount(() => {
    const createParam = new URL(window.location.href).searchParams.get('create');
    if (createParam === '1' || createParam === 'true') {
      openCreate();
    }
  });
</script>

<Seo title="Entities" noindex />

<div class="entities-page">
  <div class="page-header">
    <h1 class="page-title">Entities</h1>
    <div class="header-actions">
      <button class="import-btn" onclick={() => showImportModal = true}>Import existing</button>
      <button class="new-project-btn" onclick={openCreate}>New entity</button>
    </div>
  </div>

  {#if showCreateModal}
    <div
      class="modal-backdrop"
      role="button"
      tabindex="0"
      onclick={() => (showCreateModal = false)}
      onkeydown={(e) => e.key === 'Escape' && (showCreateModal = false)}
    >
      <div
        class="modal"
        role="dialog"
        tabindex="-1"
        onmousedown={(e) => e.stopPropagation()}
        onclick={(e) => e.stopPropagation()}
        onkeydown={(e) => e.stopPropagation()}
      >
        <h3 class="modal-title">Create entity</h3>
        <p class="empty-copy" style="margin-top:6px;">Start with the name. Operations will onboard it from there.</p>
        <div class="modal-form">
          <label>Name <input type="text" bind:value={createName} placeholder="Entity name" /></label>
          <label>Description <textarea bind:value={createDescription} placeholder="Optional" rows="2"></textarea></label>
        </div>
        {#if createError}
          <p class="delete-error">{createError}</p>
        {/if}
        <div class="modal-actions">
          <button class="btn-secondary" disabled={creating} onclick={() => showCreateModal = false}>Cancel</button>
          <button class="btn-primary" disabled={creating || !createName.trim()} onclick={createProject}>
            {creating ? 'Creating…' : 'Create'}
          </button>
        </div>
      </div>
    </div>
  {/if}

  {#if showImportModal}
    <EntityImportModal onclose={() => (showImportModal = false)} ondone={importDone} />
  {/if}

  {#if deleteTarget}
    <div
      class="modal-backdrop"
      role="button"
      tabindex="0"
      onclick={() => { if (!deleting) deleteTarget = null; }}
      onkeydown={(e) => e.key === 'Escape' && !deleting && (deleteTarget = null)}
    >
      <div
        class="modal"
        role="dialog"
        tabindex="-1"
        onmousedown={(e) => e.stopPropagation()}
        onclick={(e) => e.stopPropagation()}
        onkeydown={(e) => e.stopPropagation()}
      >
        <h3 class="modal-title">Delete “{deleteTarget.name}”?</h3>
        <p class="empty-copy" style="margin-top:6px;">
          Removes the entity and its cloud data — heartbeats, tasks, missions,
          canonical docs, and entity memory. Files in the local vault folder
          stay on disk.
        </p>
        {#if deleteError}
          <p class="delete-error">{deleteError}</p>
        {/if}
        <div class="modal-actions">
          <button class="btn-secondary" disabled={deleting} onclick={() => (deleteTarget = null)}>Cancel</button>
          <button class="btn-danger" disabled={deleting} onclick={confirmDelete}>
            {deleting ? 'Deleting…' : 'Delete entity'}
          </button>
        </div>
      </div>
    </div>
  {/if}

  <div class="summary-bar">
    <span class="summary-item">
      Entities: <strong>{entities.length}</strong>
    </span>
    <span class="summary-item">
      Files: <strong>{totalFiles}</strong>
    </span>
    <span class="summary-item">
      <span class="status-dot"></span>
      Active: <strong>{activeCount}</strong>
    </span>
  </div>

  <div class="entities-grid">
    {#if entities.length === 0}
      <div class="empty-entities">
        <h3>No entities yet</h3>
        <p>Create your first entity to begin onboarding, tasks, missions, and execution.</p>
        <div class="empty-entity-actions">
          <button class="new-project-btn" onclick={openCreate}>Create first entity</button>
          <button class="import-btn" onclick={() => showImportModal = true}>Import an existing project</button>
          <a class="view-link" href="/">Open Home capture</a>
        </div>
      </div>
    {/if}
    {#each entities as entity}
      <div class="entity-card" class:expanded={entity.expanded}>
        <div
          class="entity-card-header"
          class:is-expandable={entity.files && entity.files.length > 0}
          role="button"
          tabindex="0"
          aria-disabled={!(entity.files && entity.files.length > 0)}
          onclick={() => {
            if (entity.files && entity.files.length > 0) toggle(entity.id);
          }}
          onkeydown={(e) => {
            if ((e.key === 'Enter' || e.key === ' ') && entity.files && entity.files.length > 0) {
              e.preventDefault();
              toggle(entity.id);
            }
          }}
        >
          <span class="expand-btn" aria-hidden="true">
            {#if entity.files && entity.files.length > 0}
              {entity.expanded ? '▼' : '▶'}
            {:else}
              <span style="opacity:0">▶</span>
            {/if}
          </span>
          <span class="entity-icon">{entity.type ? '🟡' : '🖥'}</span>
          <div class="entity-name-block">
            <span class="entity-name">{entity.name}</span>
            <span class="entity-status-label">Lifecycle:</span>
            <span class="entity-status" style="color: {getStatusColor(entity.status)}">{entity.status}</span>
            {#if entity.enrichment}
              {#if entity.enrichment.status === 'pending' || entity.enrichment.status === 'running'}
                <span class="enrichment-pill drafting">Enrichment draft: running</span>
              {:else if entity.enrichment.status === 'ready'}
                <span class="enrichment-pill ready">Enrichment draft: ready</span>
              {:else if entity.enrichment.status === 'provider_invalid'}
                <span class="enrichment-pill provider-invalid">Enrichment draft: invalid provider</span>
              {:else if entity.enrichment.status === 'failed'}
                <span class="enrichment-pill failed">Enrichment draft: failed</span>
              {:else if entity.enrichment.status === 'ai_unavailable'}
                <span class="enrichment-pill unavailable">Enrichment draft: AI unavailable</span>
              {/if}
            {/if}
          </div>
        </div>

        {#if entity.type}
          <div class="entity-type">{entity.type}</div>
        {/if}
        {#if entity.readiness}
          <div class="entity-stage">Readiness: {entity.readiness.percent}% · {entity.readiness.label}</div>
          {#if entity.readiness.nextQuestion}
            <div class="entity-markets">Next question: {entity.readiness.nextQuestion}</div>
          {/if}
        {/if}
        {#if entity.stage}
          <div class="entity-stage">Stage: {entity.stage}</div>
        {/if}
        {#if entity.markets && entity.markets.length > 0}
          <div class="entity-markets">Markets: {entity.markets.join(', ')}</div>
        {/if}

        <div class="entity-meta">
          <span class="meta-dept">{entity.dept || '—'}</span>
          <span class="meta-sep">•</span>
          <span class="meta-files">{entity.fileCount ?? 0} files</span>
          {#if entity.path}
            <span class="meta-sep">•</span>
            <span class="meta-path">{entity.path}</span>
          {/if}
        </div>

        <div class="entity-actions">
          <a href="/entities/{entity.id}" class="view-link">View →</a>
          {#if !entity.isPersonal}
            <button class="delete-link" onclick={() => { deleteError = ''; deleteTarget = entity; }}>Delete</button>
          {/if}
        </div>

        {#if entity.expanded && entity.files}
          <div class="entity-details">
            {#if entity.entityMeta}
              <div class="entity-meta-str">{entity.entityMeta}</div>
            {/if}
            <div class="files-section">
              <div class="files-header">
                <span>Files</span>
                <button class="upload-btn">↑ Upload</button>
              </div>
              <div class="files-list">
                {#each entity.files as file}
                  <div class="file-row">
                    <span class="file-icon">📄</span>
                    <span class="file-name">{file.name}</span>
                    <span class="file-size">{file.size}</span>
                  </div>
                {/each}
              </div>
            </div>
          </div>
        {/if}
      </div>
    {/each}
  </div>
</div>

<style>
  .entities-page {
    padding: 16px 20px;
  }

  .page-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 12px;
    flex-wrap: wrap;
    gap: 8px;
  }

  .page-title {
    margin: 0;
    font-size: 20px;
    font-weight: 700;
    color: var(--text-primary);
  }

  .new-project-btn {
    background: linear-gradient(180deg, rgba(201, 138, 75, 0.28), rgba(138, 90, 46, 0.6));
    color: #ffffff;
    border: 1px solid rgba(201, 138, 75, 0.45);
    border-radius: 2px;
    padding: 8px 14px;
    font-size: 12px;
    font-weight: 700;
    box-shadow: none;
    transition: border-color 0.15s ease;
  }
  .new-project-btn:hover {
    border-color: rgba(94, 234, 212, 0.66);
  }

  .summary-bar {
    display: flex;
    align-items: center;
    gap: 16px;
    font-size: 12px;
    color: var(--text-secondary);
    margin-bottom: 16px;
    flex-wrap: wrap;
  }

  .summary-item {
    display: flex;
    align-items: center;
    gap: 5px;
  }
  .summary-item strong { color: var(--text-primary); }

  .status-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: #7a8163;
  }

  .entities-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 10px;
  }
  .empty-entities {
    grid-column: 1 / -1;
    border: 1px dashed var(--border);
    border-radius: 2px;
    padding: 14px;
    background: var(--bg-card);
    display: grid;
    gap: 8px;
  }
  .empty-entities h3 {
    margin: 0;
    font-size: 14px;
    color: var(--text-primary);
  }
  .empty-entities p {
    margin: 0;
    font-size: 12px;
    color: var(--text-secondary);
  }
  .empty-entity-actions {
    display: grid;
    grid-template-columns: 1fr;
    gap: 8px;
  }

  @media (max-width: 640px) {
    .entities-grid { grid-template-columns: 1fr; }
  }

  .entity-card {
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 2px;
    padding: var(--space-3);
    cursor: default;
    transition: border-color var(--transition-base);
  }

  .entity-card:hover {
    border-color: var(--color-border-subtle);
  }

  .entity-card.expanded {
    border-color: var(--accent-purple);
    box-shadow: 0 0 0 1px var(--color-purple-border);
  }

  .entity-card-header {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin-bottom: var(--space-2);
    border-radius: var(--radius-md);
    padding: 2px 0;
    transition: background var(--transition-base);
  }

  .entity-card-header.is-expandable {
    cursor: pointer;
  }
  .entity-card-header.is-expandable:hover {
    background: var(--bg-card-hover);
  }

  .expand-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 14px;
    font-size: var(--text-2xs);
    color: var(--text-muted);
    flex-shrink: 0;
    transition: color var(--transition-base);
  }

  .entity-card-header.is-expandable:hover .expand-btn {
    color: var(--text-secondary);
  }

  .entity-icon { font-size: 16px; }

  .entity-name-block {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .entity-name {
    font-size: 13px;
    font-weight: 700;
    color: var(--text-primary);
  }

  .entity-status-label {
    font-size: 10px;
    text-transform: uppercase;
    color: var(--text-muted);
    letter-spacing: 0.04em;
  }

  .entity-status {
    font-size: 11px;
    font-weight: 600;
  }

  .enrichment-pill {
    font-size: 10px;
    font-weight: 600;
    padding: 2px 6px;
    border-radius: 4px;
    margin-left: 6px;
  }
  .enrichment-pill.drafting {
    background: rgba(122, 129, 99, 0.15);
    color: var(--accent-cyan, #c98a4b);
  }
  .enrichment-pill.ready {
    background: rgba(122, 129, 99, 0.15);
    color: var(--accent-green, #7a8163);
  }
  .enrichment-pill.failed {
    background: rgba(201, 106, 75, 0.1);
    color: var(--color-error, #c96a4b);
  }
  .enrichment-pill.provider-invalid {
    background: rgba(138, 90, 46, 0.12);
    color: #ea580c;
  }
  .enrichment-pill.unavailable {
    background: rgba(224, 165, 94, 0.12);
    color: var(--color-warning, #e0a55e);
  }

  .entity-type {
    font-size: 11px;
    color: var(--text-muted);
    margin-bottom: 4px;
    padding-left: 20px;
  }

  .entity-stage {
    font-size: 11px;
    color: var(--text-muted);
    margin-bottom: 4px;
    padding-left: 20px;
  }

  .entity-markets {
    font-size: 11px;
    color: var(--text-secondary);
    margin-bottom: 8px;
    padding-left: 20px;
  }

  .entity-actions {
    margin-top: var(--space-2);
    padding-top: var(--space-2);
    border-top: 1px solid var(--border);
    display: flex;
    align-items: center;
    justify-content: space-between;
  }

  .delete-link {
    background: none;
    border: none;
    padding: 0;
    font-size: 12px;
    font-weight: 600;
    color: var(--color-error, #c96a4b);
    cursor: pointer;
  }
  .delete-link:hover { text-decoration: underline; }

  .delete-error {
    font-size: 12px;
    color: var(--color-error, #c96a4b);
    margin: 6px 0 0;
  }

  .btn-danger {
    background: var(--color-error, #c96a4b);
    color: #fff;
    border: none;
    padding: 6px 14px;
    border-radius: 6px;
    font-weight: 600;
    cursor: pointer;
  }
  .btn-danger:disabled, .btn-secondary:disabled { opacity: 0.6; cursor: default; }

  .view-link {
    font-size: 12px;
    font-weight: 600;
    color: var(--accent-purple);
    text-decoration: none;
  }
  .view-link:hover { text-decoration: underline; }

  .entity-meta {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 11px;
    color: var(--text-muted);
    flex-wrap: wrap;
  }

  .meta-dept { color: var(--accent-cyan); }
  .meta-sep { color: var(--border); }
  .meta-path {
    font-size: 10px;
    color: var(--text-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 200px;
  }

  .entity-details {
    margin-top: var(--space-3);
    border-top: 1px solid var(--border);
    padding-top: var(--space-2);
    animation: detailsOpen 0.18s var(--ease-out);
  }

  @keyframes detailsOpen {
    from { opacity: 0; transform: translateY(-4px); }
    to   { opacity: 1; transform: translateY(0); }
  }

  .entity-meta-str {
    font-size: 10px;
    color: var(--text-muted);
    margin-bottom: 10px;
    font-family: monospace;
    line-height: 1.6;
  }

  .files-section { display: flex; flex-direction: column; gap: 6px; }

  .files-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-size: 12px;
    font-weight: 600;
    color: var(--text-secondary);
  }

  .upload-btn {
    background: none;
    border: 1px solid var(--border);
    color: var(--text-secondary);
    border-radius: 4px;
    padding: 2px 8px;
    font-size: 11px;
  }
  .upload-btn:hover { background: var(--bg-card-hover); }

  .files-list {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .file-row {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 4px 6px;
    border-radius: 4px;
    font-size: 11px;
  }
  .file-row:hover { background: var(--bg-card-hover); }

  .file-icon { font-size: 12px; }
  .file-name { flex: 1; color: var(--text-primary); }
  .file-size { color: var(--text-muted); font-size: 10px; }

  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0,0,0,0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 100;
    padding: 16px;
  }
  .modal {
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: var(--radius-lg);
    padding: 20px;
    max-width: 400px;
    width: 100%;
  }
  .modal-title { font-size: 14px; font-weight: 700; margin-bottom: 12px; color: var(--text-primary); }
  .modal-form { display: flex; flex-direction: column; gap: 10px; margin-bottom: 16px; }
  .modal-form label { font-size: 12px; color: var(--text-secondary); display: flex; flex-direction: column; gap: 4px; }
  .modal-form input, .modal-form textarea {
    padding: 6px 10px; border-radius: 6px; border: 1px solid var(--border);
    background: var(--bg-card); color: var(--text-primary); font-size: 14px;
  }
  .modal-actions { display: flex; justify-content: flex-end; gap: 8px; }

  .header-actions { display: flex; align-items: center; gap: 8px; }

  .import-btn {
    background: var(--bg-card);
    color: var(--text-primary);
    border: 1px solid var(--border);
    border-radius: 2px;
    padding: 8px 14px;
    font-size: 12px;
    font-weight: 700;
    transition: border-color 0.15s ease;
  }
  .import-btn:hover { border-color: rgba(94, 234, 212, 0.66); }
  .btn-primary { background: var(--accent-purple); color: #fff; border: none; padding: 6px 14px; border-radius: 6px; font-weight: 600; cursor: pointer; }
  .btn-secondary { background: var(--bg-card); border: 1px solid var(--border); color: var(--text-secondary); padding: 6px 14px; border-radius: 6px; cursor: pointer; }
</style>
