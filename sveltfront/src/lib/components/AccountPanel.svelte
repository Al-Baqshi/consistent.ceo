<script lang="ts">
  // Where the machine-owner account stops being only-on-this-computer.
  //
  // The desktop app signs you in as the owner of the machine you are sitting
  // at, with no form — which is right for the common case and wrong the moment
  // you want the phone app, or a browser on your laptop, to reach the same
  // workspace. Claiming keeps the *same* account, so everything already in it
  // comes along; it is an upgrade, not a second identity.
  import { onMount } from 'svelte';
  import {
    claimAccount,
    getAccountStatus,
    listAuthSessions,
    revokeAuthSession,
    revokeOtherSessions,
    type AccountStatus,
    type SessionInfo
  } from '$lib/api';

  let status = $state<AccountStatus | null>(null);
  let sessions = $state<SessionInfo[]>([]);
  let loading = $state(true);
  let error = $state<string | null>(null);

  let username = $state('');
  let password = $state('');
  let busy = $state(false);
  let recoveryCodes = $state<string[] | null>(null);

  onMount(load);

  async function load() {
    loading = true;
    try {
      status = await getAccountStatus();
      sessions = await listAuthSessions();
    } catch (e) {
      error = e instanceof Error ? e.message : 'Could not read your account.';
    } finally {
      loading = false;
    }
  }

  async function claim(e: SubmitEvent) {
    e.preventDefault();
    busy = true;
    error = null;
    try {
      const r = await claimAccount(username.trim(), password);
      recoveryCodes = r.recoveryCodes;
      status = { hasPassword: true, username: r.username };
      password = '';
    } catch (e) {
      error = e instanceof Error ? e.message : 'Could not set that up.';
    } finally {
      busy = false;
    }
  }

  async function revoke(id: string) {
    try {
      await revokeAuthSession(id);
      sessions = await listAuthSessions();
    } catch (e) {
      error = e instanceof Error ? e.message : 'Could not sign that device out.';
    }
  }

  async function revokeOthers() {
    try {
      await revokeOtherSessions();
      sessions = await listAuthSessions();
    } catch (e) {
      error = e instanceof Error ? e.message : 'Could not sign the other devices out.';
    }
  }

  function when(iso: string | null): string {
    if (!iso) return 'never';
    const mins = Math.round((Date.now() - new Date(iso).getTime()) / 60_000);
    if (!Number.isFinite(mins)) return '—';
    if (mins < 1) return 'just now';
    if (mins < 60) return `${mins}m ago`;
    if (mins < 1440) return `${Math.round(mins / 60)}h ago`;
    return `${Math.round(mins / 1440)}d ago`;
  }
</script>

