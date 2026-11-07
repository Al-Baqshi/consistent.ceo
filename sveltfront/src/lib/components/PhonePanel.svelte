<script lang="ts">
  // Pair a phone by being at this computer.
  //
  // The QR carries a code that works once, for 90 seconds, and exists only on
  // this screen. The phone answers with its own key and shows six digits;
  // this panel shows the same six digits next to Allow. Matching numbers mean
  // the phone in your hand is the one asking. After that the phone signs
  // itself back in with its key, from anywhere, with no password.
  // Protocol: crates/gateway/src/pairing.rs.
  import { onDestroy, onMount } from 'svelte';
  import {
    approvePairRequest,
    createPairOffer,
    denyPairRequest,
    getRemoteStatus,
    getWifiAccess,
    setupRemoteAccess,
    listDevices,
    listPairRequests,
    removeDevice,
    setWifiAccess,
    type PairedDevice,
    type PairOffer,
    type PendingPairing,
    type RemoteStatus,
    type WifiAccess
  } from '$lib/api';

  let devices = $state<PairedDevice[]>([]);
  let offer = $state<PairOffer | null>(null);
  let pending = $state<PendingPairing[]>([]);
  let secondsLeft = $state(0);
  let busy = $state(false);
  let error = $state<string | null>(null);
  let notice = $state<string | null>(null);
  // Phone access on Wi-Fi: switched on by the first pairing code when nothing
  // else reaches this computer, and remembered. Shown so it can be turned off.
  let wifi = $state<WifiAccess | null>(null);

  let poll: ReturnType<typeof setInterval> | null = null;
  let tick: ReturnType<typeof setInterval> | null = null;

  onMount(() => {
    void loadDevices();
    void loadWifi();
    void loadRemote();
  });

  async function loadWifi() {
    try {
      wifi = await getWifiAccess();
    } catch {
      wifi = null;
    }
  }

  // Remote access (Tailscale): off by default, switched on here with permission.
  let remote = $state<RemoteStatus | null>(null);
  let remoteBusy = $state(false);
  let remoteError = $state<string | null>(null);

  async function loadRemote() {
    try {
      remote = await getRemoteStatus();
    } catch {
      remote = null;
    }
  }

  async function turnOnRemote() {
    if (remoteBusy) return;
    if (
      !confirm(
        'Turn on remote access?\n\nThis runs “tailscale serve” on this computer so your phone can reach it from anywhere over your private Tailscale network. It is not opened to the public internet, and you can turn it off in Tailscale at any time.'
      )
    )
      return;
    remoteBusy = true;
    remoteError = null;
    try {
      remote = await setupRemoteAccess();
      notice = 'Remote access is on. Make a new code and scan it, and your phone will reach this computer from anywhere.';
    } catch (err) {
      const raw = err instanceof Error ? err.message : '';
      try {
        remoteError = JSON.parse(raw).error ?? raw;
      } catch {
        remoteError = raw || 'Could not turn on remote access.';
      }
      await loadRemote();
    } finally {
      remoteBusy = false;
    }
  }

  async function turnWifiOff() {
    if (
      !confirm(
        'Turn off phone access on Wi-Fi? Your phone will stop reaching this computer at home until you pair again.'
      )
    )
      return;
    try {
      wifi = await setWifiAccess(false);
      cancel();
    } catch (e) {
      error = e instanceof Error ? e.message : 'Could not turn it off.';
    }
  }
  onDestroy(stop);

  async function loadDevices() {
    try {
      devices = await listDevices();
    } catch {
      // No account store on this gateway: the list stays empty.
      devices = [];
    }
  }

  function stop() {
    if (poll) clearInterval(poll);
    if (tick) clearInterval(tick);
    poll = tick = null;
  }

  async function start() {
    busy = true;
    error = null;
    notice = null;
    try {
      offer = await createPairOffer();
      pending = [];
      stop();
      void loadWifi();
      if (!offer.url) return;
      const expires = new Date(offer.expiresAt).getTime();
      const update = () => {
        secondsLeft = Math.max(0, Math.round((expires - Date.now()) / 1000));
      };
      update();
      tick = setInterval(update, 1000);
      // Keep listening after the code expires: a phone that scanned in time
      // has three minutes to be allowed.
      poll = setInterval(checkPending, 2000);
    } catch (e) {
      error = e instanceof Error ? e.message : 'Could not make a pairing code.';
    } finally {
      busy = false;
    }
  }

  async function checkPending() {
    try {
      pending = await listPairRequests();
    } catch {
      /* transient; the next tick retries */
    }
  }

  async function allow(p: PendingPairing) {
    busy = true;
    try {
      await approvePairRequest(p.requestId);
      notice = `${p.label} is paired. It signs itself in from now on.`;
      offer = null;
      pending = [];
      stop();
      await loadDevices();
    } catch (e) {
      error = e instanceof Error ? e.message : 'Could not allow that phone.';
    } finally {
      busy = false;
    }
  }

  async function deny(p: PendingPairing) {
    try {
      await denyPairRequest(p.requestId);
      pending = pending.filter((x) => x.requestId !== p.requestId);
    } catch (e) {
      error = e instanceof Error ? e.message : 'Could not refuse that phone.';
    }
  }

  async function remove(d: PairedDevice) {
    if (!confirm(`Remove ${d.label}? It is signed out at once and must be paired again.`)) return;
    try {
      await removeDevice(d.deviceId);
      await loadDevices();
    } catch (e) {
      error = e instanceof Error ? e.message : 'Could not remove that device.';
    }
  }

  function cancel() {
    stop();
    offer = null;
    pending = [];
  }

  function spaced(code: string): string {
    return code.length === 6 ? `${code.slice(0, 3)} ${code.slice(3)}` : code;
  }

  function when(iso: string): string {
    const mins = Math.round((Date.now() - new Date(iso).getTime()) / 60_000);
    if (!Number.isFinite(mins)) return '—';
    if (mins < 1) return 'just now';
    if (mins < 60) return `${mins}m ago`;
    if (mins < 1440) return `${Math.round(mins / 60)}h ago`;
    return `${Math.round(mins / 1440)}d ago`;
  }
