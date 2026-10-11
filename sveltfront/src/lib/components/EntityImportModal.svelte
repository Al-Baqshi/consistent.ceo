<script lang="ts">
  import { onDestroy } from 'svelte';
  import {
    analyzeEntityImport,
    apiErrorStatus,
    commitEntityImport,
    getEntityImportJob,
    type Entity,
    type EntityImportContext,
    type EntityImportDraft,
  } from '$lib/api';

  // One modal, two jobs: draft a brand-new entity from a source, or — when
  // `entityId` is set — fill an existing entity from its sources. Nothing is
  // written until the operator commits the reviewed draft.
  let {
    entityId = null,
    entityName = '',
    initialSource = '',
    initialContext = {},
    onclose,
    ondone,
  }: {
    entityId?: string | null;
    entityName?: string;
    initialSource?: string;
    /** Seeds the context step from what the entity already holds. */
    initialContext?: Partial<EntityImportContext>;
    onclose: () => void;
    ondone: (entity: Entity) => void;
  } = $props();

  let step = $state<'source' | 'context' | 'analyzing' | 'review' | 'failed'>('source');
  // The prop is only the starting value; the operator edits freely after.
  // svelte-ignore state_referenced_locally
  let source = $state(initialSource);

  const STAGE_OPTIONS = ['idea', 'prototype', 'building', 'launched', 'growing', 'maintenance'];

  function seedStage(value?: string): string {
    const v = (value ?? '').trim().toLowerCase();
    return STAGE_OPTIONS.includes(v) ? v : '';
  }

  // The operator's account of the entity. The analysis reads the code; this is
  // the intent behind it, and the gateway treats it as authoritative.
  // svelte-ignore state_referenced_locally
  let context = $state<EntityImportContext>({
    whatItIs: initialContext.whatItIs ?? '',
    primaryGoal: initialContext.primaryGoal ?? '',
    successCriteria: initialContext.successCriteria ?? '',
    stage: seedStage(initialContext.stage),
    activeContext: initialContext.activeContext ?? '',
    operatingRules: initialContext.operatingRules ?? '',
    notes: initialContext.notes ?? '',
  });
  let nameHint = $state('');
  let linksText = $state('');
  let jobId = $state<string | null>(null);
  let error = $state('');
  let draft = $state<EntityImportDraft | null>(null);
  let committing = $state(false);
  let pollTimer: ReturnType<typeof setTimeout> | null = null;
  /** Consecutive failed polls. A blip is retried; a dead gateway is not. */
  let pollFailures = 0;
  const POLL_INTERVAL_MS = 3000;
  const MAX_POLL_FAILURES = 10;
  /** Set on destroy, so a poll already awaiting the network doesn't re-arm. */
  let disposed = false;

  const includedCount = $derived(draft?.suggestedTasks?.filter((t) => t.include).length ?? 0);
  const contextReady = $derived(
    !!context.whatItIs?.trim() && !!context.primaryGoal?.trim()
  );

  function stopPolling() {
    if (pollTimer) {
      clearTimeout(pollTimer);
      pollTimer = null;
    }
  }

  function close() {
    stopPolling();
    onclose();
  }

  /** Advance from the source step; the context step is where the real answers live. */
  function toContext() {
    if (!source.trim()) return;
    error = '';
    step = 'context';
  }

  /** Trimmed, empties dropped — the gateway only wants what the operator said. */
  function trimmedContext(): EntityImportContext | undefined {
    const out: Record<string, string> = {};
    for (const [key, value] of Object.entries(context)) {
      const v = typeof value === 'string' ? value.trim() : '';
      if (v) out[key] = v;
    }
    return Object.keys(out).length > 0 ? (out as EntityImportContext) : undefined;
  }

  function parsedLinks(): string[] {
    return linksText
      .split(/[\n,]+/)
      .map((l) => l.trim())
      .filter((l) => l.startsWith('http://') || l.startsWith('https://'));
  }

  async function startAnalysis() {
    const trimmed = source.trim();
    if (!trimmed || !contextReady) return;
    error = '';
    step = 'analyzing';
    try {
      const res = await analyzeEntityImport({
        source: trimmed,
        name: nameHint.trim() || undefined,
        entityId: entityId ?? undefined,
        links: parsedLinks(),
        context: trimmedContext(),
      });
      jobId = res.jobId;
      pollFailures = 0;
      poll();
    } catch (err) {
      step = 'failed';
      error = err instanceof Error ? err.message : 'Analysis failed to start.';
    }
  }

  function poll() {
    stopPolling();
    if (disposed) return;
    pollTimer = setTimeout(async () => {
      if (!jobId || disposed) return;
      try {
        const job = await getEntityImportJob(jobId);
        pollFailures = 0;
        if (job.status === 'ready' && job.draft) {
          draft = job.draft;
          step = 'review';
        } else if (job.status === 'failed') {
          step = 'failed';
          error = job.error || 'Analysis failed.';
        } else {
          poll();
        }
      } catch (err) {
        // Import jobs live in gateway memory, so after a restart the job is
        // simply gone (404) — waiting longer cannot bring it back.
        if (apiErrorStatus(err) === 404) {
          step = 'failed';
          error = 'The gateway restarted, so this analysis was lost. Start the import again.';
          return;
        }
        // Anything else may be a blip (network, gateway mid-restart): retry a
        // bounded number of times, then say so instead of spinning forever.
        pollFailures += 1;
        if (pollFailures >= MAX_POLL_FAILURES) {
          step = 'failed';
          error = 'Lost contact with the gateway while analysing. Check that it is running, then start the import again.';
          return;
        }
        poll();
      }
    }, POLL_INTERVAL_MS);
  }

  async function commit() {
    if (!draft || !draft.name.trim() || committing) return;
    committing = true;
    error = '';
    try {
      const res = await commitEntityImport({
        jobId: jobId ?? undefined,
        entityId: entityId ?? undefined,
        draft,
      });
      stopPolling();
      ondone(res.entity);
    } catch (err) {
      committing = false;
      error = err instanceof Error ? err.message : 'Import failed.';
    }
  }

  onDestroy(() => {
    disposed = true;
    stopPolling();
  });
