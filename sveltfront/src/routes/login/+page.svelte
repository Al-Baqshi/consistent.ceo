<script lang="ts">
  // The only way into the app, and the reason the desktop can log in at all.
  //
  // Everything here talks to the gateway on this origin: no hosted identity
  // service, no email, no redirect anywhere. A self-hosted instance with no
  // internet behaves exactly like a connected one.
  //
  // With `?approval=<id>` this doubles as the approving half of the desktop
  // login: sign in (or already be signed in), then confirm the code shown in
  // the app's window. That code is what ties this browser to that request.
  import { onDestroy, onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import { page } from '$app/stores';
  import {
    approveSignIn,
    denySignIn,
    getApprovalRequest,
    getAuthToken,
    login,
    passkeyLogin,
    passkeysAvailable,
    recoverAccount,
    registerAccount,
    setAuthToken,
    setTenantId,
    type ApprovalDetail,
    type AuthSession,
  } from '$lib/api';
  import { invoke, isTauri } from '$lib/tauri';
  import Seo from '$lib/components/Seo.svelte';

  type Mode = 'signin' | 'register' | 'recover';

  let mode = $state<Mode>('signin');
  let username = $state('');
  let password = $state('');
  let recoveryCode = $state('');
  let newPassword = $state('');
  let inviteCode = $state('');
  let busy = $state(false);
  let error = $state<string | null>(null);

  /** Shown once, after registering. Losing these means losing the account. */
  let recoveryCodes = $state<string[] | null>(null);

  // ----- Approval mode (the desktop is waiting on this browser) -----
  let approvalId = $state<string | null>(null);
  let approval = $state<ApprovalDetail | null>(null);
  let approvalCode = $state('');
  let approvalDone = $state<'approved' | 'denied' | null>(null);
  let signedIn = $state(false);

  const canUsePasskey = passkeysAvailable();

  // ----- Desktop mode (this window waits while the browser does the signing in) -----
  const isDesktop = isTauri();
  type DesktopLogin = { requestId: string; code: string; expiresAt: string; verificationUrl: string };
  let desktop = $state<DesktopLogin | null>(null);
  let desktopError = $state<string | null>(null);
  let pollTimer: ReturnType<typeof setInterval> | null = null;
  /** Suppresses the sign-in options until we know whether we need any. */
  let checking = $state(isDesktop);

  onMount(async () => {
    approvalId = $page.url.searchParams.get('approval');
    signedIn = !!getAuthToken();
    if (approvalId) {
      await loadApproval();
      return;
    }
    if (signedIn) {
      // Already signed in and nothing to approve — nothing to do here.
      goto('/');
      return;
    }
    if (isDesktop) {
      // Your own machine, your own data: sign in as its owner and get out of
      // the way. Only if that isn't available do we ask anything of anyone.
      if (await tryLocalLogin()) return;
      checking = false;
      await startDesktopLogin();
    }
  });

  /**
   * Trade this machine's local token for a session, with no form and no
   * browser. Returns true when it worked and we've already navigated away.
   */
  async function tryLocalLogin(): Promise<boolean> {
    try {
      const token = await invoke<string | null>('login_local');
      if (!token) return false;
      setAuthToken(token);
      goto('/');
      return true;
    } catch (err) {
      // Not fatal: the browser flow below still works.
      console.warn('local sign-in unavailable', err);
      return false;
    }
  }

  onDestroy(stopPolling);

  /**
   * Ask the Tauri side to register a sign-in and open the system browser. The
   * PKCE verifier stays in Rust — this window only ever sees the code to show
   * and, eventually, the session it was granted.
   */
  async function startDesktopLogin() {
    desktopError = null;
    try {
      desktop = await invoke<DesktopLogin>('login_start');
      startPolling();
    } catch (err) {
      desktopError = err instanceof Error ? err.message : String(err);
    }
  }

  async function openVerificationUrl() {
    if (!desktop?.verificationUrl) return;
    try {
      await invoke('open_verification_url', { url: desktop.verificationUrl });
    } catch (err) {
      desktopError = err instanceof Error ? err.message : String(err);
    }
  }

  function startPolling() {
    stopPolling();
    pollTimer = setInterval(async () => {
      try {
        const r = await invoke<{ status: string; token?: string }>('login_poll');
        if (r.token) {
          stopPolling();
          setAuthToken(r.token);
          goto('/');
        } else if (r.status === 'denied' || r.status === 'expired') {
          stopPolling();
          desktopError =
            r.status === 'denied'
              ? 'That sign-in was refused in the browser.'
              : 'This sign-in timed out. Start again.';
          desktop = null;
        }
      } catch (err) {
        stopPolling();
        desktopError = err instanceof Error ? err.message : String(err);
      }
    }, 1500);
  }

  function stopPolling() {
    if (pollTimer) clearInterval(pollTimer);
    pollTimer = null;
  }

  async function cancelDesktopLogin() {
    stopPolling();
    try {
      await invoke('login_cancel');
    } catch (_) {}
    desktop = null;
  }

  async function loadApproval() {
    if (!approvalId) return;
    try {
      approval = await getApprovalRequest(approvalId);
    } catch (_) {
      // Unauthenticated callers can't read it; sign in first and we retry.
      approval = null;
    }
  }

  /** Land a fresh session and go wherever the user was actually headed. */
  async function accept(session: AuthSession) {
    setAuthToken(session.token);
    setTenantId(session.workspaceId);
    signedIn = true;
    if (approvalId) {
      await loadApproval();
      return;
    }
    if (recoveryCodes) return; // let them write the codes down first
    goto('/');
  }

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    busy = true;
    error = null;
    try {
      if (mode === 'signin') {
        await accept(await login(username, password, deviceLabel()));
      } else if (mode === 'register') {
        const session = await registerAccount(username, password, {
          device: deviceLabel(),
          inviteCode: inviteCode.trim() || undefined,
        });
        recoveryCodes = session.recoveryCodes ?? [];
        await accept(session);
      } else {
        await accept(
          await recoverAccount(
            username,
            recoveryCode,
            newPassword.trim() || undefined,
            deviceLabel(),
          ),
        );
      }
    } catch (err) {
      error = err instanceof Error ? err.message : 'Something went wrong.';
    } finally {
      busy = false;
    }
  }

  async function signInWithPasskey() {
    if (!username.trim()) {
      error = 'Enter your username first — the passkey is looked up against it.';
      return;
    }
    busy = true;
    error = null;
    try {
      await accept(await passkeyLogin(username));
    } catch (err) {
      error = err instanceof Error ? err.message : 'That passkey did not work.';
    } finally {
      busy = false;
    }
  }

  async function confirmApproval() {
    if (!approvalId) return;
    busy = true;
    error = null;
    try {
      await approveSignIn(approvalId, approvalCode.trim(), 'Browser');
      approvalDone = 'approved';
    } catch (err) {
      error = err instanceof Error ? err.message : 'Could not approve.';
    } finally {
      busy = false;
    }
  }

  async function refuseApproval() {
    if (!approvalId) return;
    busy = true;
    error = null;
    try {
      await denySignIn(approvalId);
      approvalDone = 'denied';
    } catch (err) {
      error = err instanceof Error ? err.message : 'Could not deny.';
    } finally {
      busy = false;
    }
  }

  /** A label the user will recognise in their session list later. */
  function deviceLabel(): string {
    if (typeof navigator === 'undefined') return 'Browser';
    const ua = navigator.userAgent;
    const browser = /Firefox/.test(ua)
      ? 'Firefox'
      : /Edg\//.test(ua)
        ? 'Edge'
        : /Chrome/.test(ua)
          ? 'Chrome'
          : /Safari/.test(ua)
            ? 'Safari'
            : 'Browser';
    const os = /Mac/.test(ua) ? 'macOS' : /Win/.test(ua) ? 'Windows' : /Linux/.test(ua) ? 'Linux' : '';
    return os ? `${browser} on ${os}` : browser;
  }

  function switchTo(next: Mode) {
    mode = next;
    error = null;
  }

  const heading = $derived(
    mode === 'register' ? 'Create your account' : mode === 'recover' ? 'Use a backup code' : 'Sign in',
  );