<section class="account" aria-label="Your account">
  <h2>Your account</h2>

  {#if loading}
    <p class="muted">Loading…</p>
  {:else if recoveryCodes}
    <p class="lead">
      Saved. Keep these backup codes somewhere safe — they are the only way back
      in if you forget your password, and this is the only time they are shown.
    </p>
    <ul class="codes">
      {#each recoveryCodes as code}<li>{code}</li>{/each}
    </ul>
    <button class="ghost" onclick={() => navigator.clipboard?.writeText(recoveryCodes?.join('\n') ?? '')}>
      Copy all
    </button>
    <button class="ghost" onclick={() => (recoveryCodes = null)}>Done</button>
  {:else if status && !status.hasPassword}
    <p class="lead">
      This account lives on this computer, and you're signed in because you're
      sitting at it. Your phone doesn't need a password: pair it below. Add a
      username and password only to sign in from another browser — same
      account, same everything in it.
    </p>
    <form onsubmit={claim}>
      <label class="field">
        <span>Username</span>
        <input bind:value={username} autocapitalize="none" autocomplete="username" required />
      </label>
      <label class="field">
        <span>Password</span>
        <input bind:value={password} type="password" autocomplete="new-password" minlength="8" required />
      </label>
      <button class="primary" type="submit" disabled={busy || !username.trim() || password.length < 8}>
        {busy ? 'Saving…' : 'Set up sign-in'}
      </button>
    </form>
  {:else if status}
    <p class="lead">
      Signed in as <strong>{status.username}</strong>. You can sign in from any
      browser with this username. Pair your phone below instead of typing it there.
    </p>
  {/if}

  {#if error}<p class="error">{error}</p>{/if}

  {#if sessions.length}
    <h3>Signed-in devices</h3>
    <ul class="sessions">
      {#each sessions as s}
        <li>
          <div>
            <span class="label">{s.deviceLabel ?? 'Unknown device'}</span>
            {#if s.current}<span class="badge">this one</span>{/if}
            <span class="muted">last used {when(s.lastUsedAt)}</span>
          </div>
          {#if !s.current}
            <button class="ghost small" onclick={() => revoke(s.id)}>Sign out</button>
          {/if}
        </li>
      {/each}
    </ul>
    {#if sessions.length > 1}
      <button class="ghost" onclick={revokeOthers}>Sign out everywhere else</button>
    {/if}
  {/if}
</section>

<style>
  .account {
    padding: 1.1rem 1.25rem;
    border: 1px solid var(--color-border);
    border-radius: 12px;
    background: var(--color-bg-card);
    margin-bottom: 1.25rem;
  }

  h2 {
    margin: 0 0 0.6rem;
    font-size: 1.05rem;
    color: var(--color-text-primary);
  }

  h3 {
    margin: 1.25rem 0 0.5rem;
    font-size: 0.85rem;
    color: var(--color-text-secondary);
  }

  .lead,
  .muted {
    color: var(--color-text-muted);
    font-size: 0.85rem;
    line-height: 1.5;
  }

  .lead {
    margin: 0 0 1rem;
  }

  .field {
    display: block;
    margin-bottom: 0.75rem;
    max-width: 22rem;
  }

  .field span {
    display: block;
    margin-bottom: 0.3rem;
    font-size: 0.8rem;
    color: var(--color-text-secondary);
  }

  input {
    width: 100%;
    padding: 0.55rem 0.7rem;
    border-radius: 8px;
    border: 1px solid var(--color-border);
    background: var(--color-bg-surface);
    color: var(--color-text-primary);
  }

  input:focus {
    outline: none;
    border-color: var(--color-border-focus);
  }

  button {
    cursor: pointer;
    font: inherit;
  }

  .primary {
    padding: 0.55rem 1rem;
    border: none;
    border-radius: 8px;
    background: var(--color-orange);
    color: var(--color-text-inverse);
    font-weight: 600;
  }

  .primary:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }

  .ghost {
    padding: 0.45rem 0.85rem;
    margin-right: 0.5rem;
    border-radius: 8px;
    border: 1px solid var(--color-border);
    background: transparent;
    color: var(--color-text-secondary);
    font-size: 0.85rem;
  }

  .ghost.small {
    padding: 0.3rem 0.6rem;
    font-size: 0.8rem;
  }

  .sessions {
    list-style: none;
    margin: 0 0 0.75rem;
    padding: 0;
  }

  .sessions li {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
    padding: 0.55rem 0;
    border-bottom: 1px solid var(--color-border-subtle);
  }

  .label {
    color: var(--color-text-primary);
    font-size: 0.9rem;
    margin-right: 0.5rem;
  }

  .badge {
    padding: 0.1rem 0.4rem;
    margin-right: 0.5rem;
    border-radius: 5px;
    background: var(--color-orange-dim);
    color: var(--color-orange);
    font-size: 0.7rem;
  }

  .codes {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(8rem, 1fr));
    gap: 0.4rem;
    margin: 0 0 0.9rem;
    padding: 0.8rem;
    list-style: none;
    border-radius: 8px;
    background: var(--color-bg-surface);
    border: 1px solid var(--color-border);
    font-family: ui-monospace, monospace;
    font-size: 0.85rem;
    color: var(--color-text-primary);
  }

  .error {
    margin: 0.75rem 0 0;
    color: var(--color-red);
    font-size: 0.85rem;
  }
</style>
