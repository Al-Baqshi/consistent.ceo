<script lang="ts">
  // The AI page: one question — "what should Consistent CEO think with?" —
  // with every honest answer on one screen: the subscription you already pay
  // for, a model on this computer, or an API key. Each row says whether it is
  // ready and offers the single next step. Every option is tested with one
  // real message before it becomes the AI, and once chosen it runs everything
  // (chat, heartbeats, missions, entity analysis), not just chat.
  //
  // Backed by `GET /api/ai`, `POST /api/ai/connect`, `DELETE /api/ai/choice`
  // (crates/gateway/src/control_api/ai_api.rs) and `/api/ollama/pull`.
  import { onMount } from 'svelte';
  import { api, getLocalModel, type LocalModelInfo } from '$lib/api';
  import { isTauri } from '$lib/tauri';
  import { openSignInUrl } from '$lib/pty';
  import Terminal from '$lib/components/Terminal.svelte';

  type Kind = 'subscription' | 'local' | 'api_key';
  type Provider = {
    id: string;
    kind: Kind;
    label: string;
    ready: boolean;
    status: 'ready' | 'not_installed' | 'signed_out' | 'not_running' | 'no_model' | 'no_key';
    detail: string;
    models: string[];
    defaultModel: string;
    helpUrl: string;
    cliPath?: string | null;
  };
  type Status = {
    active: { provider: string; model: string; label: string; kind: Kind; chosen: boolean } | null;
    brokenChoice: { provider: string; model: string } | null;
    providers: Provider[];
  };
  type ConnectResult = {
    ok: boolean;
    provider: string;
    model: string;
    reply?: string | null;
    latencyMs?: number | null;
    error?: string | null;
  };
  type Pull = {
    model: string | null;
    status: 'idle' | 'pulling' | 'done' | 'failed';
    step: string | null;
    completed: number;
    total: number;
    error: string | null;
  };

  const CHIP: Record<Provider['status'], string> = {
    ready: 'Ready',
    not_installed: 'Not installed',
    signed_out: 'Needs sign-in',
    not_running: 'Not running',
    no_model: 'No model yet',
    no_key: 'No key yet'
  };

  let status = $state<Status | null>(null);
  let loadError = $state('');
  let busy = $state<string | null>(null);
  let result = $state<{ provider: string; ok: boolean; text: string } | null>(null);
  // '' means "let the app choose" — the default for API keys, whose usable
  // models depend on the account and are looked up when connecting.
  let model = $state<Record<string, string>>({});
  let syncedActive = '';
  let keyOpen = $state(false);
  let keyProvider = $state('anthropic');
  let apiKey = $state('');
  let pull = $state<Pull | null>(null);
  let inDesktop = $state(false);
  let alive = true;

  const byKind = (k: Kind) => (status?.providers ?? []).filter((p) => p.kind === k);
  const keyProviders = $derived(byKind('api_key'));
  const selectedKeyProvider = $derived(keyProviders.find((p) => p.id === keyProvider));
  const savedKeys = $derived(keyProviders.filter((p) => p.ready));
  const ollama = $derived(byKind('local')[0]);

  /** A model list that always contains the current pick (e.g. one the app found for a key). */
  function optionsFor(p: Provider): string[] {
    const cur = model[p.id];
    return cur && !p.models.includes(cur) ? [cur, ...p.models] : p.models;
  }

  async function refresh() {
    try {
      const next = await api<Status>('/api/ai', { silent: true });
      status = next;
      loadError = '';
      for (const p of next.providers) {
        if (model[p.id] === undefined) {
          model[p.id] = p.kind === 'api_key' ? '' : p.defaultModel;
        } else if (p.kind === 'local' && p.models.length > 0 && !p.models.includes(model[p.id])) {
          // The model the person had picked was removed from Ollama.
          model[p.id] = p.defaultModel;
        }
      }
      // Follow what is actually in use, but only when that changes — not on
      // every poll, which would undo a model the person is choosing right now.
      const key = next.active ? `${next.active.provider}:${next.active.model}` : '';
      if (key !== syncedActive) {
        syncedActive = key;
        if (next.active) model[next.active.provider] = next.active.model;
      }
      if (!keyProviders.some((p) => p.id === keyProvider) && keyProviders[0]) {
        keyProvider = keyProviders[0].id;
      }
    } catch (err) {
      loadError = 'Could not reach the gateway to check your AI. Is the app running?';
      console.error('AI status failed', err);
    }
  }

  function errorText(err: unknown): string {
    const msg = err instanceof Error ? err.message : String(err ?? '');
    try {
      const parsed = JSON.parse(msg);
      if (parsed && typeof parsed.error === 'string') return parsed.error;
    } catch {
      /* not JSON */
    }
    return msg || 'Something went wrong. Try again.';
  }

  async function connect(p: Provider, key?: string) {
    busy = p.id;
    result = null;
    try {
      const res = await api<ConnectResult>('/api/ai/connect', {
        method: 'POST',
        body: JSON.stringify({ provider: p.id, model: model[p.id] || undefined, apiKey: key || undefined }),
        silent: true
      });
      if (res.ok) {
        const secs = res.latencyMs ? ` in ${(res.latencyMs / 1000).toFixed(1)}s` : '';
        result = {
          provider: p.id,
          ok: true,
          text: `Connected. ${p.label} replied${secs}. Everything now runs on ${p.label} · ${res.model}.`
        };
        if (key) apiKey = '';
      } else {
        result = { provider: p.id, ok: false, text: res.error ?? 'It did not answer. Try again.' };
      }
    } catch (err) {
      result = { provider: p.id, ok: false, text: errorText(err) };
    } finally {
      busy = null;
      await refresh();
    }
  }

  async function useAutomatic() {
    result = null;
    try {
      await api('/api/ai/choice', { method: 'DELETE', silent: true });
    } catch (err) {
      result = { provider: '', ok: false, text: errorText(err) };
    }
    await refresh();
  }

  // ── Local model download ──────────────────────────────────────────────────

  const FALLBACK_LOCAL_MODEL = 'gemma4:e2b';
  let localModel = $state<LocalModelInfo | null>(null);
  const consistentModel = $derived(localModel?.ollama.model || FALLBACK_LOCAL_MODEL);
  const consistentName = $derived(localModel?.name || 'Consistent AI');
  const sameModel = (a: string, b: string) => a.replace(/:latest$/, '') === b.replace(/:latest$/, '');
  const consistentInstalled = $derived(!!ollama?.models.some((m) => sameModel(m, consistentModel)));
  const consistentInUse = $derived(
    status?.active?.provider === ollama?.id && sameModel(status?.active?.model ?? '', consistentModel)
  );

  async function watchPull() {
    let misses = 0;
    while (alive && pull && pull.status === 'pulling') {
      await new Promise((r) => setTimeout(r, 1000));
      try {
        pull = await api<Pull>('/api/ollama/pull', { silent: true });
        misses = 0;
      } catch {
        // A busy gateway is fine for a while; a gone one is not.
        if (++misses >= 30) {
          pull = null;
          result = { provider: 'ollama', ok: false, text: 'Lost contact with the app during the download.' };
          return;
        }
      }
    }
    if (!alive) return;
    if (pull?.status === 'failed') {
      result = { provider: 'ollama', ok: false, text: pull.error ?? 'The download failed.' };
      return;
    }
    const pulled = pull?.model ?? null;
    pull = null;
    await refresh();
    if (ollama && pulled) {
      const listed = ollama.models.find((m) => sameModel(m, pulled));
      if (listed) model[ollama.id] = listed;
    }
    if (ollama?.ready) await connect(ollama);
  }

  async function downloadLocal(name?: string) {
    result = null;
    try {
      pull = await api<Pull>('/api/ollama/pull', {
        method: 'POST',
        body: JSON.stringify(name ? { model: name } : {}),
        silent: true
      });
    } catch (err) {
      result = { provider: 'ollama', ok: false, text: errorText(err) };
      return;
    }
    await watchPull();
  }

  const pct = (p: Pull) => (p.total > 0 ? Math.min(100, Math.round((p.completed / p.total) * 100)) : 0);
  const gb = (n: number) => (n / 1e9).toFixed(1);
  const downloadLabel = $derived(
    localModel?.ollama.approxBytes
      ? `Download ${consistentName} · ~${gb(localModel.ollama.approxBytes)} GB`
      : `Download ${consistentName}`
  );

  // ── In-app sign-in (desktop only) ─────────────────────────────────────────
  // Runs the CLI's own login in an embedded terminal, using the exact binary
  // detection found — an editor's bundled `claude` is not on PATH.
  let signIn = $state<Provider | null>(null);
  let signInScript = $state('');
  let signInUrl = $state<string | null>(null);
  let signInDone = $state(false);
  let term = $state.raw<Terminal | null>(null);

  function shellQuote(s: string) {
    return `'${s.replace(/'/g, `'\\''`)}'`;
  }

  function openSignIn(p: Provider) {
    const bin = shellQuote(p.cliPath || (p.id === 'codex' ? 'codex' : 'claude'));
    signInScript = p.id === 'codex' ? `${bin} login` : `${bin} auth login`;
    signInUrl = null;
    signInDone = false;
    signIn = p;
  }

  async function closeSignIn() {
    try {
      await term?.kill();
    } catch {
      /* already stopped */
    }
    signIn = null;
  }

  async function onSignInExit(p: Provider, code: number | null) {
    signInDone = true;
    if (code === 0) {
      signIn = null;
      await refresh();
      await connect(p);
    } else {
      result = { provider: p.id, ok: false, text: 'Sign-in did not finish. Read the terminal, then try again.' };
    }
  }

  function canSignIn(p: Provider) {
    return inDesktop && p.kind === 'subscription' && p.status !== 'not_installed';
  }

  onMount(() => {
    inDesktop = isTauri();
    void refresh();
    getLocalModel()
      .then((m) => {
        if (alive && m?.ollama?.model) localModel = m;
      })
      .catch(() => {});
    // A download started earlier (this page was closed and reopened) is still
    // running in the gateway; pick the progress bar back up.
    api<Pull>('/api/ollama/pull', { silent: true })
      .then((p) => {
        if (alive && p.status === 'pulling' && !pull) {
          pull = p;
          void watchPull();
        }
      })
      .catch(() => {});
    const timer = setInterval(() => {
      if (!busy && !pull) void refresh();
    }, 20_000);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  });
</script>

{#snippet chip(p: Provider, inUse: boolean)}
  {#if inUse}
    <span class="chip chip-use">In use</span>
  {:else}
    <span class="chip" class:chip-ok={p.status === 'ready'} class:chip-warn={p.status === 'signed_out' || p.status === 'no_model'}>
      {CHIP[p.status]}
    </span>
  {/if}
{/snippet}

{#snippet progress()}
  {#if pull && pull.status === 'pulling'}
    <div class="progress" role="status" aria-live="polite">
      <div class="bar"><span style="width: {pct(pull)}%"></span></div>
      <p class="opt-detail">
        Downloading {pull.model && sameModel(pull.model, consistentModel) ? consistentName : pull.model}:
        {pull.total > 0 ? `${gb(pull.completed)} of ${gb(pull.total)} GB (${pct(pull)}%)` : `${pull.step ?? 'starting'}…`}
        You can leave this page; it keeps going.
      </p>
    </div>
  {/if}
{/snippet}

{#snippet resultLine(id: string)}
  {#if result?.provider === id}
    <p class="result" class:bad={!result.ok} role="status">{result.ok ? '✓' : '✗'} {result.text}</p>
  {/if}
{/snippet}

<section class="ai" aria-label="AI">
  <div class="hero" class:hero-ok={!!status?.active} class:hero-none={status && !status.active}>
    <span class="dot" aria-hidden="true"></span>
    <div class="hero-text">
      {#if !status}
        <h2>{loadError ? 'Can’t check your AI' : 'Checking your AI…'}</h2>
        {#if loadError}<p>{loadError}</p>{/if}
      {:else if status.active}
        <h2>{status.active.label} · {status.active.model}</h2>
        <p>
          {#if status.active.chosen}
            This is your AI. Chat, missions, heartbeats and entity analysis all run on it.
          {:else}
            Found on this computer and used automatically, but not tested yet. Press Use on one below to test it and make it yours.
          {/if}
        </p>
      {:else}
        <h2>Connect an AI to get started</h2>
        <p>Pick one below. It takes about a minute, and you can change it any time.</p>
      {/if}
      {#if status?.brokenChoice}
        <p class="warn">
          Your chosen AI ({status.brokenChoice.provider} · {status.brokenChoice.model}) isn’t working right now{status.active ? ', so the app is using the one above until it is back' : ''}.
        </p>
      {/if}
    </div>
    {#if loadError}
      <button type="button" class="btn ghost" onclick={refresh}>Try again</button>
    {:else if status?.active?.chosen}
      <button type="button" class="link" onclick={useAutomatic}>Pick automatically</button>
    {/if}
  </div>

  {#if status && ollama && !consistentInUse}
    <div class="local-cta">
      <div class="opt-text">
        <span class="opt-title">{consistentName} on this computer</span>
        <span class="opt-detail">
          {#if ollama.status === 'not_running'}
            Private and free, and it keeps working offline. It runs in Ollama, so install that first, then come back here.
          {:else if consistentInstalled}
            Downloaded. Press Use to test it and make it your AI.
          {:else}
            Private and free, and it keeps working offline. One download, then it connects itself.
          {/if}
        </span>
      </div>
      <div class="opt-actions">
        {#if ollama.status === 'not_running'}
          <a class="btn primary" href={ollama.helpUrl} target="_blank" rel="noreferrer">Get Ollama</a>
          <button type="button" class="btn ghost" onclick={refresh}>Check again</button>
        {:else if consistentInstalled}
          <button
            type="button"
            class="btn primary"
            disabled={!!busy}
            onclick={() => {
              model[ollama.id] = ollama.models.find((m) => sameModel(m, consistentModel)) ?? consistentModel;
              void connect(ollama);
            }}
          >
            {busy === ollama.id ? 'Testing…' : `Use ${consistentName}`}
          </button>
        {:else}
          <button
            type="button"
            class="btn primary"
            disabled={pull?.status === 'pulling'}
            onclick={() => downloadLocal(consistentModel)}
          >
            {pull?.status === 'pulling' ? 'Downloading…' : downloadLabel}
          </button>
        {/if}
      </div>
      {@render progress()}
      {#if result?.provider === ollama.id}<div class="cta-result">{@render resultLine(ollama.id)}</div>{/if}
    </div>
  {/if}

  {#if status}
    <div class="options">
      {#each byKind('subscription') as p (p.id)}
        {@const inUse = status.active?.provider === p.id}
        <div class="opt" class:opt-use={inUse}>
          <div class="opt-head">
            <div class="opt-text">
              <span class="opt-title">{p.label} {@render chip(p, inUse)}</span>
              <span class="opt-detail">{p.detail}</span>
            </div>
            <div class="opt-actions">
              {#if p.models.length > 1 && p.status !== 'not_installed'}
                <select bind:value={model[p.id]} aria-label="{p.label} model">
                  {#each optionsFor(p) as m (m)}<option value={m}>{m}</option>{/each}
                </select>
              {/if}
              {#if p.status === 'not_installed'}
                <a class="btn" href={p.helpUrl} target="_blank" rel="noreferrer">Get {p.label}</a>
                <button type="button" class="btn ghost" onclick={refresh}>Check again</button>
              {:else if p.status === 'signed_out' && canSignIn(p)}
                <button type="button" class="btn" onclick={() => openSignIn(p)}>Sign in</button>
              {:else}
                <button type="button" class="btn" disabled={!!busy} onclick={() => connect(p)}>
                  {busy === p.id ? 'Testing…' : inUse && status.active?.chosen ? 'Test again' : `Use ${p.label}`}
                </button>
                {#if canSignIn(p)}
                  <button type="button" class="btn ghost" onclick={() => openSignIn(p)}>Sign in again</button>
                {/if}
              {/if}
            </div>
          </div>
          {@render resultLine(p.id)}
          {#if signIn?.id === p.id}
            <div class="signin">
              <p class="opt-detail">
                Your browser opens to sign in. Finish there and come back. This connects automatically.
              </p>
              {#key p.id}
                <Terminal
                  bind:this={term}
                  script={signInScript}
                  rows={12}
                  onready={() => term?.focus()}
                  onurl={(url) => (signInUrl = signInUrl ?? url)}
                  onexit={(code) => void onSignInExit(p, code)}
                />
              {/key}
              <div class="opt-actions">
                {#if signInUrl}
                  <button type="button" class="btn" onclick={() => signInUrl && openSignInUrl(signInUrl).catch(() => {})}>
                    Open sign-in page
                  </button>
                {/if}
                <button type="button" class="btn ghost" onclick={closeSignIn}>{signInDone ? 'Close' : 'Cancel'}</button>
              </div>
            </div>
          {/if}
        </div>
      {/each}

      {#if ollama}
        {@const p = ollama}
        {@const inUse = status.active?.provider === p.id}
        <div class="opt" class:opt-use={inUse}>
          <div class="opt-head">
            <div class="opt-text">
              <span class="opt-title">On this computer · {p.label} {@render chip(p, inUse)}</span>
              <span class="opt-detail">{p.detail}</span>
            </div>
            <div class="opt-actions">
              {#if p.status === 'not_running'}
                <a class="btn" href={p.helpUrl} target="_blank" rel="noreferrer">Get Ollama</a>
                <button type="button" class="btn ghost" onclick={refresh}>Check again</button>
              {:else if p.status === 'no_model'}
                <!-- The download button lives in the Consistent AI card above. -->
              {:else}
                <select bind:value={model[p.id]} aria-label="Local model">
                  {#each optionsFor(p) as m (m)}<option value={m}>{m}</option>{/each}
                </select>
                <button type="button" class="btn" disabled={!!busy} onclick={() => connect(p)}>
                  {busy === p.id ? 'Testing…' : inUse && status.active?.chosen ? 'Test again' : 'Use this'}
                </button>
              {/if}
            </div>
          </div>
          {#if consistentInUse}{@render progress()}{/if}
          {#if consistentInUse}{@render resultLine(p.id)}{/if}
        </div>
      {/if}

      {#if keyProviders.length > 0}
        {@const keyInUse = status.active?.kind === 'api_key'}
        <div class="opt" class:opt-use={keyInUse}>
          <div class="opt-head">
            <div class="opt-text">
              <span class="opt-title">
                API key
                {#if keyInUse}
                  <span class="chip chip-use">In use</span>
                {:else if savedKeys.length > 0}
                  <span class="chip chip-ok">Key saved</span>
                {:else}
                  <span class="chip">No key yet</span>
                {/if}
              </span>
              <span class="opt-detail">
                Anthropic, OpenAI, Google Gemini or DeepSeek. You pay the provider for what you use.
              </span>
            </div>
            <div class="opt-actions">
              <button type="button" class="btn" aria-expanded={keyOpen} onclick={() => (keyOpen = !keyOpen)}>
                {keyOpen ? 'Close' : savedKeys.length > 0 ? 'Choose a key' : 'Add a key'}
              </button>
            </div>
          </div>
          {#if keyOpen}
            <form
              class="key-form"
              onsubmit={(e) => {
                e.preventDefault();
                if (selectedKeyProvider) void connect(selectedKeyProvider, apiKey.trim());
              }}
            >
              <label class="field">
                <span>Provider</span>
                <select bind:value={keyProvider} onchange={() => (result = null)}>
                  {#each keyProviders as p (p.id)}
                    <option value={p.id}>{p.label}{p.ready ? ' · key saved' : ''}</option>
                  {/each}
                </select>
              </label>
              {#if selectedKeyProvider}
                {@const p = selectedKeyProvider}
                <label class="field">
                  <span>API key</span>
                  <input
                    type="password"
                    autocomplete="off"
                    spellcheck="false"
                    placeholder={p.ready ? 'Saved. Paste a new key to replace it' : 'Paste your key'}
                    bind:value={apiKey}
                  />
                </label>
                <label class="field">
                  <span>Model</span>
                  <select bind:value={model[p.id]}>
                    <option value="">Automatic (recommended)</option>
                    {#each optionsFor(p) as m (m)}<option value={m}>{m}</option>{/each}
                  </select>
                </label>
                <div class="opt-actions">
                  <button type="submit" class="btn" disabled={!!busy || (!apiKey.trim() && !p.ready)}>
                    {busy === p.id ? 'Testing…' : p.ready && !apiKey.trim() ? `Use ${p.label}` : 'Test and connect'}
                  </button>
                  <a class="link" href={p.helpUrl} target="_blank" rel="noreferrer">Get a key</a>
                </div>
                <p class="note">
                  The key is tested with one short message, then saved in this computer’s keychain. It only leaves this computer to call {p.label}.
                </p>
                {@render resultLine(p.id)}
              {/if}
            </form>
          {/if}
        </div>
      {/if}
    </div>
  {/if}
</section>

<style>
  .ai {
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 16px;
    border: 1px solid var(--border);
    border-radius: 12px;
    background: var(--bg-secondary);
  }
  .hero {
    display: flex;
    align-items: flex-start;
    gap: 12px;
  }
  .dot {
    flex-shrink: 0;
    width: 10px;
    height: 10px;
    margin-top: 7px;
    border-radius: 50%;
    background: var(--text-muted);
  }
  .hero-ok .dot { background: #7a8163; box-shadow: 0 0 0 4px rgba(122, 129, 99, 0.18); }
  .hero-none .dot { background: #c96a4b; box-shadow: 0 0 0 4px rgba(201, 106, 75, 0.18); }
  .hero-text { flex: 1; min-width: 0; }
  .hero h2 {
    margin: 0;
    font-size: 1rem;
    color: var(--text-primary);
    overflow-wrap: anywhere;
  }
  .hero p {
    margin: 4px 0 0;
    font-size: 0.8125rem;
    line-height: 1.45;
    color: var(--text-muted);
  }
  .hero p.warn { color: #c96a4b; }
  .options {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .opt {
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: 12px;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--bg-card);
  }
  .opt-use {
    border-color: rgba(122, 129, 99, 0.55);
    background: rgba(122, 129, 99, 0.07);
  }
  .opt-head {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 10px;
  }
  .opt-text {
    flex: 1 1 240px;
    display: flex;
    flex-direction: column;
    gap: 3px;
    min-width: 0;
  }
  .opt-title {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
    font-size: 0.875rem;
    font-weight: 700;
    color: var(--text-primary);
  }
  .opt-detail {
    margin: 0;
    font-size: 0.75rem;
    line-height: 1.45;
    color: var(--text-muted);
  }
  .opt-actions {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px;
  }
  .chip {
    padding: 1px 8px;
    border-radius: 999px;
    border: 1px solid var(--border);
    color: var(--text-muted);
    font-size: 0.6875rem;
    font-weight: 700;
    white-space: nowrap;
  }
  .chip-ok { border-color: rgba(122, 129, 99, 0.5); color: #7a8163; }
  .chip-warn { border-color: rgba(201, 138, 75, 0.55); color: #c98a4b; }
  .chip-use { border-color: transparent; background: rgba(122, 129, 99, 0.2); color: #7a8163; }
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
    text-decoration: none;
    cursor: pointer;
  }
  .btn:hover:not(:disabled) { background: rgba(201, 138, 75, 0.24); }
  .btn.ghost {
    border-color: var(--border);
    background: var(--bg-secondary);
    color: var(--text-secondary);
  }
  .btn.primary {
    border-color: #c98a4b;
    background: #c98a4b;
    color: #1a1410;
  }
  .btn.primary:hover:not(:disabled) { background: #d69a5c; }
  .btn:disabled { opacity: 0.6; cursor: default; }
  .local-cta {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 10px;
    padding: 14px;
    border: 1px solid rgba(201, 138, 75, 0.5);
    border-radius: 10px;
    background: rgba(201, 138, 75, 0.08);
  }
  .local-cta .progress,
  .cta-result { flex-basis: 100%; }
  .link {
    align-self: flex-start;
    padding: 0;
    border: 0;
    background: none;
    color: var(--text-secondary);
    font-size: 0.75rem;
    text-decoration: underline;
    cursor: pointer;
  }
  .link:disabled { opacity: 0.6; cursor: default; }
  .opt-actions .link { align-self: center; }
  select,
  input {
    min-height: 34px;
    padding: 0 10px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-secondary);
    color: var(--text-primary);
    font-size: 0.8125rem;
  }
  .key-form {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: 0.75rem;
    font-weight: 700;
    color: var(--text-secondary);
  }
  .result {
    margin: 0;
    font-size: 0.8125rem;
    line-height: 1.45;
    color: #7a8163;
    overflow-wrap: anywhere;
  }
  .result.bad { color: #c96a4b; }
  .note {
    margin: 0;
    font-size: 0.75rem;
    line-height: 1.45;
    color: var(--text-muted);
  }
  .progress { display: flex; flex-direction: column; gap: 6px; }
  .bar {
    height: 6px;
    border-radius: 3px;
    background: var(--border);
    overflow: hidden;
  }
  .bar span {
    display: block;
    height: 100%;
    background: #c98a4b;
    transition: width 0.4s ease;
  }
  .signin {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding-top: 10px;
    border-top: 1px solid var(--border);
  }
</style>
