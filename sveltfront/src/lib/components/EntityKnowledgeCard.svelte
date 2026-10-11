<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import {
    deleteEntityKnowledgeDoc,
    getEntityKnowledge,
    reindexEntityKnowledge,
    setEntityKnowledgeDocEnabled,
    type EntityKnowledgeDoc,
    type EntityKnowledgeIndex,
  } from '$lib/api';
  import { safeHttpUrl } from '$lib/safeUrl';

  // What the engine actually knows about this entity: the source, indexed into
  // docs. Enabled docs ride along with every chat turn and heartbeat, so this
  // card is where the operator sees and prunes them.
  let {
    entityId,
    entityName = '',
    class: className = '',
  }: {
    entityId: string;
    entityName?: string;
    class?: string;
  } = $props();

  let index = $state<EntityKnowledgeIndex | null>(null);
  let docs = $state<EntityKnowledgeDoc[]>([]);
  let loading = $state(true);
  let error = $state('');
  let reindexing = $state(false);
  let reindexReason = $state('');
  let expanded = $state<Record<string, boolean>>({});
  let confirmingRemove = $state<string | null>(null);
  let removing = $state<string | null>(null);
  let pollTimer: ReturnType<typeof setTimeout> | null = null;

  // An index run that has said "indexing" for more than 15 minutes is treated
  // as dead by the gateway too (it died with a restart, or hung), so a fresh
  // re-index is allowed instead of the button staying disabled forever.
  const STALE_INDEXING_MS = 15 * 60 * 1000;
  let now = $state(Date.now());
  const indexingStartedMs = $derived(index?.startedAt ? new Date(index.startedAt).getTime() : NaN);
  const indexingStale = $derived(
    index?.status === 'indexing' &&
      Number.isFinite(indexingStartedMs) &&
      now - indexingStartedMs > STALE_INDEXING_MS
  );
  /** Actively indexing — not merely stuck on the label. */
  const indexing = $derived(index?.status === 'indexing' && !indexingStale);

  function stopPolling() {
    if (pollTimer) {
      clearTimeout(pollTimer);
      pollTimer = null;
    }
  }

  function schedulePoll() {
    stopPolling();
    pollTimer = setTimeout(() => {
      void load(true);
    }, 3000);
  }

  async function load(quiet = false) {
    if (!quiet) loading = true;
    try {
      const res = await getEntityKnowledge(entityId);
      index = res.index ?? null;
      docs = res.docs ?? [];
      error = '';
      now = Date.now();
      if (indexing) schedulePoll();
      else stopPolling();
    } catch (err) {
      // A transient failure while polling shouldn't wipe what's on screen.
      if (!quiet) error = err instanceof Error ? err.message : 'Could not load the knowledge index.';
      now = Date.now();
      if (indexing) schedulePoll();
    } finally {
      loading = false;
    }
  }

  async function reindex() {
    if (reindexing) return;
    reindexing = true;
    reindexReason = '';
    error = '';
    try {
      const res = await reindexEntityKnowledge(entityId);
      if (res.index) index = res.index;
      if (!res.started) {
        reindexReason = res.reason || 'Nothing to index — set a repo path or source first.';
      } else {
        schedulePoll();
      }
    } catch (err) {
      error = err instanceof Error ? err.message : 'Re-index failed to start.';
    } finally {
      reindexing = false;
    }
  }

  async function toggleDoc(doc: EntityKnowledgeDoc) {
    const next = !doc.enabled;
    const previous = doc.enabled;
    doc.enabled = next;
    try {
      const updated = await setEntityKnowledgeDocEnabled(entityId, doc.id, next);
      const row = docs.find((d) => d.id === doc.id);
      if (row && updated) row.enabled = updated.enabled;
    } catch (err) {
      const row = docs.find((d) => d.id === doc.id);
      if (row) row.enabled = previous;
      error = err instanceof Error ? err.message : 'Could not change that doc.';
    }
  }

  async function removeDoc(docId: string) {
    if (removing) return;
    removing = docId;
    try {
      await deleteEntityKnowledgeDoc(entityId, docId);
      docs = docs.filter((d) => d.id !== docId);
      confirmingRemove = null;
    } catch (err) {
      error = err instanceof Error ? err.message : 'Could not remove that doc.';
    } finally {
      removing = null;
    }
  }

  function fmtWhen(iso?: string): string {
    if (!iso) return '';
    const t = new Date(iso).getTime();
    if (Number.isNaN(t)) return '';
    const secs = Math.round((Date.now() - t) / 1000);
    if (secs < 60) return 'just now';
    if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
    if (secs < 86400) return `${Math.floor(secs / 3600)}h ago`;
    if (secs < 604800) return `${Math.floor(secs / 86400)}d ago`;
    return new Date(t).toLocaleDateString();
  }

  onMount(() => {
    void load();
  });

  onDestroy(stopPolling);
