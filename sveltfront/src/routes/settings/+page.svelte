<script lang="ts">
  import { onMount } from 'svelte';
  import { API_BASE, api } from '$lib/api';
  import Seo from '$lib/components/Seo.svelte';
  import AccountPanel from '$lib/components/AccountPanel.svelte';
  import PhonePanel from '$lib/components/PhonePanel.svelte';
  import ShortcutsPanel from '$lib/components/ShortcutsPanel.svelte';
  import { invoke, isTauri } from '$lib/tauri';
  import Terminal from '$lib/components/Terminal.svelte';
  import { openSignInUrl } from '$lib/pty';
  import {
    ensureProfile,
    saveProfile,
    userProfile,
    workingStyleLabel,
    type WorkingStyle
  } from '$lib/userProfile';

  type LlmProvider = { id: string; name: string; provider_type: string; base_url?: string; api_key_env?: string };
  type LlmModel = { id: string; provider_id: string; name: string; is_default: boolean };
  type ProviderModelsResponse = { models: { id: string }[] };
  type LlmProviderStatus = {
    name: string;
    baseUrl: string;
    apiKeyEnv: string;
    keyless: boolean;
    secretConfigured: boolean;
    secretSource: string;
  };
  type LlmProfile = {
    mode: string;
    current_model: string | null;
    provider?: string | null;
    model?: string | null;
    base_url?: string | null;
    api_key_env?: string | null;
    preferred_provider?: string | null;
    preferred_model?: string | null;
    secret_configured?: boolean;
    secret_source?: string | null;
  };
  type ProviderTemplate = {
    key: string;
    label: string;
    name: string;
    providerType: string;
    baseUrl: string;
    apiKeyEnv: string;
  };
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

  const operationsAgentId = 'ops-coordinator';

  let providers = $state<LlmProvider[]>([]);
  let models = $state<LlmModel[]>([]);
  let providerStatuses = $state<LlmProviderStatus[]>([]);
  let activeProfile = $state<LlmProfile | null>(null);
  let showConnectProvider = $state(false);
  let notice = $state<string>('');
  let providerApiKeys = $state<Record<string, string>>({});
  let providerBusy = $state<Record<string, boolean>>({});
  let manualProvider = $state('ollama');
  let manualModel = $state('gemma4:latest');
  // Model ids each provider can actually serve right now, keyed by provider
  // name. Asked for on load rather than only when a provider is saved — the
  // switcher should offer what is really there, not just whatever someone
  // remembered to press "import" on.
  let discoveredModels = $state<Record<string, string[]>>({});
  let discovering = $state(false);
  let importingAll = $state(false);
  // Set when the operator picks "Other…" and types a model id by hand.
  let customModel = $state('');
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

  const providerTemplates: ProviderTemplate[] = [
    // Keyless: rides the operator's Claude subscription through the installed
    // `claude` CLI, so it is listed first — it is the one that needs nothing.
    { key: 'claude-code', label: 'Claude Code (subscription)', name: 'claude-code', providerType: 'claude_code', baseUrl: 'claude-code://', apiKeyEnv: 'CLAUDE_CODE_CLI' },
    { key: 'codex', label: 'Codex (ChatGPT subscription)', name: 'codex', providerType: 'codex', baseUrl: 'codex://', apiKeyEnv: 'CODEX_CLI' },
    { key: 'ollama', label: 'Ollama (local)', name: 'ollama', providerType: 'ollama', baseUrl: 'http://127.0.0.1:11434', apiKeyEnv: 'OLLAMA_API_KEY' },
    { key: 'anthropic', label: 'Anthropic', name: 'anthropic', providerType: 'anthropic', baseUrl: 'https://api.anthropic.com/v1', apiKeyEnv: 'ANTHROPIC_API_KEY' },
    { key: 'openai', label: 'OpenAI', name: 'openai', providerType: 'openai', baseUrl: 'https://api.openai.com/v1', apiKeyEnv: 'OPENAI_API_KEY' },
    { key: 'google-ai-studio', label: 'Google AI Studio', name: 'google-ai-studio', providerType: 'google_ai_studio', baseUrl: 'https://generativelanguage.googleapis.com/v1beta/openai', apiKeyEnv: 'GOOGLE_API_KEY' },
    { key: 'deepseek', label: 'DeepSeek', name: 'deepseek', providerType: 'deepseek', baseUrl: 'https://api.deepseek.com/v1', apiKeyEnv: 'DEEPSEEK_API_KEY' }
  ];

  function dedupeModelsByName(input: LlmModel[]): LlmModel[] {
    const byName = new Map<string, LlmModel>();
    for (const model of input) {
      const key = model.name.trim().toLowerCase();
      const existing = byName.get(key);
      if (!existing) {
        byName.set(key, model);
        continue;
      }
      if (model.is_default && !existing.is_default) byName.set(key, model);
    }
    return Array.from(byName.values()).sort((a, b) => a.name.localeCompare(b.name));
  }

  const importedModels = $derived(dedupeModelsByName(models));

  /** Providers to offer in the switcher: the ones registered in the database,
   *  plus any the gateway knows from config. Without the second half a provider
   *  that is configured but never "connected" is invisible, which is how this
   *  page ends up looking like Ollama is the only choice there has ever been. */
  const providerChoices = $derived.by(() => {
    const names = new Map<string, string>();
    for (const p of providers) names.set(p.name.toLowerCase(), p.name);
    for (const st of providerStatuses) {
      if (!names.has(st.name.toLowerCase())) names.set(st.name.toLowerCase(), st.name);
    }
    return [...names.values()].sort((a, b) => a.localeCompare(b));
  });

  /** Every model the selected provider can serve: what it reports live, plus
   *  anything already registered against it — config aliases like
   *  `operator-default` are real targets but discovery never returns them. */
  const modelChoices = $derived.by(() => {
    const wanted = manualProvider.trim().toLowerCase();
    const provider = providers.find((p) => p.name.toLowerCase() === wanted);
    const live = discoveredModels[provider?.name ?? manualProvider.trim()] ?? [];
    const registered = provider
      ? models.filter((m) => m.provider_id === provider.id).map((m) => m.name)
      : [];
    return [...new Set([...live, ...registered])].sort((a, b) => a.localeCompare(b));
  });

  /** Whether the chosen provider can actually be reached — a model picked from
   *  a provider with no key will resolve to nothing at the next turn. */
  const selectedProviderReady = $derived.by(() => {
    const st = providerStatuses.find(
      (s) => s.name.toLowerCase() === manualProvider.trim().toLowerCase()
    );
    if (!st) return null;
    return st.keyless || st.secretConfigured;
  });

  const activeSourceLabel = $derived(
    activeProfile?.current_model ??
    (activeProfile?.provider && activeProfile?.model ? `${activeProfile.provider} · ${activeProfile.model}` : 'No active source resolved')
  );

  const activeConnectionLabel = $derived.by(() => {
    const name = activeProfile?.provider ?? activeProfile?.preferred_provider;
    if (!name) return 'No provider resolved.';
    const st = providerStatuses.find((s) => s.name.toLowerCase() === name.toLowerCase());
    if (!st) return activeProfile?.secret_configured ? 'Connected' : 'Status unavailable';
    if (st.keyless || st.secretConfigured) return 'Connected';
    return 'Not connected — add an API key or set the env var for this provider.';
  });

  function codingAgentReady(kind: string): boolean {
    const agent = codingAgents.find((item) => item.kind.toLowerCase() === kind.toLowerCase());
    return !!agent?.installed && !!agent.credentialsLikely;
  }

  /** Discovered model ids for a provider, matched case-insensitively. */
  function discoveredFor(name: string): string[] {
    const key = Object.keys(discoveredModels).find((k) => k.toLowerCase() === name.toLowerCase());
    return key ? (discoveredModels[key] ?? []) : [];
  }

  function templateConnected(template: ProviderTemplate): boolean {
    // CLI subscriptions are authenticated outside this page, and Ollama is a
    // local server that is either answering or not. For all three the live
    // probe is the source of truth — a registered-but-dead lane is not "Ready".
    if (template.key === 'codex') return codingAgentReady('codex');
    if (template.key === 'claude-code') return codingAgentReady('claude');
    if (template.key === 'ollama') return discoveredFor('ollama').length > 0;
    const status = providerStatuses.find((item) => item.name.toLowerCase() === template.name.toLowerCase());
    return !!(status?.keyless || status?.secretConfigured);
  }

  function isActiveTemplate(t: ProviderTemplate): boolean {
    return (activeProfile?.provider ?? '').toLowerCase() === t.name.toLowerCase();
  }

  function openProvider(template: ProviderTemplate) {
    manualProvider = template.name;
    showConnectProvider = true;
  }

  function isKeylessTemplate(t: ProviderTemplate): boolean {
    return t.apiKeyEnv === 'OLLAMA_API_KEY' || t.apiKeyEnv === 'CLAUDE_CODE_CLI' || t.apiKeyEnv === 'CODEX_CLI';
  }

  // ----- One-click connect for the keyless lanes -----
  //
  // "Connect" on a setup card must mean exactly one thing: chat answers
  // through this brain when the click finishes. No modal, no separate
  // save/import/activate steps.
  let cardBusy = $state<Record<string, boolean>>({});
  let cardResult = $state<Record<string, { ok: boolean; text: string }>>({});

  // ── First run with Ollama installed but empty ─────────────────────────────
  // The gateway downloads a model on request (`/api/ollama/pull`) and reports
  // progress; when it lands, the card connects it like any other click.
  type OllamaPull = {
    model: string | null;
    status: 'idle' | 'pulling' | 'done' | 'failed';
    step: string | null;
    completed: number;
    total: number;
    error: string | null;
  };
  let ollamaNeedsModel = $state(false);
  let ollamaPull = $state<OllamaPull | null>(null);

  async function downloadLocalModel(t: ProviderTemplate) {
    try {
      ollamaPull = await api<OllamaPull>('/api/ollama/pull', { method: 'POST', body: '{}' });
    } catch (e) {
      cardResult[t.key] = { ok: false, text: e instanceof Error ? e.message : 'Could not start the download.' };
      return;
    }
    while (ollamaPull && ollamaPull.status === 'pulling') {
      await new Promise((r) => setTimeout(r, 1000));
      try {
        ollamaPull = await api<OllamaPull>('/api/ollama/pull', { silent: true });
      } catch {
        /* the gateway is busy; keep polling */
      }
    }
    if (ollamaPull?.status === 'done') {
      ollamaNeedsModel = false;
      ollamaPull = null;
      await quickConnect(t);
    } else if (ollamaPull?.status === 'failed') {
      cardResult[t.key] = { ok: false, text: ollamaPull.error ?? 'The download failed.' };
    }
  }

  function pullPercent(p: OllamaPull): number {
    return p.total > 0 ? Math.min(100, Math.round((p.completed / p.total) * 100)) : 0;
  }

  function gb(bytes: number): string {
    return (bytes / 1e9).toFixed(1);
  }

  function templateAgentKind(t: ProviderTemplate): 'claude' | 'codex' | null {
    if (t.key === 'claude-code') return 'claude';
    if (t.key === 'codex') return 'codex';
    return null;
  }

  const CLI_INSTALL_HINTS: Record<string, string> = {
    claude:
      'Install it with `npm install -g @anthropic-ai/claude-code`, run `claude` once to sign in, then press Connect again.',
    codex:
      'Install it with `npm install -g @openai/codex`, run `codex login`, then press Connect again.'
  };

  async function ensureProviderRegistered(t: ProviderTemplate): Promise<void> {
    const existing = providers.find((p) => p.name.toLowerCase() === t.name.toLowerCase());
    if (existing) {
      await api(`/api/llm/providers/${encodeURIComponent(existing.id)}`, {
        method: 'PATCH',
        body: JSON.stringify({ provider_type: t.providerType, base_url: t.baseUrl, api_key_env: t.apiKeyEnv })
      });
    } else {
      await api('/api/llm/providers', {
        method: 'POST',
        body: JSON.stringify({ name: t.name, provider_type: t.providerType, base_url: t.baseUrl, api_key_env: t.apiKeyEnv })
      });
    }
  }

  /** Register the lane, import what it serves, and make it the brain that
   *  answers chat — one click, or one clear reason why not. */
  async function quickConnect(t: ProviderTemplate): Promise<boolean> {
    cardBusy[t.key] = true;
    cardResult[t.key] = undefined as unknown as { ok: boolean; text: string };
    try {
      const kind = templateAgentKind(t);
      let cliSignInUnconfirmed = false;
      if (kind) {
        if (codingAgents.length === 0) await loadCodingAgents();
        const agent = codingAgents.find((a) => a.kind.toLowerCase() === kind);
        if (agent && !agent.installed) {
          cardResult[t.key] = {
            ok: false,
            text: `The ${kind} CLI isn’t installed on this computer. ${CLI_INSTALL_HINTS[kind]}`
          };
          return false;
        }
        cliSignInUnconfirmed = !!agent && !agent.credentialsLikely;
      }

      await ensureProviderRegistered(t);
      await load();

      let model = 'default';
      if (t.key === 'ollama') {
        const res = await api<ProviderModelsResponse>(
          `/api/llm/providers/${encodeURIComponent(t.name)}/models`,
          { silent: true }
        ).catch(() => null);
        const served = (res?.models ?? []).map((m) => m.id).filter((x) => x && x.trim().length > 0);
        if (res === null) {
          ollamaNeedsModel = false;
          cardResult[t.key] = {
            ok: false,
            text: 'Ollama isn’t running on this computer. Not installed yet? Get it free at ollama.com/download, open it, then press Connect again.'
          };
          return false;
        }
        if (served.length === 0) {
          // Answering, with nothing to chat with: offer the download.
          ollamaNeedsModel = true;
          cardResult[t.key] = {
            ok: false,
            text: 'Ollama is running but has no chat model yet. Download one below; it runs on this computer and works offline.'
          };
          return false;
        }
        ollamaNeedsModel = false;
        model = served.includes(manualModel) ? manualModel : served[0];
      }
      await importProviderModels(t.name).catch(() => {});

      // Failures are reported on the card itself; keep raw JSON out of the banner.
      activeProfile = await api<LlmProfile>('/api/llm/profile', {
        method: 'POST',
        body: JSON.stringify({ provider: t.name, model }),
        silent: true
      });
      manualProvider = t.name;
      manualModel = model;
      await load();
      void loadCodingAgents();

      cardResult[t.key] = { ok: true, text: `Connected — chat now answers through ${t.label}.` };
      notice = `${t.label} connected. The next chat turn uses it — no restart needed.`;
      // Immediately prove it with a real turn. Registering a provider is
      // bookkeeping; the operator deserves to see the model answer before this
      // page claims anything. `cliSignInUnconfirmed` only predicted trouble —
      // the verification either confirms it or clears it for good.
      void verifyTemplate(t);
      if (cliSignInUnconfirmed) {
        console.info(`${kind} CLI credentials were not detected on disk; verifying live`);
      }
      return true;
    } catch (err) {
      const detail = extractProbeError(err);
      console.error('Quick connect failed', err);
      cardResult[t.key] = {
        ok: false,
        text: detail ?? `Could not connect ${t.label}. Check that the gateway is running and try again.`
      };
      return false;
    } finally {
      cardBusy[t.key] = false;
    }
  }

  // ----- Proof, not a guess -----
  //
  // Everything above this line is inference: a binary exists on disk, a
  // credentials file is present, a provider row was saved. None of it proves
  // the subscription behind the lane is signed in and serving. `verifyTemplate`
  // runs one real turn and shows what came back, which is the only honest
  // answer to "how do I know it is connected".
  interface ProviderVerifyResult {
    ok: boolean;
    provider?: string;
    model?: string;
    latencyMs?: number;
    reply?: string;
    error?: string;
  }

  let cardTesting = $state<Record<string, boolean>>({});

  /** Model to verify a lane against: CLI subscriptions take the `default`
   *  sentinel, Ollama needs a real tag that it actually serves. */
  function verifyModelFor(t: ProviderTemplate): string {
    if (templateAgentKind(t)) return 'default';
    if (isActiveTemplate(t) && activeProfile?.model) return activeProfile.model;
    const served = discoveredFor(t.name);
    if (served.length > 0) return served[0];
    return manualModel?.trim() || 'default';
  }

  /** Turn a raw provider error into something the operator can act on. */
  function verifyFailureText(t: ProviderTemplate, raw: string | undefined): string {
    const detail = (raw ?? '').trim() || 'The provider did not answer.';
    // The gateway already distils the failures it recognises into an
    // instruction; adding a second one just makes the card contradict itself.
    if (/\brun `/i.test(detail) || /\bpress (test|connect)\b/i.test(detail)) {
      // Desktop: the same fix is one press away on the card itself.
      if (canSignIn(t) && /\bin Terminal\b/i.test(detail)) {
        return `${detail.replace(/\.\s*$/, '')} — or press Sign in below.`;
      }
      return detail;
    }
    const kind = templateAgentKind(t);
    if (kind) {
      if (canSignIn(t)) return `${detail} Press Sign in on this card to fix it here.`;
      const signIn = kind === 'codex' ? 'codex login' : 'claude';
      return `${detail} Run \`${signIn}\` in Terminal and sign in, then press Test again.`;
    }
    if (t.key === 'ollama') {
      return `${detail} Start the Ollama app (or run \`ollama serve\`), then press Test again.`;
    }
    return detail;
  }

  async function verifyTemplate(t: ProviderTemplate): Promise<boolean> {
    cardTesting[t.key] = true;
    try {
      const res = await api<ProviderVerifyResult>(
        `/api/llm/providers/${encodeURIComponent(t.name)}/verify`,
        { method: 'POST', body: JSON.stringify({ model: verifyModelFor(t) }), silent: true }
      );
      if (res.ok) {
        const seconds = res.latencyMs ? `${(res.latencyMs / 1000).toFixed(1)}s` : 'instantly';
        const via = res.model ? ` via ${res.model}` : '';
        cardResult[t.key] = {
          ok: true,
          text: `Answered in ${seconds}${via} — replied “${res.reply}”. This lane is live.`
        };
        return true;
      }
      cardResult[t.key] = { ok: false, text: verifyFailureText(t, res.error) };
      return false;
    } catch (err) {
      console.error('Provider verify failed', err);
      cardResult[t.key] = {
        ok: false,
        text: verifyFailureText(t, extractProbeError(err) ?? undefined)
      };
      return false;
    } finally {
      cardTesting[t.key] = false;
    }
  }

  /** The card's primary action. The lane already answering chat has no primary
   *  action left — it renders as a state pill, not a button. */
  function cardCta(t: ProviderTemplate): string {
    return templateConnected(t) ? 'Use for chat' : 'Connect';
  }

  /** Whether the lane should still be presented as good. A failed test outranks
   *  the probe every time: the probe is a guess about a binary on disk, the
   *  test is a fact about a turn that did not come back. */
  function cardHealthy(t: ProviderTemplate): boolean {
    const result = cardResult[t.key];
    if (result && !result.ok) return false;
    return templateConnected(t);
  }

  function cardStatusLabel(t: ProviderTemplate): string {
    const result = cardResult[t.key];
    if (result && !result.ok) return 'Needs attention';
    if (!templateConnected(t)) return 'Not connected';
    if (isActiveTemplate(t)) return result?.ok ? 'Answering chat · verified' : 'Answering chat';
    return result?.ok ? 'Verified' : 'Ready';
  }

  function cardClick(t: ProviderTemplate) {
    void quickConnect(t);
  }

  async function quickConnectFromModal(t: ProviderTemplate) {
    const ok = await quickConnect(t);
    if (ok) showConnectProvider = false;
    else if (cardResult[t.key] && !cardResult[t.key].ok) notice = cardResult[t.key].text;
  }

  function isActiveModel(model: LlmModel): boolean {
    const providerName = getProviderName(model.provider_id);
    return providerName.toLowerCase() === (activeProfile?.provider ?? '').toLowerCase()
      && model.name.toLowerCase() === (activeProfile?.model ?? '').toLowerCase();
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

  // ----- In-app sign-in for the CLI lanes (desktop only) -----
  //
  // The subscription lanes used to dead-end at "run `codex login` in
  // Terminal". Inside the desktop shell that login runs in an embedded
  // terminal under the cards instead: the CLI still opens the browser itself,
  // and once the process exits cleanly the card re-verifies with a real turn.
  // Signing in never switches which brain answers chat — that stays with
  // Connect / Use for chat.
  const SIGN_IN_SCRIPTS: Record<'claude' | 'codex', string> = {
    // Log out first: a revoked refresh token makes `login status` still claim
    // "Logged in" and a plain login a no-op. `;` rather than `&&` so a
    // "not logged in" logout error cannot abort the login that follows.
    claude: 'claude auth logout; claude auth login',
    codex: 'codex logout; codex login'
  };
  /** For when the CLI cannot open a browser: prints a code + URL instead. */
  const CODEX_DEVICE_SCRIPT = 'codex login --device-auth';
  // Mirrors `SIGNIN_HOSTS` in desktop/src-tauri/src/pty.rs so that, when a
  // CLI prints several links, the one the shell will actually open wins.
  const SIGN_IN_HOSTS = [
    'auth.openai.com',
    'chatgpt.com',
    'platform.openai.com',
    'claude.ai',
    'console.anthropic.com',
    'anthropic.com'
  ];

  let signInFor = $state<ProviderTemplate | null>(null);
  let signInScript = $state('');
  let signInUrl = $state<string | null>(null);
  // The backend refused to open the host — show the link as text instead.
  let signInUrlFallback = $state(false);
  let signInExited = $state(false);
  let signInTerm = $state.raw<Terminal | null>(null);
  let signInPanelEl = $state<HTMLDivElement | null>(null);
  // True while a session is being killed on purpose (cancel, switching lane,
  // device code): the exit that follows is ours, not a failed sign-in.
  let signInSwitching = false;

  function cliInstalled(t: ProviderTemplate): boolean {
    const kind = templateAgentKind(t);
    if (!kind) return false;
    return !!codingAgents.find((a) => a.kind.toLowerCase() === kind)?.installed;
  }

  /** Sign in is offered only where it can actually run: the desktop shell, a
   *  CLI lane, and the CLI present on disk (otherwise the install hint stays). */
  function canSignIn(t: ProviderTemplate): boolean {
    return inDesktopShell && cliInstalled(t);
  }

  function signInLabel(t: ProviderTemplate): string {
    if (signInFor?.key === t.key && !signInExited) return 'Signing in…';
    const failed = !!cardResult[t.key] && !cardResult[t.key].ok;
    return templateConnected(t) && !failed ? 'Sign in again' : 'Sign in';
  }

  function isSignInHost(url: string): boolean {
    try {
      const host = new URL(url).hostname.toLowerCase();
      return SIGN_IN_HOSTS.some((h) => host === h || host.endsWith(`.${h}`));
    } catch {
      return false;
    }
  }

  function resetSignInSession(): void {
    signInUrl = null;
    signInUrlFallback = false;
    signInExited = false;
  }

  /** Kill the current session as an intentional act, not a failed sign-in. */
  async function stopSignInSession(): Promise<void> {
    signInSwitching = true;
    try {
      await signInTerm?.kill();
    } catch (_) {
      // kill is idempotent on the backend; nothing left to stop
    } finally {
      signInSwitching = false;
    }
  }

  async function openSignIn(t: ProviderTemplate): Promise<void> {
    const kind = templateAgentKind(t);
    if (!kind || !inDesktopShell) return;
    if (signInFor?.key === t.key) {
      // Same lane: a live session only needs focus, a finished one restarts.
      if (signInTerm?.isRunning()) {
        signInTerm.focus();
        return;
      }
      await stopSignInSession();
      signInScript = SIGN_IN_SCRIPTS[kind];
      resetSignInSession();
      await signInTerm?.start();
      return;
    }
    if (signInFor) await stopSignInSession();
    signInScript = SIGN_IN_SCRIPTS[kind];
    resetSignInSession();
    signInFor = t;
    requestAnimationFrame(() => {
      signInPanelEl?.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
    });
  }

  async function useDeviceCode(): Promise<void> {
    if (!signInFor || templateAgentKind(signInFor) !== 'codex') return;
    await stopSignInSession();
    signInScript = CODEX_DEVICE_SCRIPT;
    resetSignInSession();
    await signInTerm?.start();
  }

  async function cancelSignIn(): Promise<void> {
    await stopSignInSession();
    signInFor = null;
  }

  function closeSignIn(): void {
    signInFor = null;
  }

  function onSignInUrl(t: ProviderTemplate, url: string): void {
    if (signInFor?.key !== t.key) return;
    if (isSignInHost(url)) {
      signInUrl = url;
      signInUrlFallback = false;
      return;
    }
    if (!signInUrl) signInUrl = url;
  }

  async function openSignInPage(): Promise<void> {
    const url = signInUrl;
    if (!url) return;
    try {
      await openSignInUrl(url);
    } catch (err) {
      console.warn('open_signin_url refused', err);
      signInUrlFallback = true;
    }
  }

  async function onSignInExit(t: ProviderTemplate, code: number | null): Promise<void> {
    if (signInSwitching || signInFor?.key !== t.key) return;
    signInExited = true;
    if (code === 0) {
      try {
        await ensureProviderRegistered(t);
        await load();
        await loadCodingAgents();
      } catch (err) {
        console.error('Post sign-in registration failed', err);
        cardResult[t.key] = {
          ok: false,
          text:
            extractProbeError(err) ??
            `Signed in, but could not register ${t.label}. Check that the gateway is running, then press Test.`
        };
        return;
      }
      // Only a real turn may flip the card to connected.
      void verifyTemplate(t);
      return;
    }
    const why = code === null ? 'the process ended unexpectedly' : `exit code ${code}`;
    cardResult[t.key] = {
      ok: false,
      text: `Sign-in did not complete (${why}). Read the terminal below, then try again.`
    };
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
    try {
      const [p, m, st, prof, vault] = await Promise.all([
        api<LlmProvider[]>('/api/llm/providers').catch(() => []),
        api<LlmModel[]>('/api/llm/models').catch(() => []),
        api<LlmProviderStatus[]>('/api/llm/provider-status').catch(() => []),
        api<LlmProfile>('/api/llm/profile').catch(() => null as LlmProfile | null),
        api<LocalVault>('/api/local-vault').catch(() => null as LocalVault | null)
      ]);
      providers = p ?? [];
      models = m ?? [];
      providerStatuses = st ?? [];
      activeProfile = prof;
      localVault = vault;
      if (prof?.provider) manualProvider = prof.provider;
      if (prof?.model) manualModel = prof.model;
      void discoverAllModels();
    } catch (err) {
      console.error('Failed to load AI settings', err);
    }
  }

  /**
   * Ask every registered provider what it can serve, so the switcher is
   * populated on arrival. A provider that is down or missing a key just yields
   * an empty list — one dead provider must not blank out the others, hence the
   * per-provider catch rather than one around the whole batch.
   */
  async function discoverAllModels() {
    // Config-known providers (e.g. a first-run ollama that was never
    // "connected" in the UI) are probed too — the setup cards read this to
    // tell a live lane from a registered-but-dead one.
    const names = providerChoices;
    if (names.length === 0) return;
    discovering = true;
    try {
      const results = await Promise.all(
        names.map(async (name) => {
          try {
            // silent: a provider with no key answering "can't list models" is an
            // expected outcome here, not an error the whole app should banner.
            const res = await api<ProviderModelsResponse>(
              `/api/llm/providers/${encodeURIComponent(name)}/models`,
              { silent: true }
            );
            const ids = (res.models ?? []).map((m) => m.id).filter((x) => x && x.trim().length > 0);
            return [name, ids] as const;
          } catch {
            return [name, [] as string[]] as const;
          }
        })
      );
      discoveredModels = Object.fromEntries(results);
    } finally {
      discovering = false;
    }
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

  function getProviderName(providerId: string): string {
    return providers.find((p) => p.id === providerId)?.name ?? providerId;
  }

  async function upsertProviderTemplate(t: ProviderTemplate) {
    providerBusy[t.name] = true;
    const existing = providers.find((p) => p.name.toLowerCase() === t.name.toLowerCase());
    try {
      if (existing) {
        await api(`/api/llm/providers/${encodeURIComponent(existing.id)}`, {
          method: 'PATCH',
          body: JSON.stringify({ provider_type: t.providerType, base_url: t.baseUrl, api_key_env: t.apiKeyEnv })
        });
      } else {
        await api('/api/llm/providers', {
          method: 'POST',
          body: JSON.stringify({ name: t.name, provider_type: t.providerType, base_url: t.baseUrl, api_key_env: t.apiKeyEnv })
        });
      }
      await load();
      const apiKey = providerApiKeys[t.name]?.trim();
      if (apiKey) {
        try {
          // Backend probes the key against the provider before persisting (Phase 8).
          // A bad key returns 400 with `{error}` and never reaches the keyring.
          await api(`/api/llm/providers/${encodeURIComponent(t.name)}/credential`, {
            method: 'POST',
            body: JSON.stringify({ apiKey }),
            silent: true
          });
          providerApiKeys[t.name] = '';
        } catch (err) {
          const detail = extractProbeError(err);
          console.error('Credential probe failed', err);
          notice = `${t.label}: API key rejected. ${detail ?? 'Check the key and try again.'}`;
          await load();
          return;
        }
      }

      try {
        await importProviderModels(t.name);
        await load();
        // Connecting a key provider used to stop here and tell the operator to
        // go find a model in another section. "Connect" has to mean the same
        // thing on every card: this brain answers chat now.
        // Read back what was just registered rather than the discovery cache —
        // `load()` refreshes `models` synchronously but only kicks discovery
        // off in the background, so the cache is usually still empty here.
        const saved = providers.find((p) => p.name.toLowerCase() === t.name.toLowerCase());
        const registered = saved ? models.filter((m) => m.provider_id === saved.id) : [];
        const model =
          registered.find((m) => m.is_default)?.name ??
          registered[0]?.name ??
          discoveredFor(t.name)[0] ??
          manualModel?.trim();
        if (model) {
          activeProfile = await api<LlmProfile>('/api/llm/profile', {
            method: 'POST',
            body: JSON.stringify({ provider: t.name, model }),
            silent: true
          });
          manualProvider = t.name;
          manualModel = model;
          await load();
          cardResult[t.key] = { ok: true, text: `Connected — chat now answers through ${t.label}.` };
          notice = `${t.label} connected. The next chat turn uses it — no restart needed.`;
          void verifyTemplate(t);
        } else {
          cardResult[t.key] = {
            ok: false,
            text: `${t.label} saved, but it served no models. Check the API key and press Connect again.`
          };
          notice = cardResult[t.key].text;
        }
      } catch (err) {
        console.error('Model import failed', err);
        cardResult[t.key] = {
          ok: false,
          text: `${t.label} was saved, but model discovery failed. Check the API key and retry.`
        };
        notice = cardResult[t.key].text;
      }
    } catch (err) {
      console.error('Provider upsert failed', err);
      cardResult[t.key] = {
        ok: false,
        text: `Could not save ${t.label}. Check the gateway and credentials backend.`
      };
      notice = cardResult[t.key].text;
    } finally {
      providerBusy[t.name] = false;
    }
  }

  async function importProviderModels(providerName: string) {
    const provider = providers.find((p) => p.name.toLowerCase() === providerName.toLowerCase());
    if (!provider) return;
    const res = await api<ProviderModelsResponse>(
      `/api/llm/providers/${encodeURIComponent(providerName)}/models`,
      { silent: true }
    );
    const discovered = (res.models ?? []).map((m) => m.id).filter((x) => x && x.trim().length > 0);
    if (discovered.length === 0) return;
    const existingNames = new Set(
      models
        .filter((m) => m.provider_id === provider.id)
        .map((m) => m.name.trim().toLowerCase())
    );
    for (const modelName of discovered) {
      const key = modelName.trim().toLowerCase();
      if (!key || existingNames.has(key)) continue;
      await api('/api/llm/models', {
        method: 'POST',
        body: JSON.stringify({
          provider_id: provider.id,
          name: modelName
        })
      });
      existingNames.add(key);
    }
  }

  /** Register every model each provider currently reports, for all providers at
   *  once. Previously this only ever ran as a side effect of saving a provider,
   *  which is why the list could sit on a single stale entry indefinitely. */
  async function importAllModels() {
    importingAll = true;
    try {
      for (const p of providers) {
        try {
          await importProviderModels(p.name);
        } catch (err) {
          console.error(`Model import failed for ${p.name}`, err);
        }
      }
      await load();
      notice = 'Model list refreshed from every connected provider.';
    } finally {
      importingAll = false;
    }
  }

  async function setActiveProviderModel(providerName: string, modelName: string) {
    const provider = providerName.trim();
    const model = modelName.trim();
    if (!provider || !model) {
      notice = 'Provider and model are required.';
      return;
    }
    providerBusy[`active:${provider}`] = true;
    try {
      activeProfile = await api<LlmProfile>('/api/llm/profile', {
        method: 'POST',
        body: JSON.stringify({ provider, model }),
        silent: true
      });
      manualProvider = provider;
      manualModel = model;
      notice = `Active AI source set to ${provider} · ${model}.`;
      await load();
    } catch (err) {
      console.error('Failed to set active AI source', err);
      notice = 'Could not set active AI source. Connect that provider first, then try again.';
    } finally {
      providerBusy[`active:${provider}`] = false;
    }
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

  // The status strip reads only what this page already loads. Ollama has a
  // live signal here only when it is a provider the page probes for models.
  const ollamaKnown = $derived(providerChoices.some((n) => n.toLowerCase() === 'ollama'));
  const ollamaAnswering = $derived(discoveredFor('ollama').length > 0);

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
      <p class="page-sub">Connect the brains you already use, choose which one answers chat, and let Consistent CEO route execution to the right director.</p>
    </div>
    <button type="button" class="primary-btn" onclick={() => (showConnectProvider = true)}>Connect provider</button>
  </header>

  <AccountPanel />
  <PhonePanel />

  <section class="setup-section" aria-label="Connect your AI">
    <div class="setup-heading">
      <div>
        <h2 class="section-title">Connect your AI</h2>
        <p class="section-hint">
          Start with one — press Connect and it answers chat immediately. Subscriptions sign in
          {#if inDesktopShell}
            right here: press Sign in on a card, finish in your browser,
          {:else}
            outside this app: run <code>claude</code> or <code>codex login</code> once in Terminal
          {/if}
          and Consistent CEO reuses that login. Press Test any time to send a real message and see
          the reply.
        </p>
      </div>
      <span class="setup-count">
        {providerTemplates.slice(0, 3).filter(cardHealthy).length} of 3 ready
      </span>
    </div>
    <div class="setup-grid">
      {#each providerTemplates.slice(0, 3) as template (template.key)}
        <article
          class="setup-card"
          class:ready={cardHealthy(template)}
          class:needs-attention={cardResult[template.key] && !cardResult[template.key].ok}
        >
          <div class="setup-card-head">
            <span class="setup-icon" aria-hidden="true">
              {template.key === 'codex' ? '⌘' : template.key === 'claude-code' ? '✦' : '◉'}
            </span>
            <span
              class="setup-status"
              class:setup-status-ready={cardHealthy(template)}
              class:setup-status-bad={cardResult[template.key] && !cardResult[template.key].ok}
            >
              {cardStatusLabel(template)}
            </span>
          </div>
          <h3>{template.label}</h3>
          <p>
            {#if template.key === 'codex'}
              Use your authenticated Codex CLI and ChatGPT plan on this machine.
            {:else if template.key === 'claude-code'}
              Use your authenticated Claude Code CLI and Claude subscription.
            {:else}
              Run a local model privately with no cloud account or API key.
            {/if}
          </p>
          {#if cardTesting[template.key]}
            <p class="card-result" role="status">
              ⟳ Sending a real test message through {template.label}…
            </p>
          {:else if cardResult[template.key]}
            <p
              class="card-result"
              class:card-result-bad={!cardResult[template.key].ok}
              role="status"
            >
              {cardResult[template.key].ok ? '✓ ' : '✗ '}{cardResult[template.key].text}
            </p>
          {/if}
          {#if template.key === 'ollama' && ollamaPull && ollamaPull.status === 'pulling'}
            <div class="pull-progress" role="status" aria-live="polite">
              <div class="pull-bar"><span style="width: {pullPercent(ollamaPull)}%"></span></div>
              <p class="card-result">
                Downloading {ollamaPull.model}:
                {#if ollamaPull.total > 0}
                  {gb(ollamaPull.completed)} of {gb(ollamaPull.total)} GB ({pullPercent(ollamaPull)}%)
                {:else}
                  {ollamaPull.step ?? 'starting'}…
                {/if}
              </p>
            </div>
          {/if}
          <div class="setup-actions" class:setup-actions-wrap={canSignIn(template)}>
            {#if templateConnected(template) && isActiveTemplate(template)}
              <span class="setup-active-pill" class:setup-active-pill-bad={!cardHealthy(template)}>
                {cardHealthy(template) ? '✓ Answering chat' : 'Answering chat · failing'}
              </span>
            {:else if template.key === 'ollama' && ollamaNeedsModel}
              <button
                type="button"
                class="setup-action"
                disabled={ollamaPull?.status === 'pulling'}
                onclick={() => downloadLocalModel(template)}
              >
                {ollamaPull?.status === 'pulling' ? 'Downloading…' : 'Download a local model'}
              </button>
            {:else}
              <button
                type="button"
                class="setup-action"
                disabled={cardBusy[template.key] || cardTesting[template.key]}
                onclick={() => cardClick(template)}
              >
                {cardBusy[template.key] ? 'Connecting…' : cardCta(template)}
              </button>
            {/if}
            {#if templateConnected(template)}
              <button
                type="button"
                class="setup-test"
                disabled={cardBusy[template.key] || cardTesting[template.key]}
                onclick={() => verifyTemplate(template)}
              >
                {cardTesting[template.key] ? 'Testing…' : 'Test'}
              </button>
            {/if}
            {#if canSignIn(template)}
              <button
                type="button"
                class="setup-test setup-signin"
                class:setup-signin-urgent={cardResult[template.key] && !cardResult[template.key].ok}
                disabled={cardBusy[template.key] || (signInFor?.key === template.key && !signInExited)}
                onclick={() => openSignIn(template)}
              >
                {signInLabel(template)}
              </button>
            {/if}
          </div>
        </article>
      {/each}
    </div>
    {#if inDesktopShell && signInFor}
      {@const signInTemplate = signInFor}
      <!-- Full width under the grid, not inside a cell: a terminal squeezed
           into one of three columns would be unreadable. -->
      <div class="signin-panel" bind:this={signInPanelEl} aria-label="Sign in to {signInTemplate.label}">
        <div class="signin-head">
          <div>
            <h3 class="signin-title">Sign in to {signInTemplate.label}</h3>
            <p class="signin-hint">
              The CLI opens your browser — finish signing in there and come back. This card
              verifies automatically.
            </p>
          </div>
          <button
            type="button"
            class="icon-btn"
            onclick={() => (signInExited ? closeSignIn() : void cancelSignIn())}
            aria-label={signInExited ? 'Close' : 'Cancel sign-in'}
          >
            ✕
          </button>
        </div>
        {#key signInTemplate.key}
          <Terminal
            bind:this={signInTerm}
            script={signInScript}
            rows={16}
            onready={() => signInTerm?.focus()}
            onurl={(url) => onSignInUrl(signInTemplate, url)}
            onexit={(code) => void onSignInExit(signInTemplate, code)}
          />
        {/key}
        {#if signInUrl && signInUrlFallback}
          <p class="signin-url">
            Open this link in your browser: <code>{signInUrl}</code>
          </p>
        {/if}
        <div class="signin-actions">
          {#if signInUrl && !signInUrlFallback}
            <button type="button" class="setup-action" onclick={() => openSignInPage()}>
              Open sign-in page
            </button>
          {/if}
          {#if templateAgentKind(signInTemplate) === 'codex' && !signInExited && signInScript !== CODEX_DEVICE_SCRIPT}
            <button type="button" class="setup-test" onclick={() => useDeviceCode()}>
              Use device code instead
            </button>
          {/if}
          {#if signInExited}
            <button type="button" class="setup-test" onclick={closeSignIn}>Close</button>
          {:else}
            <button type="button" class="setup-test" onclick={() => cancelSignIn()}>Cancel</button>
          {/if}
        </div>
      </div>
    {/if}
  </section>

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
          {#if ollamaKnown}
            <span class="shell-pill">
              <span
                class="status-dot"
                class:dot-green={ollamaAnswering}
                class:dot-amber={!ollamaAnswering && discovering}
                class:dot-red={!ollamaAnswering && !discovering}
              ></span>
              Ollama · {ollamaAnswering ? 'answering' : discovering ? 'checking…' : 'not answering'}
            </span>
          {/if}
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

  {#if notice}
    <p class="notice" role="status">{notice}</p>
  {/if}

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

  <section class="status-card" aria-label="Current AI source">
    <div class="status-grid">
      <div class="status-item">
        <span class="status-label">Active source</span>
        <span class="status-value truncate">{activeSourceLabel}</span>
      </div>
      <div class="status-item">
        <span class="status-label">Runtime agent</span>
        <span class="status-value truncate">{operationsAgentId}</span>
      </div>
      <div class="status-item">
        <span class="status-label">Provider URL</span>
        <span class="status-value truncate">{activeProfile?.base_url ?? 'Not resolved'}</span>
      </div>
      <div class="status-item status-wide">
        <span class="status-label">Credential source</span>
        <span class="status-value connection">
          {activeProfile?.api_key_env ?? 'No key env'} · {activeProfile?.secret_source ?? 'unknown'} · {activeConnectionLabel}
        </span>
      </div>
      <div class="status-item status-wide">
        <span class="status-label">What this means</span>
          <span class="status-value connection">
          Claude Code and Codex use the authenticated local CLI on this computer. API providers use keys stored in the OS credential vault. Ollama runs locally without a cloud account.
        </span>
      </div>
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

  <section class="models-section" aria-label="Manual active AI source">
    <h2 class="section-title">The brain</h2>
    <p class="section-hint">
      Which model answers the next turn. Picked live from what each provider reports it can
      serve — this is separate from the runtime agent above, which is the CLI that executes work.
    </p>
    <div class="manual-row">
      <select
        class="key-input"
        bind:value={manualProvider}
        onchange={() => {
          manualModel = '';
          customModel = '';
        }}
        aria-label="Provider"
      >
        {#each providerChoices as name (name)}
          <option value={name}>{name}</option>
        {/each}
      </select>

      <select class="key-input" bind:value={manualModel} aria-label="Model">
        {#if modelChoices.length === 0}
          <option value="" disabled>
            {discovering ? 'Looking for models…' : 'None found — use Other'}
          </option>
        {/if}
        {#each modelChoices as name (name)}
          <option value={name}>{name}</option>
        {/each}
        <option value="__custom">Other… (type it)</option>
      </select>

      <button
        type="button"
        class="secondary-btn"
        disabled={providerBusy[`active:${manualProvider}`] ||
          (manualModel === '__custom' ? customModel.trim() === '' : manualModel.trim() === '')}
        onclick={() =>
          setActiveProviderModel(
            manualProvider,
            manualModel === '__custom' ? customModel : manualModel
          )}
      >
        Use
      </button>
    </div>

    {#if manualModel === '__custom'}
      <div class="custom-row">
        <input
          class="key-input"
          bind:value={customModel}
          placeholder="Model id, e.g. claude-opus-4-5"
        />
      </div>
    {/if}

    {#if selectedProviderReady === false}
      <p class="section-hint">
        <strong>{manualProvider}</strong> has no API key yet, so picking a model here will not
        take effect. Connect it first.
      </p>
    {/if}
  </section>

  <section class="models-section" aria-label="Provider model inventory">
    <h2 class="section-title">Imported models</h2>
    <p class="section-hint">
      What has been registered in the database. Import only ran when a provider was saved, so
      this can lag behind what the provider actually serves — refresh to pull the current list.
    </p>
    <div class="manual-row">
      <button type="button" class="secondary-btn" disabled={importingAll} onclick={importAllModels}>
        {importingAll ? 'Refreshing…' : 'Refresh from providers'}
      </button>
    </div>
    {#if importedModels.length === 0}
      <div class="empty-state">
        <p class="empty-title">No models imported yet</p>
        <p class="empty-copy">Connect Ollama or add an API key provider to import available models.</p>
      </div>
    {:else}
      <ul class="model-list">
        {#each importedModels as model}
          <li class="model-row">
            <div class="model-meta min-w-0">
              <span class="model-name truncate">{model.name}</span>
              <span class="model-provider truncate">{getProviderName(model.provider_id)}</span>
            </div>
            {#if isActiveModel(model)}
              <span class="badge-default">Active</span>
            {:else}
              <button type="button" class="secondary-btn" onclick={() => setActiveProviderModel(getProviderName(model.provider_id), model.name)}>
                Use
              </button>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
  </section>

  {#if showConnectProvider}
    <div
      class="modal-backdrop"
      role="button"
      tabindex="0"
      onclick={() => (showConnectProvider = false)}
      onkeydown={(e) => e.key === 'Escape' && (showConnectProvider = false)}
    >
      <div
        class="modal"
        role="dialog"
        aria-label="Connect AI provider"
        tabindex="-1"
        onmousedown={(e) => e.stopPropagation()}
        onclick={(e) => e.stopPropagation()}
        onkeydown={(e) => e.stopPropagation()}
      >
        <div class="modal-head">
          <h3>Connect provider</h3>
          <button type="button" class="icon-btn" onclick={() => (showConnectProvider = false)} aria-label="Close">✕</button>
        </div>
        <p class="modal-copy">Choose the brain you want to connect. Subscriptions reuse the login already on this Mac — no API key. API providers take a key stored in the OS credential vault. Ollama stays on this machine.</p>
        <div class="template-grid">
          {#each providerTemplates as t}
            <div class="template-card" class:template-card-active={isActiveTemplate(t)}>
              <div class="template-row">
                <span class="template-label">{t.label}</span>
                <!-- The same live truth the cards above show. A row saved in
                     the database is not the same thing as a lane that works,
                     and labelling it "Added" made those two look identical. -->
                <span
                  class="badge-state"
                  class:badge-state-on={cardHealthy(t)}
                  class:badge-state-active={isActiveTemplate(t) && cardHealthy(t)}
                  class:badge-state-bad={cardResult[t.key] && !cardResult[t.key].ok}
                >
                  {cardStatusLabel(t)}
                </span>
              </div>
              {#if isKeylessTemplate(t)}
                <p class="template-copy">
                  {#if t.apiKeyEnv === 'CLAUDE_CODE_CLI'}
                    No API key. Runs the <code>claude</code> CLI already signed in to your
                    Claude subscription.
                  {:else if t.apiKeyEnv === 'CODEX_CLI'}
                    No API key. Runs the <code>codex</code> CLI already signed in to your
                    ChatGPT plan.
                  {:else}
                    Local models on this machine, via {t.baseUrl}.
                  {/if}
                </p>
              {:else}
                <input
                  class="key-input"
                  type="password"
                  autocomplete="off"
                  placeholder="API key (optional if env is set)"
                  bind:value={providerApiKeys[t.name]}
                />
              {/if}
              {#if cardTesting[t.key]}
                <p class="template-result" role="status">⟳ Testing…</p>
              {:else if cardResult[t.key]}
                <p
                  class="template-result"
                  class:template-result-bad={!cardResult[t.key].ok}
                  role="status"
                >
                  {cardResult[t.key].ok ? '✓ ' : '✗ '}{cardResult[t.key].text}
                </p>
              {/if}
              <div class="template-actions">
                {#if isActiveTemplate(t)}
                  <span class="template-pill" class:template-pill-bad={!cardHealthy(t)}>
                    {cardHealthy(t) ? '✓ Answering chat' : 'Answering chat · failing'}
                  </span>
                {:else}
                  <button
                    type="button"
                    class="template-btn"
                    disabled={providerBusy[t.name] || cardBusy[t.key] || cardTesting[t.key]}
                    onclick={() => (isKeylessTemplate(t) ? quickConnectFromModal(t) : upsertProviderTemplate(t))}
                  >
                    {providerBusy[t.name] || cardBusy[t.key]
                      ? 'Connecting…'
                      : templateConnected(t)
                        ? 'Use for chat'
                        : 'Connect'}
                  </button>
                {/if}
                {#if templateConnected(t)}
                  <button
                    type="button"
                    class="template-test"
                    disabled={providerBusy[t.name] || cardBusy[t.key] || cardTesting[t.key]}
                    onclick={() => verifyTemplate(t)}
                  >
                    {cardTesting[t.key] ? 'Testing…' : 'Test'}
                  </button>
                {/if}
                {#if canSignIn(t)}
                  <button
                    type="button"
                    class="template-test"
                    disabled={providerBusy[t.name] || cardBusy[t.key]}
                    onclick={() => {
                      showConnectProvider = false;
                      void openSignIn(t);
                    }}
                  >
                    {signInLabel(t)}
                  </button>
                {/if}
              </div>
            </div>
          {/each}
        </div>
      </div>
    </div>
  {/if}
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
  .primary-btn {
    min-height: 44px;
    padding: 0 16px;
    border-radius: 10px;
    font-size: 0.875rem;
    font-weight: 700;
    border: 1px solid rgba(201, 138, 75, 0.46);
    background: linear-gradient(180deg, rgba(201, 138, 75, 0.34), rgba(138, 90, 46, 0.6));
    color: #ffffff;
    width: 100%;
  }
  @media (min-width: 640px) {
    .primary-btn {
      width: auto;
      flex-shrink: 0;
    }
  }
  .system-status {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .setup-section {
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: 16px;
    border: 1px solid rgba(201, 138, 75, 0.35);
    border-radius: 12px;
    background: linear-gradient(145deg, rgba(201, 138, 75, 0.10), var(--bg-secondary));
  }
  .setup-heading {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
  }
  .setup-count {
    flex-shrink: 0;
    font-size: 0.75rem;
    font-weight: 700;
    color: var(--accent-orange, #c98a4b);
  }
  .setup-grid {
    display: grid;
    grid-template-columns: 1fr;
    gap: 10px;
  }
  @media (min-width: 560px) {
    .setup-grid { grid-template-columns: repeat(3, 1fr); }
  }
  .setup-card {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 13px;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--bg-card);
  }
  .setup-card.ready {
    border-color: rgba(122, 129, 99, 0.55);
  }
  .setup-card.needs-attention {
    border-color: rgba(201, 106, 75, 0.55);
  }
  .setup-card-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }
  .setup-icon {
    display: grid;
    place-items: center;
    width: 28px;
    height: 28px;
    border-radius: 8px;
    background: var(--surface-accent-soft);
    color: var(--accent-orange, #c98a4b);
    font-weight: 800;
  }
  .setup-status {
    font-size: 0.6875rem;
    font-weight: 700;
    color: var(--text-muted);
  }
  .setup-status-ready { color: #7a8163; }
  .setup-status-bad { color: #c96a4b; }
  .setup-card h3 {
    margin: 0;
    font-size: 0.875rem;
    color: var(--text-primary);
  }
  .setup-card p {
    min-height: 3.4em;
    margin: 0;
    font-size: 0.75rem;
    line-height: 1.45;
    color: var(--text-muted);
  }
  .setup-card p.card-result {
    min-height: 0;
    margin: 0;
    font-size: 0.75rem;
    line-height: 1.45;
    color: #7a8163;
    overflow-wrap: anywhere;
  }
  .setup-card p.card-result-bad {
    color: #c96a4b;
  }
  .setup-actions {
    display: flex;
    align-items: stretch;
    gap: 6px;
    margin-top: auto;
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
  .setup-active-pill {
    flex: 1;
    display: grid;
    place-items: center;
    min-height: 36px;
    border: 1px solid rgba(122, 129, 99, 0.5);
    border-radius: 8px;
    background: rgba(122, 129, 99, 0.14);
    color: #7a8163;
    font-size: 0.75rem;
    font-weight: 700;
  }
  .setup-active-pill-bad {
    border-color: rgba(201, 106, 75, 0.5);
    background: rgba(201, 106, 75, 0.12);
    color: #c96a4b;
  }
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
  .setup-actions-wrap { flex-wrap: wrap; }
  .setup-signin { flex: 1 1 100%; }
  .setup-signin-urgent {
    border-color: rgba(201, 138, 75, 0.42);
    color: var(--accent-orange, #c98a4b);
  }
  .signin-panel {
    display: flex;
    flex-direction: column;
    gap: 10px;
    min-width: 0;
    padding: 13px;
    border: 1px solid rgba(201, 138, 75, 0.42);
    border-radius: 10px;
    background: var(--bg-card);
  }
  .signin-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
  }
  .signin-title {
    margin: 0;
    font-size: 0.875rem;
    color: var(--text-primary);
  }
  .signin-hint {
    margin: 4px 0 0;
    font-size: 0.75rem;
    line-height: 1.45;
    color: var(--text-muted);
  }
  .signin-url {
    margin: 0;
    font-size: 0.75rem;
    line-height: 1.45;
    color: var(--text-secondary);
    overflow-wrap: anywhere;
  }
  .signin-url code {
    font-family: var(--font-mono);
    color: var(--text-primary);
    user-select: all;
    -webkit-user-select: all;
  }
  .signin-actions {
    display: flex;
    flex-wrap: wrap;
    align-items: stretch;
    gap: 6px;
  }
  .signin-actions .setup-action {
    flex: 0 1 auto;
    padding: 0 14px;
  }
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
  .manual-row {
    display: grid;
    grid-template-columns: 1fr;
    gap: 8px;
  }
  /* Full width on every breakpoint — it is one field, not a row of three. */
  .custom-row {
    display: grid;
    grid-template-columns: 1fr;
    gap: 8px;
    margin-top: 8px;
  }
  @media (min-width: 640px) {
    .manual-row {
      grid-template-columns: minmax(120px, 0.8fr) minmax(180px, 1.2fr) auto;
      align-items: center;
    }
  }
  .empty-state {
    border: 1px dashed var(--border);
    border-radius: 10px;
    padding: 24px 16px;
    text-align: center;
  }
  .empty-title {
    margin: 0 0 6px;
    font-size: 0.9375rem;
    font-weight: 600;
    color: var(--text-secondary);
  }
  .empty-copy {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--text-muted);
  }
  .model-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .model-row {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 12px;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 10px;
    min-height: 44px;
  }
  .model-meta {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .model-name {
    font-size: 0.875rem;
    font-weight: 600;
    color: var(--text-primary);
  }
  .model-provider {
    font-size: 0.75rem;
    color: var(--text-muted);
  }
  .min-w-0 {
    min-width: 0;
  }
  .badge-default {
    font-size: 0.6875rem;
    font-weight: 700;
    color: #7a8163;
    border: 1px solid rgba(122, 129, 99, 0.35);
    background: rgba(122, 129, 99, 0.12);
    border-radius: 999px;
    padding: 6px 12px;
    flex-shrink: 0;
  }
  .secondary-btn {
    flex-shrink: 0;
    min-height: 40px;
    padding: 0 12px;
    border-radius: 8px;
    font-size: 0.8125rem;
    font-weight: 600;
    border: 1px solid rgba(201, 138, 75, 0.42);
    background: rgba(201, 138, 75, 0.16);
    color: #ffffff;
  }
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(10, 9, 8, 0.65);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 80;
    padding: 14px;
  }
  .modal {
    width: min(640px, 96vw);
    max-height: min(88vh, 720px);
    overflow: auto;
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 14px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .modal-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }
  .modal-head h3 {
    margin: 0;
    font-size: 1rem;
    color: var(--text-primary);
  }
  .icon-btn {
    min-height: 44px;
    min-width: 44px;
    border-radius: 8px;
    border: 1px solid var(--border);
    background: var(--bg-card);
    color: var(--text-secondary);
  }
  .modal-copy {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--text-muted);
  }
  .template-grid {
    display: grid;
    grid-template-columns: 1fr;
    gap: 10px;
  }
  @media (min-width: 520px) {
    .template-grid {
      grid-template-columns: repeat(2, 1fr);
    }
  }
  .template-card {
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--bg-card);
    padding: 10px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .template-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }
  .template-label {
    font-size: 0.8125rem;
    font-weight: 700;
    color: var(--text-primary);
  }
  /* Deliberately not a flex container. As one, the inline <code> and each run
     of text around it became separate flex items laid out in a row, which is
     what tore this sentence into columns. */
  .template-copy {
    margin: 0;
    min-height: 44px;
    display: block;
    font-size: 0.75rem;
    color: var(--text-muted);
    line-height: 1.45;
  }
  .template-copy code,
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
  .badge-state {
    flex-shrink: 0;
    font-size: 0.625rem;
    font-weight: 700;
    color: var(--text-muted);
    border: 1px solid var(--border);
    background: var(--bg-secondary);
    border-radius: 999px;
    padding: 4px 8px;
  }
  .badge-state-on {
    color: #7a8163;
    border-color: rgba(122, 129, 99, 0.35);
    background: rgba(122, 129, 99, 0.12);
  }
  .badge-state-active {
    color: #c98a4b;
    border-color: rgba(201, 138, 75, 0.45);
    background: rgba(201, 138, 75, 0.14);
  }
  .badge-state-bad {
    color: #c96a4b;
    border-color: rgba(201, 106, 75, 0.45);
    background: rgba(201, 106, 75, 0.12);
  }
  .template-card-active {
    border-color: rgba(201, 138, 75, 0.5);
  }
  .template-result {
    margin: 0;
    font-size: 0.6875rem;
    line-height: 1.45;
    color: #7a8163;
    overflow-wrap: anywhere;
  }
  .template-result-bad {
    color: #c96a4b;
  }
  .template-actions {
    display: flex;
    align-items: stretch;
    gap: 6px;
    margin-top: auto;
  }
  .template-pill {
    flex: 1;
    display: grid;
    place-items: center;
    min-height: 44px;
    border: 1px solid rgba(201, 138, 75, 0.45);
    border-radius: 8px;
    background: rgba(201, 138, 75, 0.14);
    color: #c98a4b;
    font-size: 0.75rem;
    font-weight: 700;
  }
  .template-pill-bad {
    border-color: rgba(201, 106, 75, 0.5);
    background: rgba(201, 106, 75, 0.12);
    color: #c96a4b;
  }
  .template-test {
    flex-shrink: 0;
    min-height: 44px;
    padding: 0 12px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-secondary);
    color: var(--text-secondary);
    font-size: 0.75rem;
    font-weight: 700;
  }
  .template-btn:disabled,
  .template-test:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .key-input {
    min-height: 44px;
    border-radius: 8px;
    border: 1px solid var(--border);
    background: var(--bg-secondary);
    color: var(--text-primary);
    padding: 0 10px;
    font-size: 16px;
  }
  /* The brain switcher reuses this box for <select>. WebKit renders a native
     select at its own height and ignores the border, so it has to be reset and
     given a chevron of its own. */
  select.key-input {
    appearance: none;
    -webkit-appearance: none;
    cursor: pointer;
    padding-right: 30px;
    background-image: linear-gradient(45deg, transparent 50%, var(--text-secondary) 50%),
      linear-gradient(135deg, var(--text-secondary) 50%, transparent 50%);
    background-position: calc(100% - 16px) calc(50% + 1px), calc(100% - 11px) calc(50% + 1px);
    background-size: 5px 5px, 5px 5px;
    background-repeat: no-repeat;
  }
  .template-btn {
    flex: 1;
    min-height: 44px;
    border-radius: 8px;
    border: 1px solid rgba(201, 138, 75, 0.42);
    background: rgba(201, 138, 75, 0.14);
    color: var(--text-primary);
    font-size: 0.8125rem;
    font-weight: 700;
    cursor: pointer;
  }
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
  .template-btn:hover:not(:disabled) {
    background: rgba(201, 138, 75, 0.24);
  }

  .pull-progress {
    margin: 0.4rem 0 0.2rem;
  }

  .pull-bar {
    height: 6px;
    border-radius: 999px;
    background: var(--color-bg-surface);
    border: 1px solid var(--color-border);
    overflow: hidden;
  }

  .pull-bar span {
    display: block;
    height: 100%;
    background: var(--color-orange);
    transition: width 0.4s ease;
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
