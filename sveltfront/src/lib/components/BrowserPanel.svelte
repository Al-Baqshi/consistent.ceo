<script lang="ts">
  // Every entity's browser at a glance. Each entity has a browser window of its own: sign in
  // to the entity's accounts in it, and Consistent CEO works in that same window when it works
  // for that entity. Passwords and cookies stay in the browser; the agent only ever sees pages.
  // Which sites it may use is set per entity, in the entity's Browser tab.
  // Backed by `/api/browser` and `/api/entities/{id}/browser*`
  // (crates/gateway/src/control_api/browser_api.rs).
  import { onMount } from 'svelte';
  import {
    closeEntityBrowser,
    getBrowsers,
    openEntityBrowser,
    type BrowsersOverview,
    type EntityBrowser
  } from '$lib/api';

  let overview = $state<BrowsersOverview | null>(null);
  let busyId = $state<string | null>(null);
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
      overview = await getBrowsers();
      loadError = false;
    } catch {
      loadError = true;
    }
  }

  async function act(entityId: string, action: (id: string) => Promise<EntityBrowser>, fallback: string) {
    if (busyId) return;
    busyId = entityId;
    error = '';
    try {
      const updated = await action(entityId);
      if (overview) {
        overview = {
          ...overview,
          browsers: overview.browsers.map((b) => (b.entityId === entityId ? updated : b))
        };
      }
    } catch (err) {
      error = reason(err, fallback);
      await refresh();
    } finally {
      busyId = null;
    }
  }

  function summary(b: EntityBrowser): string {
    const state = b.running
      ? b.openSites.length > 0
        ? `Open: ${b.openSites.map((s) => s.host).join(', ')}`
        : 'Open'
      : b.usedBefore
        ? 'Closed, sign-ins kept'
        : 'Not opened yet';
    const sites = b.sites.length === 0 ? 'no sites allowed yet' : `${b.sites.length} site${b.sites.length === 1 ? '' : 's'} allowed`;
    return `${state} · ${sites}`;
  }

  onMount(() => {
    void refresh();
    // Notice windows closed by hand, and the sites open in them.
    const timer = setInterval(() => {
      if (alive && !busyId) void refresh();
    }, 5000);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  });
</script>

<section class="browser" aria-label="Entity browsers">
  <div class="head">
    <div>
      <h2>Browsers</h2>
      <p class="lead">
        Every entity has a browser window of its own. Sign in to an entity’s accounts in its browser, and
        Consistent CEO works in that same window when it works for that entity. One entity’s sign-ins never
        reach another’s, and the agent never sees passwords or cookies, only the pages.
      </p>
    </div>
  </div>

  {#if loadError && !overview}
    <p class="note">Can’t reach the app to check the browsers. <button type="button" class="link" onclick={refresh}>Try again</button></p>
  {:else if overview && !overview.chromeFound}
    <p class="note">
      They need Google Chrome (or Brave, Edge or Chromium) on this computer.
      <a href="https://www.google.com/chrome/" target="_blank" rel="noreferrer">Get Chrome</a>, then
      <button type="button" class="link" onclick={refresh}>check again</button>.
    </p>
  {:else if overview}
    {#if overview.browsers.length === 0}
      <p class="note">No entities yet. Each entity you add gets a browser of its own.</p>
    {:else}
      <ul class="list">
        {#each overview.browsers as b (b.entityId)}
          <li class="row">
            <div class="who">
              <a href={`/entities/${encodeURIComponent(b.entityId)}?tab=browser`}><strong>{b.entityName}</strong></a>
              <span class="meta">{summary(b)}</span>
            </div>
            <div class="actions">
              <button
                type="button"
                class="btn"
                disabled={busyId !== null}
                onclick={() => act(b.entityId, openEntityBrowser, 'Could not open the browser.')}
              >
                {busyId === b.entityId ? 'Working…' : b.running ? 'Bring forward' : 'Open'}
              </button>
              {#if b.running}
                <button
                  type="button"
                  class="btn ghost"
                  disabled={busyId !== null}
                  onclick={() => act(b.entityId, closeEntityBrowser, 'Could not close the browser.')}
                >
                  Close
                </button>
              {/if}
            </div>
          </li>
        {/each}
      </ul>
    {/if}

    {#if error}<p class="error" role="alert">{error}</p>{/if}

    <ul class="rules">
      <li>Which sites the agent may use is set for each entity, in its <strong>Browser</strong> tab. Nothing is allowed until you name a site.</li>
      <li>Banks, payments and sign-in pages (PayPal, Stripe, Google sign-in…) are always off limits to the agent, though you can use them yourself.</li>
      <li>The agent works in its own tab and leaves yours alone. It stops at anything that posts, sends or buys until you approve it.</li>
    </ul>
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
  h2 { margin: 0; font-size: 1rem; color: var(--text-primary); }
  .lead { margin: 4px 0 0; font-size: 0.8125rem; line-height: 1.45; color: var(--text-muted); }
  .list { margin: 0; padding: 0; list-style: none; display: flex; flex-direction: column; gap: 6px; }
  .row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 12px;
    padding: 8px 10px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-card);
  }
  .who { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
  .who a { color: var(--text-primary); font-size: 0.8125rem; text-decoration: none; }
  .who a:hover { text-decoration: underline; }
  .meta { font-size: 0.75rem; color: var(--text-muted); overflow-wrap: anywhere; }
  .actions { display: flex; flex-wrap: wrap; gap: 6px; flex-shrink: 0; }
  .btn {
    display: inline-grid;
    place-items: center;
    min-height: 30px;
    padding: 0 12px;
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
  .rules { margin: 0; padding-left: 18px; display: flex; flex-direction: column; gap: 4px; font-size: 0.75rem; line-height: 1.45; color: var(--text-muted); }
</style>
