<script lang="ts">
  // One entity's own browser: a Chrome window with a profile of its own. Sign in to the
  // entity's accounts in it, and Consistent CEO works in that same window when it works for
  // this entity. Sign-ins never cross to another entity's window, and the agent only ever
  // sees pages, never passwords or cookies.
  // Backed by `/api/entities/{id}/browser*` (crates/gateway/src/control_api/browser_api.rs).
  import { onMount } from 'svelte';
  import {
    closeEntityBrowser,
    getEntityBrowser,
    openEntityBrowser,
    type EntityBrowser
  } from '$lib/api';

  let { entityId, entityName }: { entityId: string; entityName: string } = $props();

  let status = $state<EntityBrowser | null>(null);
  let busy = $state(false);
  let error = $state('');
  let loadError = $state(false);
  let alive = true;

  function reason(err: unknown, fallback: string): string {
    const raw = err instanceof Error ? err.message : '';
    try {
      const parsed = JSON.parse(raw);
      if (parsed && typeof parsed.error === 'string') return parsed.error;
    } catch {
      /* not JSON */
    }
    return raw || fallback;
  }

  async function refresh() {
    try {
      status = await getEntityBrowser(entityId);
      loadError = false;
    } catch {
      loadError = true;
    }
  }

  async function run(action: (id: string) => Promise<EntityBrowser>, fallback: string) {
    if (busy) return;
    busy = true;
    error = '';
    try {
      status = await action(entityId);
    } catch (err) {
      error = reason(err, fallback);
      await refresh();
    } finally {
      busy = false;
    }
  }

  onMount(() => {
    void refresh();
    // Notice a window closed by hand, and the sites open in it.
    const timer = setInterval(() => {
      if (alive && !busy) void refresh();
    }, 5000);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  });
</script>

<section class="browser" aria-label={`${entityName}’s browser`}>
  <div class="head">
    <div>
      <h3>{entityName}’s browser</h3>
      <p class="lead">
        A browser window of its own. Sign in to {entityName}’s accounts in it (its social media, its
        dashboards, anything with a login), and Consistent CEO works in that same window. These sign-ins stay
        apart from your everyday browser and from your other entities.
      </p>
    </div>
    {#if status?.running}
      <span class="chip chip-ok">Open</span>
    {:else if status}
      <span class="chip">Closed</span>
    {/if}
  </div>

  {#if loadError && !status}
    <p class="note">Can’t reach the app to check the browser. <button type="button" class="link" onclick={refresh}>Try again</button></p>
  {:else if status && !status.chromeFound}
    <p class="note">
      It needs Google Chrome (or Brave, Edge or Chromium) on this computer.
      <a href="https://www.google.com/chrome/" target="_blank" rel="noreferrer">Get Chrome</a>, then
      <button type="button" class="link" onclick={refresh}>check again</button>.
    </p>
  {:else if status}
    <div class="actions">
      <button type="button" class="btn" disabled={busy} onclick={() => run(openEntityBrowser, 'Could not open the browser.')}>
        {busy ? 'Working…' : status.running ? 'Bring it to the front' : status.usedBefore ? `Open ${entityName}’s browser` : 'Open it and sign in'}
      </button>
      {#if status.running}
        <button type="button" class="btn ghost" disabled={busy} onclick={() => run(closeEntityBrowser, 'Could not close the browser.')}>
          Close it
        </button>
      {/if}
    </div>

    {#if status.running}
      {#if status.openSites.length > 0}
        <p class="note">
          Open now:
          {#each status.openSites as site, i (site.host + i)}{i > 0 ? ', ' : ''}<strong>{site.host}</strong>{/each}.
        </p>
      {/if}
    {:else if status.usedBefore}
      <p class="note">Its sign-ins are kept. Open it again and they are still there.</p>
    {/if}

    {#if error}<p class="error" role="alert">{error}</p>{/if}
  {/if}
</section>

<style>
  .browser {
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 16px;
    border: 1px solid var(--border);
    border-radius: 12px;
    background: var(--bg-secondary);
  }
  .head { display: flex; justify-content: space-between; gap: 12px; align-items: flex-start; }
  h3 { margin: 0; font-size: 0.9375rem; color: var(--text-primary); }
  .lead { margin: 4px 0 0; font-size: 0.8125rem; line-height: 1.45; color: var(--text-muted); }
  .chip {
    flex-shrink: 0;
    padding: 1px 8px;
    border-radius: 999px;
    border: 1px solid var(--border);
    color: var(--text-muted);
    font-size: 0.6875rem;
    font-weight: 700;
  }
  .chip-ok { border-color: rgba(122, 129, 99, 0.5); color: #7a8163; }
  .actions { display: flex; flex-wrap: wrap; gap: 6px; }
  .btn {
    display: inline-grid;
    place-items: center;
    min-height: 34px;
    padding: 0 14px;
    border: 1px solid rgba(201, 138, 75, 0.42);
    border-radius: 8px;
    background: rgba(201, 138, 75, 0.14);
    color: var(--text-primary);
    font-size: 0.75rem;
    font-weight: 700;
    cursor: pointer;
  }
  .btn:hover:not(:disabled) { background: rgba(201, 138, 75, 0.24); }
  .btn.ghost { border-color: var(--border); background: var(--bg-card); color: var(--text-secondary); }
  .btn:disabled { opacity: 0.6; cursor: default; }
  .note { margin: 0; font-size: 0.8125rem; line-height: 1.45; color: var(--text-secondary); }
  .note a, .link { color: var(--text-secondary); }
  .link { padding: 0; border: 0; background: none; font: inherit; text-decoration: underline; cursor: pointer; }
  .error { margin: 0; font-size: 0.8125rem; color: #c96a4b; overflow-wrap: anywhere; }
</style>