</script>

<Seo title="Sign in" description="Sign in to Consistent CEO." />

<div class="shell">
  <main class="card">
    <header class="brand">
      <span class="mark">C</span>
      <div>
        <h1>Consistent CEO</h1>
        <p class="sub">Your accounts live on this machine.</p>
      </div>
    </header>

    {#if approvalDone}
      <section class="done">
        <h2>{approvalDone === 'approved' ? 'Signed in' : 'Refused'}</h2>
        <p>
          {approvalDone === 'approved'
            ? 'The app has been signed in. You can close this tab and go back to it.'
            : 'That request was refused and nothing was signed in.'}
        </p>
      </section>
    {:else if checking}
      <section class="waiting">
        <div class="spinner-row">
          <span class="spinner" aria-hidden="true"></span>
          <span>Opening your workspace…</span>
        </div>
      </section>
    {:else if desktop}
      <!-- The waiting half of the desktop login. The six digits are the whole
           point: they are what the person types in the browser to prove the two
           windows belong to the same intention. -->
      <section class="waiting">
        <h2>Finish signing in</h2>
        <p class="lead">
          Your browser is open. Sign in there, then enter this code to confirm it
          was you.
        </p>
        <p class="big-code">{desktop.code}</p>
        <p class="hint">
          Browser didn't open?
          <button class="inline-link" type="button" onclick={openVerificationUrl}>
            Open it manually
          </button>
        </p>
        <div class="spinner-row">
          <span class="spinner" aria-hidden="true"></span>
          <span>Waiting for you to approve…</span>
        </div>
        <button class="ghost wide" onclick={cancelDesktopLogin}>Cancel</button>
      </section>
    {:else if isDesktop && !approvalId}
      <section class="waiting">
        <h2>Sign in</h2>
        <p class="lead">
          Signing in happens in your browser, so your password manager and
          passkeys work. Nothing leaves this machine.
        </p>
        {#if desktopError}<p class="error">{desktopError}</p>{/if}
        <button class="primary" onclick={startDesktopLogin}>Open my browser</button>
      </section>
    {:else if approvalId && signedIn}
      <!-- The approving half of the desktop login. -->
      <section class="approve">
        <h2>Sign in to {approval?.context ?? 'the app'}?</h2>
        <p class="lead">
          Type the six digits shown in the app's window. If you didn't start this,
          refuse it.
        </p>
        {#if approval?.status && approval.status !== 'pending'}
          <p class="warn">This request is {approval.status}. Start the sign-in again from the app.</p>
        {/if}
        <label class="field">
          <span>Code from the app</span>
          <input
            bind:value={approvalCode}
            inputmode="numeric"
            autocomplete="one-time-code"
            maxlength="6"
            placeholder="000000"
            class="code-input"
          />
        </label>
        {#if error}<p class="error">{error}</p>{/if}
        <div class="row">
          <button class="primary" onclick={confirmApproval} disabled={busy || approvalCode.trim().length !== 6}>
            {busy ? 'Confirming…' : 'Approve'}
          </button>
          <button class="ghost" onclick={refuseApproval} disabled={busy}>Not me</button>
        </div>
      </section>
    {:else if recoveryCodes}
      <section class="codes">
        <h2>Save these backup codes</h2>
        <p class="lead">
          They are the only way back into this account if you forget your password.
          Each one works once, and this is the only time they are shown.
        </p>
        <ul>
          {#each recoveryCodes as code}<li>{code}</li>{/each}
        </ul>
        <button
          class="ghost"
          onclick={() => navigator.clipboard?.writeText(recoveryCodes?.join('\n') ?? '')}
        >
          Copy all
        </button>
        <button
          class="primary"
          onclick={() => {
            recoveryCodes = null;
            approvalId ? loadApproval() : goto('/');
          }}
        >
          I've saved them
        </button>
      </section>
    {:else}
      {#if approvalId}
        <p class="notice">Sign in to approve the request from {approval?.context ?? 'the app'}.</p>
      {/if}

      <h2>{heading}</h2>

      <form onsubmit={submit}>
        <label class="field">
          <span>Username</span>
          <!-- svelte-ignore a11y_autofocus -->
          <input bind:value={username} autocomplete="username" autocapitalize="none" autofocus required />
        </label>

        {#if mode === 'recover'}
          <label class="field">
            <span>Backup code</span>
            <input bind:value={recoveryCode} placeholder="XXXXX-XXXXX" autocapitalize="characters" required />
          </label>
          <label class="field">
            <span>New password <em>(optional)</em></span>
            <input bind:value={newPassword} type="password" autocomplete="new-password" minlength="8" />
          </label>
          <p class="hint">Recovering signs every other device out.</p>
        {:else}
          <label class="field">
            <span>Password</span>
            <input
              bind:value={password}
              type="password"
              autocomplete={mode === 'register' ? 'new-password' : 'current-password'}
              minlength={mode === 'register' ? 8 : undefined}
              required
            />
          </label>
        {/if}

        {#if mode === 'register'}
          <label class="field">
            <span>Invite code <em>(only if this gateway asks for one)</em></span>
            <input bind:value={inviteCode} autocapitalize="none" />
          </label>
        {/if}

        {#if error}<p class="error">{error}</p>{/if}

        <button class="primary" type="submit" disabled={busy}>
          {busy ? 'Working…' : mode === 'register' ? 'Create account' : mode === 'recover' ? 'Recover' : 'Sign in'}
        </button>
      </form>

      {#if mode === 'signin' && canUsePasskey}
        <button class="ghost wide" onclick={signInWithPasskey} disabled={busy}>
          Use a passkey
        </button>
      {/if}

      <nav class="alt">
        {#if mode !== 'signin'}
          <button class="link" onclick={() => switchTo('signin')}>Sign in</button>
        {/if}
        {#if mode !== 'register'}
          <button class="link" onclick={() => switchTo('register')}>Create an account</button>
        {/if}
        {#if mode !== 'recover'}
          <button class="link" onclick={() => switchTo('recover')}>Use a backup code</button>
        {/if}
      </nav>
    {/if}
  </main>
</div>

<style>
  .shell {
    min-height: 100vh;
    display: grid;
    place-items: center;
    padding: 2rem 1rem;
    background: var(--color-bg-base);
  }

  .card {
    width: 100%;
    max-width: 26rem;
    background: var(--color-bg-card);
    border: 1px solid var(--color-border);
    border-radius: 14px;
    padding: 1.75rem;
  }

  .brand {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    margin-bottom: 1.5rem;
  }

  .mark {
    display: grid;
    place-items: center;
    width: 2.25rem;
    height: 2.25rem;
    border-radius: 9px;
    background: var(--color-orange-dim);
    color: var(--color-orange);
    font-weight: 700;
  }

  h1 {
    font-size: 1rem;
    margin: 0;
    color: var(--color-text-primary);
  }

  h2 {
    font-size: 1.35rem;
    margin: 0 0 1rem;
    color: var(--color-text-primary);
  }

  .sub,
  .lead,
  .hint {
    margin: 0;
    color: var(--color-text-muted);
    font-size: 0.85rem;
  }

  .lead {
    margin-bottom: 1.25rem;
    line-height: 1.5;
  }

  .hint {
    margin: -0.35rem 0 0.25rem;
  }

  .notice {
    margin: 0 0 1rem;
    padding: 0.7rem 0.85rem;
    border-radius: 9px;
    background: var(--color-orange-dim);
    color: var(--color-text-secondary);
    font-size: 0.85rem;
  }

  .field {
    display: block;
    margin-bottom: 0.9rem;
  }

  .field span {
    display: block;
    margin-bottom: 0.35rem;
    font-size: 0.8rem;
    color: var(--color-text-secondary);
  }

  .field em {
    color: var(--color-text-muted);
    font-style: normal;
  }

  input {
    width: 100%;
    padding: 0.65rem 0.75rem;
    border-radius: 9px;
    border: 1px solid var(--color-border);
    background: var(--color-bg-surface);
    color: var(--color-text-primary);
    font-size: 0.95rem;
  }

  input:focus {
    outline: none;
    border-color: var(--color-border-focus);
  }

  .code-input {
    font-size: 1.6rem;
    letter-spacing: 0.4em;
    text-align: center;
    font-variant-numeric: tabular-nums;
  }

  button {
    cursor: pointer;
    font: inherit;
  }

  .primary {
    width: 100%;
    padding: 0.7rem;
    margin-top: 0.35rem;
    border: none;
    border-radius: 9px;
    background: var(--color-orange);
    color: var(--color-text-inverse);
    font-weight: 600;
  }

  .primary:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }

  .ghost {
    padding: 0.7rem;
    border-radius: 9px;
    border: 1px solid var(--color-border);
    background: transparent;
    color: var(--color-text-secondary);
  }

  .ghost.wide {
    width: 100%;
    margin-top: 0.6rem;
  }

  .row {
    display: flex;
    gap: 0.6rem;
    align-items: stretch;
  }

  .row .primary {
    flex: 1;
  }

  .alt {
    display: flex;
    flex-wrap: wrap;
    gap: 1rem;
    margin-top: 1.25rem;
    padding-top: 1rem;
    border-top: 1px solid var(--color-border-subtle);
  }

  .link {
    padding: 0;
    border: none;
    background: none;
    color: var(--color-orange);
    font-size: 0.85rem;
  }

  .error {
    margin: 0 0 0.75rem;
    color: var(--color-red);
    font-size: 0.85rem;
  }

  .warn {
    margin: 0 0 0.75rem;
    color: var(--color-orange);
    font-size: 0.85rem;
  }

  .codes ul {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 0.4rem;
    margin: 0 0 1rem;
    padding: 0.9rem;
    list-style: none;
    border-radius: 9px;
    background: var(--color-bg-surface);
    border: 1px solid var(--color-border);
    font-family: ui-monospace, monospace;
    font-size: 0.9rem;
    color: var(--color-text-primary);
  }

  .codes .ghost {
    width: 100%;
    margin-bottom: 0.6rem;
  }

  .done p {
    color: var(--color-text-secondary);
    line-height: 1.5;
  }

  .big-code {
    margin: 0 0 0.75rem;
    padding: 0.9rem;
    border-radius: 10px;
    background: var(--color-bg-surface);
    border: 1px solid var(--color-border);
    text-align: center;
    font-family: ui-monospace, monospace;
    font-size: 2.1rem;
    letter-spacing: 0.35em;
    text-indent: 0.35em;
    color: var(--color-text-primary);
    font-variant-numeric: tabular-nums;
  }

  .inline-link {
    padding: 0;
    border: none;
    background: none;
    color: var(--color-orange);
    text-decoration: underline;
  }

  .spinner-row {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    margin: 1rem 0;
    color: var(--color-text-muted);
    font-size: 0.85rem;
  }

  .spinner {
    width: 0.85rem;
    height: 0.85rem;
    border-radius: 50%;
    border: 2px solid var(--color-border);
    border-top-color: var(--color-orange);
    animation: spin 0.8s linear infinite;
  }

  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .spinner {
      animation: none;
    }
  }
</style>
