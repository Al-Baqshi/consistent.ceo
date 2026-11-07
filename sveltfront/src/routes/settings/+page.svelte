<script lang="ts">
  import { onMount } from 'svelte';
  import { API_BASE, api } from '$lib/api';
  import Seo from '$lib/components/Seo.svelte';
  import AccountPanel from '$lib/components/AccountPanel.svelte';
  import PhonePanel from '$lib/components/PhonePanel.svelte';
  import BrowserPanel from '$lib/components/BrowserPanel.svelte';
  import AiPanel from '$lib/components/AiPanel.svelte';
  import ShortcutsPanel from '$lib/components/ShortcutsPanel.svelte';
  import { invoke, isTauri } from '$lib/tauri';
  import Terminal from '$lib/components/Terminal.svelte';
  import {
    ensureProfile,
    saveProfile,
    userProfile,
    workingStyleLabel,
    type WorkingStyle
  } from '$lib/userProfile';

  type SystemHealth = {
    gateway: 'connected' | 'unreachable';
    database: 'connected' | 'unreachable';
    version: string;
  };
  type LocalVault = {
    root: string;
    exists: boolean;
    canonicalDatabase: string;
    mirrorFormat: string;
    workspace?: string;
    chats?: string;
    entities?: string;
    notes?: string;
    environments?: string;
    wallet?: string;
  };

  let notice = $state<string>('');
  let systemHealth = $state<SystemHealth>({ gateway: 'unreachable', database: 'unreachable', version: '' });
  let localVault = $state<LocalVault | null>(null);
  let healthPolling = $state<ReturnType<typeof setInterval> | null>(null);
  let codingAgents = $state<CodingAgentStatus[]>([]);
  // Set in onMount, not inline: the page is server-rendered where the Tauri
  // bridge never exists, so deciding at hydration avoids an SSR mismatch.
  let inDesktopShell = $state(false);
  let agentTestBusy = $state<Record<string, boolean>>({});
  let agentTestResult = $state<Record<string, { ok: boolean; text: string }>>({});

  async function checkSystemHealth() {
    try {
      const res = await fetch(`${API_BASE}/health`);
      if (res.ok) {
        const data = await res.json();
        systemHealth = {
          gateway: 'connected',
          database: data.memory_backend ? 'connected' : 'unreachable',
          version: data.version || '',
        };
      } else {
        systemHealth = { gateway: 'unreachable', database: 'unreachable', version: '' };
      }
    } catch {
      systemHealth = { gateway: 'unreachable', database: 'unreachable', version: '' };
    }
  }


  interface CodingAgentStatus {
    kind: string;
    installed: boolean;
    path?: string;
    version?: string;
    credentialsLikely: boolean;
    detail?: string;
  }

  async function loadCodingAgents() {
    codingAgents = await api<CodingAgentStatus[]>('/api/integrations/coding-agents').catch(() => []);
  }


  /**
   * Runs the CLI for real (one turn, two-token answer). This is the only check
   * that separates "installed" from "signed in", so it costs a fraction of a
   * cent — hence a button rather than part of the status load.
   */
  async function testCodingAgent(kind: string) {
    agentTestBusy[kind] = true;
    agentTestResult[kind] = undefined as unknown as { ok: boolean; text: string };
    try {
      const res = await api<{ ok: boolean; output?: string; detail?: string }>(
        `/api/integrations/coding-agents/${kind}/test`,
        { method: 'POST' }
      );
      agentTestResult[kind] = {
        ok: res.ok,
        text: res.ok ? (res.output || 'Ready.') : (res.detail || 'Check failed.')
      };
    } catch (err) {
      agentTestResult[kind] = { ok: false, text: extractProbeError(err) ?? 'Check failed.' };
    } finally {
      agentTestBusy[kind] = false;
      loadCodingAgents();
    }
  }

  async function load() {
    localVault = await api<LocalVault>('/api/local-vault').catch(() => null as LocalVault | null);
  }

  function extractProbeError(raw: unknown): string | null {
    const msg = raw instanceof Error ? raw.message : String(raw ?? '');
    if (!msg) return null;
    try {
      const parsed = JSON.parse(msg);
      if (parsed && typeof parsed === 'object' && typeof parsed.error === 'string') {
        return parsed.error;
      }
    } catch {
      /* not JSON — fall through */
    }
    return msg;
  }

  // ----- Working style -----
  //
  // Whatever first run inferred is only a starting point; this is where it stays
  // reversible. Changing it reshapes the workspace immediately — the sidebar
  // reads the same store.
  const WORKING_STYLE_OPTIONS: { value: WorkingStyle; title: string; sub: string }[] = [
    {
      value: 'holistic',
      title: 'Holistic',
      sub: 'Many things moving at once. The full workspace: every surface, multiple entities.'
    },
    {
      value: 'linear',
      title: 'Focused',
      sub: 'One thing until it is done. Chat, Today, Tasks and Missions only.'
    },
    {
      value: 'unset',
      title: 'Not set',
      sub: 'No preference recorded. Behaves like Holistic — nothing is hidden.'
    }
  ];
  let workingStyleBusy = $state(false);

  async function setWorkingStyle(style: WorkingStyle) {
    if (workingStyleBusy || $userProfile?.workingStyle === style) return;
    workingStyleBusy = true;
    try {
      // Picking a style deliberately clears the "show everything" override, so
      // choosing Focused actually focuses rather than silently doing nothing.
      const saved = await saveProfile({ workingStyle: style, showAllNav: false });
      notice = saved
        ? `Working style set to ${workingStyleLabel(style)}.`
        : 'Could not save your working style. Check the gateway and try again.';
    } finally {
      workingStyleBusy = false;
    }
  }

  async function setShowAllNav(showAll: boolean) {
    if (workingStyleBusy) return;
    workingStyleBusy = true;
    try {
      const saved = await saveProfile({ showAllNav: showAll });
      if (!saved) notice = 'Could not save that. Check the gateway and try again.';
    } finally {
      workingStyleBusy = false;
    }
  }

  // Advanced: Terminal — a bare interactive shell, desktop only. Nothing
  // spawns until the operator opens the section: the component autostarts on
  // mount and kills its own session on destroy, so closing just unmounts it.
  let shellOpen = $state(false);
  // The shell ended on its own (`exit`) — Restart becomes Start again.
  let shellExited = $state(false);
  let shellTerm = $state.raw<Terminal | null>(null);
  // True while a session is being killed on purpose (restart, close): the
  // exit that follows is ours, not the operator leaving the shell.
  let shellSwitching = false;

  function shellAgentLabel(agent: CodingAgentStatus): string {
    if (!agent.installed) return 'not installed';
    return agent.credentialsLikely ? 'ready' : 'may need sign-in';
  }

  function openShell(): void {
    shellExited = false;
    shellOpen = true;
  }

  /** Restart, or start again after the operator typed `exit`. */
  async function restartShell(): Promise<void> {
    shellSwitching = true;
    try {
      await shellTerm?.restart();
    } finally {
      shellSwitching = false;
    }
    shellExited = !shellTerm?.isRunning();
    shellTerm?.focus();
  }

  async function closeShell(): Promise<void> {
    shellSwitching = true;
    try {
      await shellTerm?.kill();
    } catch (_) {
      // kill is idempotent on the backend; nothing left to stop
    } finally {
      shellSwitching = false;
    }
    shellOpen = false;
    shellExited = false;
  }

  function onShellExit(): void {
    if (shellSwitching) return;
    shellExited = true;
  }

  // Start at login (desktop app only; `desktop/src-tauri/src/autostart.rs`).
  let startAtLogin = $state<boolean | null>(null);

  async function loadStartAtLogin() {
    try {
      startAtLogin = await invoke<boolean>('autostart_status');
    } catch {
      startAtLogin = null;
    }
  }

  async function toggleStartAtLogin(enabled: boolean) {
    try {
      startAtLogin = await invoke<boolean>('autostart_set', { enabled });
    } catch (e) {
      notice = e instanceof Error ? e.message : 'Could not change start-at-login.';
      await loadStartAtLogin();
    }
  }

  onMount(() => {
    inDesktopShell = isTauri();
    if (inDesktopShell) void loadStartAtLogin();
    void ensureProfile();
    load();
    loadCodingAgents();
    checkSystemHealth();
    healthPolling = setInterval(checkSystemHealth, 30_000);
    return () => { if (healthPolling) clearInterval(healthPolling); };
  });
