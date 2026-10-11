<script lang="ts" module>
  export type TaskStatus = 'backlog' | 'queued' | 'in-progress' | 'failed' | 'completed' | 'cancelled';

  export type KanbanTask = {
    id: string;
    title: string;
    dept: string;
    roles: string[];
    status: TaskStatus;
    tries: string;
    progress: number;
    priority?: string;
    requires_human_approval?: boolean;
    outcome?: string;
    task_kind?: string;
    assigned_director_id?: string;
    assigned_agent_id?: string;
    created_at?: string;
    entity_name?: string;
  };
</script>

<script lang="ts">
  import { api } from '$lib/api';

  type Props = {
    tasks: KanbanTask[];
    selectedTaskId?: string | null;
    onSelectTask?: (task: KanbanTask) => void;
    onAfterPatch?: () => void | Promise<void>;
    onRequeue?: (taskId: string) => void | Promise<void>;
  };

  let {
    tasks,
    selectedTaskId = null,
    onSelectTask,
    onAfterPatch,
    onRequeue
  }: Props = $props();

  const COLUMNS: { id: TaskStatus; label: string; accentVar: string; emptyText: string }[] = [
    { id: 'backlog', label: 'Backlog', accentVar: '--text-muted', emptyText: 'No tasks in backlog' },
    { id: 'queued', label: 'In queue', accentVar: '--accent-orange', emptyText: 'No tasks in queue' },
    { id: 'in-progress', label: 'In progress', accentVar: '--accent-blue', emptyText: 'No active tasks' },
    { id: 'failed', label: 'Failed', accentVar: '--accent-red', emptyText: 'No failed tasks' },
    { id: 'completed', label: 'Completed', accentVar: '--accent-green', emptyText: 'No completed tasks' }
  ];

  const STATUS_LABEL: Record<TaskStatus, string> = {
    backlog: 'backlog',
    queued: 'queued',
    'in-progress': 'in progress',
    failed: 'failed',
    completed: 'completed',
    cancelled: 'cancelled'
  };

  const PRIORITY_COLOR: Record<string, string> = {
    high: 'var(--accent-red)',
    medium: 'var(--accent-orange)',
    low: 'var(--text-muted)'
  };

  function tasksIn(colId: TaskStatus) {
    return tasks.filter((t) => t.status === colId);
  }

  function progressColor(status: TaskStatus) {
    if (status === 'completed') return 'var(--accent-green)';
    if (status === 'in-progress') return 'var(--accent-blue)';
    if (status === 'failed') return 'var(--accent-red)';
    return 'var(--accent-purple)';
  }

  function outcomeLabel(outcome?: string) {
    switch ((outcome ?? '').toLowerCase()) {
      case 'needshumanapproval':
        return 'needs approval';
      case 'blockedbypolicy':
        return 'blocked by policy';
      case 'needsstrongermodel':
        return 'needs stronger model';
      case 'needsreview':
        return 'needs review';
      case 'retryablefailure':
        return 'retryable failure';
      case 'failed':
        return 'failed';
      case 'completed':
        return 'completed';
      default:
        return outcome ?? 'n/a';
    }
  }

  function formatTimestamp(value?: string): string {
    if (!value) return '';
    const d = new Date(value);
    if (Number.isNaN(d.getTime())) return value;
    return d.toLocaleString();
  }

  let dragId = $state<string | null>(null);
  let dragOver = $state<TaskStatus | null>(null);
  let suppressDetailClickUntil = 0;

  function onDragStart(e: DragEvent, id: string) {
    dragId = id;
    if (e.dataTransfer) {
      e.dataTransfer.effectAllowed = 'move';
      e.dataTransfer.setData('text/plain', id);
    }
  }

  function onDragEnd() {
    dragId = null;
    dragOver = null;
    suppressDetailClickUntil = Date.now() + 350;
  }

  function onDragOver(e: DragEvent, colId: TaskStatus) {
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = 'move';
    dragOver = colId;
  }

  function onDragLeave(e: DragEvent) {
    const target = e.currentTarget as HTMLElement;
    const related = e.relatedTarget as Node | null;
    if (!related || !target.contains(related)) dragOver = null;
  }

  async function onDrop(e: DragEvent, colId: TaskStatus) {
    e.preventDefault();
    dragOver = null;
    const id = dragId ?? e.dataTransfer?.getData('text/plain');
    if (!id) return;
    const task = tasks.find((t) => t.id === id);
    if (!task || task.status === colId) return;
    try {
      const nextProgress =
        colId === 'completed' || colId === 'failed'
          ? 100
          : colId === 'backlog' && task.progress >= 100
            ? 0
            : Math.min(100, Math.max(0, Math.round(task.progress)));
      await api('/api/tasks/' + id, {
        method: 'PATCH',
        body: JSON.stringify({ status: colId, progress: nextProgress })
      });
      await onAfterPatch?.();
    } catch (err) {
      console.error('Update task failed', err);
    }
    dragId = null;
    suppressDetailClickUntil = Date.now() + 350;
  }

  function onTaskCardClick(task: KanbanTask, e: MouseEvent) {
    if (!onSelectTask) return;
    if (Date.now() < suppressDetailClickUntil) return;
    if ((e.target as HTMLElement).closest('button')) return;
    onSelectTask(task);
  }

  async function handleRequeue(taskId: string) {
    await onRequeue?.(taskId);
  }