</script>

<div
  class="modal-backdrop"
  role="button"
  tabindex="0"
  onclick={close}
  onkeydown={(e) => e.key === 'Escape' && close()}
>
  <div
    class="modal wide"
    role="dialog"
    tabindex="-1"
    onmousedown={(e) => e.stopPropagation()}
    onclick={(e) => e.stopPropagation()}
    onkeydown={(e) => e.stopPropagation()}
  >
    {#if step === 'source' || step === 'context' || step === 'review'}
      <div class="import-steps">
        <span class="import-step" class:current={step === 'source'}>1</span>
        <span class="import-step-sep">·</span>
        <span class="import-step" class:current={step === 'context'}>2</span>
        <span class="import-step-sep">·</span>
        <span class="import-step" class:current={step === 'review'}>3</span>
      </div>
    {/if}

    {#if step === 'source'}
      <h3 class="modal-title">
        {entityId ? `Fill “${entityName}” from its sources` : 'Import an existing project'}
      </h3>
      <p class="modal-copy">
        {entityId
          ? 'Point at where this entity lives — a project folder on this machine (best), a GitHub URL, or a website. The engine reads it and drafts the missing fields, goals, and tasks; you review everything before it is applied.'
          : "Point at a project folder on this machine (best), a GitHub URL, or a website. The engine reads it and drafts the entity — you review everything before it's created."}
      </p>
      <div class="modal-form">
        <label>Folder path or URL
          <input
            type="text"
            bind:value={source}
            placeholder="~/projects/my-app or https://github.com/you/repo"
            onkeydown={(e) => e.key === 'Enter' && toContext()}
          />
        </label>
        <label>Additional links (optional, one per line)
          <textarea
            bind:value={linksText}
            rows="2"
            placeholder={'https://github.com/you/repo\nhttps://your-live-site.com'}
          ></textarea>
        </label>
        {#if !entityId}
          <label>Name (optional) <input type="text" bind:value={nameHint} placeholder="Overrides the detected name" /></label>
        {/if}
      </div>
      <div class="modal-actions">
        <button class="btn-secondary" onclick={close}>Cancel</button>
        <button class="btn-primary" onclick={toContext} disabled={!source.trim()}>Next: describe it</button>
      </div>
    {:else if step === 'context'}
      <h3 class="modal-title">
        {entityId ? `Tell the engine about “${entityName}”` : 'Tell the engine what this is'}
      </h3>
      <p class="modal-copy">
        The analysis reads the code; you know the intent. Your answers are treated as
        authoritative &mdash; they steer the goals, the suggested tasks and what gets indexed.
      </p>
      <div class="modal-form">
        <label>What is it?
          <textarea
            bind:value={context.whatItIs}
            rows="2"
            placeholder="A SaaS that turns support tickets into changelog entries, for small product teams"
          ></textarea>
          {#if !context.whatItIs?.trim()}
            <span class="field-hint">Required &mdash; one sentence is enough.</span>
          {/if}
        </label>
        <label>Main goal right now
          <input
            type="text"
            bind:value={context.primaryGoal}
            placeholder="Get the first 10 paying teams onboarded"
          />
          {#if !context.primaryGoal?.trim()}
            <span class="field-hint">Required &mdash; one sentence is enough.</span>
          {/if}
        </label>
        <label>What does success look like?
          <input
            type="text"
            bind:value={context.successCriteria}
            placeholder="$5k MRR and under 2% weekly churn"
          />
        </label>
        <label>Stage
          <select bind:value={context.stage}>
            <option value="">Not sure</option>
            <option value="idea">Idea</option>
            <option value="prototype">Prototype</option>
            <option value="building">Building</option>
            <option value="launched">Launched</option>
            <option value="growing">Growing</option>
            <option value="maintenance">Maintenance</option>
          </select>
        </label>
        <label>What's happening right now?
          <textarea
            bind:value={context.activeContext}
            rows="2"
            placeholder="Rewriting the billing flow before the launch on the 20th"
          ></textarea>
        </label>
        <label>Rules &amp; constraints
          <textarea
            bind:value={context.operatingRules}
            rows="2"
            placeholder="Never touch production data. Ship behind a flag. No new dependencies."
          ></textarea>
        </label>
        <label>Anything else the engine should know
          <textarea
            bind:value={context.notes}
            rows="2"
            placeholder="History, people, deadlines, dead ends already tried…"
          ></textarea>
        </label>
      </div>
      {#if error}
        <div class="import-error">{error}</div>
      {/if}
      <div class="modal-actions">
        <button class="btn-secondary" onclick={() => (step = 'source')}>Back</button>
        <button class="btn-primary" onclick={startAnalysis} disabled={!contextReady}>Analyze</button>
      </div>
    {:else if step === 'analyzing'}
      <h3 class="modal-title">Analyzing {source}</h3>
      <p class="modal-copy">
        Reading the project — README, manifests, file tree, recent activity.
        A local repo takes a few minutes; you can leave this open.
        When you create the entity, the source is also indexed into its knowledge base —
        README, repository map, conventions and docs — so every chat and heartbeat can draw on it.
      </p>
      <div class="import-progress"><span class="import-spinner"></span> Working…</div>
      <div class="modal-actions">
        <button class="btn-secondary" onclick={close}>Cancel</button>
      </div>
    {:else if step === 'review' && draft}
      <h3 class="modal-title">Review: {draft.name || 'Imported project'}</h3>
      {#if entityId}
        <p class="modal-copy">
          Fields below are what the entity will hold after applying — analysis findings plus
          anything it already had. Edit freely; blank fields stay untouched.
        </p>
      {/if}
      {#if draft.summary}
        <div class="import-summary">{draft.summary}</div>
      {/if}
      <div class="modal-form">
        <label>Name <input type="text" bind:value={draft.name} /></label>
        <label>Description <textarea bind:value={draft.description} rows="2"></textarea></label>
        <div class="import-row">
          <label>Type <input type="text" bind:value={draft.entityType} placeholder="product, content, …" /></label>
          <label>Stage <input type="text" bind:value={draft.stage} placeholder="building, launched, …" /></label>
        </div>
        <label>Profile <textarea bind:value={draft.profile} rows="2"></textarea></label>
        <label>Goals <textarea bind:value={draft.goals} rows="2"></textarea></label>
        <label>Active context <textarea bind:value={draft.activeContext} rows="2"></textarea></label>
        <label>Notes <textarea bind:value={draft.notes} rows="2"></textarea></label>
        <div class="import-row">
          <label>Backend <input type="text" bind:value={draft.setup.backendStack} /></label>
          <label>Frontend <input type="text" bind:value={draft.setup.frontendStack} /></label>
          <label>Database <input type="text" bind:value={draft.setup.databaseStack} /></label>
        </div>
        <div class="import-row">
          <label>Domain <input type="text" bind:value={draft.setup.primaryDomain} /></label>
          <label>Success criteria <input type="text" bind:value={draft.setup.successCriteria} /></label>
        </div>
        <label>Repo / source path <input type="text" bind:value={draft.setup.repoPath} /></label>
        {#if draft.suggestedTasks.length > 0}
          <div class="import-tasks">
            <div class="import-tasks-header">Suggested tasks &amp; reminders — uncheck what you don't want</div>
            {#each draft.suggestedTasks as task}
              <label class="import-task-row">
                <input type="checkbox" bind:checked={task.include} />
                <span class="import-task-kind" class:reminder={task.kind === 'reminder'}>{task.kind}</span>
                <span class="import-task-title">{task.title}</span>
                {#if task.dueAt}
                  <span class="import-task-due">{new Date(task.dueAt).toLocaleDateString()}</span>
                {/if}
              </label>
            {/each}
          </div>
        {/if}
      </div>
      {#if error}
        <div class="import-error">{error}</div>
      {/if}
      <p class="import-note">
        On create, the source is indexed into the entity's knowledge base (see the Knowledge
        card on the entity page).
      </p>
      <div class="modal-actions">
        <button class="btn-secondary" onclick={close}>Cancel</button>
        <button class="btn-primary" onclick={commit} disabled={committing || !draft.name.trim()}>
          {committing
            ? entityId ? 'Applying…' : 'Creating…'
            : `${entityId ? 'Apply to entity' : 'Create entity'}${includedCount ? ` + ${includedCount} item${includedCount === 1 ? '' : 's'}` : ''}`}
        </button>
      </div>
    {:else if step === 'failed'}
      <h3 class="modal-title">Analysis failed</h3>
      <div class="import-error">{error}</div>
      <div class="modal-actions">
        <button class="btn-secondary" onclick={close}>Close</button>
        <button class="btn-primary" onclick={() => { step = 'source'; error = ''; }}>Try again</button>
      </div>
    {/if}
  </div>
</div>

<style>
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 100;
    padding: 16px;
  }
  .modal {
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: var(--radius-lg, 10px);
    padding: 20px;
    width: 100%;
  }
  .modal.wide {
    max-width: 580px;
    max-height: 85vh;
    overflow-y: auto;
  }
  .modal-title { font-size: 14px; font-weight: 700; margin-bottom: 12px; color: var(--text-primary); }
  .modal-copy { font-size: 12px; color: var(--text-secondary); margin: -6px 0 12px; line-height: 1.5; }
  .modal-form { display: flex; flex-direction: column; gap: 10px; margin-bottom: 16px; }
  .modal-form label { font-size: 12px; color: var(--text-secondary); display: flex; flex-direction: column; gap: 4px; }
  .modal-form input, .modal-form textarea, .modal-form select {
    padding: 6px 10px; border-radius: 6px; border: 1px solid var(--border);
    background: var(--bg-card); color: var(--text-primary); font-size: 14px;
  }
  .field-hint { font-size: 11px; color: var(--text-muted); }

  .import-steps {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 11px;
    font-weight: 600;
    color: var(--text-muted);
    margin-bottom: 10px;
  }
  .import-step { opacity: 0.5; }
  .import-step.current { opacity: 1; color: var(--accent-purple); }
  .import-step-sep { opacity: 0.35; }

  .import-note {
    font-size: 11px;
    color: var(--text-muted);
    line-height: 1.5;
    margin: 0 0 10px;
  }
  .modal-actions { display: flex; justify-content: flex-end; gap: 8px; }
  .btn-primary { background: var(--accent-purple); color: #fff; border: none; padding: 6px 14px; border-radius: 6px; font-weight: 600; cursor: pointer; }
  .btn-primary:disabled { opacity: 0.5; cursor: default; }
  .btn-secondary { background: var(--bg-card); border: 1px solid var(--border); color: var(--text-secondary); padding: 6px 14px; border-radius: 6px; cursor: pointer; }

  .import-progress {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 12px;
    color: var(--text-secondary);
    margin: 16px 0;
  }
  .import-spinner {
    width: 12px;
    height: 12px;
    border: 2px solid var(--border);
    border-top-color: var(--accent-purple);
    border-radius: 50%;
    animation: importSpin 0.8s linear infinite;
  }
  @keyframes importSpin { to { transform: rotate(360deg); } }

  .import-summary {
    font-size: 12px;
    color: var(--text-secondary);
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 10px;
    margin: 10px 0 12px;
    line-height: 1.5;
  }

  .import-row {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(120px, 1fr));
    gap: 10px;
  }

  .import-tasks {
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 8px;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .import-tasks-header {
    font-size: 11px;
    font-weight: 600;
    color: var(--text-secondary);
    margin-bottom: 4px;
  }
  .import-task-row {
    display: flex !important;
    flex-direction: row !important;
    align-items: center;
    gap: 8px;
    padding: 4px 6px;
    border-radius: 4px;
    cursor: pointer;
  }
  .import-task-row:hover { background: var(--bg-card-hover); }
  .import-task-row input[type='checkbox'] { width: auto; }
  .import-task-kind {
    font-size: 10px;
    font-weight: 600;
    text-transform: uppercase;
    color: var(--accent-cyan);
    flex-shrink: 0;
  }
  .import-task-kind.reminder { color: var(--color-warning, #e0a55e); }
  .import-task-title { font-size: 12px; color: var(--text-primary); flex: 1; }
  .import-task-due { font-size: 10px; color: var(--text-muted); flex-shrink: 0; }

  .import-error {
    font-size: 12px;
    color: var(--color-error, #c96a4b);
    background: rgba(201, 106, 75, 0.1);
    border-radius: 6px;
    padding: 8px 10px;
    margin: 10px 0;
  }
</style>
