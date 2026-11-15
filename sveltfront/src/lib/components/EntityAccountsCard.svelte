<script lang="ts">
  // One entity's accounts, from the wallet: the logins filed under this entity, and for each
  // site the agent may use in its browser, the saved account that belongs there. Names and
  // sites only; passwords stay in the wallet, and the agent never sees them.
  // Backed by `/api/vault/items` and `/api/vault/items/match` (crates/gateway/src/vault_api.rs).
  import {
    VaultHttpError,
    VaultLockedError,
    getStatus,
    listItems,
    matchItems,
    type ItemSummary
  } from '$lib/vault';

  let {
    entityId,
    entityName,
    sites,
    isPersonal = false
  }: { entityId: string; entityName: string; sites: string[]; isPersonal?: boolean } = $props();

  type Phase = 'loading' | 'unavailable' | 'no-vault' | 'locked' | 'error' | 'ready';
  let phase = $state<Phase>('loading');
  let accounts = $state<ItemSummary[]>([]);
  let bySite = $state<Record<string, ItemSummary[]>>({});

  // The Personal space's accounts are the CEO's own set, which the wallet keeps as `global`.
  const scope = $derived(isPersonal ? 'global' : `entity:${entityId}`);
  const walletHref = $derived(`/vault?scope=${encodeURIComponent(scope)}`);
  const addHref = (site?: string) =>
    `/vault?scope=${encodeURIComponent(scope)}&new=1${site ? `&site=${encodeURIComponent(site)}` : ''}`;

  async function load(forSites: string[]) {
    phase = 'loading';
    try {
      const status = await getStatus();
      if (!status.initialized) {
        phase = 'no-vault';
        return;
      }
      if (!status.unlocked) {
        phase = 'locked';
        return;
      }
      accounts = (await listItems(scope)).filter((item) => item.kind === 'login');
      const pairs = await Promise.all(
        forSites.map(async (site) => [site, await matchItems(scope, site)] as const)
      );
      bySite = Object.fromEntries(pairs);
      phase = 'ready';
    } catch (err) {
      if (err instanceof VaultLockedError) phase = 'locked';
      else if (err instanceof VaultHttpError && err.status === 503) phase = 'unavailable';
      else phase = 'error';
    }
  }

  // Again whenever the site list changes, so a site just added shows whether it has an account.
  $effect(() => {
    void load([...sites]);
  });
</script>

{#if phase !== 'unavailable'}
  <section class="accounts" aria-label={`${entityName}’s accounts`}>
    <div class="head">
      <div>
        <h3>Accounts</h3>
        <p class="lead">
          Keep each of {entityName}’s logins in the wallet, filed under {entityName}, and sign in with them in
          {entityName}’s browser. The agent works in the signed-in window and never sees the passwords.
        </p>
      </div>
      <a class="btn" href={addHref()}>Add an account</a>
    </div>

    {#if phase === 'loading'}
      <p class="note">Checking the wallet…</p>
    {:else if phase === 'no-vault'}
      <p class="note">The wallet isn’t set up yet. <a href="/vault">Set it up</a> to keep {entityName}’s passwords together.</p>
    {:else if phase === 'locked'}
      <p class="note">The wallet is locked. <a href={walletHref}>Unlock it</a> to see {entityName}’s accounts.</p>
    {:else if phase === 'error'}
      <p class="note">Couldn’t read the wallet. <button type="button" class="link" onclick={() => load([...sites])}>Try again</button></p>
    {:else}
      {#if sites.length > 0}
        <ul class="list">
          {#each sites as site (site)}
            <li class="row">
              <span class="site">{site}</span>
              {#if (bySite[site] ?? []).length > 0}
                <span class="who">{bySite[site].map((a) => a.name).join(', ')}</span>
              {:else}
                <span class="missing">No account saved · <a href={addHref(site)}>Add one</a></span>
              {/if}
            </li>
          {/each}
        </ul>
      {/if}
      {#if accounts.length === 0}
        <p class="note">No accounts saved for {entityName} yet.</p>
      {:else}
        <p class="note">
          {accounts.length} account{accounts.length === 1 ? '' : 's'} saved for {entityName}.
          <a href={walletHref}>Open them in the wallet</a>
        </p>
      {/if}
    {/if}
  </section>
{/if}

<style>
  .accounts {
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: 16px;
    border: 1px solid var(--border);
    border-radius: 12px;
    background: var(--bg-secondary);
  }
  .head { display: flex; justify-content: space-between; gap: 12px; align-items: flex-start; }
  h3 { margin: 0; font-size: 0.9375rem; color: var(--text-primary); }
  .lead { margin: 4px 0 0; font-size: 0.8125rem; line-height: 1.45; color: var(--text-muted); }
  .btn {
    flex-shrink: 0;
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
    text-decoration: none;
  }
  .btn:hover { background: rgba(201, 138, 75, 0.24); }
  .list { margin: 0; padding: 0; list-style: none; display: flex; flex-direction: column; gap: 4px; }
  .row {
    display: flex;
    justify-content: space-between;
    gap: 12px;
    padding: 6px 10px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-card);
    font-size: 0.8125rem;
  }
  .site { color: var(--text-primary); font-weight: 600; }
  .who { color: var(--text-secondary); text-align: right; overflow-wrap: anywhere; }
  .missing { color: var(--text-muted); }
  .note { margin: 0; font-size: 0.8125rem; line-height: 1.45; color: var(--text-secondary); }
  .note a, .missing a, .link { color: var(--text-secondary); }
  .link { padding: 0; border: 0; background: none; font: inherit; text-decoration: underline; cursor: pointer; }
</style>
