<script lang="ts">
  import { page } from '$app/stores';
  import Seo from '$lib/components/Seo.svelte';
  import {
    SKILL_TEMPLATE,
    copySkill,
    createSkill,
    deleteSkill,
    getSkill,
    importLocalSkills,
    listLocalSkillSources,
    listSkills,
    renderSkillMarkdown,
    setSkillEnabled,
    skillErrorMessage,
    slugify,
    updateSkill,
    uploadSkill,
    type ExternalSkill,
    type ImportReport,
    type SkillDetail,
    type SkillEntityRef,
    type SkillSummary,
    type SkillTarget
  } from '$lib/skills';

  type Filter = 'all' | 'global' | 'entity' | 'builtin';
  type ImportTab = 'claude' | 'upload' | 'path';

  // ── List state ──
  let skills = $state<SkillSummary[]>([]);
  let entities = $state<SkillEntityRef[]>([]);
  let localImport = $state(false);
  let loading = $state(true);
  let loadError = $state<string | null>(null);
  let notice = $state<string | null>(null);
  let filter = $state<Filter>('all');
  let query = $state('');
  let pending = $state<Record<string, boolean>>({});
  let rowError = $state<string | null>(null);

  // ── Detail drawer ──
  let openId = $state<string | null>(null);
  let detail = $state<SkillDetail | null>(null);
  let detailLoading = $state(false);
  let detailError = $state<string | null>(null);
  let editing = $state(false);
  let rawMode = $state(false);
  let draftDescription = $state('');
  let draftBody = $state('');
  let draftRaw = $state('');
  let saving = $state(false);
  let confirmDelete = $state(false);
  let copyTarget = $state('global');

  // ── Create ──
  let creating = $state(false);
  let newName = $state('');
  let newDescription = $state('');
  let newBody = $state(SKILL_TEMPLATE);
  let newTarget = $state('global');
  let createBusy = $state(false);
  let createError = $state<string | null>(null);

  // ── Import ──
  let importing = $state(false);
  let importTab = $state<ImportTab>('upload');
  let importTarget = $state('global');
  let importOverwrite = $state(false);
  let importBusy = $state(false);
  let importError = $state<string | null>(null);
  let importResult = $state<ImportReport | null>(null);
  let claudeDir = $state<string | null>(null);
  let claudeSkills = $state<ExternalSkill[] | null>(null);
  let claudeSelected = $state<Record<string, boolean>>({});
  let uploadSkillMd = $state<string | null>(null);
  let uploadFiles = $state<{ path: string; content: string }[]>([]);
  let uploadLabel = $state<string | null>(null);
  let pathInput = $state('');

  const TEXT_EXTENSIONS =
    /\.(md|markdown|txt|json|ya?ml|csv|tsv|py|js|mjs|ts|sh|bash|zsh|html|css|toml|xml|sql|rb|go|rs|ini|cfg|env\.example)$/i;
  const MAX_UPLOAD_FILE_BYTES = 2 * 1024 * 1024;

  const nameBySlugId = $derived(new Map(skills.map((s) => [s.id, s.title])));

  function entityName(id: string | undefined): string {
    if (!id) return 'Project';
    return entities.find((e) => e.id === id)?.name ?? 'Project';
  }

  function targetFrom(value: string): SkillTarget {
    return value === 'global' ? { scope: 'global' } : { scope: 'entity', entityId: value };
  }

  function matches(s: SkillSummary, q: string): boolean {
    if (!q) return true;
    const hay = `${s.title} ${s.slug} ${s.description}`.toLowerCase();
    return q
      .toLowerCase()
      .split(/\s+/)
      .filter(Boolean)
      .every((w) => hay.includes(w));
  }

  const visible = $derived(
    skills.filter((s) => (filter === 'all' || s.scope === filter) && matches(s, query.trim()))
  );
  const workspaceSkills = $derived(visible.filter((s) => s.scope === 'global'));
  const builtinSkills = $derived(visible.filter((s) => s.scope === 'builtin'));
  const projectGroups = $derived.by(() => {
    const groups = new Map<string, SkillSummary[]>();
    for (const s of visible) {
      if (s.scope !== 'entity' || !s.entityId) continue;
      const list = groups.get(s.entityId) ?? [];
      list.push(s);
      groups.set(s.entityId, list);
    }
    return [...groups.entries()]
      .map(([entityId, list]) => ({ entityId, name: entityName(entityId), skills: list }))
      .sort((a, b) => a.name.localeCompare(b.name));
  });
  const activeCount = $derived(skills.filter((s) => s.active).length);
  const customCount = $derived(skills.filter((s) => s.scope !== 'builtin').length);
  const hasCustom = $derived(customCount > 0);

  /** `?entity=<id>`: show exactly what that project's chat sees. */
  const focusEntity = $derived($page.url.searchParams.get('entity'));

  async function load(): Promise<void> {
    loadError = null;
    try {
      const res = await listSkills(focusEntity ?? undefined);
      skills = res.skills;
      entities = res.entities;
      localImport = res.localImport;
    } catch (err) {
      loadError = skillErrorMessage(err, 'Could not load skills.');
    } finally {
      loading = false;
    }
  }

  $effect(() => {
    // Re-read whenever the focused project changes (or is cleared).
    void focusEntity;
    void load();
  });

  function flash(message: string): void {
    notice = message;
    setTimeout(() => {
      if (notice === message) notice = null;
    }, 3200);
  }

  // ── Row actions ──
  async function toggle(skill: SkillSummary): Promise<void> {
    if (pending[skill.id]) return;
    pending = { ...pending, [skill.id]: true };
    rowError = null;
    const next = !skill.enabled;
    // Optimistic: flip now, reconcile (shadowing can change) after.
    skills = skills.map((s) => (s.id === skill.id ? { ...s, enabled: next, active: next && s.active } : s));
    try {
      await setSkillEnabled(skill.id, next);
      await load();
      if (detail?.id === skill.id) await openSkill(skill.id, false);
    } catch (err) {
      skills = skills.map((s) => (s.id === skill.id ? skill : s));
      rowError = skillErrorMessage(err, 'Could not change that skill.');
    } finally {
      const { [skill.id]: _done, ...rest } = pending;
      pending = rest;
    }
  }

  // ── Detail ──
  async function openSkill(id: string, resetMode = true): Promise<void> {
    openId = id;
    detailLoading = true;
    detailError = null;
    if (resetMode) {
      editing = false;
      rawMode = false;
      confirmDelete = false;
    }
    try {
      const d = await getSkill(id);
      if (openId !== id) return;
      detail = d;
      draftDescription = d.description;
      draftBody = d.body;
      draftRaw = d.raw;
      copyTarget = d.scope === 'builtin' ? 'global' : (entities[0]?.id ?? 'global');
    } catch (err) {
      detailError = skillErrorMessage(err, 'Could not open that skill.');
    } finally {
      detailLoading = false;
    }
  }

  function closeSkill(): void {
    if (saving) return;
    openId = null;
    detail = null;
    editing = false;
    confirmDelete = false;
    detailError = null;
  }

  function startEdit(): void {
    if (!detail) return;
    draftDescription = detail.description;
    draftBody = detail.body;
    draftRaw = detail.raw;
    rawMode = false;
    editing = true;
    detailError = null;
  }

  async function saveEdit(): Promise<void> {
    if (!detail || saving) return;
    saving = true;
    detailError = null;
    try {
      const patch = rawMode
        ? { raw: draftRaw }
        : { description: draftDescription.trim(), body: draftBody };
      await updateSkill(detail.id, patch);
      editing = false;
      await Promise.all([load(), openSkill(detail.id, false)]);
      flash('Saved.');
    } catch (err) {
      detailError = skillErrorMessage(err, 'Could not save the skill.');
    } finally {
      saving = false;
    }
  }

  async function removeSkill(): Promise<void> {
    if (!detail || saving) return;
    saving = true;
    detailError = null;
    try {
      const title = detail.title;
      await deleteSkill(detail.id);
      saving = false;
      closeSkill();
      await load();
      flash(`Deleted “${title}”.`);
    } catch (err) {
      detailError = skillErrorMessage(err, 'Could not delete the skill.');
    } finally {
      saving = false;
    }
  }

  async function copyCurrent(): Promise<void> {
    if (!detail || saving) return;
    saving = true;
    detailError = null;
    try {
      const copy = await copySkill(detail.id, targetFrom(copyTarget));
      await load();
      await openSkill(copy.id);
      flash(
        detail?.scope === 'builtin'
          ? 'Your copy is live — it replaces the built-in everywhere.'
          : `Copied to ${copy.scope === 'global' ? 'your workspace' : entityName(copy.entityId)}.`
      );
      startEdit();
    } catch (err) {
      detailError = skillErrorMessage(err, 'Could not copy the skill.');
    } finally {
      saving = false;
    }
  }

  // ── Create ──
  function openCreate(): void {
    newName = '';
    newDescription = '';
    newBody = SKILL_TEMPLATE;
    newTarget = focusEntity ?? 'global';
    createError = null;
    creating = true;
  }

  const newSlug = $derived(slugify(newName));
  const canCreate = $derived(
    newSlug.length > 0 && newDescription.trim().length > 0 && newDescription.length <= 1024
  );

  async function submitCreate(): Promise<void> {
    if (!canCreate || createBusy) return;
    createBusy = true;
    createError = null;
    try {
      const made = await createSkill(targetFrom(newTarget), {
        name: newName,
        description: newDescription.trim(),
        body: newBody
      });
      creating = false;
      await load();
      flash(`“${made.title}” is ready — the agent will use it when a request matches.`);
      await openSkill(made.id);
    } catch (err) {
      createError = skillErrorMessage(err, 'Could not create the skill.');
    } finally {
      createBusy = false;
    }
  }

  // ── Import ──
  function openImport(): void {
    importTab = localImport ? 'claude' : 'upload';
    importTarget = focusEntity ?? 'global';
    importOverwrite = false;
    importError = null;
    importResult = null;
    uploadSkillMd = null;
    uploadFiles = [];
    uploadLabel = null;
    pathInput = '';
    importing = true;
    if (localImport && claudeSkills === null) void loadClaudeSkills();
  }

  async function loadClaudeSkills(): Promise<void> {
    try {
      const res = await listLocalSkillSources();
      claudeDir = res.claudeSkillsDir;
      claudeSkills = res.skills;
      claudeSelected = {};
    } catch (err) {
      claudeSkills = [];
      importError = skillErrorMessage(err, 'Could not read your Claude skills.');
    }
  }

  const claudeChosen = $derived(
    (claudeSkills ?? []).filter((s) => claudeSelected[s.path]).map((s) => s.path)
  );

  async function onPickFiles(event: Event): Promise<void> {
    importError = null;
    importResult = null;
    const input = event.currentTarget as HTMLInputElement;
    const files = Array.from(input.files ?? []);
    input.value = '';
    if (!files.length) return;
    const rel = (f: File) =>
      ((f as File & { webkitRelativePath?: string }).webkitRelativePath || f.name).replace(/\\/g, '/');
    const skillFile = files
      .filter((f) => rel(f).split('/').pop()?.toUpperCase() === 'SKILL.MD')
      .sort((a, b) => rel(a).length - rel(b).length)[0];
    const markdown = skillFile ?? files.find((f) => /\.md$/i.test(f.name));
    if (!markdown) {
      importError = 'Pick a SKILL.md file, or a skill folder that contains one.';
      return;
    }
    const root = rel(markdown).split('/').slice(0, -1).join('/');
    const support: { path: string; content: string }[] = [];
    for (const f of files) {
      if (f === markdown) continue;
      const path = rel(f);
      if (root && !path.startsWith(`${root}/`)) continue;
      const inner = root ? path.slice(root.length + 1) : path;
      if (!inner || inner.split('/').some((seg) => seg.startsWith('.'))) continue;
      if (!TEXT_EXTENSIONS.test(inner) || f.size > MAX_UPLOAD_FILE_BYTES) continue;
      support.push({ path: inner, content: await f.text() });
    }
    uploadSkillMd = await markdown.text();
    uploadFiles = support;
    uploadLabel = `${markdown.name}${support.length ? ` + ${support.length} file${support.length === 1 ? '' : 's'}` : ''}`;
  }

  async function submitImport(): Promise<void> {
    if (importBusy) return;
    importBusy = true;
    importError = null;
    importResult = null;
    const target = targetFrom(importTarget);
    try {
      if (importTab === 'upload') {
        if (!uploadSkillMd) {
          importError = 'Choose a SKILL.md or a skill folder first.';
          return;
        }
        const made = await uploadSkill(target, uploadSkillMd, uploadFiles, importOverwrite);
        importResult = { imported: [made], skipped: [] };
      } else {
        const paths = importTab === 'claude' ? claudeChosen : [pathInput.trim()].filter(Boolean);
        if (!paths.length) {
          importError = importTab === 'claude' ? 'Pick at least one skill.' : 'Enter a folder path.';
          return;
        }
        importResult = await importLocalSkills(target, paths, importOverwrite);
      }
      await load();
      if (importResult.imported.length && !importResult.skipped.length) {
        importing = false;
        flash(
          importResult.imported.length === 1
            ? `Imported “${importResult.imported[0].title}”.`
            : `Imported ${importResult.imported.length} skills.`
        );
      }
    } catch (err) {
      importError = skillErrorMessage(err, 'Import failed.');
    } finally {
      importBusy = false;
    }
  }

  function onKey(e: KeyboardEvent): void {
    if (e.key !== 'Escape') return;
    if (creating && !createBusy) creating = false;
    else if (importing && !importBusy) importing = false;
    else if (openId) closeSkill();
  }

  function scopeLabel(s: SkillSummary): string {
    if (s.scope === 'builtin') return 'Built-in';
    if (s.scope === 'global') return 'Workspace';
    return entityName(s.entityId);
  }

  function formatUpdated(iso?: string): string {
    if (!iso) return '';
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return '';
    return d.toLocaleDateString(undefined, { month: 'short', day: 'numeric', year: 'numeric' });
  }