</script>

<div class="kanban-root">
  {#snippet taskCardInner(task: KanbanTask)}
    {#if task.priority}
      <div class="priority-bar" style="background: {PRIORITY_COLOR[task.priority] ?? 'var(--text-muted)'}"></div>
    {/if}

    <div class="card-inner">
      <div class="task-title">{task.title}</div>

      {#if task.entity_name}
        <div class="task-created-at">Entity: {task.entity_name}</div>
      {/if}

      <div class="task-roles">
        {#each task.roles as role}
          <span class="role-tag">{role}</span>
        {/each}
      </div>

      <div class="task-footer">
        <span class="status-badge status-{task.status}">
          {STATUS_LABEL[task.status]}
        </span>
        <span class="tries-label">{task.tries} tries</span>
      </div>

      {#if task.created_at}
        <div class="task-created-at">Created {formatTimestamp(task.created_at)}</div>
      {/if}

      {#if task.requires_human_approval || task.outcome === 'needshumanapproval'}
        <div class="task-roles">
          <span class="role-tag" style="border-color: var(--accent-orange); color: var(--accent-orange);">needs approval</span>
        </div>
      {:else if task.outcome && task.outcome !== 'completed'}
        <div class="task-roles">
          <span class="role-tag">{outcomeLabel(task.outcome)}</span>
        </div>
      {/if}

      <div class="progress-track">
        <div
          class="progress-fill"
          style="width: {task.progress}%; background: {progressColor(task.status)}"
        ></div>
      </div>
    </div>
  {/snippet}

  <div class="kanban">
    {#each COLUMNS as col}
      {@const colTasks = tasksIn(col.id)}
      <div
        class="kanban-col"
        class:drag-over={dragOver === col.id}
        ondragover={(e) => onDragOver(e, col.id)}
        ondragleave={onDragLeave}
        ondrop={(e) => onDrop(e, col.id)}
        role="region"
        aria-label={col.label}
      >
        <div class="col-header">
          <span class="col-dot" style="background: var({col.accentVar})"></span>
          <span class="col-label">{col.label}</span>
          <span class="col-count">{colTasks.length}</span>
        </div>

        <div class="col-body" class:is-empty={colTasks.length === 0} role="list">
          {#if colTasks.length === 0}
            <div class="empty-drop-hint">
              <span class="empty-icon">+</span>
              <span>{col.emptyText}</span>
              <span class="empty-hint">Drop tasks here</span>
            </div>
          {:else}
            {#each colTasks as task (task.id)}
              <div
                class="task-card"
                class:is-dragging={dragId === task.id}
                class:selected={selectedTaskId === task.id}
                class:clickable={!!onSelectTask}
                role="listitem"
                aria-label={task.title}
                draggable="true"
                ondragstart={(e) => onDragStart(e, task.id)}
                ondragend={onDragEnd}
              >
                {#if onSelectTask}
                  <div
                    class="card-main"
                    role="button"
                    tabindex="0"
                    onclick={(e) => onTaskCardClick(task, e)}
                    onkeydown={(e) => {
                      if (e.key === 'Enter' || e.key === ' ') {
                        e.preventDefault();
                        if (Date.now() < suppressDetailClickUntil) return;
                        onSelectTask(task);
                      }
                    }}
                  >
                    {@render taskCardInner(task)}
                  </div>
                {:else}
                  <div class="card-main">
                    {@render taskCardInner(task)}
                  </div>
                {/if}

                {#if task.status === 'backlog' && onRequeue}
                  <button
                    type="button"
                    class="requeue-btn"
                    onclick={(e) => {
                      e.stopPropagation();
                      e.preventDefault();
                      handleRequeue(task.id);
                    }}
                  >
                    ↑ Requeue
                  </button>
                {/if}
              </div>
            {/each}
          {/if}
        </div>
      </div>
    {/each}
  </div>
</div>

<style>
  .kanban-root {
    min-height: 0;
    flex: 1;
    display: flex;
    flex-direction: column;
  }

  .kanban {
    display: grid;
    grid-template-columns: repeat(5, minmax(0, 1fr));
    gap: var(--space-2, 8px);
    flex: 1;
    min-height: 0;
  }

  @media (max-width: 1200px) {
    .kanban {
      grid-template-columns: repeat(3, minmax(0, 1fr));
    }
  }
  @media (max-width: 900px) {
    .kanban {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
  }
  @media (max-width: 560px) {
    .kanban {
      grid-template-columns: 1fr;
    }
  }

  .kanban-col {
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: var(--radius-lg, 8px);
    display: flex;
    flex-direction: column;
    overflow: hidden;
    transition:
      border-color var(--transition-base, 0.15s ease),
      background var(--transition-base, 0.15s ease);
  }

  .kanban-col.drag-over {
    border-color: var(--accent-purple);
    background: rgba(201, 138, 75, 0.06);
  }

  .col-header {
    display: flex;
    align-items: center;
    gap: var(--space-2, 8px);
    padding: var(--space-2, 8px) var(--space-3, 12px);
    font-size: var(--text-sm, 12px);
    font-weight: var(--font-semibold, 600);
    color: var(--text-secondary);
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }

  .col-dot {
    width: var(--dot-size, 7px);
    height: var(--dot-size, 7px);
    border-radius: var(--radius-full, 999px);
    flex-shrink: 0;
  }

  .col-label {
    flex: 1;
  }

  .col-count {
    color: var(--text-muted);
    font-weight: var(--font-medium, 500);
    background: var(--bg-card);
    border-radius: var(--radius-full, 999px);
    padding: 0 6px;
    font-size: var(--text-2xs, 10px);
    min-width: 18px;
    text-align: center;
  }

  .col-body {
    flex: 1;
    padding: var(--space-2, 8px);
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: var(--space-2, 8px);
  }

  .col-body.is-empty {
    justify-content: center;
    align-items: center;
  }

  .empty-drop-hint {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: var(--space-1, 4px);
    color: var(--text-muted);
    font-size: var(--text-xs, 11px);
    text-align: center;
    border: 1.5px dashed var(--border);
    border-radius: var(--radius-lg, 8px);
    padding: var(--space-5, 20px) var(--space-3, 12px);
    width: 100%;
    transition:
      border-color var(--transition-base, 0.15s ease),
      color var(--transition-base, 0.15s ease);
  }

  .kanban-col.drag-over .empty-drop-hint {
    border-color: var(--accent-purple);
    color: var(--accent-purple);
  }

  .empty-icon {
    font-size: var(--text-xl, 18px);
    font-style: normal;
    opacity: 0.4;
  }

  .empty-hint {
    font-size: var(--text-2xs, 10px);
    opacity: 0.6;
    margin-top: var(--space-1, 4px);
  }

  .task-card {
    display: flex;
    flex-direction: column;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: var(--radius-md, 6px);
    cursor: grab;
    transition:
      opacity var(--transition-base, 0.15s ease),
      border-color var(--transition-base, 0.15s ease),
      box-shadow var(--transition-base, 0.15s ease),
      transform var(--transition-fast, 0.1s ease);
    user-select: none;
    position: relative;
    overflow: hidden;
  }

  .task-card.clickable {
    cursor: grab;
  }

  .task-card.clickable:hover {
    border-color: var(--accent-purple);
    box-shadow: var(--shadow-sm, 0 2px 8px rgba(0, 0, 0, 0.12));
    transform: translateY(-1px);
  }

  .task-card:active {
    cursor: grabbing;
  }

  .task-card.is-dragging {
    opacity: 0.35;
    transform: scale(0.98);
    cursor: grabbing;
  }

  .task-card.selected {
    border-color: var(--accent-purple);
    box-shadow: 0 0 0 2px rgba(201, 138, 75, 0.34);
  }

  .card-main {
    position: relative;
    flex: 1;
    min-width: 0;
  }

  .card-main[role='button'] {
    border: none;
    background: transparent;
    padding: 0;
    margin: 0;
    width: 100%;
    font: inherit;
    text-align: left;
    color: inherit;
    cursor: pointer;
  }

  .priority-bar {
    position: absolute;
    top: 0;
    left: 0;
    width: 3px;
    height: 100%;
    opacity: 0.7;
  }

  .card-inner {
    padding: var(--space-2, 8px);
    padding-left: calc(var(--space-2, 8px) + 3px);
    display: flex;
    flex-direction: column;
    gap: 5px;
  }

  .task-title {
    font-size: var(--text-xs, 11px);
    color: var(--text-primary);
    line-height: var(--leading-normal, 1.45);
  }

  .task-roles {
    display: flex;
    flex-wrap: wrap;
    gap: 3px;
  }

  .role-tag {
    font-size: var(--text-2xs, 10px);
    padding: 1px 6px;
    height: var(--tag-height, 20px);
    display: flex;
    align-items: center;
    border-radius: var(--tag-radius, 4px);
    background: rgba(74, 61, 82, 0.2);
    color: var(--text-secondary);
    border: 1px solid var(--border);
    line-height: 1;
  }

  .task-footer {
    display: flex;
    justify-content: space-between;
    align-items: center;
  }

  .task-created-at {
    font-size: 0.72rem;
    color: var(--text-muted);
  }

  .status-badge {
    font-size: var(--text-2xs, 10px);
    padding: 1px 6px;
    border-radius: var(--tag-radius, 4px);
    font-weight: var(--font-semibold, 600);
  }
  .status-badge.status-backlog {
    background: var(--color-red-dim, rgba(201, 106, 75, 0.15));
    color: var(--color-red-text, #c96a4b);
  }
  .status-badge.status-queued {
    background: var(--color-orange-dim, rgba(224, 165, 94, 0.15));
    color: var(--accent-orange);
  }
  .status-badge.status-in-progress {
    background: var(--color-blue-dim, rgba(59, 130, 246, 0.15));
    color: var(--accent-blue);
  }
  .status-badge.status-failed {
    background: rgba(201, 106, 75, 0.15);
    color: #c96a4b;
  }
  .status-badge.status-completed {
    background: var(--color-green-dim, rgba(122, 129, 99, 0.15));
    color: var(--accent-green);
  }

  .tries-label {
    font-size: var(--text-2xs, 10px);
    color: var(--text-muted);
  }

  .progress-track {
    height: 3px;
    background: var(--border);
    border-radius: var(--radius-full, 999px);
    overflow: hidden;
  }

  .progress-fill {
    height: 100%;
    border-radius: var(--radius-full, 999px);
    transition: width var(--duration-slow, 0.35s) var(--ease-out, ease-out);
  }

  .requeue-btn {
    flex-shrink: 0;
    align-self: stretch;
    margin: 0 var(--space-2, 8px) var(--space-2, 8px);
    background: var(--color-purple-dim, rgba(201, 138, 75, 0.12));
    border: 1px solid var(--color-purple-border, rgba(201, 138, 75, 0.35));
    color: var(--accent-purple);
    border-radius: var(--radius-sm, 4px);
    padding: 3px var(--space-2, 8px);
    font-size: var(--text-2xs, 10px);
    font-weight: var(--font-semibold, 600);
    box-sizing: border-box;
    transition: background var(--transition-base, 0.15s ease);
    cursor: pointer;
  }
  .requeue-btn:hover {
    background: rgba(201, 138, 75, 0.28);
  }
</style>