</script>

<section class="phone" aria-label="Your phone">
  <h2>Your phone</h2>
  <p class="lead">
    Pair your phone by scanning a code on this screen. No password: the phone
    keeps its own key, and only this computer can let it in.
  </p>

  {#if notice}<p class="ok">{notice}</p>{/if}
  {#if error}<p class="error">{error}</p>{/if}

  {#if offer && !offer.url}
    <div class="callout">
      {#if offer.wifiAvailable}
        <p>
          Couldn't find your Wi-Fi network. Make sure this computer is connected to
          Wi-Fi, then try again.
        </p>
      {:else}
        <p>Your phone can't reach this computer yet.</p>
      {/if}
      <button class="ghost" onclick={start} disabled={busy}>Try again</button>
      <button class="ghost" onclick={cancel}>Close</button>
    </div>
    {@render awayFromHome()}
  {:else if offer}
    <div class="pairing">
      <div class="qr" aria-label="Pairing QR code">
        {@html offer.qrSvg}
      </div>
      <div class="steps">
        {#if pending.length === 0}
          <p>
            In the Consistent CEO app, open <strong>Settings → Scan to connect</strong>
            and point it at this code.
          </p>
          {#if offer.via === 'lan'}
            <p>Your phone needs to be on the same Wi-Fi as this computer.</p>
            <p class="muted">
              If this computer asks whether to allow incoming connections, click
              <strong>Allow</strong>.
            </p>
          {/if}
          <p class="muted">
            {secondsLeft > 0 ? `Works once, for ${secondsLeft}s more.` : 'Code expired.'}
          </p>
          {#if secondsLeft === 0}
            <button class="ghost" onclick={start} disabled={busy}>New code</button>
          {/if}
          <button class="ghost" onclick={cancel}>Cancel</button>
        {:else}
          {#each pending as p (p.requestId)}
            <div class="request">
              <p><strong>{p.label}</strong> wants to pair.</p>
              <p class="muted">Allow it only if your phone shows</p>
              <p class="digits">{spaced(p.code)}</p>
              <button class="primary" onclick={() => allow(p)} disabled={busy}>Allow</button>
              <button class="ghost" onclick={() => deny(p)} disabled={busy}>Deny</button>
            </div>
          {/each}
        {/if}
      </div>
    </div>
    {#if offer.via === 'lan'}{@render awayFromHome()}{/if}
  {:else}
    <button class="primary" onclick={start} disabled={busy}>
      {busy ? 'Making a code…' : 'Pair a phone'}
    </button>
    {@render awayFromHome()}
  {/if}

  {#if wifi?.enabled}
    <p class="muted wifi">
      Phone access on Wi-Fi is on.
      <button class="link" onclick={turnWifiOff}>Turn off</button>
    </p>
  {/if}

  {#if devices.length}
    <h3>Devices</h3>
    <ul class="devices">
      {#each devices as d (d.deviceId)}
        <li>
          <div>
            <span class="label">{d.label}</span>
            {#if d.paired}<span class="badge">paired</span>{/if}
            <span class="muted">last seen {when(d.lastSeen || d.createdAt)}</span>
          </div>
          <button class="ghost small" onclick={() => remove(d)}>Remove</button>
        </li>
      {/each}
    </ul>
  {/if}
</section>

{#snippet awayFromHome()}
  <details class="away" open={!!remoteError}>
    <summary>
      {remote?.serveUrl ? 'Remote access is on' : 'Want to use it away from home? Set up remote access (optional)'}
    </summary>
    {#if remote?.serveUrl}
      <p>
        Your phone can reach this computer from anywhere through your private Tailscale network
        (<code>{remote.serveUrl}</code>). Make a new code if the phone was paired before this was on.
      </p>
    {:else if remote && !remote.tailscaleInstalled}
      <p>
        Away from home, your phone reaches this computer through
        <a href="https://tailscale.com/download" target="_blank" rel="noreferrer">Tailscale</a>
        (free). Install it on this computer and on your phone and sign in to the same account on both.
      </p>
      <button type="button" class="ghost small" onclick={loadRemote}>I installed it, check again</button>
    {:else if remote && !remote.tailscaleRunning}
      <p>Tailscale is installed but not signed in. Open it, sign in, and come back.</p>
      <button type="button" class="ghost small" onclick={loadRemote}>Check again</button>
    {:else}
      <p>
        Tailscale is ready on this computer. One click turns on remote access so your phone reaches
        this computer from anywhere, over your private network only. Install Tailscale on your phone
        and sign in to the same account too.
      </p>
      <button type="button" class="small" onclick={turnOnRemote} disabled={remoteBusy}>
        {remoteBusy ? 'Turning on…' : 'Turn on remote access'}
      </button>
    {/if}
    {#if remoteError}<p class="error">{remoteError}</p>{/if}
  </details>
{/snippet}

<style>
  .phone {
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

  .pairing {
    display: flex;
    flex-wrap: wrap;
    gap: 1.25rem;
    align-items: flex-start;
  }

  /* The QR stays black on white in both themes: phone cameras read dark
     modules on a light ground most reliably. */
  .qr {
    width: 240px;
    max-width: 100%;
    padding: 8px;
    border-radius: 10px;
    background: #fff;
    line-height: 0;
  }

  .qr :global(svg) {
    width: 100%;
    height: auto;
  }

  .steps {
    flex: 1 1 14rem;
    min-width: 0;
    color: var(--color-text-primary);
    font-size: 0.9rem;
  }

  .steps p {
    margin: 0 0 0.6rem;
  }

  .digits {
    font-family: ui-monospace, monospace;
    font-size: 1.9rem;
    letter-spacing: 0.08em;
    color: var(--color-text-primary);
  }

  .request + .request {
    margin-top: 1rem;
    padding-top: 1rem;
    border-top: 1px solid var(--color-border-subtle);
  }

  .callout {
    padding: 0.8rem 0.9rem;
    border-radius: 8px;
    border: 1px solid var(--color-border);
    background: var(--color-bg-surface);
    color: var(--color-text-primary);
    font-size: 0.85rem;
    line-height: 1.5;
  }

  .callout p {
    margin: 0 0 0.7rem;
  }

  .away {
    margin-top: 0.9rem;
    color: var(--color-text-muted);
    font-size: 0.82rem;
    line-height: 1.5;
  }

  .away summary {
    cursor: pointer;
    color: var(--color-text-secondary);
  }

  .away p {
    margin: 0.5rem 0 0;
  }

  .wifi {
    margin: 1rem 0 0;
  }

  .link {
    padding: 0;
    border: none;
    background: none;
    color: var(--color-text-secondary);
    text-decoration: underline;
    font-size: inherit;
  }

  code {
    font-family: ui-monospace, monospace;
    font-size: 0.8rem;
  }

  button {
    cursor: pointer;
    font: inherit;
  }

  .primary {
    padding: 0.55rem 1rem;
    margin-right: 0.5rem;
    border: none;
    border-radius: 8px;
    background: var(--color-orange);
    color: var(--color-text-inverse);
    font-weight: 600;
  }

  .primary:disabled,
  .ghost:disabled {
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

  .devices {
    list-style: none;
    margin: 0;
    padding: 0;
  }

  .devices li {
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

  .ok {
    margin: 0 0 0.75rem;
    color: var(--color-green, var(--color-text-primary));
    font-size: 0.85rem;
  }

  .error {
    margin: 0 0 0.75rem;
    color: var(--color-red);
    font-size: 0.85rem;
  }
</style>
