<script lang="ts">
  import { onMount } from 'svelte';
  import { api, fetchAllPages, type PaginatedResponse } from '$lib/api';
  import { deriveMissionExecutionSummary } from '$lib/missionExecution';
  import { describeMissionFeedEvent } from '$lib/missionFeed';
  import Seo from '$lib/components/Seo.svelte';

  type Mission = {
    id: string;
    entity_id: string;
    title: string;
    summary: string;
    status: string;
    priority: string;
    updated_at: string;
  };
  type Task = {
    id: string;
    title: string;
    status: string;
    mission_id?: string;
    assigned_director_id?: string;
    assigned_agent_id?: string;
    result?: string;
  };
  type FeedEvent = {
    id: string;
    timestamp: string;
    kind: string;
    title: string;
    detail?: string;
    missionId?: string;
  };

  let missions = $state<Mission[]>([]);
  let tasks = $state<Task[]>([]);
  let feed = $state<FeedEvent[]>([]);
  let loadError = $state(false);

  const missionCards = $derived(
    missions.map((mission) => {
      const missionTasks = tasks.filter((task) => task.mission_id === mission.id);
      const missionEvents = feed.filter((event) => event.missionId === mission.id);
      return {
        mission,
        execution: deriveMissionExecutionSummary(missionTasks, missionEvents),
        latestFeedSummary: missionEvents[0] ? describeMissionFeedEvent(missionEvents[0]) : null
      };
    })
  );

  async function load() {
    try {
      // Every page of both: the default page is the oldest 50, so newer
      // missions vanished and their task counts read as zero.
      const [missionRes, taskRes, feedRes] = await Promise.all([
        fetchAllPages<Mission>('/api/missions'),
        fetchAllPages<Task>('/api/tasks').catch(() => [] as Task[]),
        api<PaginatedResponse<FeedEvent>>('/api/operator-feed?limit=80').then(r => r.items ?? []).catch(() => [])
      ]);
      missions = missionRes ?? [];
      tasks = taskRes ?? [];
      feed = feedRes ?? [];
      loadError = false;
    } catch {
      loadError = true;
      missions = [];
      tasks = [];
      feed = [];
    }
  }

  onMount(load);
</script>

<Seo title="Missions" noindex />

<div class="missions-page">
  <div class="page-head">
    <h1 class="page-title">Missions</h1>
    <p class="page-lead">Goals and workstreams linked to entities. Open one for detail, tasks, and follow-ups.</p>
  </div>

  {#if loadError}
    <p class="muted">Could not load missions. Check the gateway and try again.</p>
  {:else if missions.length === 0}
    <div class="empty-state">
      <p class="muted">No missions yet.</p>
      <p class="muted">Start by creating an entity, then add your first mission from that entity page.</p>
      <div class="empty-actions">
        <a class="action-link" href="/entities?create=1">Create entity</a>
        <a class="action-link" href="/">Open Home capture</a>
      </div>
    </div>
  {:else}
    <ul class="mission-list">
      {#each missionCards as card}
        <li>
          <a class="mission-card" href="/missions/{card.mission.id}">
            <span class="mission-title">{card.mission.title}</span>
            <span class="mission-meta">
              <span class="pill">{card.mission.status}</span>
              <span class="pill low">{card.mission.priority}</span>
              <span class="pill execution-pill execution-{card.execution.status}">{card.execution.label}</span>
              <span class="muted">entity {card.mission.entity_id}</span>
            </span>
            <span class="mission-summary">{card.execution.detail}</span>
            {#if card.latestFeedSummary}
              <span class="mission-summary mission-summary-strong">{card.latestFeedSummary}</span>
            {/if}
            {#if card.mission.summary}
              <span class="mission-summary">{card.mission.summary}</span>
            {/if}
          </a>
        </li>
      {/each}
    </ul>
  {/if}
</div>

<style>
  .missions-page {
    padding: 16px;
    max-width: 720px;
    margin: 0 auto;
  }
  .page-head {
    margin-bottom: 16px;
  }
  .page-title {
    font-size: 20px;
    font-weight: 700;
    margin: 0 0 6px;
    color: var(--text-primary);
  }
  .page-lead {
    margin: 0;
    font-size: 13px;
    color: var(--text-secondary);
    line-height: 1.5;
  }
  .muted {
    color: var(--text-muted);
    font-size: 13px;
  }
  .empty-state {
    border: 1px solid var(--border);
    border-radius: 10px;
    padding: 12px;
    background: var(--bg-card);
    display: grid;
    gap: 6px;
  }
  .empty-actions {
    display: grid;
    grid-template-columns: 1fr;
    gap: 8px;
    margin-top: 4px;
  }
  .action-link {
    min-height: 44px;
    border: 1px solid var(--border);
    border-radius: 8px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    text-decoration: none;
    color: var(--text-primary);
    font-size: 12px;
    font-weight: 700;
    background: var(--bg-secondary);
  }
  .action-link:hover {
    border-color: var(--accent-purple);
  }
  .mission-list {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .mission-card {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 12px 14px;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--bg-card);
    text-decoration: none;
    color: inherit;
    transition: border-color var(--transition-base), background var(--transition-base);
  }
  .mission-card:hover {
    border-color: var(--accent-purple);
    background: var(--bg-card-hover);
  }
  .mission-title {
    font-weight: 700;
    font-size: 14px;
    color: var(--text-primary);
  }
  .mission-meta {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    align-items: center;
    font-size: 11px;
  }
  .pill {
    display: inline-flex;
    align-items: center;
    padding: 2px 8px;
    border-radius: 999px;
    border: 1px solid var(--border);
    text-transform: capitalize;
    font-weight: 600;
    color: var(--text-secondary);
  }
  .pill.low {
    opacity: 0.85;
  }
  .execution-pill.execution-active { border-color: rgba(122, 129, 99, 0.45); color: #7a8163; }
  .execution-pill.execution-queued { border-color: rgba(201, 138, 75, 0.45); color: #eed9ae; }
  .execution-pill.execution-failed { border-color: rgba(201, 106, 75, 0.45); color: #c96a4b; }
  .execution-pill.execution-recently-active { border-color: rgba(224, 165, 94, 0.45); color: #eed9ae; }
  .execution-pill.execution-idle { color: var(--text-muted); }
  .mission-summary {
    font-size: 12px;
    color: var(--text-secondary);
    line-height: 1.45;
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
  .mission-summary-strong {
    color: var(--text-primary);
  }
</style>
