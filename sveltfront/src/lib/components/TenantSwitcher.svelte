<script lang="ts">
  import type { TenantSummary } from '$lib/api';
  import ChevronIcon from '$lib/components/icons/ChevronIcon.svelte';

  let {
    workspaces,
    activeId,
    loading = false,
    error = null,
    username = null,
    onChange
  }: {
    workspaces: TenantSummary[];
    activeId: string;
    loading?: boolean;
    error?: string | null;
    /** The signed-in account's username. When known it is what the pill
     *  shows — a tenant is a person here, not a slug — and the workspace
     *  name steps back to a suffix, shown only when there is a choice. */
    username?: string | null;
    onChange: (event: Event) => void | Promise<void>;
  } = $props();

  const activeWorkspace = $derived(workspaces.find((w) => w.id === activeId) ?? null);
  const workspaceName = $derived(activeWorkspace?.name ?? activeId);
  const displayName = $derived(username ?? workspaceName);
  // A personal workspace is named after its owner, so the suffix would only
  // repeat the pill; show it when it adds information and there is a choice.
  const workspaceSuffix = $derived(
    username &&
      workspaces.length > 1 &&
      workspaceName.trim().toLowerCase() !== username.trim().toLowerCase()
      ? workspaceName
      : null
  );
  const disabled = $derived(loading || workspaces.length <= 1);
</script>

<label class="switcher" title={error ?? (username ? `Signed in as ${username} · Switch tenant` : 'Switch tenant')}>
  <span class="switcher-label">Tenant</span>
  <span class="switcher-value">
    <span class="switcher-name">{displayName}</span>
    {#if workspaceSuffix}
      <span class="switcher-suffix">· {workspaceSuffix}</span>
    {/if}
    <ChevronIcon size={14} class="switcher-chevron" />
  </span>
  <select class="switcher-select" value={activeId} onchange={onChange} {disabled}>
    {#if workspaces.length === 0}
      <option value={activeId}>{activeId}</option>
    {:else}
      {#each workspaces as workspace (workspace.id)}
        <option value={workspace.id}>{workspace.name}</option>
      {/each}
    {/if}
  </select>
</label>

<style>
  .switcher {
    position: relative;
    display: inline-flex;
    align-items: center;
    gap: 6px;
    min-height: 36px;
    padding: 0 12px;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: var(--surface-overlay);
    color: var(--text-primary);
    cursor: pointer;
    transition: border-color var(--transition-base), background var(--transition-base);
  }
  .switcher:hover {
    border-color: rgba(201, 138, 75, 0.45);
    background: var(--surface-overlay-hover);
  }
  .switcher-label {
    font-size: 10px;
    font-weight: 700;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--text-muted);
    margin-right: 2px;
  }
  .switcher-value {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
    max-width: 180px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .switcher-name {
    max-width: 160px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .switcher-suffix {
    font-weight: 500;
    color: var(--text-muted);
    white-space: nowrap;
  }
  :global(.switcher-chevron) {
    color: var(--text-muted);
  }
  .switcher-select {
    position: absolute;
    inset: 0;
    opacity: 0;
    cursor: pointer;
    width: 100%;
    height: 100%;
  }
  .switcher-select:disabled {
    cursor: not-allowed;
  }
</style>