</script>

<svelte:window onkeydown={onKey} />

<Seo title="Skills" noindex />

{#snippet card(skill: SkillSummary)}
  <li class="card" class:is-off={!skill.active}>
    <button
      type="button"
      class="card-open"
      onclick={() => void openSkill(skill.id)}
      aria-label={`Open ${skill.title}`}
    >
      <span class="card-title">{skill.title}</span>
      <code class="card-slug">{skill.slug}</code>
      <span class="card-desc">{skill.description || 'No description — the agent will never pick this skill.'}</span>
      <span class="card-meta">
        {#if skill.shadowedBy}
          <span class="pill pill-quiet">Replaced by {nameBySlugId.get(skill.shadowedBy) ?? 'your copy'}</span>
        {:else if !skill.enabled}
          <span class="pill pill-quiet">Off</span>
        {/if}
        {#if skill.warnings?.length}
          <span class="pill pill-warn" title={skill.warnings.join('\n')}>Needs attention</span>
        {/if}
        {#if skill.files.length}
          <span class="pill pill-quiet">{skill.files.length} file{skill.files.length === 1 ? '' : 's'}</span>
        {/if}
      </span>
    </button>
    <button
      type="button"
      role="switch"
      class="switch"
      aria-checked={skill.enabled}
      aria-label={`${skill.enabled ? 'Turn off' : 'Turn on'} ${skill.title}`}
      disabled={Boolean(pending[skill.id])}
      onclick={() => void toggle(skill)}
    >
      <span class="switch-knob"></span>
    </button>
  </li>
{/snippet}

<div class="skills-page">
  <header class="head">
    <div class="head-text">
      <h1 class="title">Skills</h1>
      <p class="subtitle">
        Proven procedures your agents load when a request calls for them — in chat and in
        unattended runs. Same format as Claude's skills, so yours travel both ways.
      </p>
    </div>
    <div class="head-actions">
      <button type="button" class="btn btn-ghost" onclick={openImport}>Import</button>
      <button type="button" class="btn btn-primary" onclick={openCreate}>New skill</button>
    </div>
  </header>

  {#if focusEntity}
    <p class="focus-bar">
      Showing what <strong>{entityName(focusEntity)}</strong>'s chat sees — the built-in playbook,
      your workspace skills, and its own. New skills go to this project.
      <a href="/skills">Show all</a>
    </p>
  {/if}

  {#if notice}
    <p class="notice" role="status">{notice}</p>
  {/if}
  {#if rowError}
    <p class="inline-error" role="alert">{rowError}</p>
  {/if}

  {#if loading}
    <p class="muted">Loading skills…</p>
  {:else if loadError}
    <div class="error-card" role="status">
      <p class="error-title">Skills couldn't load.</p>
      <p class="error-detail">{loadError}</p>
      <button type="button" class="btn btn-ghost" onclick={() => void load()}>Try again</button>
    </div>
  {:else}
    <div class="toolbar">
      <div class="segmented" role="tablist" aria-label="Filter skills">
        {#each [['all', 'All'], ['global', 'Workspace'], ['entity', 'Projects'], ['builtin', 'Built-in']] as [value, label] (value)}
          <button
            type="button"
            role="tab"
            aria-selected={filter === value}
            class:is-active={filter === value}
            onclick={() => (filter = value as Filter)}>{label}</button
          >
        {/each}
      </div>
      <input
        class="search"
        type="search"
        placeholder="Search skills"
        aria-label="Search skills"
        bind:value={query}
      />
      <span class="count">{activeCount} active</span>
    </div>

    {#if !visible.length && query.trim()}
      <p class="muted">No skills match “{query.trim()}”.</p>
    {/if}

    {#if (filter === 'all' || filter === 'global') && !query.trim() && !hasCustom}
      <section class="empty">
        <h2 class="empty-title">Teach it how you work</h2>
        <p class="empty-text">
          A skill is a short playbook — how you run a weekly review, price a deal, or brief a
          contractor. Write one here, import the ones you already use with Claude, or just say
          in chat: <em>“save how we did this as a skill.”</em>
        </p>
        <div class="empty-actions">
          <button type="button" class="btn btn-primary" onclick={openCreate}>Write a skill</button>
          <button type="button" class="btn btn-ghost" onclick={openImport}>Import</button>
        </div>
      </section>
    {/if}

    {#if workspaceSkills.length}
      <section class="group">
        <div class="group-head">
          <h2 class="group-title">Your workspace</h2>
          <p class="group-sub">Offered in every venture.</p>
        </div>
        <ul class="grid">
          {#each workspaceSkills as skill (skill.id)}
            {@render card(skill)}
          {/each}
        </ul>
      </section>
    {/if}

    {#each projectGroups as group (group.entityId)}
      <section class="group">
        <div class="group-head">
          <h2 class="group-title">{group.name}</h2>
          <p class="group-sub">Only while this project is active.</p>
        </div>
        <ul class="grid">
          {#each group.skills as skill (skill.id)}
            {@render card(skill)}
          {/each}
        </ul>
      </section>
    {/each}

    {#if builtinSkills.length}
      <section class="group">
        <div class="group-head">
          <h2 class="group-title">The Consistent CEO playbook</h2>
          <p class="group-sub">Built in. Turn any off, or customize your own copy.</p>
        </div>
        <ul class="grid">
          {#each builtinSkills as skill (skill.id)}
            {@render card(skill)}
          {/each}
        </ul>
      </section>
    {/if}
  {/if}
</div>

<!-- ── Detail drawer ── -->
{#if openId}
  <div class="scrim" role="presentation" onclick={closeSkill}></div>
  <div class="drawer" role="dialog" aria-modal="true" aria-labelledby="skill-drawer-title">
    <header class="drawer-head">
      <div class="drawer-head-text">
        <h2 id="skill-drawer-title" class="drawer-title">{detail?.title ?? 'Skill'}</h2>
        {#if detail}
          <p class="drawer-sub">
            <code>{detail.slug}</code> · {scopeLabel(detail)}{#if detail.updatedAt} · updated {formatUpdated(detail.updatedAt)}{/if}
          </p>
        {/if}
      </div>
      <button type="button" class="icon-btn" aria-label="Close" onclick={closeSkill}>✕</button>
    </header>

    <div class="drawer-body">
      {#if detailLoading && !detail}
        <p class="muted">Opening…</p>
      {:else if detail}
        {#if detail.shadowedBy}
          <p class="callout">
            Not in use: “{nameBySlugId.get(detail.shadowedBy) ?? detail.shadowedBy}” has the same name
            and takes precedence.
          </p>
        {/if}
        {#each detail.warnings ?? [] as warning (warning)}
          <p class="callout callout-warn">{warning}</p>
        {/each}

        {#if editing}
          {#if detail.editable}
            <div class="mode-row">
              <button type="button" class="link" onclick={() => (rawMode = !rawMode)}>
                {rawMode ? 'Edit fields' : 'Edit raw SKILL.md'}
              </button>
            </div>
            {#if rawMode}
              <label class="field">
                <span class="field-label">SKILL.md</span>
                <textarea class="mono" rows="24" bind:value={draftRaw} spellcheck="false"></textarea>
              </label>
            {:else}
              <label class="field">
                <span class="field-label">When to use it</span>
                <textarea rows="4" bind:value={draftDescription}></textarea>
                <span class="field-hint">{draftDescription.length}/1024 — the agent decides to load the skill from this.</span>
              </label>
              <label class="field">
                <span class="field-label">Instructions</span>
                <textarea class="mono" rows="20" bind:value={draftBody} spellcheck="false"></textarea>
              </label>
            {/if}
          {/if}
        {:else}
          <section class="drawer-section">
            <h3 class="section-label">When to use it</h3>
            <p class="description">{detail.description || '—'}</p>
          </section>
          <section class="drawer-section">
            <h3 class="section-label">Instructions</h3>
            <div class="markdown">{@html renderSkillMarkdown(detail.body)}</div>
          </section>
          {#if detail.files.length}
            <section class="drawer-section">
              <h3 class="section-label">Supporting files</h3>
              <ul class="file-list">
                {#each detail.files as file (file)}
                  <li><code>{file}</code></li>
                {/each}
              </ul>
            </section>
          {/if}
        {/if}

        {#if detailError}
          <p class="inline-error" role="alert">{detailError}</p>
        {/if}
      {:else if detailError}
        <p class="inline-error" role="alert">{detailError}</p>
      {/if}
    </div>

    {#if detail}
      <footer class="drawer-foot">
        {#if editing}
          <button type="button" class="btn btn-ghost" disabled={saving} onclick={() => (editing = false)}>Cancel</button>
          <button type="button" class="btn btn-primary" disabled={saving} onclick={() => void saveEdit()}>
            {saving ? 'Saving…' : 'Save'}
          </button>
        {:else if confirmDelete}
          <span class="confirm-text">Delete “{detail.title}” for good?</span>
          <button type="button" class="btn btn-ghost" disabled={saving} onclick={() => (confirmDelete = false)}>Keep it</button>
          <button type="button" class="btn btn-danger" disabled={saving} onclick={() => void removeSkill()}>
            {saving ? 'Deleting…' : 'Delete'}
          </button>
        {:else}
          <div class="foot-left">
            <label class="copy-row">
              <span class="sr-only">Copy to</span>
              <select bind:value={copyTarget} disabled={saving}>
                {#if detail.scope !== 'global'}
                  <option value="global">Your workspace</option>
                {/if}
                {#each entities.filter((e) => e.id !== detail?.entityId) as e (e.id)}
                  <option value={e.id}>{e.name}</option>
                {/each}
              </select>
              <button type="button" class="btn btn-ghost" disabled={saving} onclick={() => void copyCurrent()}>
                {detail.scope === 'builtin' ? 'Customize' : 'Copy'}
              </button>
            </label>
          </div>
          {#if detail.editable}
            <button type="button" class="btn btn-ghost btn-danger-text" onclick={() => (confirmDelete = true)}>Delete</button>
            <button type="button" class="btn btn-primary" onclick={startEdit}>Edit</button>
          {/if}
        {/if}
      </footer>
    {/if}
  </div>
{/if}

<!-- ── New skill ── -->
{#if creating}
  <div class="scrim" role="presentation" onclick={() => !createBusy && (creating = false)}></div>
  <div class="modal" role="dialog" aria-modal="true" aria-labelledby="new-skill-title">
    <header class="modal-head">
      <h2 id="new-skill-title" class="modal-title">New skill</h2>
      <button type="button" class="icon-btn" aria-label="Close" disabled={createBusy} onclick={() => (creating = false)}>✕</button>
    </header>
    <form
      class="modal-body"
      onsubmit={(e) => {
        e.preventDefault();
        void submitCreate();
      }}
    >
      <div class="field-row">
        <label class="field grow">
          <span class="field-label">Name</span>
          <!-- svelte-ignore a11y_autofocus -->
          <input type="text" bind:value={newName} placeholder="Weekly pricing review" autofocus />
          <span class="field-hint">
            {#if newName.trim() && !newSlug}
              Use letters a–z or digits — skill names are ASCII, like Claude's.
            {:else if newSlug}
              Saved as <code>{newSlug}</code>
            {:else}
              Lowercase-hyphenated on disk, like Claude's skills.
            {/if}
          </span>
        </label>
        <label class="field">
          <span class="field-label">Available in</span>
          <select bind:value={newTarget}>
            <option value="global">Every venture</option>
            {#each entities as e (e.id)}
              <option value={e.id}>{e.name} only</option>
            {/each}
          </select>
        </label>
      </div>
      <label class="field">
        <span class="field-label">When to use it</span>
        <textarea
          rows="3"
          bind:value={newDescription}
          placeholder="Reviews a venture's pricing against margin targets. Use when the user asks about pricing, discounts, or a quote."
        ></textarea>
        <span class="field-hint" class:is-over={newDescription.length > 1024}>
          {newDescription.length}/1024 — say what it does and the words that should trigger it.
        </span>
      </label>
      <label class="field">
        <span class="field-label">Instructions</span>
        <textarea class="mono" rows="14" bind:value={newBody} spellcheck="false"></textarea>
      </label>
      {#if createError}
        <p class="inline-error" role="alert">{createError}</p>
      {/if}
      <footer class="modal-foot">
        <button type="button" class="btn btn-ghost" disabled={createBusy} onclick={() => (creating = false)}>Cancel</button>
        <button type="submit" class="btn btn-primary" disabled={!canCreate || createBusy}>
          {createBusy ? 'Creating…' : 'Create skill'}
        </button>
      </footer>
    </form>
  </div>
{/if}

<!-- ── Import ── -->
{#if importing}
  <div class="scrim" role="presentation" onclick={() => !importBusy && (importing = false)}></div>
  <div class="modal" role="dialog" aria-modal="true" aria-labelledby="import-title">
    <header class="modal-head">
      <h2 id="import-title" class="modal-title">Import skills</h2>
      <button type="button" class="icon-btn" aria-label="Close" disabled={importBusy} onclick={() => (importing = false)}>✕</button>
    </header>
    <div class="modal-body">
      <div class="segmented" role="tablist" aria-label="Import source">
        {#if localImport}
          <button type="button" role="tab" aria-selected={importTab === 'claude'} class:is-active={importTab === 'claude'} onclick={() => (importTab = 'claude')}>From Claude</button>
        {/if}
        <button type="button" role="tab" aria-selected={importTab === 'upload'} class:is-active={importTab === 'upload'} onclick={() => (importTab = 'upload')}>Upload</button>
        {#if localImport}
          <button type="button" role="tab" aria-selected={importTab === 'path'} class:is-active={importTab === 'path'} onclick={() => (importTab = 'path')}>From a folder</button>
        {/if}
      </div>

      {#if importTab === 'claude'}
        {#if claudeSkills === null}
          <p class="muted">Looking in ~/.claude/skills…</p>
        {:else if !claudeSkills.length}
          <p class="muted">
            {claudeDir ? `No skills in ${claudeDir}.` : 'No ~/.claude/skills folder on this computer.'}
          </p>
        {:else}
          <div class="pick-head">
            <span class="muted">{claudeSkills.length} found in <code>{claudeDir}</code></span>
            <button
              type="button"
              class="link"
              onclick={() => {
                const all = claudeChosen.length < (claudeSkills?.length ?? 0);
                claudeSelected = Object.fromEntries((claudeSkills ?? []).map((s) => [s.path, all]));
              }}>{claudeChosen.length < claudeSkills.length ? 'Select all' : 'Clear'}</button
            >
          </div>
          <ul class="pick-list">
            {#each claudeSkills as s (s.path)}
              <li>
                <label class="pick">
                  <input type="checkbox" bind:checked={claudeSelected[s.path]} />
                  <span class="pick-text">
                    <span class="pick-name">{s.name}</span>
                    <span class="pick-desc">{s.description || 'No description'}</span>
                  </span>
                </label>
              </li>
            {/each}
          </ul>
        {/if}
      {:else if importTab === 'upload'}
        <p class="muted">A <code>SKILL.md</code> file, or a whole skill folder with its supporting files.</p>
        <div class="upload-row">
          <label class="btn btn-ghost file-btn">
            Choose SKILL.md
            <input type="file" accept=".md,text/markdown" onchange={(e) => void onPickFiles(e)} />
          </label>
          <label class="btn btn-ghost file-btn">
            Choose folder
            <input type="file" webkitdirectory multiple onchange={(e) => void onPickFiles(e)} />
          </label>
          {#if uploadLabel}<span class="upload-label">{uploadLabel}</span>{/if}
        </div>
      {:else}
        <label class="field">
          <span class="field-label">Folder on this computer</span>
          <input type="text" bind:value={pathInput} placeholder="~/skills/vendor-review" spellcheck="false" />
          <span class="field-hint">A skill folder, or a folder of skill folders.</span>
        </label>
      {/if}

      <div class="field-row">
        <label class="field grow">
          <span class="field-label">Add to</span>
          <select bind:value={importTarget}>
            <option value="global">Every venture</option>
            {#each entities as e (e.id)}
              <option value={e.id}>{e.name} only</option>
            {/each}
          </select>
        </label>
        <label class="check">
          <input type="checkbox" bind:checked={importOverwrite} />
          Replace skills with the same name
        </label>
      </div>

      {#if importError}
        <p class="inline-error" role="alert">{importError}</p>
      {/if}
      {#if importResult}
        <div class="result">
          {#if importResult.imported.length}
            <p>Imported {importResult.imported.map((s) => s.title).join(', ')}.</p>
          {/if}
          {#each importResult.skipped as skip (skip.source)}
            <p class="result-skip"><code>{skip.source.split('/').pop()}</code> — {skip.reason}</p>
          {/each}
        </div>
      {/if}

      <footer class="modal-foot">
        <button type="button" class="btn btn-ghost" disabled={importBusy} onclick={() => (importing = false)}>
          {importResult ? 'Done' : 'Cancel'}
        </button>
        <button type="button" class="btn btn-primary" disabled={importBusy} onclick={() => void submitImport()}>
          {importBusy ? 'Importing…' : 'Import'}
        </button>
      </footer>
    </div>
  </div>
{/if}

<style>
  .skills-page {
    padding: 16px 14px 48px;
    max-width: 1080px;
    margin: 0 auto;
    display: flex;
    flex-direction: column;
    gap: 18px;
  }

  /* ── Header ── */
  .head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 16px;
    flex-wrap: wrap;
  }
  .head-text {
    max-width: 620px;
  }
  .title {
    margin: 0;
    font-size: 22px;
    font-weight: 800;
    letter-spacing: -0.01em;
    color: var(--text-primary);
  }
  .subtitle {
    margin: 4px 0 0;
    font-size: 13px;
    line-height: 1.5;
    color: var(--text-secondary);
  }
  .head-actions {
    display: flex;
    gap: 8px;
  }

  /* ── Buttons ── */
  .btn {
    min-height: 32px;
    padding: 0 14px;
    border-radius: 999px;
    font-size: 12px;
    font-weight: 700;
    cursor: pointer;
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-primary);
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 6px;
    white-space: nowrap;
    transition: background var(--transition-base), border-color var(--transition-base), opacity var(--transition-base);
  }
  .btn:hover:not(:disabled) {
    background: var(--bg-card-hover);
  }
  .btn:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .btn-primary {
    border-color: transparent;
    background: var(--gradient-primary-button);
    color: #fff7ea;
  }
  .btn-primary:hover:not(:disabled) {
    background: var(--gradient-primary-button);
    filter: brightness(1.08);
  }
  .btn-danger {
    border-color: transparent;
    background: var(--accent-red);
    color: #fff7ea;
  }
  .btn-danger-text {
    color: var(--accent-red);
  }
  .icon-btn {
    width: 30px;
    height: 30px;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
    cursor: pointer;
    font-size: 12px;
    flex-shrink: 0;
  }
  .link {
    background: none;
    border: none;
    padding: 0;
    font-size: 12px;
    font-weight: 600;
    color: var(--accent-cyan);
    cursor: pointer;
  }

  /* ── Feedback ── */
  .muted {
    margin: 0;
    font-size: 13px;
    color: var(--text-muted);
  }
  .focus-bar {
    margin: 0;
    padding: 10px 12px;
    border-radius: 10px;
    border: 1px solid color-mix(in srgb, var(--accent-cyan) 35%, var(--border));
    background: var(--bg-selected);
    font-size: 12px;
    line-height: 1.5;
    color: var(--text-secondary);
  }
  .focus-bar strong {
    color: var(--text-primary);
  }
  .focus-bar a {
    margin-left: 6px;
    font-weight: 700;
    color: var(--accent-cyan);
    text-decoration: none;
  }
  .notice {
    margin: 0;
    padding: 10px 12px;
    border-radius: 10px;
    border: 1px solid color-mix(in srgb, var(--accent-green) 45%, var(--border));
    background: var(--color-green-dim);
    font-size: 12px;
    color: var(--text-primary);
  }
  .inline-error {
    margin: 0;
    padding: 10px 12px;
    border-radius: 10px;
    border: 1px solid color-mix(in srgb, var(--accent-red) 40%, var(--border));
    background: var(--color-red-dim);
    font-size: 12px;
    color: var(--text-primary);
    overflow-wrap: anywhere;
  }
  .error-card {
    border: 1px solid color-mix(in srgb, var(--accent-red) 40%, var(--border));
    background: var(--color-red-dim);
    border-radius: 12px;
    padding: 14px;
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 8px;
  }
  .error-title {
    margin: 0;
    font-size: 13px;
    font-weight: 700;
    color: var(--text-primary);
  }
  .error-detail {
    margin: 0;
    font-size: 12px;
    color: var(--text-secondary);
  }

  /* ── Toolbar ── */
  .toolbar {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
  }
  .segmented {
    display: inline-flex;
    padding: 3px;
    gap: 2px;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: var(--bg-secondary);
  }
  .segmented button {
    min-height: 26px;
    padding: 0 12px;
    border: none;
    border-radius: 999px;
    background: transparent;
    color: var(--text-secondary);
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
  }
  .segmented button.is-active {
    background: var(--bg-selected);
    color: var(--text-primary);
  }
  .search {
    flex: 1;
    min-width: 160px;
    max-width: 320px;
    min-height: 32px;
    padding: 0 12px;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: var(--bg-secondary);
    color: var(--text-primary);
    font-size: 12px;
  }
  .search:focus,
  input:focus,
  textarea:focus,
  select:focus {
    outline: none;
    border-color: var(--border-focus);
  }
  .count {
    margin-left: auto;
    font-size: 12px;
    color: var(--text-muted);
  }

  /* ── Empty state ── */
  .empty {
    border: 1px dashed color-mix(in srgb, var(--accent-cyan) 35%, var(--border));
    border-radius: 14px;
    padding: 20px;
    background: color-mix(in srgb, var(--accent-cyan) 5%, transparent);
  }
  .empty-title {
    margin: 0;
    font-size: 15px;
    font-weight: 700;
    color: var(--text-primary);
  }
  .empty-text {
    margin: 6px 0 14px;
    max-width: 640px;
    font-size: 13px;
    line-height: 1.55;
    color: var(--text-secondary);
  }
  .empty-actions {
    display: flex;
    gap: 8px;
  }

  /* ── Groups + cards ── */
  .group {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .group-head {
    display: flex;
    align-items: baseline;
    gap: 10px;
    flex-wrap: wrap;
  }
  .group-title {
    margin: 0;
    font-size: 14px;
    font-weight: 700;
    color: var(--text-primary);
  }
  .group-sub {
    margin: 0;
    font-size: 12px;
    color: var(--text-muted);
  }
  .grid {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(290px, 1fr));
    gap: 10px;
  }
  .card {
    position: relative;
    display: flex;
    border: 1px solid var(--border);
    border-radius: 12px;
    background: var(--bg-card);
    transition: border-color var(--transition-base), background var(--transition-base);
  }
  .card:hover {
    border-color: color-mix(in srgb, var(--accent-cyan) 35%, var(--border));
    background: var(--bg-card-hover);
  }
  .card.is-off {
    opacity: 0.62;
  }
  .card-open {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 4px;
    padding: 14px 8px 14px 14px;
    border: none;
    background: none;
    text-align: left;
    color: inherit;
    cursor: pointer;
    border-radius: 12px;
  }
  .card-title {
    font-size: 13px;
    font-weight: 700;
    color: var(--text-primary);
  }
  .card-slug {
    font-family: var(--font-mono);
    font-size: 11px;
    color: var(--text-muted);
  }
  .card-desc {
    margin-top: 4px;
    font-size: 12px;
    line-height: 1.5;
    color: var(--text-secondary);
    display: -webkit-box;
    -webkit-line-clamp: 3;
    line-clamp: 3;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
  .card-meta {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin-top: 6px;
  }
  .card-meta:empty {
    display: none;
  }
  .pill {
    font-size: 10px;
    font-weight: 700;
    letter-spacing: 0.02em;
    padding: 2px 8px;
    border-radius: 999px;
  }
  .pill-quiet {
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    color: var(--text-muted);
  }
  .pill-warn {
    background: var(--color-orange-dim);
    color: var(--accent-orange);
  }

  /* ── Switch ── */
  .switch {
    align-self: flex-start;
    margin: 14px 14px 0 0;
    width: 34px;
    height: 20px;
    flex-shrink: 0;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: var(--bg-secondary);
    position: relative;
    cursor: pointer;
    padding: 0;
    transition: background var(--transition-base), border-color var(--transition-base);
  }
  .switch-knob {
    position: absolute;
    top: 2px;
    left: 2px;
    width: 14px;
    height: 14px;
    border-radius: 999px;
    background: var(--text-muted);
    transition: transform var(--duration-slow) var(--ease-out), background var(--transition-base);
  }
  .switch[aria-checked='true'] {
    background: color-mix(in srgb, var(--accent-cyan) 70%, transparent);
    border-color: transparent;
  }
  .switch[aria-checked='true'] .switch-knob {
    transform: translateX(14px);
    background: #fff7ea;
  }
  .switch:disabled {
    opacity: 0.5;
    cursor: default;
  }

  /* ── Overlays ── */
  .scrim {
    position: fixed;
    inset: 0;
    background: var(--color-bg-overlay);
    z-index: 60;
    animation: fade var(--duration-slow) var(--ease-out);
  }
  .drawer {
    position: fixed;
    top: 0;
    right: 0;
    bottom: 0;
    width: min(620px, 100vw);
    z-index: 61;
    display: flex;
    flex-direction: column;
    background: var(--bg-secondary);
    border-left: 1px solid var(--border);
    box-shadow: var(--shadow-xl);
    animation: slide-in var(--duration-slow) var(--ease-out);
  }
  .drawer-head,
  .modal-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
    padding: 18px 18px 12px;
    border-bottom: 1px solid var(--border);
  }
  .drawer-title,
  .modal-title {
    margin: 0;
    font-size: 17px;
    font-weight: 800;
    color: var(--text-primary);
  }
  .drawer-sub {
    margin: 4px 0 0;
    font-size: 12px;
    color: var(--text-muted);
  }
  .drawer-sub code {
    font-family: var(--font-mono);
  }
  .drawer-body {
    flex: 1;
    overflow-y: auto;
    padding: 16px 18px;
    display: flex;
    flex-direction: column;
    gap: 16px;
  }
  .drawer-section {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .section-label {
    margin: 0;
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--text-muted);
  }
  .description {
    margin: 0;
    font-size: 13px;
    line-height: 1.55;
    color: var(--text-primary);
  }
  .callout {
    margin: 0;
    padding: 10px 12px;
    border-radius: 10px;
    border: 1px solid var(--border);
    background: var(--bg-card);
    font-size: 12px;
    color: var(--text-secondary);
  }
  .callout-warn {
    border-color: color-mix(in srgb, var(--accent-orange) 40%, var(--border));
    background: var(--color-orange-dim);
    color: var(--text-primary);
  }
  .file-list {
    margin: 0;
    padding-left: 18px;
    font-size: 12px;
    color: var(--text-secondary);
  }
  .file-list code {
    font-family: var(--font-mono);
  }
  .drawer-foot,
  .modal-foot {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 8px;
    padding: 12px 18px;
    border-top: 1px solid var(--border);
    flex-wrap: wrap;
  }
  .modal-foot {
    padding: 12px 0 0;
  }
  .foot-left {
    margin-right: auto;
  }
  .copy-row {
    display: flex;
    gap: 6px;
    align-items: center;
  }
  .confirm-text {
    margin-right: auto;
    font-size: 12px;
    color: var(--text-secondary);
  }
  .mode-row {
    display: flex;
    justify-content: flex-end;
  }

  .modal {
    position: fixed;
    left: 50%;
    top: 6vh;
    transform: translateX(-50%);
    width: min(720px, calc(100vw - 24px));
    max-height: 88vh;
    z-index: 61;
    display: flex;
    flex-direction: column;
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 16px;
    box-shadow: var(--shadow-xl);
    animation: rise var(--duration-slow) var(--ease-out);
  }
  .modal-body {
    padding: 16px 18px 16px;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  /* ── Fields ── */
  .field {
    display: flex;
    flex-direction: column;
    gap: 6px;
    min-width: 0;
  }
  .field.grow {
    flex: 1;
  }
  .field-row {
    display: flex;
    gap: 12px;
    align-items: flex-end;
    flex-wrap: wrap;
  }
  .field-label {
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    color: var(--text-muted);
  }
  .field-hint {
    font-size: 11px;
    color: var(--text-muted);
  }
  .field-hint.is-over {
    color: var(--accent-red);
  }
  .field-hint code {
    font-family: var(--font-mono);
    color: var(--text-secondary);
  }
  input[type='text'],
  textarea,
  select {
    width: 100%;
    padding: 9px 11px;
    border-radius: 10px;
    border: 1px solid var(--border);
    background: var(--bg-card);
    color: var(--text-primary);
    font-size: 13px;
    font-family: var(--font-sans);
    line-height: 1.5;
  }
  select {
    width: auto;
    min-height: 32px;
    padding: 0 10px;
    font-size: 12px;
  }
  textarea {
    resize: vertical;
  }
  textarea.mono {
    font-family: var(--font-mono);
    font-size: 12px;
    line-height: 1.6;
  }
  .check {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    font-size: 12px;
    color: var(--text-secondary);
    min-height: 32px;
  }

  /* ── Import ── */
  .pick-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 8px;
  }
  .pick-head code {
    font-family: var(--font-mono);
    font-size: 11px;
  }
  .pick-list {
    list-style: none;
    margin: 0;
    padding: 0;
    max-height: 300px;
    overflow-y: auto;
    border: 1px solid var(--border);
    border-radius: 12px;
  }
  .pick-list li + li {
    border-top: 1px solid var(--border);
  }
  .pick {
    display: flex;
    gap: 10px;
    padding: 10px 12px;
    cursor: pointer;
    align-items: flex-start;
  }
  .pick input {
    margin-top: 3px;
  }
  .pick-text {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .pick-name {
    font-size: 13px;
    font-weight: 700;
    color: var(--text-primary);
  }
  .pick-desc {
    font-size: 12px;
    color: var(--text-secondary);
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
  .upload-row {
    display: flex;
    gap: 8px;
    align-items: center;
    flex-wrap: wrap;
  }
  .file-btn {
    position: relative;
    overflow: hidden;
  }
  .file-btn input {
    position: absolute;
    inset: 0;
    opacity: 0;
    cursor: pointer;
  }
  .upload-label {
    font-size: 12px;
    color: var(--text-secondary);
  }
  .result {
    padding: 10px 12px;
    border-radius: 10px;
    border: 1px solid var(--border);
    background: var(--bg-card);
    font-size: 12px;
    color: var(--text-primary);
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .result p {
    margin: 0;
  }
  .result-skip {
    color: var(--text-secondary);
  }
  .result code {
    font-family: var(--font-mono);
  }

  /* ── Rendered instructions ── */
  .markdown {
    font-size: 13px;
    line-height: 1.6;
    color: var(--text-primary);
  }
  .markdown :global(h2),
  .markdown :global(h3),
  .markdown :global(h4),
  .markdown :global(h5) {
    margin: 16px 0 6px;
    font-size: 13px;
    font-weight: 700;
    color: var(--text-primary);
  }
  .markdown :global(h2) {
    font-size: 15px;
  }
  .markdown :global(p) {
    margin: 0 0 10px;
    color: var(--text-secondary);
  }
  .markdown :global(ul),
  .markdown :global(ol) {
    margin: 0 0 10px;
    padding-left: 20px;
    color: var(--text-secondary);
  }
  .markdown :global(li) {
    margin: 3px 0;
  }
  .markdown :global(code) {
    font-family: var(--font-mono);
    font-size: 12px;
    padding: 1px 5px;
    border-radius: 5px;
    background: var(--bg-card);
    color: var(--text-primary);
  }
  .markdown :global(pre) {
    margin: 0 0 12px;
    padding: 12px;
    border-radius: 10px;
    border: 1px solid var(--border);
    background: var(--bg-card);
    overflow-x: auto;
  }
  .markdown :global(pre code) {
    padding: 0;
    background: none;
    white-space: pre;
  }
  .markdown :global(blockquote) {
    margin: 0 0 10px;
    padding-left: 12px;
    border-left: 2px solid var(--accent-cyan);
    color: var(--text-secondary);
  }

  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
    white-space: nowrap;
  }

  @keyframes fade {
    from {
      opacity: 0;
    }
  }
  @keyframes slide-in {
    from {
      transform: translateX(24px);
      opacity: 0;
    }
  }
  @keyframes rise {
    from {
      transform: translate(-50%, 12px);
      opacity: 0;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .scrim,
    .drawer,
    .modal,
    .switch-knob {
      animation: none;
      transition: none;
    }
  }
  @media (max-width: 640px) {
    .head-actions {
      width: 100%;
    }
    .head-actions .btn {
      flex: 1;
    }
    .count {
      margin-left: 0;
    }
  }
</style>