</script>

<Seo title="AI settings" noindex />

<div class="ai-settings">
  <header class="page-header">
    <div class="header-text">
      <h1 class="page-title">AI settings</h1>
      <p class="page-sub">Choose what Consistent CEO thinks with, connect your phone, and tune how you work.</p>
    </div>
  </header>

  {#if notice}
    <p class="notice" role="status">{notice}</p>
  {/if}

  <AiPanel />
  <BrowserPanel />
  <AccountPanel />
  <PhonePanel />

  <section class="models-section" aria-label="How you work">
    <h2 class="section-title">How you work</h2>
    <p class="section-hint">
      This shapes the workspace itself, not just a label. Holistic keeps every surface open for
      several entities at once; Focused strips the sidebar back to Chat, Today, Tasks and Missions
      so one thing can actually get finished. Change it whenever you like.
    </p>
    <div class="style-options">
      {#each WORKING_STYLE_OPTIONS as option (option.value)}
        <button
          type="button"
          class="style-option"
          class:selected={($userProfile?.workingStyle ?? 'unset') === option.value}
          disabled={workingStyleBusy}
          aria-pressed={($userProfile?.workingStyle ?? 'unset') === option.value}
          onclick={() => setWorkingStyle(option.value)}
        >
          <span class="style-option-title">{option.title}</span>
          <span class="style-option-sub">{option.sub}</span>
        </button>
      {/each}
    </div>
    {#if $userProfile?.workingStyle === 'linear'}
      <label class="style-toggle">
        <input
          type="checkbox"
          checked={$userProfile?.showAllNav ?? false}
          disabled={workingStyleBusy}
          onchange={(e) => setShowAllNav(e.currentTarget.checked)}
        />
        <span>Show everything in the sidebar anyway</span>
      </label>
    {/if}
  </section>

  {#if inDesktopShell}
    <ShortcutsPanel />
  {/if}

  <details class="advanced">
    <summary>
      <span class="adv-title">Advanced</span>
      <span class="adv-sub">Status, coding agents, the local vault{inDesktopShell ? ' and a terminal' : ''}</span>
    </summary>
    <div class="adv-body">
  <section class="system-status" aria-label="System status">
    <h2 class="section-title">System status</h2>
    <div class="status-row-group">
      <div class="system-status-row">
        <span class="status-dot" class:dot-green={systemHealth.gateway === 'connected'} class:dot-red={systemHealth.gateway !== 'connected'}></span>
        <span class="system-status-label">Gateway</span>
        <span class="system-status-value">
          {#if systemHealth.gateway === 'connected'}
            Running{systemHealth.version ? ` (v${systemHealth.version})` : ''}
          {:else}
            Not connected
          {/if}
        </span>
      </div>
      <div class="system-status-row">
        <span class="status-dot" class:dot-green={systemHealth.database === 'connected'} class:dot-red={systemHealth.database !== 'connected'}></span>
        <span class="system-status-label">Database</span>
        <span class="system-status-value">
          {systemHealth.database === 'connected' ? 'Connected (SurrealDB)' : 'Not connected'}
        </span>
      </div>
      {#if inDesktopShell && startAtLogin !== null}
        <label class="system-status-row start-at-login">
          <input
            type="checkbox"
            checked={startAtLogin}
            onchange={(e) => toggleStartAtLogin((e.currentTarget as HTMLInputElement).checked)}
          />
          <span class="system-status-label">Start with this computer</span>
          <span class="system-status-value">
            Runs in the menu bar so missions, reminders and your phone keep working. Closing the window keeps it running; Quit stops it.
          </span>
        </label>
      {/if}
      {#if systemHealth.gateway !== 'connected'}
        <div class="status-actions">
          <button
            type="button"
            class="start-gateway-btn"
            onclick={() => {
              notice = inDesktopShell
                ? 'Quit Consistent CEO from its menu-bar icon and open it again; the app starts the gateway itself.'
                : 'Run `consistent-world start` in a terminal on this computer, or open the Consistent CEO app.';
            }}
          >
            How to start it
          </button>
          <button type="button" class="retry-btn" onclick={checkSystemHealth}>
            Retry connection
          </button>
        </div>
      {/if}
    </div>
  </section>
  <section class="coding-agents" aria-label="Coding agents">
    <h2 class="section-title">Coding agents on this machine</h2>
    <p class="section-sub">
      Work you hand to Consistent CEO can run through a coding-agent CLI installed here — no MCP server, no extra keys.
      Install one and sign into it, and it is picked up automatically.
    </p>
    <div class="agent-list">
      {#each codingAgents as agent (agent.kind)}
        <div class="agent-row">
          <span
            class="status-dot"
            class:dot-green={agent.installed && agent.credentialsLikely}
            class:dot-amber={agent.installed && !agent.credentialsLikely}
            class:dot-red={!agent.installed}
          ></span>
          <div class="agent-text">
            <span class="agent-name">{agent.kind}</span>
            <span class="agent-detail">
              {#if !agent.installed}
                Not installed
              {:else if agent.version}
                {agent.version}{agent.credentialsLikely ? '' : ' · may need sign-in'}
              {:else}
                Installed{agent.credentialsLikely ? '' : ' · may need sign-in'}
              {/if}
            </span>
            {#if agentTestResult[agent.kind]}
              <span class="agent-result" class:agent-result-bad={!agentTestResult[agent.kind].ok}>
                {agentTestResult[agent.kind].ok ? '✓ ' : '✗ '}{agentTestResult[agent.kind].text}
              </span>
            {:else if agent.detail && !agent.installed}
              <span class="agent-result">{agent.detail}</span>
            {/if}
          </div>
          <button
            type="button"
            class="retry-btn"
            disabled={!agent.installed || agentTestBusy[agent.kind]}
            onclick={() => testCodingAgent(agent.kind)}
          >
            {agentTestBusy[agent.kind] ? 'Testing…' : 'Test'}
          </button>
        </div>
      {:else}
        <p class="agent-detail">Checking…</p>
      {/each}
    </div>
  </section>
  <section class="status-card" aria-label="Local vault">
    <div class="status-grid">
      <div class="status-item status-wide">
        <span class="status-label">Local vault</span>
        <span class="status-value connection">{localVault?.root ?? 'Not resolved yet'}</span>
      </div>
      <div class="status-item">
        <span class="status-label">Canonical database</span>
        <span class="status-value truncate">{localVault?.canonicalDatabase ?? 'SurrealDB'}</span>
      </div>
      <div class="status-item">
        <span class="status-label">File mirror</span>
        <span class="status-value truncate">{localVault?.mirrorFormat ?? 'Markdown'}</span>
      </div>
      <div class="status-item status-wide">
        <span class="status-label">What saves there</span>
        <span class="status-value connection">
          Chat transcripts, saved notes, tasks, reminders, and entity folders are mirrored locally. SurrealDB remains the source of truth.
        </span>
      </div>
      <div class="status-item status-wide">
        <span class="status-label">Chats</span>
        <span class="status-value connection">{localVault?.chats ?? 'Not resolved yet'}</span>
      </div>
      <div class="status-item status-wide">
        <span class="status-label">Notes</span>
        <span class="status-value connection">{localVault?.notes ?? 'Not resolved yet'}</span>
      </div>
      <div class="status-item status-wide">
        <span class="status-label">Environments</span>
        <span class="status-value connection">{localVault?.environments ?? 'Not resolved yet'}</span>
      </div>
      <div class="status-item status-wide">
        <span class="status-label">Wallet</span>
        <span class="status-value connection">{localVault?.wallet ?? 'Not resolved yet'}</span>
      </div>
    </div>
  </section>
  {#if inDesktopShell}
    <section class="shell-section" aria-label="Terminal">
      <div class="shell-head">
        <div class="shell-head-text">
          <h2 class="section-title">Terminal</h2>
          <p class="section-hint">
            A shell on this computer, inside the app — for checking on <code>claude</code>,
            <code>codex</code>, <code>ollama</code>, or the gateway.
          </p>
        </div>
        {#if !shellOpen}
          <button type="button" class="setup-action" onclick={openShell}>Open terminal</button>
        {/if}
      </div>
      {#if shellOpen}
        <div class="shell-status" aria-label="Status at a glance">
          <span class="shell-pill">
            <span
              class="status-dot"
              class:dot-green={systemHealth.gateway === 'connected'}
              class:dot-red={systemHealth.gateway !== 'connected'}
            ></span>
            Gateway · {systemHealth.gateway === 'connected' ? 'connected' : 'unreachable'}
          </span>
          {#each codingAgents as agent (agent.kind)}
            <span class="shell-pill">
              <span
                class="status-dot"
                class:dot-green={agent.installed && agent.credentialsLikely}
                class:dot-amber={agent.installed && !agent.credentialsLikely}
                class:dot-red={!agent.installed}
              ></span>
              {agent.kind} · {shellAgentLabel(agent)}
            </span>
          {/each}
        </div>
        <p class="shell-trust">Commands run as you, with this app's permissions.</p>
        <Terminal
          rows={22}
          bind:this={shellTerm}
          onready={() => shellTerm?.focus()}
          onexit={onShellExit}
        />
        <div class="shell-actions">
          {#if shellExited}
            <button type="button" class="setup-action" onclick={() => restartShell()}>Start again</button>
          {:else}
            <button type="button" class="setup-test" onclick={() => restartShell()}>Restart</button>
          {/if}
          <button type="button" class="setup-test" onclick={() => closeShell()}>Close</button>
        </div>
      {/if}
    </section>
  {/if}
    </div>
  </details>






</div>

<style>
  .ai-settings {
    padding: 16px 20px;
    display: flex;
    flex-direction: column;
    gap: 16px;
    max-width: 720px;
    margin: 0 auto;
  }
  .page-header {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  @media (min-width: 640px) {
    .page-header {
      flex-direction: row;
      align-items: flex-start;
      justify-content: space-between;
    }
  }
  .page-title {
    margin: 0;
    font-size: 1.125rem;
    font-weight: 700;
    color: var(--text-primary);
  }
  .page-sub {
    margin: 4px 0 0;
    font-size: 0.8125rem;
    color: var(--text-muted);
    line-height: 1.4;
  }
  .system-status {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .setup-action:disabled,
  .setup-test:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .setup-action {
    flex: 1;
    min-height: 36px;
    border: 1px solid rgba(201, 138, 75, 0.42);
    border-radius: 8px;
    background: rgba(201, 138, 75, 0.14);
    color: var(--text-primary);
    font-size: 0.75rem;
    font-weight: 700;
    cursor: pointer;
  }
  .setup-action:hover:not(:disabled) { background: rgba(201, 138, 75, 0.24); }
  /* The lane already answering chat is a state, not an action — showing it as a
     button invited clicks that could not do anything. */
  .setup-test {
    flex-shrink: 0;
    min-height: 36px;
    padding: 0 12px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-secondary);
    color: var(--text-secondary);
    font-size: 0.75rem;
    font-weight: 700;
    cursor: pointer;
  }
  .setup-test:hover:not(:disabled) { color: var(--text-primary); }
  /* Sign in takes the Test shape but its own row: three actions fighting over
     one narrow grid cell left the primary button a few pixels wide. */
  .status-row-group {
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 12px 16px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .system-status-row {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .status-dot {
    width: 10px;
    height: 10px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .dot-green {
    background: #7a8163;
    box-shadow: 0 0 6px rgba(122, 129, 99, 0.5);
  }
  .dot-red {
    background: #c96a4b;
    box-shadow: 0 0 6px rgba(201, 106, 75, 0.5);
  }
  /* Installed but sign-in unconfirmed — usable-but-check-me, between the two. */
  .dot-amber {
    background: #c9a84b;
    box-shadow: 0 0 6px rgba(201, 168, 75, 0.5);
  }
  .coding-agents {
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 16px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .section-sub {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--text-secondary);
  }
  .agent-list {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .agent-row {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .agent-text {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
    flex: 1;
  }
  .agent-name {
    font-size: 0.875rem;
    font-weight: 600;
    color: var(--text-primary);
  }
  .agent-detail {
    font-size: 0.8125rem;
    color: var(--text-secondary);
  }
  .agent-result {
    font-size: 0.8125rem;
    color: var(--text-secondary);
    overflow-wrap: anywhere;
  }
  .agent-result-bad {
    color: #c96a4b;
  }
  .system-status-label {
    font-size: 0.875rem;
    font-weight: 600;
    color: var(--text-primary);
    min-width: 80px;
  }
  .system-status-value {
    font-size: 0.8125rem;
    color: var(--text-secondary);
  }
  .status-actions {
    display: flex;
    gap: 8px;
    padding-top: 4px;
  }
  .start-gateway-btn {
    min-height: 38px;
    padding: 0 14px;
    border-radius: 8px;
    font-size: 0.8125rem;
    font-weight: 700;
    border: 1px solid rgba(201, 138, 75, 0.46);
    background: linear-gradient(180deg, rgba(201, 138, 75, 0.34), rgba(138, 90, 46, 0.6));
    color: #ffffff;
  }
  .retry-btn {
    min-height: 38px;
    padding: 0 14px;
    border-radius: 8px;
    font-size: 0.8125rem;
    font-weight: 600;
    border: 1px solid var(--border);
    background: var(--bg-card);
    color: var(--text-secondary);
  }
  .advanced {
    border: 1px solid var(--border);
    border-radius: 12px;
    background: var(--bg-secondary);
  }
  .advanced > summary {
    position: relative;
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 14px 40px 14px 16px;
    cursor: pointer;
    list-style: none;
  }
  .advanced > summary::-webkit-details-marker { display: none; }
  .advanced > summary::after {
    content: '▸';
    position: absolute;
    right: 16px;
    top: 50%;
    transform: translateY(-50%);
    color: var(--text-muted);
    font-size: 1rem;
  }
  .advanced[open] > summary::after { content: '▾'; }
  .adv-title { font-size: 0.9375rem; font-weight: 700; color: var(--text-primary); }
  .adv-sub { font-size: 0.75rem; color: var(--text-muted); }
  .adv-body {
    display: flex;
    flex-direction: column;
    gap: 16px;
    padding: 0 16px 16px;
  }
  .notice {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--accent-cyan);
  }
  .status-card {
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 14px 16px;
  }
  .status-grid {
    display: grid;
    gap: 12px;
  }
  @media (min-width: 480px) {
    .status-grid {
      grid-template-columns: 1fr 1fr;
    }
    .status-wide {
      grid-column: 1 / -1;
    }
  }
  .status-item {
    display: flex;
    flex-direction: column;
    gap: 4px;
    min-width: 0;
  }
  .status-label {
    font-size: 0.6875rem;
    font-weight: 700;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--text-secondary);
  }
  .status-value {
    font-size: 0.9375rem;
    font-weight: 600;
    color: var(--text-primary);
  }
  .status-value.connection {
    font-weight: 500;
    font-size: 0.875rem;
    color: var(--text-secondary);
    line-height: 1.35;
  }
  .truncate {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .models-section {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .section-title {
    margin: 0;
    font-size: 1rem;
    font-weight: 700;
    color: var(--text-primary);
  }
  .section-hint {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--text-muted);
    line-height: 1.4;
  }

  .style-options {
    display: grid;
    gap: 8px;
    grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
  }
  .style-option {
    display: flex;
    flex-direction: column;
    gap: 4px;
    text-align: left;
    padding: 12px;
    border-radius: 12px;
    border: 1px solid var(--border);
    background: var(--bg-secondary);
    color: var(--text-primary);
    font: inherit;
    cursor: pointer;
    transition: border-color var(--transition-base), background var(--transition-base);
  }
  .style-option:hover:not(:disabled) {
    border-color: rgba(201, 138, 75, 0.55);
  }
  .style-option.selected {
    border-color: rgba(201, 138, 75, 0.7);
    background: var(--surface-accent-soft);
  }
  .style-option:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .style-option-title {
    font-size: 0.875rem;
    font-weight: 700;
  }
  .style-option-sub {
    font-size: 0.75rem;
    line-height: 1.45;
    color: var(--text-muted);
  }
  .style-toggle {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 0.8125rem;
    color: var(--text-muted);
    cursor: pointer;
  }
  /* Full width on every breakpoint — it is one field, not a row of three. */
  /* Deliberately not a flex container. As one, the inline <code> and each run
     of text around it became separate flex items laid out in a row, which is
     what tore this sentence into columns. */
  .section-hint code {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 0.95em;
    padding: 1px 5px;
    border-radius: 5px;
    border: 1px solid var(--border);
    background: var(--bg-secondary);
    color: var(--text-secondary);
    white-space: nowrap;
  }
  /* The brain switcher reuses this box for <select>. WebKit renders a native
     select at its own height and ignores the border, so it has to be reset and
     given a chevron of its own. */
  /* Advanced: Terminal — desktop only, collapsed until opened. */
  .shell-section {
    display: flex;
    flex-direction: column;
    gap: 10px;
    min-width: 0;
    padding: 16px;
    border: 1px solid var(--border);
    border-radius: 12px;
    background: var(--bg-secondary);
  }
  .shell-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
  }
  .shell-head-text {
    min-width: 0;
  }
  .shell-head .setup-action {
    flex: 0 0 auto;
    padding: 0 14px;
  }
  .shell-status {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    min-width: 0;
  }
  .shell-pill {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    min-height: 24px;
    padding: 0 9px;
    border: 1px solid var(--border);
    border-radius: 999px;
    background: var(--bg-card);
    font-size: 0.6875rem;
    font-weight: 600;
    color: var(--text-secondary);
    white-space: nowrap;
  }
  .shell-pill .status-dot {
    width: 8px;
    height: 8px;
  }
  .shell-trust {
    margin: 0;
    font-size: 0.75rem;
    line-height: 1.45;
    color: var(--text-muted);
  }
  .shell-actions {
    display: flex;
    flex-wrap: wrap;
    align-items: stretch;
    gap: 6px;
  }
  .shell-actions .setup-action {
    flex: 0 1 auto;
    padding: 0 14px;
  }




  .start-at-login {
    cursor: pointer;
    align-items: flex-start;
  }

  .start-at-login input {
    margin: 0.2rem 0.1rem 0 0;
    accent-color: var(--color-orange);
  }
</style>