</script>

<section class={`knowledge-card ${className}`} aria-label={`Knowledge index${entityName ? ` for ${entityName}` : ''}`}>
  <div class="knowledge-head">
    <h2 class="knowledge-title">Knowledge index</h2>
    {#if indexingStale}
      <span class="knowledge-pill knowledge-pill--failed">Looks stuck</span>
    {:else if indexing}
      <span class="knowledge-pill knowledge-pill--working">
        <span class="knowledge-spinner"></span> Indexing…
      </span>
    {:else if index?.status === 'ready'}
      <span class="knowledge-pill knowledge-pill--ready">
        Indexed {index.docCount} doc{index.docCount === 1 ? '' : 's'}{index.embeddedCount > 0
          ? ` · ${index.embeddedCount} embedded`
          : ''}
        {#if index.indexedAt}<span class="knowledge-when">{fmtWhen(index.indexedAt)}</span>{/if}
      </span>
    {:else if index?.status === 'failed'}
      <span class="knowledge-pill knowledge-pill--failed">Failed</span>
    {:else if index?.status === 'skipped'}
      <span class="knowledge-pill">Not indexed</span>
    {:else}
      <span class="knowledge-pill">Not indexed yet</span>
    {/if}
  </div>

  <p class="knowledge-sub">
    What the engine can draw on for this entity: the source's README, repository map,
    conventions and docs. Enabled docs are injected into every chat turn and heartbeat.
  </p>

  {#if index?.status === 'failed' && index.error}
    <div class="knowledge-error">{index.error}</div>
  {:else if index?.status === 'skipped' && index.error}
    <p class="knowledge-reason">{index.error}</p>
  {/if}

  {#if index?.source}
    <p class="knowledge-source">{index.sourceKind ? `${index.sourceKind}: ` : ''}{index.source}</p>
  {/if}

  <div class="knowledge-actions">
    <button type="button" class="knowledge-btn" onclick={reindex} disabled={reindexing || indexing}>
      {reindexing ? 'Starting…' : 'Re-index'}
    </button>
    {#if indexingStale}
      <span class="knowledge-reason-inline">
        Indexing started {fmtWhen(index?.startedAt)} and never finished — it looks stuck. Re-index to start over.
      </span>
    {/if}
    {#if reindexReason}
      <span class="knowledge-reason-inline">{reindexReason}</span>
    {/if}
  </div>

  {#if error}
    <div class="knowledge-error">{error}</div>
  {/if}

  {#if loading}
    <p class="knowledge-empty">Loading…</p>
  {:else if docs.length === 0}
    {#if indexing}
      <p class="knowledge-empty">Reading the source — docs appear here as they land.</p>
    {:else}
      <p class="knowledge-empty">Nothing indexed yet. Set a repo path or source and press Re-index.</p>
    {/if}
  {:else}
    <ul class="knowledge-docs">
      {#each docs as doc (doc.id)}
        <li class="knowledge-doc" class:knowledge-doc--off={!doc.enabled}>
          <div class="knowledge-doc-row">
            <label class="knowledge-toggle">
              <input
                type="checkbox"
                checked={doc.enabled}
                onchange={() => toggleDoc(doc)}
              />
              <span class="knowledge-doc-title">{doc.title || doc.id}</span>
            </label>
            <div class="knowledge-doc-tools">
              <button
                type="button"
                class="knowledge-link-btn"
                onclick={() => (expanded[doc.id] = !expanded[doc.id])}
              >{expanded[doc.id] ? 'Hide' : 'Show'}</button>
              {#if confirmingRemove === doc.id}
                <button
                  type="button"
                  class="knowledge-link-btn knowledge-link-btn--danger"
                  onclick={() => removeDoc(doc.id)}
                  disabled={removing === doc.id}
                >{removing === doc.id ? 'Removing…' : 'Confirm'}</button>
                <button
                  type="button"
                  class="knowledge-link-btn"
                  onclick={() => (confirmingRemove = null)}
                >Keep</button>
              {:else}
                <button
                  type="button"
                  class="knowledge-link-btn knowledge-link-btn--danger"
                  onclick={() => (confirmingRemove = doc.id)}
                >Remove</button>
              {/if}
            </div>
          </div>
          <div class="knowledge-doc-meta">
            {doc.source}
            <span class="knowledge-dot">·</span>{doc.chars.toLocaleString()} chars
            {#if doc.hasEmbedding}<span class="knowledge-dot">·</span>embedded{/if}
            {#if doc.url}
              {@const docHref = safeHttpUrl(doc.url)}
              <span class="knowledge-dot">·</span>
              {#if docHref}
                <a class="knowledge-doc-url" href={docHref} target="_blank" rel="noreferrer noopener">{doc.url}</a>
              {:else}
                <span class="knowledge-doc-url">{doc.url}</span>
              {/if}
            {/if}
          </div>
          {#if expanded[doc.id]}
            <pre class="knowledge-doc-content">{doc.content}</pre>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .knowledge-card {
    border: 1px solid var(--border);
    border-radius: 2px;
    padding: 14px;
    background: var(--bg-card);
    scroll-margin-top: 72px;
  }

  .knowledge-head {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
  }

  .knowledge-title {
    margin: 0;
    font-size: 1.05rem;
    font-weight: 700;
    color: var(--text-primary);
  }

  .knowledge-pill {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 0.6875rem;
    font-weight: 600;
    color: var(--text-secondary);
    background: var(--bg-card-hover);
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 2px 9px;
  }
  .knowledge-pill--ready { color: var(--accent-green); }
  .knowledge-pill--failed { color: var(--accent-red); }
  .knowledge-pill--working { color: var(--accent-orange); }
  .knowledge-when { color: var(--text-muted); font-weight: 500; }

  .knowledge-spinner {
    width: 10px;
    height: 10px;
    border: 2px solid var(--border);
    border-top-color: var(--accent-orange);
    border-radius: 50%;
    animation: knowledgeSpin 0.8s linear infinite;
  }
  @keyframes knowledgeSpin { to { transform: rotate(360deg); } }

  .knowledge-sub {
    margin: 6px 0 10px;
    font-size: 0.8125rem;
    color: var(--text-muted);
    line-height: 1.45;
  }

  .knowledge-source {
    margin: 0 0 10px;
    font-size: 0.75rem;
    color: var(--text-secondary);
    font-family: var(--font-mono);
    word-break: break-all;
  }

  .knowledge-reason,
  .knowledge-reason-inline {
    font-size: 0.75rem;
    color: var(--text-muted);
    line-height: 1.45;
  }
  .knowledge-reason { margin: 0 0 10px; }

  .knowledge-actions {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
    margin-bottom: 10px;
  }

  .knowledge-btn {
    background: var(--bg-card-hover);
    border: 1px solid var(--border);
    color: var(--text-secondary);
    padding: 4px 12px;
    border-radius: 4px;
    font-size: 0.75rem;
    font-weight: 600;
    cursor: pointer;
  }
  .knowledge-btn:hover:not(:disabled) { color: var(--text-primary); }
  .knowledge-btn:disabled { opacity: 0.5; cursor: default; }

  .knowledge-error {
    font-size: 0.75rem;
    color: var(--color-red-text, #c96a4b);
    background: var(--color-red-dim, rgba(201, 106, 75, 0.16));
    border-radius: 4px;
    padding: 6px 9px;
    margin: 0 0 10px;
    line-height: 1.45;
  }

  .knowledge-empty {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--text-muted);
  }

  .knowledge-docs {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .knowledge-doc {
    border: 1px solid var(--border-subtle, var(--border));
    border-radius: 4px;
    padding: 8px 10px;
    background: var(--bg-secondary);
  }
  .knowledge-doc--off { opacity: 0.55; }

  .knowledge-doc-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
  }

  .knowledge-toggle {
    display: flex;
    align-items: center;
    gap: 8px;
    cursor: pointer;
    min-width: 0;
  }
  .knowledge-toggle input[type='checkbox'] { flex-shrink: 0; }

  .knowledge-doc-title {
    font-size: 0.8125rem;
    font-weight: 600;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .knowledge-doc-tools {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-shrink: 0;
  }

  .knowledge-link-btn {
    background: none;
    border: none;
    padding: 0;
    font-size: 0.6875rem;
    font-weight: 600;
    color: var(--text-muted);
    cursor: pointer;
  }
  .knowledge-link-btn:hover:not(:disabled) { color: var(--text-primary); }
  .knowledge-link-btn:disabled { opacity: 0.5; cursor: default; }
  .knowledge-link-btn--danger { color: var(--accent-red); }

  .knowledge-doc-meta {
    margin-top: 3px;
    font-size: 0.6875rem;
    color: var(--text-muted);
    display: flex;
    align-items: center;
    gap: 4px;
    flex-wrap: wrap;
  }
  .knowledge-dot { opacity: 0.5; }
  .knowledge-doc-url {
    color: var(--text-muted);
    text-decoration: underline;
    max-width: 100%;
    overflow-wrap: anywhere;
  }

  .knowledge-doc-content {
    margin: 8px 0 0;
    max-height: 240px;
    overflow: auto;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    line-height: 1.5;
    color: var(--text-secondary);
    background: var(--bg-primary);
    border: 1px solid var(--border-subtle, var(--border));
    border-radius: 4px;
    padding: 8px;
  }
</style>
