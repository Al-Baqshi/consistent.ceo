<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { goto } from '$app/navigation';
  import { listActivity, type ActivityItem, type ActivitySource } from '$lib/api';
  import Seo from '$lib/components/Seo.svelte';
  import { feedKindLabel, feedKindTone, type FeedTone } from '$lib/feedKinds';

  let items = $state<ActivityItem[]>([]);
  let loading = $state(false);
  let error = $state<string | null>(null);
  let sourceFilter = $state<'all' | ActivitySource>('all');
  let timer: ReturnType<typeof setInterval> | undefined;

  const sources: { id: 'all' | ActivitySource; label: string }[] = [
    { id: 'all', label: 'All' },
    { id: 'feed', label: 'Events' },
    { id: 'capture', label: 'Captures' },
    { id: 'agent_run', label: 'Agent runs' },
    { id: 'verification', label: 'Verification' }
  ];

  async function load() {
    loading = true;
    try {
      items = await listActivity({
        source: sourceFilter === 'all' ? undefined : sourceFilter,
        limit: 100
      });
      error = null;
    } catch (err) {
      error = err instanceof Error ? err.message : 'Could not load the activity timeline.';
    } finally {
      loading = false;
    }
  }

  function setSource(id: 'all' | ActivitySource) {
    sourceFilter = id;
    void load();
  }

  onMount(() => {
    void load();
    timer = setInterval(load, 15000);
  });
  onDestroy(() => {
    if (timer) clearInterval(timer);
  });

  function dotColor(it: ActivityItem): string {
    if (it.source === 'capture') return 'var(--accent-blue)';
    if (it.source === 'verification')
      return it.status === 'passed'
        ? 'var(--accent-green)'
        : it.status === 'failed'
          ? 'var(--accent-red)'
          : 'var(--accent-orange)';
    if (it.source === 'agent_run')
      return it.status === 'failed' || it.status === 'stale'
        ? 'var(--accent-red)'
        : 'var(--accent-purple, #4a3d52)';
    return TONE_COLOR[feedKindTone(it.kind)];
  }

  const TONE_COLOR: Record<FeedTone, string> = {
    alert: 'var(--accent-red)',
    warn: 'var(--accent-orange)',
    good: 'var(--accent-green)',
    info: 'var(--accent-blue)'
  };

  function kindLabel(it: ActivityItem): string {
    if (it.source === 'capture') return 'Capture';
    if (it.source === 'agent_run') return 'Agent run';
    if (it.source === 'verification') return it.kind === 'verification_browser' ? 'Browser check' : 'Test check';
    return feedKindLabel(it.kind);
  }

  function formatTime(iso: string): string {
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    return d.toLocaleString(undefined, {
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit'
    });
  }

  function onRowClick(it: ActivityItem) {
    if (it.missionId) {
      goto(`/missions/${it.missionId}`);
    } else if (it.entityId) {
      goto(`/entities/${it.entityId}`);
    }
  }
</script>

<Seo title="Activity" noindex />

<div class="activity-page">
  <header class="activity-head">
    <div>
      <h1 class="activity-title">Activity</h1>
      <p class="activity-sub">
        One timeline: captures, tasks, agent runs, verification, approvals, and mission progress.
      </p>
    </div>
    <button type="button" class="activity-refresh" onclick={() => void load()} disabled={loading}>
      {loading ? 'Refreshing…' : 'Refresh'}
    </button>
  </header>

  <div class="activity-filters" role="tablist" aria-label="Filter activity by source">
    {#each sources as s (s.id)}
      <button
        type="button"
        role="tab"
        aria-selected={sourceFilter === s.id}
        class="activity-filter"
        class:active={sourceFilter === s.id}
        onclick={() => setSource(s.id)}
      >{s.label}</button>
    {/each}
  </div>

  {#if error}
    <p class="activity-error" role="status">{error}</p>
  {/if}

  {#if items.length === 0 && !loading}
    <p class="activity-empty">
      No activity yet. Capture a prompt, create a task, or run an agent to see it here.
    </p>
  {:else}
    <ul class="activity-list" role="list">
      {#each items as it (it.id)}
        <li class="activity-item">
          <button
            type="button"
            class="activity-row"
            class:clickable={Boolean(it.missionId || it.entityId)}
            onclick={() => onRowClick(it)}
          >
            <span class="activity-dot" style={`background:${dotColor(it)}`} aria-hidden="true"></span>
            <div class="activity-body">
              <div class="activity-row-top">
                <span class="activity-kind">{kindLabel(it)}</span>
                {#if it.status}<span class="activity-status">{it.status.replaceAll('_', ' ')}</span>{/if}
                <span class="activity-time">{formatTime(it.timestamp)}</span>
              </div>
              <span class="activity-item-title">{it.title}</span>
              {#if it.detail}<span class="activity-detail">{it.detail}</span>{/if}
            </div>
          </button>
        </li>
      {/each}
    </ul>
  {/if}
</div>

<style>
  .activity-page {
    padding: 16px 14px;
    max-width: 900px;
    margin: 0 auto;
  }
  .activity-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 12px;
  }
  .activity-title {
    margin: 0;
    font-size: 18px;
    font-weight: 800;
    color: var(--text-primary);
  }
  .activity-sub {
    margin: 4px 0 0;
    font-size: 12px;
    color: var(--text-muted);
  }
  .activity-refresh {
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
  .activity-refresh:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .activity-filters {
    display: flex;
    gap: 6px;
    margin-bottom: 14px;
    flex-wrap: wrap;
  }
  .activity-filter {
    min-height: 28px;
    padding: 0 12px;
    border: 1px solid var(--border);
    border-radius: 999px;
    background: transparent;
    color: var(--text-secondary);
    font-size: 12px;
    font-weight: 700;
    cursor: pointer;
  }
  .activity-filter.active {
    color: var(--text-primary);
    border-color: color-mix(in srgb, var(--accent-blue) 55%, var(--border));
    background: color-mix(in srgb, var(--accent-blue) 12%, transparent);
  }
  .activity-error {
    font-size: 13px;
    color: var(--accent-red);
    margin: 0 0 12px;
  }
  .activity-empty {
    font-size: 13px;
    color: var(--text-muted);
    padding: 24px 0;
  }
  .activity-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .activity-row {
    width: 100%;
    display: flex;
    gap: 10px;
    align-items: flex-start;
    padding: 10px;
    border: 1px solid var(--border-subtle, var(--border));
    border-radius: 10px;
    background: color-mix(in srgb, var(--bg-card) 84%, transparent);
    text-align: left;
    cursor: default;
  }
  .activity-row.clickable {
    cursor: pointer;
  }
  .activity-row.clickable:hover {
    border-color: color-mix(in srgb, var(--accent-blue) 40%, var(--border));
  }
  .activity-dot {
    width: 8px;
    height: 8px;
    border-radius: 999px;
    margin-top: 5px;
    flex-shrink: 0;
  }
  .activity-body {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .activity-row-top {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .activity-kind {
    font-size: 11px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--text-muted);
  }
  .activity-status {
    font-size: 11px;
    font-weight: 700;
    color: var(--text-secondary);
    padding: 1px 6px;
    border: 1px solid var(--border);
    border-radius: 999px;
  }
  .activity-time {
    font-size: 11px;
    color: var(--text-muted);
    margin-left: auto;
  }
  .activity-item-title {
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
    overflow-wrap: anywhere;
  }
  .activity-detail {
    font-size: 12px;
    color: var(--text-muted);
    overflow-wrap: anywhere;
  }
</style>
