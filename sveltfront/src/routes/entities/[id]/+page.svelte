<script lang="ts">
  import { goto, invalidate, invalidateAll } from '$app/navigation';
  import { onDestroy, onMount } from 'svelte';
  import { page } from '$app/stores';
  import EntityImportModal from '$lib/components/EntityImportModal.svelte';
  import EntityKnowledgeCard from '$lib/components/EntityKnowledgeCard.svelte';
  import {
    applyEnrichment,
    createEntityConnection,
    createEntityAgentRun,
    createEntityConnectionSession,
    createMission,
    createScheduledTask,
    createTask,
    deleteEntity,
    deleteEntityConnection,
    deleteScheduledTask,
    discardEnrichment,
    finalizeEntityConnectionSession,
    getEntityAgentRun,
    getEntityWorkspace,
    setEntityWorkspace,
    listActivity,
    listCaptures,
    listEntityFiles,
    dismissCapture,
    promoteCaptureToTask,
    promoteCaptureToMemory,
    updateCaptureNarrative,
    patchApproval,
    patchEntity,
    regenerateEnrichment,
    testEntityConnection,
    updateEntityConnection,
    updateMission,
    uploadEntityFile,
    verifyEntityConnection,
    apiErrorStatus
  } from '$lib/api';
  import type {
    ActivityItem,
    EntityConnectionSession,
    EntityWorkspaceStatus,
    RawCapture,
    WorkspaceDecisionInput
  } from '$lib/api';
  import { deriveMissionExecutionSummary } from '$lib/missionExecution';
  import Seo from '$lib/components/Seo.svelte';
  import type {
    AgentRoleLite,
    Entity,
    EntityConnection,
    EntityConnectionEvent,
    EntityFile,
    EntityMemoryPreview,
    EntityRecord,
    Mission,
    MissionPolicy,
    OperatorFeedEvent,
    PendingApprovalRow,
    ScheduledTaskRow,
    Task
  } from './+page';
  import { _ENTITY_DEPENDENCY } from './+page';
  import { feedKindLabel } from '$lib/feedKinds';
  import { safeHttpUrl } from '$lib/safeUrl';

  let { data } = $props<{
    data: {
      entity: Entity | null;
      tasks: Task[];
      missions: Mission[];
      missionPolicy: MissionPolicy | null;
      connections: EntityConnection[];
      connectionEvents: EntityConnectionEvent[];
      records: EntityRecord[];
      operatorFeed: OperatorFeedEvent[];
      pendingApprovals: PendingApprovalRow[];
      memoryPreview: EntityMemoryPreview;
      scheduleTasks: ScheduledTaskRow[];
      agents: AgentRoleLite[];
      error: string | null;
    };
  }>();

  const entity = $derived(data.entity);

  // Danger zone: two-step inline confirm, then back to the entity list.
  let confirmingDelete = $state(false);
  let deletingEntity = $state(false);
  let entityDeleteError = $state('');

  async function deleteThisEntity() {
    if (!entity || deletingEntity) return;
    deletingEntity = true;
    entityDeleteError = '';
    try {
      await deleteEntity(entity.id);
      await goto('/entities');
    } catch (err) {
      entityDeleteError = err instanceof Error ? err.message : 'Delete failed';
      deletingEntity = false;
    }
  }

  // "Auto-fill from sources": run the import engine against this entity —
  // observe its folder/repo/site, review the draft, apply. See EntityImportModal.
  let showAutoFillModal = $state(false);

  async function autoFillDone() {
    showAutoFillModal = false;
    try {
      await invalidate(_ENTITY_DEPENDENCY);
    } catch (_) {
      // transient dev-server reloads
    }
    void loadEntityActivity();
  }
  const tasks = $derived<Task[]>(data.tasks ?? []);
  const missions = $derived<Mission[]>(data.missions ?? []);
  const missionPolicy = $derived(data.missionPolicy);
  const connections = $derived<EntityConnection[]>(data.connections ?? []);
  const connectionEvents = $derived<EntityConnectionEvent[]>(data.connectionEvents ?? []);
  const entityRecords = $derived<EntityRecord[]>(data.records ?? []);
  // Grouped by kind, most-recently-fetched kind group first — the same shape
  // the ledger doc describes: "42 customers, 7 open leads, 3 content ideas".
  const recordGroups = $derived.by(() => {
    const byKind = new Map<string, EntityRecord[]>();
    for (const r of entityRecords) {
      const bucket = byKind.get(r.kind);
      if (bucket) bucket.push(r);
      else byKind.set(r.kind, [r]);
    }
    return [...byKind.entries()]
      .map(([kind, items]) => ({ kind, items }))
      .sort((a, b) => (b.items[0]?.fetchedAt ?? '').localeCompare(a.items[0]?.fetchedAt ?? ''));
  });

  function recordFieldsPreview(fields: Record<string, unknown>): string {
    const entries = Object.entries(fields ?? {}).slice(0, 4);
    if (entries.length === 0) return '(no fields)';
    return entries
      .map(([k, v]) => `${k}: ${typeof v === 'object' ? JSON.stringify(v) : String(v)}`)
      .join(' · ');
  }
  const operatorFeed = $derived<OperatorFeedEvent[]>(data.operatorFeed ?? []);
  const pendingApprovals = $derived<PendingApprovalRow[]>(data.pendingApprovals ?? []);
  const memoryPreview = $derived<EntityMemoryPreview>(data.memoryPreview ?? { shortTerm: [], longTerm: [] });
  const scheduleTasks = $derived<ScheduledTaskRow[]>(data.scheduleTasks ?? []);
  const scheduleForEntity = $derived(
    entity ? scheduleTasks.filter((row) => row.entityId === entity.id) : []
  );
  const availableAgents = $derived<AgentRoleLite[]>(data.agents ?? []);

  let scheduleForm = $state({
    cronExpression: '0 9 * * * *',
    agentId: '',
    taskDescription: '',
    saving: false,
    error: null as string | null
  });

  $effect(() => {
    if (!scheduleForm.agentId && availableAgents.length > 0) {
      scheduleForm.agentId = availableAgents[0].id;
    }
  });

  async function submitScheduledTask(event: Event) {
    event.preventDefault();
    if (!entity) return;
    const cron = scheduleForm.cronExpression.trim();
    const desc = scheduleForm.taskDescription.trim();
    const agent = scheduleForm.agentId.trim();
    if (!cron || !desc || !agent) {
      scheduleForm.error = 'Cron, agent, and task description are required.';
      return;
    }
    scheduleForm.saving = true;
    scheduleForm.error = null;
    try {
      await createScheduledTask({
        cronExpression: cron,
        agentId: agent,
        taskDescription: desc,
        entityId: entity.id
      });
      scheduleForm.taskDescription = '';
      await invalidateAll();
    } catch (err) {
      scheduleForm.error = err instanceof Error ? err.message : 'Failed to create scheduled task';
    } finally {
      scheduleForm.saving = false;
    }
  }

  async function removeScheduledTask(id: string) {
    try {
      await deleteScheduledTask(id);
      await invalidateAll();
    } catch (err) {
      console.error('Failed to delete scheduled task', err);
    }
  }
  const error = $derived(data.error);
  const enrichment = $derived(entity?.enrichment);
  const enrichmentStatus = $derived(enrichment?.status ?? '');
  const isDrafting = $derived(enrichmentStatus === 'pending' || enrichmentStatus === 'running');
  const isReady = $derived(enrichmentStatus === 'ready');
  const isFailed = $derived(enrichmentStatus === 'failed');
  const isProviderInvalid = $derived(enrichmentStatus === 'provider_invalid');
  const isAiUnavailable = $derived(enrichmentStatus === 'ai_unavailable');
  const needsEnrichmentRecovery = $derived(isFailed || isProviderInvalid || isAiUnavailable);
  const draft = $derived(enrichment?.draft);
  let draftSelection = $state<{ fields: string[] }>({ fields: [] });
  const selectedDraftFields = $derived(draftSelection.fields);
  const enrichmentTasks = $derived(tasks.filter((task) => task.entity_id === entity?.id && task.task_kind === 'entity-enrichment'));
  const currentEnrichmentTask = $derived(enrichmentTasks.find((task) => task.status === 'in-progress' || task.status === 'queued') ?? enrichmentTasks[0]);
  const connectionHealth = $derived.by(() => {
    const list = connections;
    return {
      total: list.length,
      ok: list.filter((c) => c.status === 'connected' || c.status === 'limited').length,
      reauth: list.filter((c) => c.status === 'reauth_required').length,
      withError: list.filter((c) => !!c.lastError).length
    };
  });
  const connectionsByGroup = $derived.by(() => {
    const list = connections;
    const k = (c: EntityConnection) => c.kind;
    return {
      social: list.filter((c) => k(c) === 'social_account'),
      email: list.filter((c) => k(c) === 'email_account'),
      domain: list.filter((c) => k(c) === 'domain'),
      storageWeb: list.filter((c) => k(c) === 'storage' || k(c) === 'website'),
      other: list.filter(
        (c) => !['social_account', 'email_account', 'domain', 'storage', 'website'].includes(k(c))
      )
    };
  });

  const connectionGroups = $derived.by(() => {
    const g = connectionsByGroup;
    return [
      { title: 'Social', items: g.social },
      { title: 'Email', items: g.email },
      { title: 'Domains', items: g.domain },
      { title: 'Storage / web', items: g.storageWeb },
      { title: 'Dev & other', items: g.other }
    ].filter((x) => x.items.length > 0);
  });

  type EntityTab =
    | 'command'
    | 'missions'
    | 'connections'
    | 'records'
    | 'narrative'
    | 'strategy'
    | 'memory'
    | 'schedule'
    | 'activity'
    | 'policy';

  let entityTab = $state<EntityTab>('command');
  const requestedTab = $derived($page.url.searchParams.get('tab'));

  let narrativeEditing = $state<string | null>(null);
  let narrativeDraft = $state('');
  let narrativeSaving = $state(false);
  /** Last failed save of a narrative or setup field, shown under the editor. */
  let fieldSaveError = $state<string | null>(null);

  function startNarrativeEdit(field: string, currentValue: string | undefined | null) {
    narrativeEditing = field;
    narrativeDraft = currentValue ?? '';
    fieldSaveError = null;
  }

  function cancelNarrativeEdit() {
    narrativeEditing = null;
    narrativeDraft = '';
    fieldSaveError = null;
  }

  const NARRATIVE_FIELDS = [
    'profile',
    'goals',
    'activeContext',
    'operatingRules',
    'notes',
    'contentPipeline'
  ] as const;

  async function saveNarrativeEdit() {
    if (!entity || !narrativeEditing || narrativeSaving) return;
    const field = NARRATIVE_FIELDS.find((f) => f === narrativeEditing);
    if (!field) return;
    narrativeSaving = true;
    fieldSaveError = null;
    try {
      // An emptied field is sent as "" — the gateway reads that as "clear".
      // `null` means "leave unchanged", which is why clearing used to do nothing.
      await patchEntity(entity.id, { [field]: narrativeDraft.trim() });
      await invalidateAll();
      narrativeEditing = null;
      narrativeDraft = '';
    } catch (err) {
      fieldSaveError = err instanceof Error ? err.message : 'Could not save that field.';
    } finally {
      narrativeSaving = false;
    }
  }

  const entityTabOptions: { id: EntityTab; label: string; hint?: string }[] = [
    { id: 'command', label: 'Conversation', hint: 'Setup & next actions' },
    { id: 'missions', label: 'Missions', hint: 'Create work for this entity' },
    { id: 'connections', label: 'Connections', hint: 'Integrations' },
    { id: 'records', label: 'Records', hint: 'The ledger — customers, deals, content' },
    { id: 'narrative', label: 'Narrative & resources', hint: 'Story, files, links' },
    { id: 'strategy', label: 'Strategy flow', hint: 'Mirimai ID · previous / current / future' },
    { id: 'memory', label: 'Memory', hint: 'Short & long-term' },
    { id: 'schedule', label: 'Schedule', hint: 'Cron jobs' },
    { id: 'activity', label: 'Activity', hint: 'Feed' },
    { id: 'policy', label: 'Policy', hint: 'Capabilities & workspace' }
  ];

  $effect(() => {
    const tab = requestedTab;
    if (!tab) return;
    const allowed: EntityTab[] = ['command', 'missions', 'connections', 'records', 'narrative', 'strategy', 'memory', 'schedule', 'activity', 'policy'];
    if (allowed.includes(tab as EntityTab)) {
      entityTab = tab as EntityTab;
    }
  });

  function formatShortTime(iso: string): string {
    try {
      return new Date(iso).toLocaleString(undefined, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
    } catch {
      return iso;
    }
  }

  function connectionNeedsReauth(lastError: string | undefined): boolean {
    if (!lastError) return false;
    const t = lastError.toLowerCase();
    return t.includes('reauth') || t.includes('authentication failed') || t.includes('401') || t.includes('unauthorized');
  }
  const missionCards = $derived(
    missions.map((mission) => {
      const missionTasks = tasks.filter((task) => task.mission_id === mission.id);
      const missionEvents = operatorFeed.filter((event) => event.missionId === mission.id);
      return {
        mission,
        total: missionTasks.length,
        queued: missionTasks.filter((task) => task.status === 'queued' || task.status === 'backlog').length,
        in_progress: missionTasks.filter((task) => task.status === 'in-progress').length,
        blocked: missionTasks.filter((task) => task.status === 'failed').length,
        completed: missionTasks.filter((task) => task.status === 'completed').length,
        execution: deriveMissionExecutionSummary(missionTasks, missionEvents)
      };
    })
  );
  let missionStatusFilter = $state<'all' | 'planned' | 'active' | 'blocked' | 'user-input-needed' | 'completed'>('all');
  const missionCardsVisible = $derived(
    missionCards
      .filter((card) => (missionStatusFilter === 'all' ? true : card.mission.status === missionStatusFilter))
      .slice()
      .sort((a, b) => {
        const aAttention = (a.blocked > 0 ? 1000 : 0) + (a.queued + a.in_progress === 0 ? 500 : 0);
        const bAttention = (b.blocked > 0 ? 1000 : 0) + (b.queued + b.in_progress === 0 ? 500 : 0);
        if (aAttention !== bAttention) return bAttention - aAttention;
        const priorityScore = (p: string) => ({ critical: 4, high: 3, medium: 2, low: 1 }[p] ?? 0);
        return priorityScore(b.mission.priority) - priorityScore(a.mission.priority);
      })
  );
  const githubDomain = $derived(
    entity?.domains?.find((domain: string) => domain.toLowerCase().includes('github.com')) ?? null
  );
  const entityLocalWorkspace = $derived.by(() => {
    if (!entity) return [] as { label: string; value: string; hint: string }[];
    const rows: { label: string; value: string; hint: string }[] = [];
    const pushIf = (label: string, value: string | undefined, hint: string) => {
      if (!value || !String(value).trim()) return;
      rows.push({ label, value: String(value).trim(), hint });
    };
    pushIf('Primary local root', entity.localPath, 'Main folder Consistent CEO should treat as this entity’s working home');
    pushIf('Repo / project path', entity.setup?.repoPath, 'Project codebase or working directory for directors and implementation tasks');
    pushIf('Workspace root', entity.defaultWorkspaceRoot, 'Default workspace scope currently assigned to this entity');
    pushIf('Imported file path', entity.path, 'Current file-backed path already attached to this entity');
    return rows;
  });
  const connectedChannels = $derived(
    (entity?.relationships ?? []).filter((rel: { type?: string }) => {
      const type = rel.type?.toLowerCase() ?? '';
      return type.includes('channel') || type.includes('slack') || type.includes('discord') || type.includes('telegram');
    })
  );
  let applying = $state(false);
  let discarding = $state(false);
  let regenerating = $state(false);
  let creatingMission = $state(false);
  let missionError = $state<string | null>(null);
  let connectionBusyId = $state<string | null>(null);
  let workspaceDraft = $state({
    localPath: '',
    repoPath: ''
  });
  let workspaceSaving = $state(false);
  let workspaceNotice = $state<string | null>(null);
  // "Does this thing have a repo?" — the explicit answer, not a guess.
  let workspaceStatus = $state<EntityWorkspaceStatus | null>(null);
  let workspaceStatusError = $state<string | null>(null);
  /** The choice currently being submitted, so only its own button spins. */
  let workspaceChoiceBusy = $state<string | null>(null);
  let workspacePathDraft = $state('');
  let workspaceGitInit = $state(true);
  let agentRunForm = $state({
    kind: 'claude' as 'claude' | 'opencode' | 'cursor-agent',
    prompt: '',
    cwd: '',
    reviewPolicy: 'none' as 'none' | 'manual' | 'auto'
  });
  let agentRunBusy = $state(false);
  let agentRunNotice = $state<string | null>(null);
  let agentRunOutput = $state<string | null>(null);
  let agentRunReview = $state<string | null>(null);

  function reviewIndicatorLabel(status?: string): string | null {
    switch (status) {
      case 'reviewed':
        return 'Reviewed ✓';
      case 'needs_review':
        return 'Needs review';
      case 'pending_review':
        return 'Pending review';
      case 'review_running':
        return 'Review running…';
      default:
        return null;
    }
  }

  const onboardingQuestions: { key: keyof NonNullable<Entity["setup"]>; label: string; prompt: string }[] = [
    { key: 'whatItIs', label: 'What is it?', prompt: 'What is this entity about?' },
    { key: 'primaryGoal', label: 'Goal', prompt: 'What is the main goal for this entity?' },
    { key: 'successCriteria', label: 'Success', prompt: 'What does success look like?' },
    { key: 'primaryDomain', label: 'Domain', prompt: 'Does it have a primary domain?' },
    { key: 'primaryEmail', label: 'Email', prompt: 'Does it have a primary email?' },
    { key: 'repoPath', label: 'Repo / path', prompt: 'Where is the codebase or working directory?' },
    { key: 'backendStack', label: 'Backend', prompt: 'What backend stack does it use?' },
    { key: 'frontendStack', label: 'Frontend', prompt: 'What frontend stack does it use?' },
    { key: 'databaseStack', label: 'Database', prompt: 'What database does it use?' }
  ];

  let onboardingSaving = $state(false);
  const needsSetupEntity = $derived((entity?.readiness?.percent ?? 0) < 60);

  let editingSetupField = $state<string | null>(null);
  let editingSetupValue = $state('');

  const filledSetupCount = $derived(
    entity ? onboardingQuestions.filter(q => {
      const v = (entity.setup ?? {})[q.key];
      return v && String(v).trim() && String(v).trim() !== 'unknown';
    }).length : 0
  );

  const totalOnboardFields = $derived(onboardingQuestions.length + 2);
  const filledNarrativeCount = $derived(
    (entity?.profile ? 1 : 0) + (entity?.goals ? 1 : 0)
  );
  const totalFilledCount = $derived(filledSetupCount + filledNarrativeCount);

  function startSetupEdit(key: string, currentValue: string | undefined | null) {
    editingSetupField = key;
    editingSetupValue = currentValue ?? '';
  }

  function cancelSetupEdit() {
    editingSetupField = null;
    editingSetupValue = '';
  }

  async function saveSetupEdit() {
    if (!entity || !editingSetupField || !editingSetupValue.trim() || onboardingSaving) return;
    onboardingSaving = true;
    fieldSaveError = null;
    try {
      await patchEntity(entity.id, {
        setup: {
          ...(entity.setup ?? {}),
          [editingSetupField]: editingSetupValue.trim()
        }
      });
      editingSetupField = null;
      editingSetupValue = '';
      await invalidateAll();
    } catch (err) {
      fieldSaveError = err instanceof Error ? err.message : 'Could not save that field.';
    } finally {
      onboardingSaving = false;
    }
  }

  async function skipSetupField() {
    if (!entity || !editingSetupField || onboardingSaving) return;
    onboardingSaving = true;
    fieldSaveError = null;
    try {
      await patchEntity(entity.id, {
        setup: {
          ...(entity.setup ?? {}),
          [editingSetupField]: 'unknown'
        }
      });
      editingSetupField = null;
      editingSetupValue = '';
      await invalidateAll();
    } catch (err) {
      fieldSaveError = err instanceof Error ? err.message : 'Could not save that field.';
    } finally {
      onboardingSaving = false;
    }
  }

  let connectionError = $state<string | null>(null);
  let connectionNotice = $state<string | null>(null);
  /** One connection-creating request at a time (form, quick-connect buttons). */
  let connectionCreating = $state(false);
  let entityFiles = $state<EntityFile[]>([]);
  let fileUploadBusy = $state(false);
  let entityCaptures = $state<RawCapture[]>([]);
  let captureBusyId = $state<string | null>(null);
  let captureError = $state<string | null>(null);
  let entityActivity = $state<ActivityItem[]>([]);
  let fileUploadError = $state<string | null>(null);
  let connectionForm = $state({
    kind: 'social_account',
    provider: 'twitter',
    label: '',
    domain: '',
    status: 'disconnected'
  });
  let githubAuthSession = $state<EntityConnectionSession | null>(null);
  let githubAuthBusy = $state(false);
  let githubCompleteBusy = $state(false);
  let missionForm = $state({
    brief: ''
  });

  type AutomationMode = 'manual' | 'suggest' | 'auto_run';
  type HeartbeatMode = 'off' | 'suggest' | 'execute';
  // An entity with no stored runtime behaves as auto-run + execute in the
  // gateway (`entity_runtime::effective_*_mode`), so that is what we show.
  let autoMode = $state<AutomationMode>('auto_run');
  let hbMode = $state<HeartbeatMode>('execute');
  let runtimeSaving = $state(false);
  let runtimeNotice = $state<string | null>(null);
  let approvalBusyId = $state<string | null>(null);
  let approvalError = $state<string | null>(null);
  let lastSyncedEntityId = $state<string | null>(null);
  // The workspace draft re-syncs whenever the *saved* values change (a save,
  // a workspace choice, an auto-fill), not only when the entity id does — a
  // stale draft used to send `localPath: null` and wipe a folder the operator
  // had just chosen.
  // Plain (non-reactive) bookkeeping: what the drafts were last synced from.
  let syncedWorkspaceKey: string | null = null;
  let syncedRuntimeKey: string | null = null;

  $effect(() => {
    if (!entity) return;
    if (lastSyncedEntityId === entity.id) return;
    lastSyncedEntityId = entity.id;
    // Same component, different entity (client-side nav): stop the old poll.
    agentRunPollGeneration += 1;
    agentRunOutput = null;
    agentRunNotice = null;
    agentRunReview = null;
    agentRunForm = {
      kind: 'claude',
      prompt: '',
      cwd: entity.setup?.repoPath ?? entity.localPath ?? '',
      reviewPolicy: 'none'
    };
    workspaceNotice = null;
    workspacePathDraft = '';
    runtimeNotice = null;
    void loadWorkspaceStatus();
  });

  // Re-sync only when the saved values change, so a background refresh (the
  // enrichment poll) never throws away a selection the operator hasn't saved.
  $effect(() => {
    if (!entity) return;
    const savedAuto: AutomationMode = entity.entityRuntime?.automationMode ?? 'auto_run';
    const savedHb: HeartbeatMode = entity.entityRuntime?.heartbeatMode ?? 'execute';
    const key = `${entity.id}|${savedAuto}|${savedHb}`;
    if (key === syncedRuntimeKey) return;
    syncedRuntimeKey = key;
    autoMode = savedAuto;
    hbMode = savedHb;
  });

  $effect(() => {
    if (!entity) return;
    const savedLocal: string = entity.localPath ?? '';
    const savedRepo: string = entity.setup?.repoPath ?? '';
    const key = `${entity.id}|${savedLocal}|${savedRepo}`;
    if (key === syncedWorkspaceKey) return;
    syncedWorkspaceKey = key;
    workspaceDraft = { localPath: savedLocal, repoPath: savedRepo };
  });

  const runtimeDirty = $derived(
    !!entity &&
      (autoMode !== (entity.entityRuntime?.automationMode ?? 'auto_run') ||
        hbMode !== (entity.entityRuntime?.heartbeatMode ?? 'execute'))
  );

  async function saveEntityRuntime() {
    if (!entity || runtimeSaving) return;
    runtimeSaving = true;
    runtimeNotice = null;
    try {
      // The gateway replaces the whole runtime block, so carry the fields this
      // card does not edit (bound chat session, quiet hours) through unchanged.
      const rt = entity.entityRuntime ?? {};
      await patchEntity(entity.id, {
        entityRuntime: {
          automationMode: autoMode,
          heartbeatMode: hbMode,
          defaultChatSessionId: rt.defaultChatSessionId ?? null,
          quietHoursLocal: rt.quietHoursLocal ?? null
        }
      });
      await invalidateAll();
      runtimeNotice = 'Saved. The next orchestrator and heartbeat pass uses it.';
    } catch (err) {
      runtimeNotice = err instanceof Error ? err.message : 'Could not save automation settings.';
    } finally {
      runtimeSaving = false;
    }
  }

  async function resolveApproval(id: string, status: 'approved' | 'rejected') {
    if (approvalBusyId) return;
    approvalBusyId = id;
    approvalError = null;
    try {
      await patchApproval(id, status);
      await invalidateAll();
    } catch (err) {
      approvalError = err instanceof Error ? err.message : 'Could not update that approval.';
    } finally {
      approvalBusyId = null;
    }
  }

  let enrichmentPollTimer: ReturnType<typeof setInterval> | undefined;

  onMount(() => {
    void loadEntityFiles();
    void loadEntityCaptures();
    void loadEntityActivity();
  });

  // While a draft is being generated, re-run this page's load every 5s so the
  // badge and the review panel move on their own. Stops as soon as the status
  // leaves pending/running, and on navigation (effect teardown).
  $effect(() => {
    if (enrichmentPollTimer) {
      clearInterval(enrichmentPollTimer);
      enrichmentPollTimer = undefined;
    }
    if (!isDrafting || !entity?.id || typeof window === 'undefined') return;
    const refreshEnrichment = async () => {
      try {
        await invalidate(_ENTITY_DEPENDENCY);
      } catch (_) {
        // keep quiet during transient dev-server reloads
      }
    };
    enrichmentPollTimer = setInterval(refreshEnrichment, 5000);
    return () => {
      if (enrichmentPollTimer) clearInterval(enrichmentPollTimer);
      enrichmentPollTimer = undefined;
    };
  });

  const DRAFT_FIELDS: { key: (typeof NARRATIVE_FIELDS)[number]; label: string }[] = [
    { key: 'profile', label: 'Profile' },
    { key: 'goals', label: 'Goals' },
    { key: 'activeContext', label: 'Active context' },
    { key: 'operatingRules', label: 'Operating rules' },
    { key: 'notes', label: 'Notes' },
    { key: 'contentPipeline', label: 'Content pipeline' }
  ];

  /** Draft fields that actually carry a suggestion. */
  const draftRows = $derived(
    DRAFT_FIELDS.map((f) => ({
      ...f,
      suggested: String(draft?.[f.key] ?? '').trim(),
      current: String(entity?.[f.key] ?? '').trim()
    })).filter((row) => row.suggested !== '')
  );
  let enrichmentError = $state<string | null>(null);
  const enrichmentBusy = $derived(applying || discarding || regenerating);

  function enrichmentFailureMessage(err: unknown, fallback: string): string {
    if (apiErrorStatus(err) === 409) {
      return 'A newer draft arrived while you were reviewing. It is shown now — review it and apply again.';
    }
    return err instanceof Error ? err.message : fallback;
  }

  async function applyDraftFields(fields: string[]) {
    if (!entity || enrichmentBusy || fields.length === 0) return;
    applying = true;
    enrichmentError = null;
    try {
      // Always name the fields: an empty list makes the gateway copy all six,
      // including ones the draft left blank, which would wipe what is there.
      await applyEnrichment(entity.id, fields, enrichment?.draftVersion);
      draftSelection = { fields: [] };
      await invalidateAll();
    } catch (err) {
      enrichmentError = enrichmentFailureMessage(err, 'Could not apply the draft.');
      await invalidateAll().catch(() => {});
    } finally {
      applying = false;
    }
  }

  function handleApplyAll() {
    return applyDraftFields(draftRows.map((row) => row.key));
  }

  function handleApplySelected() {
    return applyDraftFields(selectedDraftFields);
  }

  async function handleDiscard() {
    if (!entity || enrichmentBusy) return;
    discarding = true;
    enrichmentError = null;
    try {
      await discardEnrichment(entity.id);
      draftSelection = { fields: [] };
      await invalidateAll();
    } catch (err) {
      enrichmentError = enrichmentFailureMessage(err, 'Could not discard the draft.');
    } finally {
      discarding = false;
    }
  }

  async function handleRegenerate() {
    if (!entity || enrichmentBusy) return;
    regenerating = true;
    enrichmentError = null;
    try {
      draftSelection = { fields: [] };
      await regenerateEnrichment(entity.id);
      await invalidateAll();
    } catch (err) {
      enrichmentError = enrichmentFailureMessage(err, 'Could not start a new draft.');
    } finally {
      regenerating = false;
    }
  }

  function toggleDraftField(field: string) {
    draftSelection = {
      fields: selectedDraftFields.includes(field)
        ? selectedDraftFields.filter((f) => f !== field)
        : [...selectedDraftFields, field]
    };
  }

  function deriveMissionDraftFromBrief(brief: string): { title: string; summary?: string; goal?: string } {
    const raw = brief.trim();
    if (!raw) return { title: '' };
    const firstLine = raw.split('\n')[0]?.trim() ?? raw;
    const shortTitle = firstLine.length > 96 ? `${firstLine.slice(0, 93)}...` : firstLine;
    const title = shortTitle.length >= 6 ? shortTitle : `Mission: ${raw.slice(0, 42)}`;
    const summary = raw.length > 180 ? `${raw.slice(0, 177)}...` : raw;
    return {
      title,
      summary: summary || undefined,
      goal: raw || undefined
    };
  }

  async function handleCreateMission(event: SubmitEvent) {
    event.preventDefault();
    if (!entity || creatingMission) return;
    const brief = missionForm.brief.trim();
    if (!brief) {
      missionError = 'Describe your mission first.';
      return;
    }
    const draft = deriveMissionDraftFromBrief(brief);
    if (!draft.title) {
      missionError = 'Could not derive a mission title from your brief.';
      return;
    }
    creatingMission = true;
    missionError = null;
    try {
      await createMission({
        entity_id: entity.id,
        title: draft.title,
        summary: draft.summary,
        priority: 'medium',
        goal: draft.goal,
        created_by: 'user',
        source: 'entity-ui'
      });
      missionForm = { brief: '' };
      await invalidateAll();
    } catch (err) {
      missionError = err instanceof Error ? err.message : 'Failed to create mission';
    } finally {
      creatingMission = false;
    }
  }

  function formatDateTime(value?: string): string {
    if (!value) return 'not reviewed';
    const d = new Date(value);
    if (Number.isNaN(d.getTime())) return 'not reviewed';
    return d.toLocaleString();
  }

  function nextReviewDue(mission: Mission): string {
    const last = mission.last_reviewed_at ? new Date(mission.last_reviewed_at) : null;
    if (!last || Number.isNaN(last.getTime())) return 'due now';
    const cadenceHours = mission.review_cadence_hours ?? missionPolicy?.defaultReviewCadenceHours ?? 24;
    const due = new Date(last.getTime() + cadenceHours * 60 * 60 * 1000);
    if (Number.isNaN(due.getTime())) return 'due now';
    return due.toLocaleString();
  }

  /** Mission row actions in flight, keyed `<action>:<missionId>`. */
  let missionActionBusy = $state<string | null>(null);
  let missionActionError = $state<string | null>(null);

  async function handleMarkReviewed(mission: Mission) {
    if (missionActionBusy) return;
    missionActionBusy = `review:${mission.id}`;
    missionActionError = null;
    try {
      await updateMission(mission.id, { last_reviewed_at: new Date().toISOString() });
      await invalidateAll();
    } catch (err) {
      missionActionError = err instanceof Error ? err.message : 'Could not mark the mission reviewed.';
    } finally {
      missionActionBusy = null;
    }
  }

  async function handleCreateNextTask(mission: Mission) {
    if (missionActionBusy) return;
    missionActionBusy = `next:${mission.id}`;
    missionActionError = null;
    try {
      await createTask({
        title: `Define next implementation step: ${mission.title}`,
        entity_id: mission.entity_id,
        mission_id: mission.id,
        priority: mission.priority,
        task_kind: 'mission-next-step'
      });
      await invalidateAll();
    } catch (err) {
      missionActionError = err instanceof Error ? err.message : 'Could not create the next task.';
    } finally {
      missionActionBusy = null;
    }
  }

  async function handleCreateConnection(event: SubmitEvent) {
    event.preventDefault();
    if (!entity || connectionCreating) return;
    const provider = connectionForm.provider.trim();
    const domain = connectionForm.domain.trim();
    if (!provider) {
      connectionError = 'Name the provider first (github, gmail, cloudflare…).';
      return;
    }
    if (connectionForm.kind === 'domain' && !domain) {
      connectionError = 'Give the domain this connection proves, e.g. example.com.';
      return;
    }
    connectionError = null;
    connectionNotice = null;
    connectionCreating = true;
    try {
      await createEntityConnection(entity.id, {
        kind: connectionForm.kind,
        provider,
        label: connectionForm.label.trim() || undefined,
        status: connectionForm.status,
        config: connectionForm.kind === 'domain' ? { domain } : undefined
      });
      connectionForm = {
        kind: connectionForm.kind,
        provider: connectionForm.provider,
        label: '',
        domain: '',
        status: 'disconnected'
      };
      await invalidateAll();
    } catch (err) {
      connectionError = err instanceof Error ? err.message : 'Failed to create connection';
    } finally {
      connectionCreating = false;
    }
  }

  async function handleQuickConnect(kind: string, provider: string, label: string) {
    if (!entity) return;
    if (provider.toLowerCase() === 'github') {
      await handleStartGithubAuth();
      return;
    }
    if (connectionCreating) return;
    const isOpenClaw = provider.toLowerCase() === 'openclaw';
    if (isOpenClaw && connections.some((c) => c.provider.toLowerCase() === 'openclaw')) {
      connectionError = null;
      connectionNotice = 'OpenClaw is already connected — copy its webhook below.';
      entityTab = 'connections';
      return;
    }
    connectionError = null;
    connectionNotice = null;
    connectionCreating = true;
    try {
      await createEntityConnection(entity.id, {
        kind,
        provider,
        label,
        status: isOpenClaw ? 'connected' : 'disconnected',
        scopes: isOpenClaw ? ['orders:ingest', 'tasks:create'] : undefined,
        capabilityState: isOpenClaw ? { orders: true, tasks: true, mode: 'webhook' } : undefined,
        config: isOpenClaw ? { accepts: ['order.created', 'order.updated'], taskKind: 'openclaw-order' } : undefined
      });
      await invalidateAll();
      entityTab = 'connections';
    } catch (err) {
      connectionError = err instanceof Error ? err.message : 'Failed to add connection';
    } finally {
      connectionCreating = false;
    }
  }

  async function handleStartGithubAuth() {
    if (!entity || githubAuthBusy) return;
    connectionError = null;
    connectionNotice = null;
    githubAuthBusy = true;
    try {
      const session = await createEntityConnectionSession(entity.id, {
        provider: 'github',
        requestedCapabilities: ['repo', 'read:user'],
        createdBy: 'user'
      });
      githubAuthSession = session;
      entityTab = 'connections';
      if (session.status === 'config_required') {
        connectionError = session.providerMessage ?? 'GitHub authentication is not configured.';
        return;
      }
      connectionNotice = session.providerMessage ?? 'GitHub authorization started.';
      if (session.authorizationUrl && typeof window !== 'undefined') {
        window.open(session.authorizationUrl, '_blank', 'noopener,noreferrer');
      }
    } catch (err) {
      connectionError = err instanceof Error ? err.message : 'Could not start GitHub authorization.';
    } finally {
      githubAuthBusy = false;
    }
  }

  async function handleCompleteGithubAuth() {
    if (!entity || !githubAuthSession || githubCompleteBusy) return;
    connectionError = null;
    connectionNotice = null;
    githubCompleteBusy = true;
    try {
      await finalizeEntityConnectionSession(entity.id, githubAuthSession.id, {});
      githubAuthSession = null;
      connectionNotice = 'GitHub connected. Private repo source imports can use this entity connection.';
      await invalidateAll();
      entityTab = 'connections';
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Could not complete GitHub authorization.';
      connectionError =
        message.includes('409') || message.toLowerCase().includes('conflict')
          ? 'GitHub is still waiting for approval. Authorize in the browser, then try again.'
          : message;
    } finally {
      githubCompleteBusy = false;
    }
  }

  function openClawOrderWebhookUrl(connection: EntityConnection): string {
    if (!entity) return '';
    const path = `/api/entities/${encodeURIComponent(entity.id)}/connections/${encodeURIComponent(connection.id)}/openclaw/orders`;
    if (typeof window === 'undefined') return path;
    return `${window.location.origin}${path}`;
  }

  async function copyOpenClawWebhook(connection: EntityConnection) {
    const url = openClawOrderWebhookUrl(connection);
    if (url) await copyText(url, 'OpenClaw order webhook copied.');
  }

  async function handleTestConnection(connection: EntityConnection) {
    if (!entity || connectionBusyId) return;
    connectionBusyId = connection.id;
    connectionError = null;
    connectionNotice = null;
    try {
      const updated = await testEntityConnection(entity.id, connection.id);
      if (updated?.lastError) connectionError = `${connection.label || connection.provider}: ${updated.lastError}`;
      else connectionNotice = `${connection.label || connection.provider}: check passed.`;
      await invalidateAll();
    } catch (err) {
      connectionError = err instanceof Error ? err.message : 'Could not test that connection.';
    } finally {
      connectionBusyId = null;
    }
  }

  // Domain ownership: the gateway fetches
  // https://<domain>/.well-known/consistent-world-verification.txt and looks
  // for the token there (`entity_adapters::verify_domain_ownership`). The token
  // is ours to choose, so we mint one and show where it has to go.
  const DOMAIN_VERIFICATION_PATH = '/.well-known/consistent-world-verification.txt';
  let verifyingConnectionId = $state<string | null>(null);
  let verifyForm = $state({ domain: '', token: '' });

  function configString(conn: EntityConnection, key: string): string {
    const value = (conn.config as Record<string, unknown> | undefined)?.[key];
    return typeof value === 'string' ? value : '';
  }

  function newVerificationToken(): string {
    const bytes = new Uint8Array(16);
    crypto.getRandomValues(bytes);
    return `consistent-verify-${Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('')}`;
  }

  function openVerifyForm(conn: EntityConnection) {
    const previous = (conn.config as Record<string, unknown> | undefined)?.verification as
      | { verification_token?: string }
      | undefined;
    verifyingConnectionId = conn.id;
    verifyForm = {
      domain: configString(conn, 'domain') || configString(conn, 'hostname'),
      token: previous?.verification_token || newVerificationToken()
    };
    connectionError = null;
    connectionNotice = null;
  }

  function verificationFileUrl(domain: string): string {
    const host = domain
      .trim()
      .replace(/^https?:\/\//, '')
      .replace(/\/.*$/, '');
    return host ? `https://${host}${DOMAIN_VERIFICATION_PATH}` : DOMAIN_VERIFICATION_PATH;
  }

  async function handleVerifyConnection(connection: EntityConnection) {
    if (!entity || connectionBusyId) return;
    const domain = verifyForm.domain.trim();
    const token = verifyForm.token.trim();
    if (!domain || !token) {
      connectionError = 'Both the domain and the token are needed to verify.';
      return;
    }
    connectionBusyId = connection.id;
    connectionError = null;
    connectionNotice = null;
    try {
      const savedDomain = configString(connection, 'domain') || configString(connection, 'hostname');
      if (savedDomain !== domain) {
        // The gateway reads the host from the connection's config, not the request.
        await updateEntityConnection(entity.id, connection.id, {
          config: { ...((connection.config as Record<string, unknown>) ?? {}), domain }
        });
      }
      const result = await verifyEntityConnection(entity.id, connection.id, {
        verificationMethod: 'well_known_file',
        token
      });
      // A failed check is a 200 carrying `lastError`, not an HTTP error.
      if (result?.lastError) {
        connectionError = result.lastError;
      } else {
        connectionNotice = `${domain} verified.`;
        verifyingConnectionId = null;
      }
      await invalidateAll();
    } catch (err) {
      connectionError =
        apiErrorStatus(err) === 400
          ? 'The gateway refused the request — check the domain and token, then try again.'
          : err instanceof Error
            ? err.message
            : 'Could not verify that domain.';
    } finally {
      connectionBusyId = null;
    }
  }

  async function copyText(text: string, done: string) {
    if (typeof navigator === 'undefined' || !navigator.clipboard) return;
    try {
      await navigator.clipboard.writeText(text);
      connectionError = null;
      connectionNotice = done;
    } catch (_) {
      // Clipboard can be refused (insecure origin, permissions); the text is on screen.
    }
  }

  // Honest editing of what a connection *says* about itself. Status is never
  // set from here — only a real check (Test / Verify / an OAuth finish) moves it.
  let editingConnectionId = $state<string | null>(null);
  let connectionEditForm = $state({ label: '', externalHandle: '' });

  function openConnectionEdit(conn: EntityConnection) {
    editingConnectionId = conn.id;
    connectionEditForm = { label: conn.label ?? '', externalHandle: conn.externalHandle ?? '' };
    connectionError = null;
    connectionNotice = null;
  }

  async function saveConnectionEdit(conn: EntityConnection) {
    if (!entity || connectionBusyId) return;
    connectionBusyId = conn.id;
    connectionError = null;
    try {
      await updateEntityConnection(entity.id, conn.id, {
        label: connectionEditForm.label.trim() || conn.provider,
        externalHandle: connectionEditForm.externalHandle.trim() || undefined
      });
      editingConnectionId = null;
      await invalidateAll();
    } catch (err) {
      connectionError = err instanceof Error ? err.message : 'Could not save that connection.';
    } finally {
      connectionBusyId = null;
    }
  }

  async function handleDisconnectConnection(connection: EntityConnection) {
    if (!entity || connectionBusyId) return;
    connectionBusyId = connection.id;
    connectionError = null;
    connectionNotice = null;
    try {
      await deleteEntityConnection(entity.id, connection.id);
      if (verifyingConnectionId === connection.id) verifyingConnectionId = null;
      if (editingConnectionId === connection.id) editingConnectionId = null;
      await invalidateAll();
    } catch (err) {
      connectionError = err instanceof Error ? err.message : 'Could not remove that connection.';
    } finally {
      connectionBusyId = null;
    }
  }

  /** Human label for the current workspace state. */
  const workspaceKindLabel = $derived.by(() => {
    const status = workspaceStatus;
    if (!status) return '';
    if (status.kind === 'repo') return 'Git repo';
    if (status.kind === 'local_folder') return 'Local folder';
    if (status.kind === 'vault_only') return status.needsDecision ? 'Notes folder (not chosen)' : 'Notes only';
    return status.exists ? 'Unknown' : 'Nowhere yet';
  });

  async function loadWorkspaceStatus() {
    if (!entity) return;
    try {
      workspaceStatus = await getEntityWorkspace(entity.id);
      workspaceStatusError = null;
    } catch (err) {
      workspaceStatusError = err instanceof Error ? err.message : 'Could not read workspace state.';
    }
  }

  /**
   * Answer the question. GitHub is deliberately a two-step: the backend hands
   * back the connection-session endpoint and we drive the device-code flow that
   * already exists, rather than inventing a second one.
   */
  async function chooseWorkspace(choice: WorkspaceDecisionInput['choice'], needsPath: boolean) {
    if (!entity || workspaceChoiceBusy) return;
    const path = workspacePathDraft.trim();
    if (needsPath && !path) {
      workspaceStatusError = 'Give a folder path first — an absolute path, or one starting with ~/.';
      return;
    }
    workspaceChoiceBusy = choice;
    workspaceStatusError = null;
    workspaceNotice = null;
    try {
      const result = await setEntityWorkspace(entity.id, {
        choice,
        ...(path ? { path } : {}),
        ...(choice === 'new_project' ? { gitInit: workspaceGitInit } : {})
      });
      workspaceStatus = result.status;
      workspaceNotice = result.message;
      if (result.nextEndpoint) {
        // GitHub: hand off to the existing authorize button's flow.
        await handleStartGithubAuth();
      } else {
        workspacePathDraft = '';
        await invalidateAll();
      }
    } catch (err) {
      workspaceStatusError = err instanceof Error ? err.message : 'Could not save that choice.';
    } finally {
      workspaceChoiceBusy = null;
    }
  }

  async function saveWorkspaceRoots() {
    if (!entity || workspaceSaving) return;
    // Send only what the operator changed. `localPath: null` means "clear" to
    // the gateway, so echoing a stale draft back would undo a folder chosen a
    // moment ago (e.g. via "Where this lives").
    const nextLocal = workspaceDraft.localPath.trim();
    const nextRepo = workspaceDraft.repoPath.trim();
    const localChanged = nextLocal !== (entity.localPath ?? '').trim();
    const repoChanged = nextRepo !== (entity.setup?.repoPath ?? '').trim();
    if (!localChanged && !repoChanged) {
      workspaceNotice = 'Nothing changed.';
      return;
    }
    workspaceSaving = true;
    workspaceNotice = null;
    try {
      await patchEntity(entity.id, {
        ...(localChanged ? { localPath: nextLocal || null } : {}),
        ...(repoChanged
          ? { setup: { ...(entity.setup ?? {}), repoPath: nextRepo || null } }
          : {})
      });
      workspaceNotice = 'Workspace paths updated.';
      await invalidateAll();
      await loadWorkspaceStatus();
    } catch (err) {
      workspaceNotice = err instanceof Error ? err.message : 'Could not update workspace paths.';
    } finally {
      workspaceSaving = false;
    }
  }

  function sleep(ms: number) {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }

  function isTerminalAgentRunStatus(status: string) {
    const normalized = status.toLowerCase();
    return normalized === 'completed' || normalized === 'failed' || normalized === 'cancelled';
  }

  /** Bumped on destroy and whenever a new run starts, so a stale poll loop
   *  notices and stops instead of running on for up to 15 minutes. */
  let agentRunPollGeneration = 0;
  onDestroy(() => {
    agentRunPollGeneration += 1;
  });

  async function pollEntityAgentRun(entityId: string, runId: string, provider: string) {
    const generation = ++agentRunPollGeneration;
    let sawOutput = false;
    for (let attempt = 0; attempt < 180; attempt += 1) {
      await sleep(attempt === 0 ? 1500 : 5000);
      if (generation !== agentRunPollGeneration) return;
      const run = await getEntityAgentRun(entityId, runId);
      if (generation !== agentRunPollGeneration) return;
      agentRunNotice = `${provider} status: ${run.status}.`;
      agentRunReview = reviewIndicatorLabel(run.reviewStatus);
      if (!isTerminalAgentRunStatus(run.status)) continue;
      if (!sawOutput) {
        agentRunOutput = run.output || run.error || 'No output returned.';
        await invalidateAll();
        sawOutput = true;
      }
      // Keep polling briefly while an auto review resolves, so the indicator
      // moves from "review running" to "reviewed" / "needs review".
      if (run.reviewStatus === 'review_running') continue;
      return;
    }
    agentRunNotice = `${provider} is still running. The run is saved and will update its task/run record when complete.`;
  }

  async function handleCreateAgentRun(event: SubmitEvent) {
    event.preventDefault();
    if (!entity || agentRunBusy) return;
    const prompt = agentRunForm.prompt.trim();
    if (!prompt) {
      agentRunNotice = 'Write a prompt for the external agent first.';
      return;
    }
    agentRunBusy = true;
    agentRunNotice = null;
    agentRunOutput = null;
    agentRunReview = null;
    try {
      const run = await createEntityAgentRun(entity.id, {
        kind: agentRunForm.kind,
        prompt,
        cwd: agentRunForm.cwd.trim() || undefined,
        timeoutSecs: 900,
        title: prompt.slice(0, 100),
        createTask: true,
        reviewPolicy: agentRunForm.reviewPolicy
      });
      agentRunNotice = `${run.provider} run started. Polling for completion.`;
      agentRunOutput = null;
      agentRunForm = { ...agentRunForm, prompt: '' };
      await invalidateAll();
      await pollEntityAgentRun(entity.id, run.runId, run.provider);
    } catch (err) {
      agentRunNotice = err instanceof Error ? err.message : 'Could not run external agent.';
    } finally {
      agentRunBusy = false;
    }
  }

  async function loadEntityFiles() {
    if (!entity) return;
    try {
      entityFiles = await listEntityFiles(entity.id);
    } catch (_) {
      entityFiles = [];
    }
  }

  async function loadEntityCaptures() {
    if (!entity) return;
    try {
      const res = await listCaptures({ entity_id: entity.id, status: 'raw', limit: 8 });
      entityCaptures = res.items ?? [];
    } catch (_) {
      entityCaptures = [];
    }
  }

  async function loadEntityActivity() {
    if (!entity) return;
    try {
      entityActivity = await listActivity({ entity_id: entity.id, limit: 25 });
    } catch (_) {
      entityActivity = [];
    }
  }

  function activityKindLabel(it: ActivityItem): string {
    if (it.source === 'capture') return 'Capture';
    if (it.source === 'agent_run') return 'Agent run';
    if (it.source === 'feed') return feedKindLabel(it.kind);
    return it.kind.replaceAll('_', ' ');
  }

  async function handleEntityCaptureAction(
    capture: RawCapture,
    action: 'task' | 'memory' | 'narrative' | 'dismiss'
  ) {
    if (!entity) return;
    captureBusyId = capture.id;
    captureError = null;
    try {
      if (action === 'task') {
        await promoteCaptureToTask(capture.id);
      } else if (action === 'memory') {
        await promoteCaptureToMemory(capture.id, { entity_id: entity.id, kind: 'canonical' });
      } else if (action === 'narrative') {
        await updateCaptureNarrative(capture.id, { entity_id: entity.id });
      } else {
        await dismissCapture(capture.id);
      }
      entityCaptures = entityCaptures.filter((c) => c.id !== capture.id);
    } catch (err) {
      captureError = err instanceof Error ? err.message : 'Could not update that capture.';
    } finally {
      captureBusyId = null;
    }
  }

  async function handleUploadFile(event: Event) {
    if (!entity) return;
    const target = event.target as HTMLInputElement;
    const file = target.files?.[0];
    if (!file) return;
    fileUploadBusy = true;
    fileUploadError = null;
    try {
      await uploadEntityFile(entity.id, file);
      target.value = '';
      await loadEntityFiles();
    } catch (err) {
      fileUploadError = err instanceof Error ? err.message : 'Failed to upload file';
    } finally {
      fileUploadBusy = false;
    }
  }

  function formatFileSize(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  }
</script>

<Seo title="Entity" noindex />

<div class="entity-detail-page">
  <div class="page-header">
    <a href="/entities" class="back-link">← Entities</a>
    {#if entity}
      <h1 class="entity-title">{entity.name}</h1>
      <div class="entity-badges">
        {#if entity.type}<span class="badge type">Type: {entity.type}</span>{/if}
        <span class="badge status">Lifecycle: {entity.status}</span>
        {#if entity.stage}<span class="badge stage">Stage: {entity.stage}</span>{/if}
        {#if isDrafting}
          <span class="badge drafting">Enrichment draft: running</span>
        {:else if isReady}
          <span class="badge ready">Enrichment draft: ready</span>
        {:else if isProviderInvalid}
          <span class="badge provider-invalid">Enrichment draft: invalid provider</span>
        {:else if isFailed}
          <span class="badge failed">Enrichment draft: failed</span>
        {:else if isAiUnavailable}
          <span class="badge ai-unavailable">Enrichment draft: AI unavailable</span>
        {/if}
        <a class="badge skills-link" href={`/skills?entity=${encodeURIComponent(entity.id)}`}>Skills →</a>
      </div>
    {:else if error}
      <p class="error-msg">{error}</p>
    {:else}
      <p class="loading">Loading…</p>
    {/if}
  </div>

  {#if entity}
    <div class="entity-tab-shell">
      <label class="entity-tab-select-label" for="entity-tab-select">Section</label>
      <select
        id="entity-tab-select"
        class="entity-tab-select"
        bind:value={entityTab}
        aria-label="Choose entity section"
      >
        {#each entityTabOptions as opt}
          <option value={opt.id}>{opt.label}</option>
        {/each}
      </select>
      <div class="entity-tab-bar" role="tablist" aria-label="Entity sections">
        {#each entityTabOptions as opt}
          <button
            type="button"
            role="tab"
            class="entity-tab"
            class:entity-tab-active={entityTab === opt.id}
            aria-selected={entityTab === opt.id}
            onclick={() => (entityTab = opt.id)}
          >
            {opt.label}
          </button>
        {/each}
      </div>
    </div>

    <div class="entity-tab-panels">
      {#if entityTab === 'command'}
        <section class="entity-tab-panel" id="panel-command">
          <div class="entity-command-hero">
            <div>
              <p class="entity-command-kicker">Entity conversation</p>
              <h2 class="entity-command-title">{needsSetupEntity ? `Feed ${entity.name} so it becomes execution-ready` : `Work with ${entity.name}`}</h2>
              <p class="entity-command-sub">{needsSetupEntity ? `Operations needs a bit more context before this entity can execute confidently.` : `This entity has enough context for Operations to start executing work.`}</p>
            </div>
            <div class="entity-command-links">
              <a class="entity-chip-link" href={`/?entity_id=${entity.id}`}>Open Capture</a>
              <a class="entity-chip-link" href={`/tasks?entity_id=${entity.id}`}>Entity tasks</a>
              <button type="button" class="entity-chip-link" onclick={() => (entityTab = 'missions')}>Missions</button>
            </div>
          </div>

          {#if isDrafting || isReady || needsEnrichmentRecovery || enrichmentError}
            <section class="draft-review" aria-label="Suggested narrative">
              <div class="draft-review-head">
                <div>
                  <h2 class="draft-review-title">Suggested narrative</h2>
                  <p class="draft-review-sub">
                    {#if isDrafting}
                      Drafting suggestions from what {entity.name} already holds…
                    {:else if isReady}
                      Compare each suggestion with what is there now. Nothing changes until you apply it.
                    {:else if needsEnrichmentRecovery}
                      The last draft didn’t finish.
                    {:else}
                      Something went wrong with the draft.
                    {/if}
                  </p>
                </div>
                {#if enrichment?.model && !isDrafting}
                  <span class="draft-review-model" title="Model that wrote this draft">{enrichment.model}</span>
                {/if}
              </div>

              {#if isDrafting}
                <p class="draft-review-status">
                  {#if currentEnrichmentTask}
                    {currentEnrichmentTask.status === 'in-progress' ? 'Working' : 'Queued'} · {currentEnrichmentTask.progress ?? 0}%
                  {:else}
                    Queued
                  {/if}
                </p>
              {:else if needsEnrichmentRecovery}
                <p class="draft-review-error">
                  {enrichment?.error ??
                    (isProviderInvalid
                      ? 'The configured model provider is not valid. Check AI settings, then regenerate.'
                      : isAiUnavailable
                        ? 'No model was available to write the draft. Check AI settings, then regenerate.'
                        : 'Drafting failed. Try regenerating.')}
                </p>
              {:else if isReady && draftRows.length === 0}
                <p class="draft-review-sub">The draft came back empty — there is nothing to apply.</p>
              {:else if isReady}
                <ul class="draft-review-list">
                  {#each draftRows as row (row.key)}
                    <li class="draft-row" class:draft-row--selected={selectedDraftFields.includes(row.key)}>
                      <label class="draft-row-head">
                        <input
                          type="checkbox"
                          checked={selectedDraftFields.includes(row.key)}
                          onchange={() => toggleDraftField(row.key)}
                          disabled={enrichmentBusy}
                        />
                        <span class="draft-row-label">{row.label}</span>
                        <span class="draft-row-change">
                          {row.current === row.suggested ? 'No change' : row.current ? 'Replaces current' : 'New'}
                        </span>
                      </label>
                      <div class="draft-row-compare">
                        <div class="draft-col">
                          <span class="draft-col-label">Now</span>
                          <p class="draft-col-text" class:draft-col-text--empty={!row.current}>{row.current || 'Empty'}</p>
                        </div>
                        <div class="draft-col draft-col--suggested">
                          <span class="draft-col-label">Suggested</span>
                          <p class="draft-col-text">{row.suggested}</p>
                        </div>
                      </div>
                    </li>
                  {/each}
                </ul>
              {/if}

              {#if enrichmentError}
                <p class="draft-review-error" role="alert">{enrichmentError}</p>
              {/if}

              {#if !isDrafting}
                <div class="draft-review-actions">
                  {#if isReady && draftRows.length > 0}
                    <button type="button" class="entity-btn entity-btn-primary" onclick={handleApplyAll} disabled={enrichmentBusy}>
                      {applying ? 'Applying…' : 'Apply all'}
                    </button>
                    <button
                      type="button"
                      class="entity-btn entity-btn-secondary"
                      onclick={handleApplySelected}
                      disabled={enrichmentBusy || selectedDraftFields.length === 0}
                    >
                      Apply selected{selectedDraftFields.length > 0 ? ` (${selectedDraftFields.length})` : ''}
                    </button>
                  {/if}
                  {#if isReady}
                    <button type="button" class="entity-btn entity-btn-ghost" onclick={handleDiscard} disabled={enrichmentBusy}>
                      {discarding ? 'Discarding…' : 'Discard'}
                    </button>
                  {/if}
                  <button type="button" class="entity-btn entity-btn-secondary" onclick={handleRegenerate} disabled={enrichmentBusy}>
                    {regenerating ? 'Starting…' : 'Regenerate'}
                  </button>
                </div>
              {/if}
            </section>
          {/if}

          <section class="onboard-panel" aria-label="Entity onboarding">
            <div class="onboard-head">
              <div class="onboard-head-text">
                <h2 class="onboard-title">Setup</h2>
                <p class="onboard-sub">{totalFilledCount}/{totalOnboardFields} fields complete</p>
                <button
                  type="button"
                  class="entity-chip-link onboard-autofill"
                  onclick={() => (showAutoFillModal = true)}
                >Auto-fill from sources</button>
              </div>
              <div class="onboard-bar-track">
                <div class="onboard-bar-fill" style="width: {Math.round((totalFilledCount / totalOnboardFields) * 100)}%"></div>
              </div>
            </div>

            {#if showAutoFillModal}
              <EntityImportModal
                entityId={entity.id}
                entityName={entity.name}
                initialSource={entity.setup?.repoPath ?? ''}
                initialContext={{
                  whatItIs: entity.setup?.whatItIs,
                  primaryGoal: entity.setup?.primaryGoal ?? entity.goals,
                  successCriteria: entity.setup?.successCriteria,
                  stage: entity.stage,
                  activeContext: entity.activeContext,
                  operatingRules: entity.operatingRules,
                  notes: entity.notes
                }}
                onclose={() => (showAutoFillModal = false)}
                ondone={autoFillDone}
              />
            {/if}

            <div class="onboard-grid">
              {#each onboardingQuestions as q (q.key)}
                {@const val = (entity.setup ?? {})[q.key]}
                {@const filled = !!(val && String(val).trim() && String(val).trim() !== 'unknown')}
                <div class="field-card" class:field-card--filled={filled} class:field-card--editing={editingSetupField === q.key}>
                  {#if editingSetupField === q.key}
                    <div class="field-card-top">
                      <span class="field-card-label">{q.label}</span>
                    </div>
                    <textarea
                      class="field-card-input"
                      rows="2"
                      bind:value={editingSetupValue}
                      placeholder={q.prompt}
                    ></textarea>
                    <div class="field-card-actions">
                      <button class="field-btn field-btn--save" type="button" onclick={saveSetupEdit} disabled={onboardingSaving || !editingSetupValue.trim()}>
                        {onboardingSaving ? 'Saving...' : 'Save'}
                      </button>
                      <button class="field-btn field-btn--skip" type="button" onclick={skipSetupField} disabled={onboardingSaving}>Skip</button>
                      <button class="field-btn field-btn--cancel" type="button" onclick={cancelSetupEdit}>Cancel</button>
                    </div>
                  {:else}
                    <button class="field-card-body" type="button" onclick={() => startSetupEdit(q.key, val)}>
                      <span class="field-card-top">
                        <span class="field-card-label">{q.label}</span>
                        {#if filled}<span class="field-card-check">Done</span>{/if}
                      </span>
                      {#if filled}
                        <span class="field-card-value">{val}</span>
                      {:else}
                        <span class="field-card-empty">{q.prompt}</span>
                      {/if}
                    </button>
                  {/if}
                </div>
              {/each}

              {#each [
                { key: 'profile', label: 'Profile', value: entity.profile, placeholder: 'Describe this entity...' },
                { key: 'goals', label: 'Goals', value: entity.goals, placeholder: 'What should this entity achieve?' }
              ] as nf (nf.key)}
                <div class="field-card field-card--narrative" class:field-card--filled={!!nf.value} class:field-card--editing={narrativeEditing === nf.key}>
                  {#if narrativeEditing === nf.key}
                    <div class="field-card-top">
                      <span class="field-card-label">{nf.label}</span>
                    </div>
                    <textarea
                      class="field-card-input"
                      rows="3"
                      bind:value={narrativeDraft}
                      placeholder={nf.placeholder}
                    ></textarea>
                    <div class="field-card-actions">
                      <button class="field-btn field-btn--save" type="button" onclick={saveNarrativeEdit} disabled={narrativeSaving}>
                        {narrativeSaving ? 'Saving...' : 'Save'}
                      </button>
                      <button class="field-btn field-btn--cancel" type="button" onclick={cancelNarrativeEdit}>Cancel</button>
                    </div>
                  {:else}
                    <button class="field-card-body" type="button" onclick={() => startNarrativeEdit(nf.key, nf.value)}>
                      <span class="field-card-top">
                        <span class="field-card-label">{nf.label}</span>
                        {#if nf.value}<span class="field-card-check">Done</span>{/if}
                      </span>
                      {#if nf.value}
                        <span class="field-card-value">{nf.value}</span>
                      {:else}
                        <span class="field-card-empty">{nf.placeholder}</span>
                      {/if}
                    </button>
                  {/if}
                </div>
              {/each}
            </div>
            {#if fieldSaveError}
              <p class="mission-error" role="alert">{fieldSaveError}</p>
            {/if}
          </section>

          <EntityKnowledgeCard entityId={entity.id} entityName={entity.name} />

          <section class="entity-command-center" id="section-command-center" aria-label="Command center">
            <div class="ecc-heading">
              <h2 class="ecc-section-title">Live status</h2>
              <p class="ecc-section-sub">Only showing active, working signals.</p>
            </div>
            <div class="ecc-grid ecc-command-grid">
              <div class="ecc-block">
                <div class="ecc-label">Connections</div>
                <ul class="health-list">
                  <li>Healthy: {connectionHealth.ok} / {connectionHealth.total}</li>
                  <li>Reconnect needed: {connectionHealth.reauth}</li>
                  <li>With errors: {connectionHealth.withError}</li>
                </ul>
              </div>
              <div class="ecc-block">
                <div class="ecc-label">Approvals</div>
                {#if pendingApprovals.length === 0}
                  <p class="ecc-none">No approvals pending.</p>
                {:else}
                  <ul class="approval-mini-list">
                    {#each pendingApprovals.slice(0, 5) as approval (approval.id)}
                      <li class="approval-mini">
                        <div class="approval-mini-text">
                          <span class="approval-mini-action">{approval.action}</span>
                          {#if approval.detail}<span class="approval-mini-detail">{approval.detail}</span>{/if}
                        </div>
                        <div class="approval-mini-actions">
                          <button
                            type="button"
                            class="entity-btn entity-btn-primary"
                            disabled={approvalBusyId !== null}
                            onclick={() => resolveApproval(approval.id, 'approved')}
                          >{approvalBusyId === approval.id ? 'Saving…' : 'Approve'}</button>
                          <button
                            type="button"
                            class="entity-btn entity-btn-ghost"
                            disabled={approvalBusyId !== null}
                            onclick={() => resolveApproval(approval.id, 'rejected')}
                          >Reject</button>
                        </div>
                      </li>
                    {/each}
                  </ul>
                  {#if pendingApprovals.length > 5}
                    <p class="ecc-none">{pendingApprovals.length - 5} more on the task list.</p>
                  {/if}
                {/if}
                {#if approvalError}
                  <p class="mission-error" role="alert">{approvalError}</p>
                {/if}
                <a class="ecc-link" href={`/tasks?entity_id=${entity.id}`}>Open entity task list →</a>
              </div>
            </div>
          </section>
        </section>
      {:else if entityTab === 'missions'}
        <section class="entity-tab-panel entity-stack-card" id="panel-missions">
          <h2 class="stack-card-title">Missions</h2>
          <p class="stack-card-sub">Focused mission planning for this entity.</p>
          <p class="entity-tab-hint"><a class="ecc-link" href={`/tasks?entity_id=${entity.id}`}>Open this entity tasks →</a></p>
          <form class="mission-form" onsubmit={handleCreateMission}>
            <textarea
              class="entity-input mission-textarea"
              placeholder="Describe the mission in one brief. AI will derive title, summary, and goal."
              bind:value={missionForm.brief}
              rows="4"
              maxlength="1200"
              required
            ></textarea>
            <div class="mission-form-row">
              <button class="entity-btn entity-btn-primary mission-submit" type="submit" disabled={creatingMission}>
                {creatingMission ? 'Creating…' : 'Create mission'}
              </button>
            </div>
            {#if missionError}
              <p class="mission-error">{missionError}</p>
            {/if}
          </form>
          <div class="mission-controls">
            <label class="mission-filter-label">
              <span class="sr-only">Filter by status</span>
              <select class="mission-filter" bind:value={missionStatusFilter}>
                <option value="all">All statuses</option>
                <option value="planned">Planned</option>
                <option value="active">Active</option>
                <option value="blocked">Blocked</option>
                <option value="user-input-needed">User input needed</option>
                <option value="completed">Completed</option>
              </select>
            </label>
          </div>
          {#if missionActionError}
            <p class="mission-error" role="alert">{missionActionError}</p>
          {/if}
          {#if missionCardsVisible.length > 0}
            <ul class="missions-list">
              {#each missionCardsVisible as card}
                <li class="mission-item">
                  <div class="mission-head">
                    <a class="mission-title" href="/missions/{card.mission.id}">{card.mission.title}</a>
                    <span class="mission-status">{card.mission.status}</span>
                  </div>
                  <p class="mission-meta">
                    Priority: {card.mission.priority} · Tasks: {card.total}
                  </p>
                  <p class="mission-meta">
                    Execution: <strong>{card.execution.label}</strong> · {card.execution.detail}
                  </p>
                  <p class="mission-meta">
                    Last reviewed: {formatDateTime(card.mission.last_reviewed_at)} · Next due: {nextReviewDue(card.mission)}
                  </p>
                  {#if card.mission.summary}
                    <p class="mission-summary">{card.mission.summary}</p>
                  {/if}
                  <div class="mission-task-stats">
                    <span>Queued {card.queued}</span>
                    <span>In progress {card.in_progress}</span>
                    <span>Blocked {card.blocked}</span>
                    <span>Done {card.completed}</span>
                  </div>
                  <div class="mission-actions">
                    <button class="btn-secondary" disabled={missionActionBusy !== null} onclick={() => handleMarkReviewed(card.mission)}>
                      {missionActionBusy === `review:${card.mission.id}` ? 'Saving…' : 'Mark reviewed'}
                    </button>
                    <button class="btn-primary" disabled={missionActionBusy !== null} onclick={() => handleCreateNextTask(card.mission)}>
                      {missionActionBusy === `next:${card.mission.id}` ? 'Creating…' : 'Create next task'}
                    </button>
                    <a class="btn-secondary" href={`/tasks?mission_id=${card.mission.id}`}>Mission tasks</a>
                  </div>
                </li>
              {/each}
            </ul>
          {:else}
            <p class="empty-foundation">No missions match current filters.</p>
          {/if}
        </section>
      {:else if entityTab === 'connections'}
        <section class="entity-tab-panel entity-stack-card" id="panel-connections">
          <h2 class="stack-card-title">Connections</h2>
          <p class="stack-card-sub">Simple integrations only. Connect what you actively use.</p>

          <div class="quick-connect-grid">
            <button
              type="button"
              class="entity-btn entity-btn-secondary"
              disabled={connectionCreating}
              onclick={() => handleQuickConnect('api_service', 'openclaw', 'OpenClaw runtime')}
            >
              Connect OpenClaw
            </button>
            <button
              type="button"
              class="entity-btn entity-btn-secondary"
              disabled={githubAuthBusy}
              onclick={() => handleQuickConnect('api_service', 'github', 'GitHub')}
            >
              Connect GitHub
            </button>
          </div>

          <div class="connection-auth-panel">
            <div class="connection-auth-copy">
              <strong>GitHub source access</strong>
              <span>Authorize Consistent CEO in GitHub, then private repo links pasted in chat can build entity context.</span>
            </div>
            <button type="button" class="entity-btn entity-btn-primary" onclick={handleStartGithubAuth} disabled={githubAuthBusy}>
              {githubAuthBusy ? 'Starting…' : 'Authorize GitHub'}
            </button>
          </div>

          {#if githubAuthSession && githubAuthSession.status !== 'config_required'}
            <div class="connection-auth-panel connection-auth-session">
              <div class="connection-auth-copy">
                <strong>Enter this code in GitHub</strong>
                {#if githubAuthSession.userCode}
                  <code class="connection-auth-code">{githubAuthSession.userCode}</code>
                {/if}
                {#if safeHttpUrl(githubAuthSession.verificationUri)}
                  <a class="connection-auth-link" href={safeHttpUrl(githubAuthSession.verificationUri)} target="_blank" rel="noreferrer noopener">
                    Open GitHub authorization
                  </a>
                {/if}
              </div>
              <button
                type="button"
                class="entity-btn entity-btn-primary"
                onclick={handleCompleteGithubAuth}
                disabled={githubCompleteBusy}
              >
                {githubCompleteBusy ? 'Completing…' : 'I approved in GitHub'}
              </button>
            </div>
          {/if}

          <form class="connection-form" onsubmit={handleCreateConnection}>
          <select class="entity-input mission-select" bind:value={connectionForm.kind}>
            <option value="social_account">Social</option>
            <option value="email_account">Email</option>
            <option value="domain">Domain</option>
            <option value="api_service">API service</option>
            <option value="storage">Storage</option>
            <option value="website">Website</option>
          </select>
          <input class="entity-input" bind:value={connectionForm.provider} placeholder="provider (github, gmail, custom...)" />
          {#if connectionForm.kind === 'domain'}
            <input class="entity-input" bind:value={connectionForm.domain} placeholder="domain (example.com)" />
          {/if}
          <input class="entity-input" bind:value={connectionForm.label} placeholder="label (optional)" />
          <button type="submit" class="entity-btn entity-btn-primary mission-submit" disabled={connectionCreating}>
            {connectionCreating ? 'Adding…' : 'Add connection'}
          </button>
          </form>
        {#if connectionError}
          <p class="mission-error">{connectionError}</p>
        {/if}
        {#if connectionNotice}
          <p class="mission-notice">{connectionNotice}</p>
        {/if}
        {#if connectionGroups.length === 0}
          <p class="empty-foundation">No connections yet — add one above.</p>
        {:else}
          {#each connectionGroups as group}
            <h3 class="conn-group-title">{group.title}</h3>
            <ul class="channels-list">
              {#each group.items as conn}
                <li class="connection-row">
                  <div class="connection-meta">
                    <span class="connection-head">{conn.provider} · {conn.kind}</span>
                    <span class="connection-sub">{conn.label}</span>
                    <span class="connection-sub">{conn.externalHandle ?? conn.externalAccountId ?? 'identity not set'}</span>
                    {#if conn.provider.toLowerCase() === 'openclaw'}
                      <span class="connection-sub webhook-url">{openClawOrderWebhookUrl(conn)}</span>
                    {/if}
                    <span class="channel-status" class:status-reauth={conn.status === 'reauth_required'}>{conn.status}</span>
                    {#if conn.status === 'reauth_required'}
                      <span class="badge reauth">Reconnect</span>
                    {/if}
                    {#if conn.lastError}
                      <p class="connection-error">{conn.lastError}</p>
                    {/if}
                    {#if connectionNeedsReauth(conn.lastError)}
                      <p class="reauth-hint">Reconnect or rotate credentials — test failed auth.</p>
                    {/if}
                  </div>
                  <div class="connection-actions">
                    <button type="button" class="entity-btn entity-btn-secondary" onclick={() => handleTestConnection(conn)} disabled={connectionBusyId !== null}>Test</button>
                    {#if conn.kind === 'domain'}
                      <button
                        type="button"
                        class="entity-btn entity-btn-secondary"
                        onclick={() => (verifyingConnectionId === conn.id ? (verifyingConnectionId = null) : openVerifyForm(conn))}
                        disabled={connectionBusyId !== null}
                        aria-expanded={verifyingConnectionId === conn.id}
                      >Verify</button>
                    {/if}
                    {#if conn.provider.toLowerCase() === 'openclaw'}
                      <button type="button" class="entity-btn entity-btn-secondary" onclick={() => copyOpenClawWebhook(conn)}>Copy webhook</button>
                    {/if}
                    <button
                      type="button"
                      class="entity-btn entity-btn-secondary"
                      onclick={() => (editingConnectionId === conn.id ? (editingConnectionId = null) : openConnectionEdit(conn))}
                      disabled={connectionBusyId !== null}
                      aria-expanded={editingConnectionId === conn.id}
                    >Edit</button>
                    <button type="button" class="entity-btn entity-btn-ghost" onclick={() => handleDisconnectConnection(conn)} disabled={connectionBusyId !== null}>Remove</button>
                  </div>
                  {#if verifyingConnectionId === conn.id}
                    <div class="connection-inline-form">
                      <p class="connection-inline-copy">
                        Prove you control the domain: publish the token below at
                        <code>{verificationFileUrl(verifyForm.domain)}</code>, then press Verify.
                      </p>
                      <label class="connection-inline-field">
                        <span class="workspace-input-label">Domain</span>
                        <input class="entity-input" bind:value={verifyForm.domain} placeholder="example.com" />
                      </label>
                      <label class="connection-inline-field">
                        <span class="workspace-input-label">Token</span>
                        <span class="connection-token-row">
                          <input class="entity-input connection-token" bind:value={verifyForm.token} readonly />
                          <button
                            type="button"
                            class="entity-btn entity-btn-ghost"
                            onclick={() => copyText(verifyForm.token, 'Verification token copied.')}
                          >Copy</button>
                        </span>
                      </label>
                      <div class="connection-inline-actions">
                        <button
                          type="button"
                          class="entity-btn entity-btn-primary"
                          onclick={() => handleVerifyConnection(conn)}
                          disabled={connectionBusyId !== null || !verifyForm.domain.trim()}
                        >{connectionBusyId === conn.id ? 'Verifying…' : 'Verify'}</button>
                        <button type="button" class="entity-btn entity-btn-ghost" onclick={() => (verifyingConnectionId = null)}>Cancel</button>
                      </div>
                    </div>
                  {/if}
                  {#if editingConnectionId === conn.id}
                    <div class="connection-inline-form">
                      <label class="connection-inline-field">
                        <span class="workspace-input-label">Label</span>
                        <input class="entity-input" bind:value={connectionEditForm.label} placeholder={conn.provider} />
                      </label>
                      <label class="connection-inline-field">
                        <span class="workspace-input-label">Account or handle</span>
                        <input class="entity-input" bind:value={connectionEditForm.externalHandle} placeholder="@handle, you@example.com…" />
                      </label>
                      <p class="connection-inline-copy">Status changes only when a check passes — Test{conn.kind === 'domain' ? ' or Verify' : ''}.</p>
                      <div class="connection-inline-actions">
                        <button
                          type="button"
                          class="entity-btn entity-btn-primary"
                          onclick={() => saveConnectionEdit(conn)}
                          disabled={connectionBusyId !== null}
                        >{connectionBusyId === conn.id ? 'Saving…' : 'Save'}</button>
                        <button type="button" class="entity-btn entity-btn-ghost" onclick={() => (editingConnectionId = null)}>Cancel</button>
                      </div>
                    </div>
                  {/if}
                </li>
              {/each}
            </ul>
          {/each}
        {/if}
        {#if connectionEvents.length > 0}
          <div class="conn-events-mini">
            <div class="ecc-label">Recent connection checks</div>
            <ul class="channels-list">
              {#each connectionEvents.slice(0, 8) as event}
                <li>
                  <span>{event.kind}</span>
                  <span class="channel-status">{new Date(event.timestamp).toLocaleString()}</span>
                </li>
              {/each}
            </ul>
          </div>
        {/if}
        </section>
      {:else if entityTab === 'records'}
        <section class="entity-tab-panel entity-stack-card" id="panel-records">
          <h2 class="stack-card-title">Records</h2>
          <p class="stack-card-sub">
            What this entity actually has, sourced from wherever it lives — a CRM, a spreadsheet, a site.
            Every row keeps where it came from and when it was last confirmed.
          </p>

          {#if recordGroups.length === 0}
            <p class="empty-foundation">
              No records yet. Once a source is connected and synced, its customers, deals, content and
              accounts land here — see "Records" in the integrations doc.
            </p>
          {:else}
            {#each recordGroups as group}
              <h3 class="conn-group-title">{group.kind} · {group.items.length}</h3>
              <ul class="channels-list">
                {#each group.items as record}
                  <li class="connection-row">
                    <div class="connection-meta">
                      <span class="connection-head">{record.source} · {record.sourceRef}</span>
                      <span class="connection-sub">{recordFieldsPreview(record.fields)}</span>
                      <span class="connection-sub">
                        fetched {new Date(record.fetchedAt).toLocaleString()}
                        {#if record.evidence}
                          {@const evidenceUrl = safeHttpUrl(record.evidence)}
                          {#if evidenceUrl}
                            · <a href={evidenceUrl} target="_blank" rel="noreferrer noopener">evidence</a>
                          {:else}
                            · evidence: {record.evidence}
                          {/if}
                        {/if}
                      </span>
                    </div>
                  </li>
                {/each}
              </ul>
            {/each}
          {/if}
        </section>
      {:else if entityTab === 'memory'}
        <section class="entity-tab-panel entity-stack-card" id="panel-memory">
        <h2 class="stack-card-title">Memory</h2>
        <p class="stack-card-sub">Short-term signals and long-term preferences.</p>
        <div class="mem-grid">
          <div>
            <div class="ecc-label">Short-term</div>
            {#if memoryPreview.shortTerm.length === 0}
              <p class="ecc-none">Nothing recent yet.</p>
            {:else}
              <ul class="ecc-mem-list">
                {#each memoryPreview.shortTerm.slice(0, 10) as line}
                  <li>{line}</li>
                {/each}
              </ul>
            {/if}
          </div>
          <div>
            <div class="ecc-label">Long-term</div>
            {#if memoryPreview.longTerm.length === 0}
              <p class="ecc-none">Nothing durable yet.</p>
            {:else}
              <ul class="ecc-mem-list ecc-long">
                {#each memoryPreview.longTerm.slice(0, 6) as line}
                  <li>{line}</li>
                {/each}
              </ul>
            {/if}
          </div>
        </div>
        </section>
      {:else if entityTab === 'policy'}
        <section class="entity-tab-panel entity-stack-card" id="panel-policy">
        <h2 class="stack-card-title">Execution policy</h2>
        <p class="stack-card-sub">Stored on the entity (when set).</p>
        {#if entity.executionPolicies && (entity.executionPolicies.allowedCapabilities?.length || entity.executionPolicies.approvalRequiredCapabilities?.length || entity.executionPolicies.allowedExecutionLevels?.length)}
          <div class="resource-item">
            <div class="resource-meta"><strong>Allowed capabilities:</strong> {entity.executionPolicies.allowedCapabilities?.join(', ') ?? '—'}</div>
            <div class="resource-meta"><strong>Execution levels:</strong> {entity.executionPolicies.allowedExecutionLevels?.join(', ') ?? '—'}</div>
            <div class="resource-meta"><strong>Approval-required:</strong> {entity.executionPolicies.approvalRequiredCapabilities?.join(', ') ?? '—'}</div>
          </div>
        {:else}
          <p class="empty-foundation">No capability overrides on this entity.</p>
        {/if}
        <div class="resource-item">
          <div class="resource-label">Workspace</div>
          <div class="resource-value">{entity.defaultWorkspaceRoot ?? 'default'}</div>
          <div class="resource-meta">Memory scope: {entity.memoryScope ?? 'default'} · Model tier: {entity.preferredModelTier ?? 'default'}</div>
          {#if entity.allowedAgentGroups?.length}
            <div class="resource-meta">Agent groups: {entity.allowedAgentGroups.join(', ')}</div>
          {/if}
        </div>
        <div class="resource-item runtime-card">
          <div class="resource-label">Automation</div>
          <p class="resource-meta">How much {entity.name}’s directors and heartbeats may do without you.</p>
          <div class="workspace-edit-grid">
            <label>
              <span class="workspace-input-label">Work</span>
              <select class="entity-input" bind:value={autoMode} disabled={runtimeSaving}>
                <option value="manual">Manual — nothing is picked up automatically</option>
                <option value="suggest">Suggest — work is proposed; outward actions wait for approval</option>
                <option value="auto_run">Auto-run — work runs on its own, within policy and approvals</option>
              </select>
            </label>
            <label>
              <span class="workspace-input-label">Heartbeat</span>
              <select class="entity-input" bind:value={hbMode} disabled={runtimeSaving}>
                <option value="execute">On — heartbeats may pick up this entity’s tasks</option>
                {#if hbMode === 'suggest'}
                  <option value="suggest">Suggest — currently behaves like On</option>
                {/if}
                <option value="off">Off — heartbeats skip this entity</option>
              </select>
            </label>
          </div>
          <div class="workspace-edit-actions">
            <button
              type="button"
              class="entity-btn entity-btn-primary"
              onclick={() => void saveEntityRuntime()}
              disabled={runtimeSaving || !runtimeDirty}
            >
              {runtimeSaving ? 'Saving…' : 'Save automation'}
            </button>
          </div>
          {#if runtimeNotice}
            <p class="workspace-notice">{runtimeNotice}</p>
          {/if}
        </div>
        {#if !entity.isPersonal}
          <div class="danger-zone">
            <h3 class="danger-zone-title">Danger zone</h3>
            <p class="danger-zone-copy">
              Deleting removes this entity and its cloud data — heartbeats, tasks,
              missions, canonical docs, and entity memory. Files in the local vault
              folder stay on disk.
            </p>
            {#if entityDeleteError}
              <p class="danger-zone-error">{entityDeleteError}</p>
            {/if}
            {#if confirmingDelete}
              <div class="danger-zone-actions">
                <button class="danger-btn" disabled={deletingEntity} onclick={deleteThisEntity}>
                  {deletingEntity ? 'Deleting…' : `Yes, delete “${entity.name}”`}
                </button>
                <button class="danger-cancel-btn" disabled={deletingEntity} onclick={() => (confirmingDelete = false)}>Cancel</button>
              </div>
            {:else}
              <button class="danger-outline-btn" onclick={() => { entityDeleteError = ''; confirmingDelete = true; }}>Delete entity</button>
            {/if}
          </div>
        {/if}
        </section>
      {:else if entityTab === 'strategy'}
        <section class="entity-tab-panel entity-stack-card" id="panel-strategy">
          <h2 class="stack-card-title">Strategy flow · Mirimai ID</h2>
          <p class="stack-card-sub">
            Mermaid source for where this entity started, where it is, and where it is heading.
            Open <code>docs/diagrams/</code> `.mmd` files in Cursor for full preview.
          </p>
          {#if entity.flowDiagrams?.updatedAt}
            <p class="resource-meta">Last updated: {entity.flowDiagrams.updatedAt}</p>
          {/if}
          {#each [
            { key: 'current', label: 'Current (as-built)', value: entity.flowDiagrams?.current },
            { key: 'future', label: 'Future (target)', value: entity.flowDiagrams?.future },
            { key: 'previous', label: 'Previous (archive)', value: entity.flowDiagrams?.previous }
          ] as slot (slot.key)}
            <details class="section-card" open={slot.key === 'current'}>
              <summary class="section-summary">
                <span class="section-title">{slot.label}</span>
              </summary>
              <div class="section-body">
                {#if slot.value}
                  <pre class="mirimai-block">{slot.value}</pre>
                {:else}
                  <p class="empty-foundation">No Mirimai ID for this slot yet.</p>
                {/if}
              </div>
            </details>
          {/each}
        </section>
      {:else if entityTab === 'narrative'}
        <section class="entity-tab-panel entity-narrative-panel" id="panel-narrative">
        <div class="entity-narrative-intro">
          <h2 class="stack-card-title">Narrative & resources</h2>
          <p class="stack-card-sub">Story, context, files, and links — grouped together.</p>
        </div>
      <div class="sections" id="entity-foundation">
        <div class="sections-header">
          <h2 class="sections-title">Entity narrative</h2>
          <p class="sections-subtitle">Profile, goals, and operating context.</p>
          <p class="sections-helper"><span aria-hidden="true">▾</span> Tap any row title to expand or collapse</p>
        </div>
        {#if fieldSaveError}
          <p class="mission-error" role="alert">{fieldSaveError}</p>
        {/if}
        <details class="section-card" open>
          <summary class="section-summary">
            <span class="section-title">Profile</span>
            <button type="button" class="section-edit-btn" onclick={(e) => { e.preventDefault(); e.stopPropagation(); startNarrativeEdit('profile', entity.profile); }}>Edit</button>
          </summary>
          <div class="section-body markdown-block">
            {#if narrativeEditing === 'profile'}
              <textarea
                class="narrative-edit-textarea"
                bind:value={narrativeDraft}
                rows={6}
                placeholder="Describe this entity..."
              ></textarea>
              <div class="narrative-edit-actions">
                <button class="btn-save" disabled={narrativeSaving} onclick={saveNarrativeEdit}>{narrativeSaving ? 'Saving…' : 'Save'}</button>
                <button class="btn-cancel" onclick={cancelNarrativeEdit}>Cancel</button>
              </div>
            {:else if entity.profile}
              {entity.profile}
            {:else}
              <span class="empty-foundation">Not set yet.</span>
            {/if}
          </div>
        </details>
        <details class="section-card">
          <summary class="section-summary">
            <span class="section-title">Goals</span>
            <button type="button" class="section-edit-btn" onclick={(e) => { e.preventDefault(); e.stopPropagation(); startNarrativeEdit('goals', entity.goals); }}>Edit</button>
          </summary>
          <div class="section-body markdown-block">
            {#if narrativeEditing === 'goals'}
              <textarea
                class="narrative-edit-textarea"
                bind:value={narrativeDraft}
                rows={6}
                placeholder="What should this entity achieve?"
              ></textarea>
              <div class="narrative-edit-actions">
                <button class="btn-save" disabled={narrativeSaving} onclick={saveNarrativeEdit}>{narrativeSaving ? 'Saving…' : 'Save'}</button>
                <button class="btn-cancel" onclick={cancelNarrativeEdit}>Cancel</button>
              </div>
            {:else if entity.goals}
              {entity.goals}
            {:else}
              <span class="empty-foundation">Not set yet.</span>
            {/if}
          </div>
        </details>
        <details class="section-card">
          <summary class="section-summary">
            <span class="section-title">Active context</span>
            <button type="button" class="section-edit-btn" onclick={(e) => { e.preventDefault(); e.stopPropagation(); startNarrativeEdit('activeContext', entity.activeContext); }}>Edit</button>
          </summary>
          <div class="section-body markdown-block">
            {#if narrativeEditing === 'activeContext'}
              <textarea
                class="narrative-edit-textarea"
                bind:value={narrativeDraft}
                rows={6}
                placeholder="Current situation, stage, challenges..."
              ></textarea>
              <div class="narrative-edit-actions">
                <button class="btn-save" disabled={narrativeSaving} onclick={saveNarrativeEdit}>{narrativeSaving ? 'Saving…' : 'Save'}</button>
                <button class="btn-cancel" onclick={cancelNarrativeEdit}>Cancel</button>
              </div>
            {:else if entity.activeContext}
              {entity.activeContext}
            {:else}
              <span class="empty-foundation">Not set yet.</span>
            {/if}
          </div>
        </details>
        <details class="section-card">
          <summary class="section-summary">
            <span class="section-title">Operating rules</span>
            <button type="button" class="section-edit-btn" onclick={(e) => { e.preventDefault(); e.stopPropagation(); startNarrativeEdit('operatingRules', entity.operatingRules); }}>Edit</button>
          </summary>
          <div class="section-body markdown-block">
            {#if narrativeEditing === 'operatingRules'}
              <textarea
                class="narrative-edit-textarea"
                bind:value={narrativeDraft}
                rows={6}
                placeholder="Rules, constraints, principles..."
              ></textarea>
              <div class="narrative-edit-actions">
                <button class="btn-save" disabled={narrativeSaving} onclick={saveNarrativeEdit}>{narrativeSaving ? 'Saving…' : 'Save'}</button>
                <button class="btn-cancel" onclick={cancelNarrativeEdit}>Cancel</button>
              </div>
            {:else if entity.operatingRules}
              {entity.operatingRules}
            {:else}
              <span class="empty-foundation">Not set yet.</span>
            {/if}
          </div>
        </details>
        <details class="section-card">
          <summary class="section-summary">
            <span class="section-title">Notes</span>
            <button type="button" class="section-edit-btn" onclick={(e) => { e.preventDefault(); e.stopPropagation(); startNarrativeEdit('notes', entity.notes); }}>Edit</button>
          </summary>
          <div class="section-body markdown-block">
            {#if narrativeEditing === 'notes'}
              <textarea
                class="narrative-edit-textarea"
                bind:value={narrativeDraft}
                rows={6}
                placeholder="Additional context..."
              ></textarea>
              <div class="narrative-edit-actions">
                <button class="btn-save" disabled={narrativeSaving} onclick={saveNarrativeEdit}>{narrativeSaving ? 'Saving…' : 'Save'}</button>
                <button class="btn-cancel" onclick={cancelNarrativeEdit}>Cancel</button>
              </div>
            {:else if entity.notes}
              {entity.notes}
            {:else}
              <span class="empty-foundation">Not set yet.</span>
            {/if}
          </div>
        </details>
        <details class="section-card">
          <summary class="section-summary">
            <span class="section-title">Content pipeline</span>
            <button type="button" class="section-edit-btn" onclick={(e) => { e.preventDefault(); e.stopPropagation(); startNarrativeEdit('contentPipeline', entity.contentPipeline); }}>Edit</button>
          </summary>
          <div class="section-body markdown-block">
            {#if narrativeEditing === 'contentPipeline'}
              <textarea
                class="narrative-edit-textarea"
                bind:value={narrativeDraft}
                rows={6}
                placeholder="Communication strategy..."
              ></textarea>
              <div class="narrative-edit-actions">
                <button class="btn-save" disabled={narrativeSaving} onclick={saveNarrativeEdit}>{narrativeSaving ? 'Saving…' : 'Save'}</button>
                <button class="btn-cancel" onclick={cancelNarrativeEdit}>Cancel</button>
              </div>
            {:else if entity.contentPipeline}
              {entity.contentPipeline}
            {:else}
              <span class="empty-foundation">Not set yet.</span>
            {/if}
          </div>
        </details>
      </div>

      <section class="entity-stack-card narrative-resources-sub" id="section-resources" aria-label="Resources">
        <h2 class="stack-card-title">Resources</h2>
        <p class="stack-card-sub">Passive links, files, and references — not authenticated connections.</p>
        <div class="resource-list">
          <div class="resource-item workspace-root-item">
            <div class="resource-label">Local workspace</div>
            {#if entityLocalWorkspace.length > 0}
              <div class="workspace-root-list">
                {#each entityLocalWorkspace as root}
                  <div class="workspace-root-row">
                    <div class="workspace-root-head">
                      <span class="workspace-root-name">{root.label}</span>
                      <code class="workspace-root-path">{root.value}</code>
                    </div>
                    <div class="resource-meta">{root.hint}</div>
                  </div>
                {/each}
              </div>
            {:else}
              <div class="resource-value muted">No local root or repo path set yet</div>
              <div class="resource-meta">Add a local path or repo path so future directors know where to work.</div>
            {/if}

            {#if workspaceStatus}
              <div class="workspace-decision-card" class:needs-decision={workspaceStatus.needsDecision}>
                <div class="workspace-decision-head">
                  <span class="workspace-decision-title">Where this lives</span>
                  <span class="workspace-decision-kind">{workspaceKindLabel}</span>
                </div>
                {#if workspaceStatus.path}
                  <code class="workspace-root-path">{workspaceStatus.path}</code>
                  {#if !workspaceStatus.exists}
                    <p class="workspace-decision-warn">That folder isn't on this machine.</p>
                  {/if}
                {/if}

                {#if workspaceStatus.needsDecision}
                  <p class="workspace-decision-question">
                    {workspaceStatus.question ?? 'Where should this entity live?'}
                  </p>
                  {#if workspaceStatus.isVaultFallback}
                    <p class="workspace-decision-warn">
                      Right now the only place I have is a notes folder I made myself. Agents won't build a
                      project in there until you choose.
                    </p>
                  {/if}
                  <label class="workspace-decision-path">
                    <span class="workspace-input-label">Folder path</span>
                    <input
                      class="entity-input"
                      bind:value={workspacePathDraft}
                      placeholder="~/projects/acme"
                    />
                  </label>
                  <label class="workspace-decision-git">
                    <input type="checkbox" bind:checked={workspaceGitInit} />
                    <span>Run <code>git init</code> in a new project folder</span>
                  </label>
                  <div class="workspace-decision-options">
                    {#each workspaceStatus.options as option (option.choice)}
                      <button
                        type="button"
                        class="entity-btn workspace-decision-btn"
                        class:entity-btn-primary={option.choice === 'local_folder'}
                        class:entity-btn-secondary={option.choice !== 'local_folder'}
                        disabled={workspaceChoiceBusy !== null}
                        onclick={() =>
                          void chooseWorkspace(
                            option.choice as WorkspaceDecisionInput['choice'],
                            option.needsPath
                          )}
                      >
                        <span class="workspace-decision-btn-label">
                          {workspaceChoiceBusy === option.choice ? 'Working…' : option.label}
                        </span>
                        <span class="workspace-decision-btn-detail">{option.detail}</span>
                      </button>
                    {/each}
                  </div>
                {:else}
                  <p class="resource-meta">
                    Answered — agents work here and nowhere else. Change it below if that moves.
                  </p>
                {/if}
                {#if workspaceStatusError}
                  <p class="workspace-decision-warn">{workspaceStatusError}</p>
                {/if}
              </div>
            {/if}

            <div class="workspace-edit-card">
              <div class="workspace-edit-title">Edit local roots</div>
              <p class="resource-meta">Set the main local folder and the main repo or project path for this entity.</p>
              <div class="workspace-edit-grid">
                <label>
                  <span class="workspace-input-label">Primary local root</span>
                  <input class="entity-input" bind:value={workspaceDraft.localPath} placeholder="~/projects/consistent-world or /Users/..." />
                </label>
                <label>
                  <span class="workspace-input-label">Repo / project path</span>
                  <input class="entity-input" bind:value={workspaceDraft.repoPath} placeholder="~/projects/consistent-world/app" />
                </label>
              </div>
              <div class="workspace-edit-actions">
                <button type="button" class="entity-btn entity-btn-primary" onclick={() => void saveWorkspaceRoots()} disabled={workspaceSaving}>
                  {workspaceSaving ? 'Saving…' : 'Save workspace paths'}
                </button>
              </div>
              {#if workspaceNotice}
                <p class="workspace-notice">{workspaceNotice}</p>
              {/if}
            </div>

            <form class="agent-run-card" onsubmit={handleCreateAgentRun}>
              <div class="workspace-edit-title">Run external agent</div>
              <p class="resource-meta">Use the entity folder as the operating context for Claude Code, OpenCode, or Cursor Agent.</p>
              <div class="workspace-edit-grid">
                <label>
                  <span class="workspace-input-label">Agent</span>
                  <select class="entity-input" bind:value={agentRunForm.kind}>
                    <option value="claude">Claude Code</option>
                    <option value="opencode">OpenCode</option>
                    <option value="cursor-agent">Cursor Agent</option>
                  </select>
                </label>
                <label>
                  <span class="workspace-input-label">Working directory</span>
                  <input class="entity-input" bind:value={agentRunForm.cwd} placeholder="Defaults to entity repo/local root" />
                </label>
                <label>
                  <span class="workspace-input-label">Review</span>
                  <select class="entity-input" bind:value={agentRunForm.reviewPolicy}>
                    <option value="none">No review</option>
                    <option value="manual">Flag for review</option>
                    <option value="auto">Auto review</option>
                  </select>
                </label>
              </div>
              <label class="agent-run-prompt">
                <span class="workspace-input-label">Prompt</span>
                <textarea
                  class="entity-input"
                  rows={4}
                  bind:value={agentRunForm.prompt}
                  placeholder="Ask the external agent to inspect, plan, fix, or summarize work for this entity"
                ></textarea>
              </label>
              <div class="workspace-edit-actions">
                <button type="submit" class="entity-btn entity-btn-primary" disabled={agentRunBusy}>
                  {agentRunBusy ? 'Running…' : 'Run agent'}
                </button>
              </div>
              {#if agentRunNotice}
                <p class="workspace-notice">{agentRunNotice}</p>
              {/if}
              {#if agentRunReview}
                <p
                  class="agent-run-review"
                  class:needs-review={agentRunReview === 'Needs review'}
                >Review: {agentRunReview}</p>
              {/if}
              {#if agentRunOutput}
                <pre class="agent-run-output">{agentRunOutput}</pre>
              {/if}
            </form>
          </div>

          <div class="resource-item">
            <div class="resource-label">Files</div>
            <div class="resource-value">{entity.fileCount ?? 0} files</div>
            {#if entity.path}
              <div class="resource-meta">{entity.path}</div>
            {/if}

            <div class="file-upload-section">
              <label class="file-upload-label">
                <span class="file-upload-btn" class:busy={fileUploadBusy}>
                  {fileUploadBusy ? 'Uploading…' : 'Upload file'}
                  <input type="file" class="file-upload-input" onchange={handleUploadFile} disabled={fileUploadBusy} />
                </span>
              </label>
              {#if fileUploadError}
                <p class="file-upload-error">{fileUploadError}</p>
              {/if}

              {#if entityFiles.length > 0}
                <ul class="file-list">
                  {#each entityFiles as file}
                    <li class="file-list-item">
                      <span class="file-name">{file.filename}</span>
                      <span class="file-size">{formatFileSize(file.size_bytes)}</span>
                      <span class="file-date">{new Date(file.uploaded_at).toLocaleDateString()}</span>
                    </li>
                  {/each}
                </ul>
              {:else}
                <p class="file-list-empty">No files uploaded yet.</p>
              {/if}
            </div>
          </div>

          <div class="resource-item">
            <div class="resource-label">GitHub reference</div>
            {#if githubDomain}
              <div class="resource-value connected">{githubDomain}</div>
              <div class="resource-meta">Linked domain (not an OAuth integration)</div>
            {:else}
              <div class="resource-value muted">Add a github.com domain to link</div>
            {/if}
          </div>

          <div class="resource-item">
            <div class="resource-label">Passive channels</div>
            {#if connectedChannels.length > 0}
              <ul class="channels-list">
                {#each connectedChannels as channel}
                  <li>
                    <span>{channel.type}</span>
                    <span class="channel-status">{channel.status}</span>
                  </li>
                {/each}
              </ul>
            {:else}
              <div class="resource-value muted">None listed</div>
            {/if}
          </div>

          <div class="resource-item">
            <div class="resource-label">Bound resource scopes</div>
            {#if entity.resources?.length}
              <ul class="channels-list">
                {#each entity.resources as resource}
                  <li>
                    <span>{resource.kind}: {resource.root}</span>
                    <span class="channel-status">{resource.readAllowed ? 'r' : '-'}{resource.writeAllowed ? 'w' : '-'}{resource.execAllowed ? 'x' : '-'}</span>
                  </li>
                {/each}
              </ul>
            {:else}
              <div class="resource-value muted">No scoped resources yet</div>
            {/if}
          </div>

          {#if entity.relationships && entity.relationships.length > 0}
            <div class="resource-item">
              <div class="resource-label">Linked entities</div>
              <ul class="relationships-list">
                {#each entity.relationships as rel}
                  <li class="rel-item">
                    <span class="rel-type">{rel.type}</span>
                    <a href="/entities/{rel.target}" class="rel-target">{rel.target}</a>
                    <span class="rel-status">{rel.status}</span>
                    {#if rel.notes}<span class="rel-notes">{rel.notes}</span>{/if}
                  </li>
                {/each}
              </ul>
            </div>
          {/if}
        </div>
      </section>

        </section>
      {:else if entityTab === 'schedule'}
        <section class="entity-tab-panel entity-stack-card" id="panel-schedule">
          <h2 class="stack-card-title">Schedule</h2>
          <p class="stack-card-sub">Cron and timed runs for this entity. Uses 6- or 7-field cron (sec min hour dom mon dow [year]).</p>

          <form class="ecc-schedule-form" onsubmit={submitScheduledTask}>
            <label class="ecc-field">
              <span class="ecc-field-label">Cron expression</span>
              <input
                class="entity-input"
                type="text"
                placeholder="0 9 * * * *"
                bind:value={scheduleForm.cronExpression}
                required
              />
              <small class="ecc-field-hint">Example: <code>0 9 * * * *</code> = every day at 09:00:00</small>
            </label>

            <label class="ecc-field">
              <span class="ecc-field-label">Agent</span>
              <select class="entity-input" bind:value={scheduleForm.agentId} required>
                {#if availableAgents.length === 0}
                  <option value="" disabled>(no agents configured)</option>
                {:else}
                  {#each availableAgents as agent}
                    <option value={agent.id}>{agent.name ?? agent.id}</option>
                  {/each}
                {/if}
              </select>
            </label>

            <label class="ecc-field">
              <span class="ecc-field-label">Task prompt</span>
              <textarea
                class="entity-input"
                rows="3"
                placeholder="e.g. Summarize my goals and surface anything I should act on today."
                bind:value={scheduleForm.taskDescription}
                required
              ></textarea>
            </label>

            {#if scheduleForm.error}
              <p class="ecc-field-error">{scheduleForm.error}</p>
            {/if}

            <div class="ecc-form-actions">
              <button
                type="submit"
                class="entity-chip-link"
                disabled={scheduleForm.saving || availableAgents.length === 0}
              >
                {scheduleForm.saving ? 'Scheduling…' : 'Schedule task'}
              </button>
            </div>
          </form>

          {#if scheduleForEntity.length === 0}
            <p class="empty-foundation">No scheduled tasks for this entity yet.</p>
          {:else}
            <ul class="ecc-schedule-list">
              {#each scheduleForEntity as row}
                <li>
                  <span class="ecc-cron">{row.cronExpression}</span>
                  <span class="ecc-sched-desc">{row.taskDescription}</span>
                  <span class="ecc-sched-next">Next · {formatShortTime(row.nextRun)}</span>
                  <span class="ecc-sched-agent">Agent · {row.agentId}</span>
                  <button
                    type="button"
                    class="ecc-sched-delete"
                    onclick={() => removeScheduledTask(row.id)}
                    aria-label="Delete scheduled task"
                  >×</button>
                </li>
              {/each}
            </ul>
          {/if}
        </section>
      {:else if entityTab === 'activity'}
        <section class="entity-tab-panel entity-stack-card" id="panel-captures">
          <h2 class="stack-card-title">Raw captures</h2>
          <p class="stack-card-sub">Un-triaged captures for this entity. Promote, save, or dismiss.</p>
          {#if captureError}
            <p class="empty-foundation">{captureError}</p>
          {/if}
          {#if entityCaptures.length === 0}
            <p class="empty-foundation">No raw captures waiting.</p>
          {:else}
            <ul class="entity-capture-list">
              {#each entityCaptures as capture (capture.id)}
                <li class="entity-capture-row">
                  <div class="entity-capture-main">
                    <span class="entity-capture-text">{capture.summary || capture.raw_text}</span>
                    <span class="entity-capture-meta">{capture.source}{capture.provider ? ` · ${capture.provider}` : ''}</span>
                  </div>
                  <div class="entity-capture-actions">
                    <button type="button" class="capture-btn" disabled={captureBusyId === capture.id} onclick={() => void handleEntityCaptureAction(capture, 'task')}>Task</button>
                    <button type="button" class="capture-btn" disabled={captureBusyId === capture.id} onclick={() => void handleEntityCaptureAction(capture, 'memory')}>Memory</button>
                    <button type="button" class="capture-btn" disabled={captureBusyId === capture.id} onclick={() => void handleEntityCaptureAction(capture, 'narrative')}>Narrative</button>
                    <button type="button" class="capture-btn" disabled={captureBusyId === capture.id} onclick={() => void handleEntityCaptureAction(capture, 'dismiss')}>Dismiss</button>
                  </div>
                </li>
              {/each}
            </ul>
          {/if}
        </section>
        <section class="entity-tab-panel entity-stack-card entity-activity-feed" id="panel-timeline">
          <h2 class="stack-card-title">Timeline</h2>
          <p class="stack-card-sub">Unified activity: captures, tasks, agent runs, verification, and mission events.</p>
          {#if entityActivity.length === 0}
            <p class="empty-foundation">No activity yet.</p>
          {:else}
            <ul class="activity-feed-list">
              {#each entityActivity as it (it.id)}
                <li class="activity-feed-row">
                  <span class="oac-kind">{activityKindLabel(it)}</span>
                  <span class="oac-title">{it.title}</span>
                  <time class="oac-time" datetime={it.timestamp}>{formatShortTime(it.timestamp)}</time>
                </li>
              {/each}
            </ul>
          {/if}
        </section>
        <section class="entity-tab-panel entity-stack-card entity-activity-feed" id="panel-activity">
          <h2 class="stack-card-title">Operator feed</h2>
          <p class="stack-card-sub">Recent operator feed events for this entity.</p>
          {#if operatorFeed.length === 0}
            <p class="empty-foundation">No feed events yet.</p>
          {:else}
            <ul class="activity-feed-list">
              {#each operatorFeed as ev}
                <li class="activity-feed-row">
                  <span class="oac-kind">{feedKindLabel(ev.kind)}</span>
                  <span class="oac-title">{ev.title}</span>
                  <time class="oac-time" datetime={ev.timestamp}>{formatShortTime(ev.timestamp)}</time>
                </li>
              {/each}
            </ul>
          {/if}
        </section>
      {/if}
    </div>
  {/if}
</div>

<style>
  .entity-detail-page {
    padding: 16px 14px;
    max-width: 1120px;
    margin: 0 auto;
  }

  .onboard-panel {
    margin: 0 0 16px;
    padding: 16px;
    border: 1px solid var(--border);
    border-radius: 2px;
    background: var(--bg-card);
  }

  .onboard-head {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin-bottom: 16px;
  }

  .onboard-head-text {
    display: flex;
    align-items: baseline;
    gap: 12px;
  }

  .onboard-autofill {
    margin-left: auto;
    align-self: center;
  }

  .onboard-title {
    margin: 0;
    font-size: 15px;
    font-weight: 700;
    color: var(--text-primary);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }

  .onboard-sub {
    margin: 0;
    font-size: 12px;
    color: var(--text-muted);
  }

  .onboard-bar-track {
    height: 3px;
    background: var(--border);
    border-radius: 0;
    overflow: hidden;
  }

  .onboard-bar-fill {
    height: 100%;
    background: var(--accent-cyan);
    transition: width 0.3s ease;
  }

  .onboard-grid {
    display: grid;
    grid-template-columns: 1fr;
    gap: 6px;
  }

  @media (min-width: 520px) {
    .onboard-grid {
      grid-template-columns: repeat(2, 1fr);
    }
  }

  @media (min-width: 900px) {
    .onboard-grid {
      grid-template-columns: repeat(3, 1fr);
    }
  }

  .field-card {
    border: 1px solid var(--border);
    border-radius: 2px;
    background: rgba(0, 0, 0, 0.12);
    transition: border-color 0.15s ease;
    overflow: hidden;
  }

  .field-card:hover {
    border-color: var(--text-muted);
  }

  .field-card--filled {
    border-color: rgba(201, 138, 75, 0.2);
  }

  .field-card--editing {
    border-color: var(--accent-cyan);
    background: rgba(201, 138, 75, 0.05);
  }

  .field-card--narrative {
    grid-column: 1 / -1;
  }

  .field-card-body {
    display: flex;
    flex-direction: column;
    gap: 6px;
    width: 100%;
    padding: 10px 12px;
    background: none;
    border: none;
    text-align: left;
    cursor: pointer;
    color: inherit;
    font-family: inherit;
  }

  .field-card-top {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    padding: 0;
  }

  .field-card--editing .field-card-top {
    padding: 10px 12px 0;
  }

  .field-card-label {
    font-size: 10px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--text-muted);
  }

  .field-card-check {
    font-size: 10px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--accent-cyan);
  }

  .field-card-value {
    font-size: 13px;
    color: var(--text-primary);
    line-height: 1.4;
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }

  .field-card-empty {
    font-size: 12px;
    color: var(--text-muted);
    font-style: italic;
  }

  .field-card-input {
    display: block;
    width: calc(100% - 24px);
    margin: 6px 12px 0;
    padding: 8px 10px;
    border: 1px solid var(--border);
    border-radius: 2px;
    background: rgba(0, 0, 0, 0.2);
    color: var(--text-primary);
    font-size: 13px;
    font-family: inherit;
    resize: vertical;
    outline: none;
  }

  .field-card-input:focus {
    border-color: var(--accent-cyan);
  }

  .field-card-input::placeholder {
    color: var(--text-muted);
  }

  .field-card-actions {
    display: flex;
    gap: 6px;
    padding: 8px 12px 10px;
  }

  .field-btn {
    padding: 5px 12px;
    border: 1px solid var(--border);
    border-radius: 2px;
    background: var(--bg-card);
    color: var(--text-secondary);
    font-size: 11px;
    font-weight: 600;
    cursor: pointer;
  }

  .field-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .field-btn--save {
    border-color: var(--accent-cyan);
    background: rgba(201, 138, 75, 0.12);
    color: var(--accent-cyan);
  }

  .field-btn--skip {
    color: var(--text-muted);
  }

  .field-btn--cancel {
    color: var(--text-muted);
  }

  .entity-command-hero {
    display: flex;
    flex-direction: column;
    gap: 12px;
    margin-bottom: 10px;
    padding: 14px;
    border: 1px solid rgba(201, 138, 75, 0.18);
    border-radius: 2px;
    background: linear-gradient(180deg, rgba(201, 138, 75, 0.08), rgba(22, 19, 15, 0.3));
  }

  .entity-command-kicker {
    margin: 0;
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--text-muted);
  }

  .entity-command-title {
    margin: 2px 0 0;
    font-size: 20px;
    line-height: 1.2;
    color: var(--text-primary);
  }

  .entity-command-sub {
    margin: 6px 0 0;
    font-size: 13px;
    color: var(--text-secondary);
  }

  .entity-command-links {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }

  .entity-chip-link {
    min-height: 40px;
    padding: 8px 12px;
    border-radius: 2px;
    border: 1px solid var(--border);
    background: rgba(22, 19, 15, 0.55);
    color: var(--text-primary);
    text-decoration: none;
    font-size: 12px;
    font-weight: 600;
    display: inline-flex;
    align-items: center;
    cursor: pointer;
  }

  .entity-input {
    width: 100%;
    min-height: 44px;
    border: 1px solid rgba(201, 138, 75, 0.28);
    border-radius: 2px;
    background: rgba(22, 19, 15, 0.62);
    color: var(--text-primary);
    padding: 10px 12px;
    font-size: 16px;
  }

  .entity-input::placeholder {
    color: var(--text-muted);
  }

  .entity-input:focus {
    outline: none;
    border-color: rgba(201, 138, 75, 0.6);
    box-shadow: 0 0 0 2px rgba(201, 138, 75, 0.2);
  }

  .entity-btn {
    min-height: 44px;
    border-radius: 2px;
    padding: 9px 12px;
    font-size: 13px;
    font-weight: 700;
    border: 1px solid var(--border);
    cursor: pointer;
    color: var(--text-primary);
    background: rgba(22, 19, 15, 0.55);
  }

  .entity-btn:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }

  .entity-btn-primary {
    border-color: rgba(201, 138, 75, 0.4);
    background: linear-gradient(180deg, rgba(201, 138, 75, 0.25), rgba(22, 19, 15, 0.72));
    color: #e6fbff;
  }

  .entity-btn-secondary {
    border-color: rgba(201, 138, 75, 0.35);
    background: rgba(74, 61, 82, 0.55);
  }

  .entity-btn-ghost {
    background: transparent;
    color: var(--text-secondary);
  }

  .quick-connect-grid {
    display: grid;
    grid-template-columns: 1fr;
    gap: 8px;
    margin-bottom: 10px;
  }

  .entity-tab-shell {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin-bottom: 14px;
  }

  .entity-tab-select-label {
    font-size: 0.7rem;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--text-muted);
  }

  .entity-tab-select {
    width: 100%;
    min-height: 44px;
    padding: 10px 12px;
    border-radius: var(--radius-md);
    border: 1px solid var(--border);
    background: var(--bg-card);
    color: var(--text-primary);
    font-size: 16px;
  }

  .entity-tab-bar {
    display: none;
    flex-wrap: wrap;
    gap: 6px;
  }

  @media (min-width: 768px) {
    .entity-tab-select-label,
    .entity-tab-select {
      display: none;
    }

    .entity-tab-bar {
      display: flex;
    }
  }

  .entity-tab {
    min-height: 40px;
    padding: 8px 12px;
    border-radius: 2px;
    border: 1px solid var(--border);
    background: var(--bg-card);
    color: var(--text-secondary);
    font-size: 0.8125rem;
    font-weight: 500;
    cursor: pointer;
    white-space: nowrap;
  }

  .entity-tab.entity-tab-active {
    border-color: var(--accent-purple);
    color: var(--text-primary);
    background: rgba(201, 138, 75, 0.12);
  }

  .entity-tab-panels {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  @media (min-width: 760px) {
    .quick-connect-grid {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
  }

  .entity-tab-panel {
    min-width: 0;
  }

  .entity-narrative-panel {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .entity-narrative-intro .stack-card-title {
    margin-bottom: 4px;
  }

  .entity-tab-hint {
    margin: 0 0 8px;
    font-size: 0.8125rem;
  }

  .narrative-resources-sub {
    margin-top: 0;
  }

  .section-edit-btn {
    margin-left: auto;
    background: none;
    border: 1px solid var(--border);
    border-radius: 4px;
    padding: 2px 8px;
    font-size: 11px;
    color: var(--text-muted);
    cursor: pointer;
  }
  .section-edit-btn:hover {
    color: var(--accent-cyan);
    border-color: var(--accent-cyan);
  }

  .narrative-edit-textarea {
    width: 100%;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 10px 12px;
    font-size: 13px;
    color: var(--text-primary);
    font-family: var(--font);
    line-height: 1.5;
    resize: vertical;
    outline: none;
  }
  .narrative-edit-textarea:focus {
    border-color: var(--accent-purple);
  }

  .narrative-edit-actions {
    display: flex;
    gap: 8px;
    margin-top: 8px;
  }
  .narrative-edit-actions .btn-save {
    padding: 6px 16px;
    border-radius: 6px;
    border: 1px solid var(--accent-cyan);
    background: rgba(201, 138, 75, 0.15);
    color: var(--accent-cyan);
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
  }
  .narrative-edit-actions .btn-save:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .narrative-edit-actions .btn-cancel {
    padding: 6px 16px;
    border-radius: 6px;
    border: 1px solid var(--border);
    background: var(--bg-card);
    color: var(--text-secondary);
    font-size: 12px;
    cursor: pointer;
  }

  .ecc-sched-agent {
    font-size: 0.65rem;
    color: var(--text-muted);
  }

  .oac-kind {
    font-size: 0.65rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--accent-purple);
  }

  .oac-title {
    font-size: 0.8125rem;
    color: var(--text-primary);
    line-height: 1.35;
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }

  .oac-time {
    font-size: 0.7rem;
    color: var(--text-muted);
  }

  .connection-error {
    margin: 6px 0 0;
    font-size: 0.75rem;
    color: var(--danger, #c96a4b);
    line-height: 1.35;
  }

  .reauth-hint {
    margin: 4px 0 0;
    font-size: 0.75rem;
    color: var(--accent-purple);
    font-weight: 500;
  }

  .entity-command-center {
    margin: 0 0 16px;
    padding: 14px;
    border: 1px solid var(--border);
    border-radius: 2px;
    background: linear-gradient(135deg, rgba(201, 138, 75, 0.12), transparent 55%), var(--bg-card);
  }

  .ecc-grid {
    display: grid;
    gap: 14px;
    grid-template-columns: 1fr;
  }

  @media (min-width: 640px) {
    .ecc-grid {
      grid-template-columns: repeat(2, 1fr);
    }
  }

  @media (min-width: 1024px) {
    .ecc-grid {
      grid-template-columns: repeat(3, 1fr);
    }
  }

  .ecc-block {
    min-width: 0;
  }

  .ecc-label {
    font-size: 0.65rem;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--text-muted);
    margin-bottom: 6px;
  }

  .ecc-none {
    font-size: 0.8125rem;
    color: var(--text-muted);
    margin: 0;
  }

  .ecc-link {
    display: inline-block;
    margin-top: 10px;
    font-size: 0.8125rem;
    color: var(--accent-purple);
    text-decoration: none;
  }

  .ecc-link:hover {
    text-decoration: underline;
  }

  .ecc-mem-list {
    margin: 0;
    padding-left: 1.1rem;
    font-size: 0.75rem;
    color: var(--text-primary);
    line-height: 1.4;
  }

  .ecc-mem-list.ecc-long {
    color: var(--text-muted);
  }

  .mirimai-block {
    margin: 0;
    padding: 0.75rem;
    font-size: 0.6875rem;
    line-height: 1.35;
    overflow-x: auto;
    white-space: pre-wrap;
    word-break: break-word;
    background: var(--surface-elevated, rgba(0, 0, 0, 0.2));
    border-radius: 6px;
    max-height: min(420px, 55vh);
  }

  @media (min-width: 640px) {
    .mirimai-block {
      font-size: 0.75rem;
    }
  }

  .ecc-schedule-list {
    list-style: none;
    margin: 8px 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .ecc-schedule-list li {
    font-size: 0.75rem;
    color: var(--text-primary);
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  @media (min-width: 640px) {
    .ecc-schedule-list li {
      flex-direction: row;
      flex-wrap: wrap;
      align-items: baseline;
      gap: 8px;
    }
  }

  .ecc-cron {
    font-family: ui-monospace, monospace;
    color: var(--accent-purple);
  }

  .ecc-sched-desc {
    flex: 1;
    min-width: 0;
    word-break: break-word;
  }

  .ecc-sched-next {
    color: var(--text-muted);
    font-size: 0.7rem;
  }

  .ecc-schedule-form {
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 12px;
    margin-bottom: 16px;
    border: 1px solid var(--border);
    border-radius: 2px;
    background: rgba(22, 19, 15, 0.35);
  }

  .ecc-field {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .ecc-field-label {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--text-muted);
  }

  .ecc-field-hint {
    color: var(--text-muted);
    font-size: 11px;
  }

  .ecc-field-hint code {
    font-family: ui-monospace, monospace;
    color: var(--accent-purple);
  }

  .ecc-field-error {
    margin: 0;
    color: #c96a4b;
    font-size: 12px;
  }

  .ecc-form-actions {
    display: flex;
    gap: 8px;
    justify-content: flex-end;
  }

  .ecc-sched-delete {
    margin-left: auto;
    background: transparent;
    border: 1px solid var(--border);
    color: var(--text-muted);
    border-radius: 999px;
    width: 26px;
    height: 26px;
    line-height: 1;
    cursor: pointer;
  }

  .ecc-sched-delete:hover {
    color: #c96a4b;
    border-color: rgba(248, 113, 113, 0.6);
  }

  .channel-status.status-reauth {
    color: var(--accent-orange, #e0a55e);
    border-color: rgba(224, 165, 94, 0.45);
  }

  .badge.reauth {
    background: rgba(224, 165, 94, 0.15);
    color: var(--accent-orange, #e0a55e);
    border: 1px solid rgba(224, 165, 94, 0.4);
    font-size: 0.65rem;
    margin-left: 6px;
  }

  .page-header {
    margin-bottom: 18px;
    padding: 14px;
    border: 1px solid var(--border);
    border-radius: 2px;
    background: linear-gradient(180deg, rgba(201, 138, 75, 0.1), transparent 65%), var(--bg-card);
  }

  .back-link {
    display: inline-block;
    font-size: 13px;
    color: var(--accent-purple);
    text-decoration: none;
    margin-bottom: 8px;
  }
  .back-link:hover { text-decoration: underline; }

  .entity-title {
    font-size: 24px;
    font-weight: 700;
    color: var(--text-primary);
    margin: 0 0 8px 0;
    letter-spacing: -0.02em;
  }

  .entity-badges {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin-bottom: 8px;
  }

  .badge {
    font-size: 11px;
    font-weight: 600;
    padding: 4px 8px;
    border-radius: 2px;
    background: var(--bg-card);
    border: 1px solid var(--border);
    color: var(--text-secondary);
  }
  .badge.skills-link { text-decoration: none; color: var(--text-primary); }
  .badge.skills-link:hover { border-color: var(--accent-cyan); }
  .badge.type { border-color: var(--accent-cyan); color: var(--accent-cyan); }
  .badge.status { border-color: var(--accent-green); color: var(--accent-green); }
  .badge.stage { border-color: var(--accent-purple); color: var(--accent-purple); }
  .badge.drafting { border-color: var(--accent-cyan); color: var(--accent-cyan); }
  .badge.ready { border-color: var(--accent-green); color: var(--accent-green); }
  .badge.failed { border-color: var(--color-error, #c96a4b); color: var(--color-error, #c96a4b); }

  .badge.ai-unavailable {
    border-color: var(--accent-orange, #f97316);
    color: var(--accent-orange, #f97316);
  }

  .badge.provider-invalid {
    border-color: #ea580c;
    color: #ea580c;
  }

  .error-msg {
    color: var(--color-error, #c96a4b);
    font-size: 14px;
  }

  .loading { color: var(--text-muted); }

  .sections {
    display: flex;
    flex-direction: column;
    gap: 12px;
    scroll-margin-top: 72px;
  }

  .entity-stack-card {
    border: 1px solid var(--border);
    border-radius: 2px;
    padding: 14px;
    background: var(--bg-card);
    scroll-margin-top: 72px;
  }

  .entity-activity-feed {
    width: 100%;
  }

  .stack-card-title {
    margin: 0 0 4px 0;
    font-size: 1.05rem;
    font-weight: 700;
    color: var(--text-primary);
  }

  .stack-card-sub {
    margin: 0 0 12px 0;
    font-size: 0.8125rem;
    color: var(--text-muted);
    line-height: 1.4;
  }

  .ecc-heading {
    margin-bottom: 10px;
  }

  .ecc-section-title {
    margin: 0;
    font-size: 0.95rem;
    font-weight: 700;
  }

  .ecc-section-sub {
    margin: 4px 0 0;
    font-size: 0.75rem;
    color: var(--text-muted);
  }

  .ecc-command-grid {
    grid-template-columns: 1fr;
  }

  @media (min-width: 768px) {
    .ecc-command-grid {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
  }

  @media (min-width: 1100px) {
    .ecc-command-grid {
      grid-template-columns: repeat(3, minmax(0, 1fr));
    }
  }

  .health-list {
    margin: 0;
    padding-left: 1.1rem;
    font-size: 0.8125rem;
    color: var(--text-primary);
    line-height: 1.5;
  }

  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
    white-space: nowrap;
    border: 0;
  }

  .mission-filter-label {
    display: block;
    width: 100%;
    max-width: 280px;
  }

  .mem-grid {
    display: grid;
    gap: 14px;
    grid-template-columns: 1fr;
  }

  @media (min-width: 640px) {
    .mem-grid {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
  }

  .conn-group-title {
    margin: 14px 0 8px;
    font-size: 0.8rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--text-secondary);
  }

  .conn-group-title:first-of-type {
    margin-top: 4px;
  }

  .conn-events-mini {
    margin-top: 14px;
    padding-top: 10px;
    border-top: 1px solid var(--border);
  }

  .activity-feed-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .entity-capture-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .entity-capture-row {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    align-items: center;
    justify-content: space-between;
    padding: 10px;
    border: 1px solid var(--border);
    border-radius: 2px;
    background: rgba(0, 0, 0, 0.06);
  }

  .entity-capture-main {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
    flex: 1 1 220px;
  }

  .entity-capture-text {
    font-size: 13px;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .entity-capture-meta {
    font-size: 11px;
    color: var(--text-muted);
  }

  .entity-capture-actions {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
  }

  .entity-capture-actions .capture-btn {
    font-size: 11px;
    padding: 3px 8px;
    border: 1px solid var(--border);
    border-radius: 2px;
    background: var(--bg-card);
    color: var(--text-primary);
    cursor: pointer;
  }

  .entity-capture-actions .capture-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .activity-feed-row {
    display: grid;
    grid-template-columns: auto 1fr auto;
    gap: 10px;
    align-items: start;
    padding: 10px;
    border: 1px solid var(--border);
    border-radius: 2px;
    background: rgba(0, 0, 0, 0.06);
  }

  .resource-list {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .resource-item {
    border: 1px solid var(--border);
    border-radius: 2px;
    padding: 10px;
    background: rgba(0, 0, 0, 0.12);
  }

  .workspace-root-list {
    display: grid;
    gap: 10px;
  }

  .workspace-decision-card {
    margin-top: 12px;
    padding: 12px;
    border: 1px solid rgba(242, 237, 228, 0.14);
    border-radius: 10px;
    display: grid;
    gap: 10px;
  }

  .workspace-decision-card.needs-decision {
    border-color: rgba(224, 168, 84, 0.55);
    background: rgba(224, 168, 84, 0.07);
  }

  .workspace-decision-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 10px;
    flex-wrap: wrap;
  }

  .workspace-decision-title {
    font-size: 12px;
    font-weight: 700;
    color: var(--text-primary);
  }

  .workspace-decision-kind {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--text-muted);
    font-weight: 700;
  }

  .workspace-decision-question {
    margin: 0;
    font-size: 13px;
    color: var(--text-primary);
    line-height: 1.5;
  }

  .workspace-decision-warn {
    margin: 0;
    font-size: 12px;
    color: var(--text-secondary);
    line-height: 1.5;
  }

  .workspace-decision-path,
  .workspace-decision-git {
    display: grid;
    gap: 6px;
  }

  .workspace-decision-git {
    grid-template-columns: auto 1fr;
    align-items: center;
    gap: 8px;
    font-size: 12px;
    color: var(--text-secondary);
  }

  .workspace-decision-options {
    display: grid;
    gap: 8px;
  }

  .workspace-decision-btn {
    display: grid;
    gap: 3px;
    text-align: left;
    padding: 10px 12px;
    height: auto;
  }

  .workspace-decision-btn-label {
    font-size: 12px;
    font-weight: 700;
  }

  .workspace-decision-btn-detail {
    font-size: 11px;
    opacity: 0.75;
    line-height: 1.4;
    white-space: normal;
  }

  .workspace-edit-card {
    margin-top: 12px;
    padding-top: 12px;
    border-top: 1px solid rgba(242, 237, 228, 0.12);
    display: grid;
    gap: 10px;
  }

  .workspace-edit-title {
    font-size: 12px;
    font-weight: 700;
    color: var(--text-primary);
  }

  .workspace-edit-grid {
    display: grid;
    gap: 10px;
  }

  .workspace-edit-grid label {
    display: grid;
    gap: 6px;
  }

  .workspace-input-label {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--text-muted);
    font-weight: 700;
  }

  .workspace-edit-actions {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
  }

  .workspace-notice {
    margin: 0;
    font-size: 12px;
    color: var(--text-secondary);
  }

  .agent-run-card {
    margin-top: 12px;
    padding-top: 12px;
    border-top: 1px solid rgba(242, 237, 228, 0.12);
    display: grid;
    gap: 10px;
  }

  .agent-run-prompt {
    display: grid;
    gap: 6px;
  }

  .agent-run-review {
    margin: 0;
    font-size: 12px;
    font-weight: 700;
    color: var(--accent-green, #34d399);
  }
  .agent-run-review.needs-review {
    color: var(--accent-orange, #e0a55e);
  }
  .agent-run-output {
    max-height: 260px;
    overflow: auto;
    margin: 0;
    padding: 10px;
    border: 1px solid rgba(242, 237, 228, 0.16);
    border-radius: 6px;
    background: rgba(10, 9, 8, 0.62);
    color: var(--text-secondary);
    font-size: 12px;
    line-height: 1.5;
    white-space: pre-wrap;
    word-break: break-word;
  }

  .workspace-root-row {
    display: grid;
    gap: 4px;
    padding: 10px;
    border-radius: 10px;
    background: rgba(22, 19, 15, 0.45);
    border: 1px solid rgba(242, 237, 228, 0.12);
  }

  .workspace-root-head {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .workspace-root-name {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--text-muted);
    font-weight: 700;
  }

  .workspace-root-path {
    display: inline-flex;
    width: fit-content;
    max-width: 100%;
    padding: 6px 8px;
    border-radius: 8px;
    background: rgba(10, 9, 8, 0.55);
    border: 1px solid rgba(201, 138, 75, 0.16);
    color: #d9fbff;
    font-size: 12px;
    white-space: pre-wrap;
    word-break: break-word;
  }

  .resource-label {
    margin: 0 0 6px 0;
    font-size: 11px;
    font-weight: 700;
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }

  .resource-value {
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
    word-break: break-word;
  }

  .resource-value.connected {
    color: var(--accent-green, #7a8163);
  }

  .resource-value.muted {
    color: var(--text-muted);
    font-weight: 500;
  }

  .resource-meta {
    margin-top: 4px;
    font-size: 11px;
    color: var(--text-muted);
    word-break: break-word;
  }

  .channels-list {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .channels-list li {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    font-size: 12px;
    color: var(--text-primary);
  }

  .connection-form {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin-bottom: 10px;
  }

  .connection-auth-panel {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 10px;
    padding: 10px;
    border: 1px solid var(--border-subtle);
    border-radius: 6px;
    background: rgba(242, 237, 228, 0.04);
  }

  .connection-auth-session {
    align-items: flex-start;
  }

  .connection-auth-copy {
    display: flex;
    min-width: 0;
    flex-direction: column;
    gap: 4px;
    color: var(--text-secondary);
    font-size: 12px;
  }

  .connection-auth-copy strong {
    color: var(--text-primary);
    font-size: 13px;
  }

  .connection-auth-code {
    display: inline-flex;
    width: fit-content;
    max-width: 100%;
    padding: 5px 8px;
    border: 1px solid var(--border-subtle);
    border-radius: 4px;
    color: var(--text-primary);
    font-size: 16px;
    font-weight: 700;
  }

  .connection-auth-link {
    color: var(--accent-blue, #c98a4b);
    font-weight: 700;
  }

  .connection-row {
    display: flex;
    flex-direction: column;
    align-items: stretch;
    gap: 8px;
    padding: 8px;
    border: 1px solid var(--border-subtle);
    border-radius: 2px;
    background: rgba(0, 0, 0, 0.08);
  }

  .connection-meta {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .connection-head {
    font-size: 12px;
    font-weight: 700;
    color: var(--text-primary);
  }

  .connection-sub {
    font-size: 11px;
    color: var(--text-muted);
  }

  .webhook-url {
    font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    word-break: break-all;
  }

  .connection-actions {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 6px;
  }

  .channel-status {
    font-size: 11px;
    color: var(--text-muted);
    border: 1px solid var(--border);
    border-radius: 2px;
    padding: 2px 8px;
  }

  .sections-header {
    margin-bottom: 4px;
  }

  .sections-title {
    margin: 0 0 4px 0;
    font-size: 16px;
    font-weight: 700;
    color: var(--text-primary);
    letter-spacing: 0.01em;
  }

  .sections-subtitle {
    margin: 0;
    font-size: 12px;
    color: var(--text-muted);
    line-height: 1.45;
  }

  .sections-helper {
    margin: 6px 0 0 0;
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 11px;
    color: var(--accent-cyan);
    font-weight: 600;
  }

  .section-card {
    overflow: hidden;
    background: linear-gradient(180deg, rgba(242, 237, 228, 0.02), transparent), var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 2px;
    box-shadow: none;
  }

  .section-summary {
    list-style: none;
    display: flex;
    align-items: center;
    justify-content: flex-start;
    gap: 10px;
    min-height: 44px;
    padding: 11px 12px;
    cursor: pointer;
    background: rgba(0, 0, 0, 0.14);
    border-bottom: 1px solid transparent;
    transition: background var(--transition-base), border-color var(--transition-base);
  }

  .section-summary::-webkit-details-marker {
    display: none;
  }

  .section-summary::after {
    content: '▾';
    font-size: 12px;
    color: var(--accent-cyan);
    margin-left: auto;
    transition: transform var(--transition-base), color var(--transition-base);
  }

  .section-card[open] .section-summary {
    border-bottom-color: var(--border);
  }
  .section-card[open] .section-summary::after {
    transform: rotate(180deg);
    color: var(--accent-cyan);
  }

  .section-card:not([open]) .section-summary {
    border-bottom-color: transparent;
  }

  .section-summary:hover {
    background: rgba(0, 0, 0, 0.2);
  }

  .section-title {
    font-size: 12px;
    font-weight: 700;
    color: var(--text-primary);
    letter-spacing: 0.04em;
    margin: 0;
    text-transform: uppercase;
  }

  .section-body {
    font-size: 13px;
    line-height: 1.7;
    color: var(--text-primary);
    padding: 14px 12px;
  }

  .empty-foundation {
    color: var(--text-muted);
    font-style: italic;
  }

  @media (min-width: 720px) {
    .entity-detail-page {
      padding: 18px 20px;
    }

    .page-header {
      padding: 16px;
    }

    .sections {
      gap: 14px;
    }

    .section-summary {
      padding: 12px 14px;
    }

    .section-body {
      padding: 14px;
    }
  }

  .markdown-block {
    white-space: pre-wrap;
    word-break: break-word;
    font-family: inherit;
  }

  .relationships-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .rel-item {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
    padding: 8px 10px;
    background: rgba(242, 237, 228, 0.02);
    border-radius: var(--radius-md);
    border: 1px solid var(--border-subtle);
    font-size: 13px;
  }

  .rel-type {
    font-weight: 600;
    color: var(--text-secondary);
  }

  .rel-target {
    color: var(--accent-purple);
    text-decoration: none;
    font-weight: 600;
  }
  .rel-target:hover { text-decoration: underline; }

  .rel-status {
    font-size: 11px;
    color: var(--text-muted);
    padding: 2px 6px;
    border-radius: 4px;
    background: var(--border);
  }

  .rel-notes {
    width: 100%;
    font-size: 12px;
    color: var(--text-muted);
    margin-top: 4px;
  }

  .missions-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .mission-item {
    border: 1px solid var(--border);
    border-radius: 2px;
    padding: 10px;
    background: rgba(242, 237, 228, 0.02);
  }

  .mission-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }

  .mission-title {
    font-size: 13px;
    font-weight: 700;
    color: var(--text-primary);
  }

  .mission-status {
    font-size: 11px;
    color: var(--text-muted);
    text-transform: uppercase;
    border: 1px solid var(--border);
    border-radius: 2px;
    padding: 2px 8px;
  }

  .mission-meta {
    font-size: 12px;
    color: var(--text-secondary);
    margin: 6px 0 0 0;
  }

  .mission-summary {
    font-size: 12px;
    color: var(--text-muted);
    margin: 6px 0 0 0;
    white-space: pre-wrap;
    word-break: break-word;
  }

  .mission-task-stats {
    margin-top: 8px;
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 6px;
    font-size: 11px;
    color: var(--text-secondary);
  }

  .mission-controls {
    margin: 4px 0 10px 0;
    display: grid;
    grid-template-columns: 1fr;
    gap: 8px;
  }

  .mission-filter {
    min-height: 44px;
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    background: var(--bg-card);
    color: var(--text-primary);
    padding: 8px 10px;
    font-size: 16px;
  }

  .mission-actions {
    margin-top: 8px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .mission-actions .btn-primary,
  .mission-actions .btn-secondary {
    min-height: 44px;
    padding: 8px 12px;
    border-radius: 6px;
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
    border: 1px solid var(--border);
  }

  .mission-actions .btn-primary {
    background: var(--accent-purple);
    color: #fff;
    border-color: var(--accent-purple);
  }

  .mission-actions .btn-secondary {
    background: var(--bg-card);
    color: var(--text-secondary);
  }

  .mission-form {
    margin: 10px 0 12px 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .mission-textarea {
    min-height: 108px;
    resize: vertical;
  }

  .mission-form-row {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .mission-submit {
    min-height: 44px;
    width: 100%;
  }

  .mission-error {
    margin: 0;
    font-size: 12px;
    color: var(--color-error, #c96a4b);
  }

  .mission-notice {
    margin: 0;
    font-size: 12px;
    color: var(--text-secondary);
  }

  @media (min-width: 720px) {
    .mission-form-row {
      flex-direction: row;
      align-items: center;
    }

    .mission-submit {
      width: 100%;
      min-width: 0;
    }

    .mission-controls {
      grid-template-columns: repeat(3, minmax(0, 1fr));
    }

    .mission-actions {
      flex-direction: row;
    }
  }

  .file-upload-section {
    margin-top: 10px;
    padding-top: 10px;
    border-top: 1px solid rgba(242, 237, 228, 0.12);
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .file-upload-label {
    display: inline-block;
  }

  .file-upload-btn {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    min-height: 36px;
    padding: 6px 14px;
    border-radius: 8px;
    border: 1px solid var(--border);
    background: rgba(22, 19, 15, 0.55);
    color: var(--text-primary);
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
  }

  .file-upload-btn.busy {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .file-upload-input {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
    white-space: nowrap;
    border: 0;
  }

  .file-upload-error {
    margin: 0;
    font-size: 12px;
    color: var(--color-error, #c96a4b);
  }

  .file-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .file-list-item {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 8px 10px;
    border-radius: 8px;
    background: rgba(22, 19, 15, 0.45);
    border: 1px solid rgba(242, 237, 228, 0.12);
    font-size: 12px;
  }

  .file-name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--text-primary);
    font-weight: 600;
  }

  .file-size {
    color: var(--text-muted);
    font-family: ui-monospace, monospace;
  }

  .file-date {
    color: var(--text-muted);
    font-size: 11px;
  }

  .file-list-empty {
    margin: 0;
    font-size: 12px;
    color: var(--text-muted);
    font-style: italic;
  }

  .danger-zone {
    margin-top: 16px;
    padding: 12px;
    border: 1px solid rgba(201, 106, 75, 0.4);
    border-radius: 4px;
  }

  .danger-zone-title {
    margin: 0 0 6px;
    font-size: 12px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--color-error, #c96a4b);
  }

  .danger-zone-copy {
    margin: 0 0 10px;
    font-size: 12px;
    color: var(--text-secondary);
  }

  .danger-zone-error {
    margin: 0 0 10px;
    font-size: 12px;
    color: var(--color-error, #c96a4b);
  }

  .danger-zone-actions {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .danger-outline-btn {
    background: none;
    border: 1px solid rgba(201, 106, 75, 0.5);
    color: var(--color-error, #c96a4b);
    border-radius: 4px;
    padding: 6px 12px;
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
  }
  .danger-outline-btn:hover { background: rgba(201, 106, 75, 0.08); }

  .danger-btn {
    background: var(--color-error, #c96a4b);
    color: #fff;
    border: none;
    border-radius: 4px;
    padding: 6px 12px;
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
  }

  .danger-cancel-btn {
    background: none;
    border: 1px solid var(--border);
    color: var(--text-secondary);
    border-radius: 4px;
    padding: 6px 12px;
    font-size: 12px;
    cursor: pointer;
  }

  .danger-btn:disabled, .danger-cancel-btn:disabled { opacity: 0.6; cursor: default; }
  .mission-actions button:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }

  /* ── Suggested narrative (enrichment draft review) ── */
  .draft-review {
    margin: 0 0 16px;
    padding: 16px;
    border: 1px solid var(--border);
    border-radius: 2px;
    background: var(--bg-card);
    display: flex;
    flex-direction: column;
    gap: 12px;
  }

  .draft-review-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
  }

  .draft-review-title {
    margin: 0;
    font-size: 15px;
    font-weight: 700;
    color: var(--text-primary);
  }

  .draft-review-sub,
  .draft-review-status {
    margin: 4px 0 0;
    font-size: 12px;
    color: var(--text-muted);
  }

  .draft-review-model {
    flex-shrink: 0;
    font-size: 11px;
    color: var(--text-muted);
    border: 1px solid var(--border);
    border-radius: 2px;
    padding: 2px 8px;
  }

  .draft-review-error {
    margin: 0;
    font-size: 12px;
    color: var(--color-error, #c96a4b);
  }

  .draft-review-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .draft-row {
    border: 1px solid var(--border-subtle, var(--border));
    border-radius: 2px;
    padding: 10px;
    background: rgba(0, 0, 0, 0.08);
  }

  .draft-row--selected {
    border-color: var(--accent-cyan);
  }

  .draft-row-head {
    display: flex;
    align-items: center;
    gap: 8px;
    cursor: pointer;
    margin-bottom: 8px;
  }

  .draft-row-label {
    font-size: 13px;
    font-weight: 700;
    color: var(--text-primary);
  }

  .draft-row-change {
    margin-left: auto;
    font-size: 11px;
    color: var(--text-muted);
  }

  .draft-row-compare {
    display: grid;
    grid-template-columns: 1fr;
    gap: 8px;
  }

  .draft-col {
    min-width: 0;
  }

  .draft-col-label {
    display: block;
    font-size: 11px;
    color: var(--text-muted);
    margin-bottom: 2px;
  }

  .draft-col-text {
    margin: 0;
    font-size: 12px;
    line-height: 1.5;
    color: var(--text-secondary);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  .draft-col--suggested .draft-col-text {
    color: var(--text-primary);
  }

  .draft-col-text--empty {
    color: var(--text-muted);
    font-style: italic;
  }

  .draft-review-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }

  @media (min-width: 720px) {
    .draft-row-compare {
      grid-template-columns: 1fr 1fr;
    }
  }

  /* ── Pending approvals on the command tab ── */
  .approval-mini-list {
    list-style: none;
    margin: 0 0 8px;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .approval-mini {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 8px;
    border: 1px solid var(--border-subtle, var(--border));
    border-radius: 2px;
  }

  .approval-mini-text {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }

  .approval-mini-action {
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
  }

  .approval-mini-detail {
    font-size: 12px;
    color: var(--text-muted);
    overflow-wrap: anywhere;
  }

  .approval-mini-actions {
    display: flex;
    gap: 6px;
  }

  .approval-mini-actions .entity-btn {
    min-height: 34px;
    padding: 6px 12px;
  }

  /* ── Inline connection forms (verify a domain, edit details) ── */
  .connection-inline-form {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 10px;
    border-top: 1px solid var(--border-subtle, var(--border));
  }

  .connection-inline-copy {
    margin: 0;
    font-size: 12px;
    color: var(--text-secondary);
    overflow-wrap: anywhere;
  }

  .connection-inline-field {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .connection-token-row {
    display: flex;
    gap: 6px;
  }

  .connection-token {
    flex: 1;
    min-width: 0;
    font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    font-size: 12px;
  }

  .connection-inline-actions {
    display: flex;
    gap: 6px;
  }

  .runtime-card .workspace-edit-grid {
    margin-top: 8px;
  }
</style>
