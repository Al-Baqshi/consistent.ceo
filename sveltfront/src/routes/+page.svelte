<script lang="ts">
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import {
    api,
    answerEntityCreation,
    cancelEntityCreation,
    completeEntityCreation,
    createCapture,
    createEntityAgentRun,
    dismissCapture,
    getEntityAgentRun,
    getTenantId,
    ingestEntitySource,
    interpretAsEntityProposal,
    listCaptures,
    listTenants,
    promoteCaptureToTask,
    promoteCaptureToMemory,
    updateCaptureNarrative,
    attachCapture,
    setTenantId,
    startEntityCreation,
    streamNdjson,
    updateEntityCreationColour,
    fetchAllPages,
    type ChatAttachment,
    type PaginatedResponse,
    type RawCapture,
    type TenantSummary,
    uploadChatAttachment
  } from '$lib/api';
  import { clearApiError } from '$lib/errors';
  import {
    ensureProfile,
    isOnboarded,
    profileLoaded,
    saveProfile,
    submitOnboarding,
    userProfile,
    type WorkingStyle
  } from '$lib/userProfile';
  import ChatComposer from '$lib/components/chat/ChatComposer.svelte';
  import ChatMessageList, { type ChatBubble, type ChatAttachmentRef, type ChatEvidence } from '$lib/components/chat/ChatMessageList.svelte';
  import ChevronIcon from '$lib/components/icons/ChevronIcon.svelte';
  import Seo from '$lib/components/Seo.svelte';

  type DirectorView = {
    id: string;
    name: string;
    icon: string;
    heartbeatEnabled: boolean;
  };

  type EntityRow = {
    id: string;
    name: string;
    dept?: string;
    colour?: string | null;
    isPersonal?: boolean;
  };

  type ChatAction = {
    actionType?: string;
    action_type?: string;
    description?: string;
    resultId?: string | null;
    result_id?: string | null;
    status?: string;
  };

  type AgentActionView = {
    actionType: string;
    label: string;
    description: string;
    resultId: string | null;
    status: string;
    href: string | null;
  };

  type ChatRouting = {
    outcome: string; // 'auto_filed' | 'suggest' | 'personal'
    entityId?: string;
    entityName?: string;
    undoToken?: string;
    reason?: string;
  };

  type CaptureChatResponse = {
    message: string;
    sessionId?: string | null;
    session_id?: string | null;
    entityContext?: string | null;
    entity_context?: string | null;
    actions?: ChatAction[];
    routing?: ChatRouting | null;
    evidence?: ChatEvidence | null;
  };

  const DEFAULT_ENTITY_TINT = '#c98a4b';

  function isHexColour(value: string | null | undefined): value is string {
    return typeof value === 'string' && /^#([0-9a-f]{3}|[0-9a-f]{6})$/i.test(value);
  }

  type TaskRow = {
    id: string;
    title: string;
    status: string;
    entity_id?: string;
    mission_id?: string;
  };

  type FeedEvent = {
    id: string;
    timestamp: string;
    kind: string;
    title: string;
    taskId?: string;
    missionId?: string;
    entityId?: string;
  };

  type CommandResponse = {
    task_id: string;
    director_id: string;
    agent_id?: string | null;
    entity_id?: string | null;
  };

  type CaptureDraft = {
    entityName?: string | null;
    entityType?: string | null;
    entitySummary?: string | null;
    spaceName?: string | null;
    taskTitle?: string | null;
    taskBody?: string | null;
    chatMessage?: string | null;
  };

  type CaptureInterpretResponse = {
    intent: string;
    confidence: number;
    entityMatch?: string | null;
    candidateEntityIds?: string[];
    clarifyingQuestion?: string | null;
    draft: CaptureDraft;
  };

  type CaptureCommitResponse = {
    ok: boolean;
    entityId?: string | null;
    taskId?: string | null;
    command?: CommandResponse | null;
    message?: string | null;
    intent?: string | null;
    clarifyingQuestion?: string | null;
    candidateEntityIds?: string[];
  };

  type CreatedAction = {
    intent: ActionIntent;
    taskId?: string;
    missionId?: string;
    entityId?: string;
    interpretedIntent?: string;
    entityLabel?: string;
    directorLabel?: string;
    summary?: string;
    queued?: boolean;
  };

  type ActionIntent = 'note' | 'task' | 'mission' | 'reminder';

  const INBOX = '__all__';

  const ACTIONS: { id: ActionIntent; label: string; hint: string }[] = [
    { id: 'note', label: 'Save Note', hint: 'Capture directly as note' },
    { id: 'task', label: 'Create Task', hint: 'Capture directly as task' },
    { id: 'mission', label: 'Create Mission', hint: 'Create mission on selected entity' },
    { id: 'reminder', label: 'Schedule Reminder', hint: 'Capture directly as reminder' }
  ];

  let body = $state('');
  let attachmentMenuOpen = $state(false);
  let selectedAttachments = $state<File[]>([]);
  let uploadedAttachmentRefs = $state<ChatAttachmentRef[]>([]);
  let isRecording = $state(false);
  let mediaRecorder: MediaRecorder | null = null;
  let recorderStream: MediaStream | null = null;
  let recordingChunks: Blob[] = [];

  let interpretWithAi = $state(true);
  let capturePreview = $state<CaptureInterpretResponse | null>(null);
  let pendingCaptureAction = $state<ActionIntent | null>(null);

  let entityPick = $state<string>(INBOX);
  let entityDropdownOpen = $state(false);

  let directors = $state<DirectorView[]>([]);
  let entities = $state<EntityRow[]>([]);
  let tasks = $state<TaskRow[]>([]);
  let feed = $state<FeedEvent[]>([]);
  let captures = $state<RawCapture[]>([]);
  let captureBusyId = $state<string | null>(null);
  let tenantSummary = $state<TenantSummary | null>(null);
  /** Workspaces on this gateway. A personal install has one, and naming it
   *  ("Tenant: default") would only add noise. */
  let tenantCount = $state(0);

  let submitting = $state(false);
  let statusLine = $state<string | null>(null);
  let createdAction = $state<CreatedAction | null>(null);
  let lastAgentActions = $state<AgentActionView[]>([]);
  // Smart-routing outcome from the last Personal-Space chat turn.
  let lastRouting = $state<ChatRouting | null>(null);
  let lastRoutingText = $state<string>('');

  let chatSessionId = $state<string | null>(null);
  /**
   * Where attachments land before the gateway has assigned a session id. The
   * gateway files uploads under `uploads/chat/<session_id>/<filename>`; sharing
   * one "default" folder across every new conversation let a same-named file
   * from one chat overwrite another's. A fresh id per conversation keeps them
   * apart.
   */
  let draftUploadScope = newUploadScope();

  function newUploadScope(): string {
    const id =
      typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function'
        ? crypto.randomUUID()
        : `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
    return `draft-${id}`;
  }

  let chatHistory = $state<ChatBubble[]>([
    {
      id: 'welcome',
      role: 'assistant',
      text: "Hey, I'm Consistent. To get started, create your first entity — describe a project, brand, product, or anything you want me to help manage. For example: \"My construction management app called QS Me\" or \"A towing marketplace startup\".",
      tone: 'question',
      meta: 'Create your first entity'
    }
  ]);
  let chatRestored = $state(false);

  let voiceRepliesEnabled = $state(true);
  let voiceStatus = $state<string | null>(null);

  const ENTITY_PICK_KEY = 'consistent_world_entity_pick';

  // ----- First run -----
  //
  // Three questions, asked once, the very first time someone opens this. They
  // record who you are and what you want, and settle *how you work*: many
  // things moving at once, or one thing until it's done. That answer decides
  // how much of the workspace you're shown from here on.
  //
  // Gated on the server-side profile (`onboardedAt`), not localStorage, so it
  // survives a browser change and never nags someone who already answered. If
  // the profile can't be loaded at all we show nothing — better a missing
  // welcome than a form whose answers go nowhere.
  const FIRST_RUN_QUESTIONS = [
    {
      key: 'identity',
      title: 'Who are you, and what do you do?',
      help: "A sentence is plenty — your role, your work, what you're building.",
      placeholder: 'e.g. I run a construction firm and I’m building a quoting app on the side.'
    },
    {
      key: 'intent',
      title: 'What do you want Consistent to do for you right now?',
      help: 'The thing on your mind today, not your five-year plan.',
      placeholder: 'e.g. Stop my launches from slipping and keep follow-ups from falling through.'
    },
    {
      key: 'focus',
      title: 'What matters more?',
      help: 'There’s no wrong answer — it decides how much of the workspace you see.',
      placeholder: 'Anything else about how you like to work (optional).'
    }
  ] as const;

  let firstRunStep = $state(0);
  let firstRunBusy = $state(false);
  let firstRunError = $state<string | null>(null);
  let firstRunDismissed = $state(false);
  let answerIdentity = $state('');
  let answerIntent = $state('');
  let answerFocusText = $state('');
  let answerFocusChoice = $state<WorkingStyle | ''>('');

  const showFirstRun = $derived(
    $profileLoaded && $userProfile !== null && !isOnboarded($userProfile) && !firstRunDismissed
  );
  const firstRunCanAdvance = $derived(
    firstRunStep === 0
      ? answerIdentity.trim().length > 0
      : firstRunStep === 1
        ? answerIntent.trim().length > 0
        : answerFocusChoice !== '' || answerFocusText.trim().length > 0
  );

  let pendingInitialEntityId = $state<string | null>(null);

  type EntityColourSwatchView = { hex: string; name: string };
  type EntityCreationFlow = {
    sessionId: string;
    proposedName: string;
    proposedSummary: string | null;
    step: number; // 0 = name confirm, 1 = goals, 2 = active context
    busy: boolean;
    proposedColour: string;
    palette: EntityColourSwatchView[];
    colourBusy: boolean;
  };
  let entityCreation = $state<EntityCreationFlow | null>(null);
  let entityConfirmVisible = $state(false);
  const ENTITY_CREATION_TOTAL_STEPS = 3;
  const entityCreationStepLabels = ['Name', 'Goals', 'Active context'];

  const defaultDirectorId = $derived(directors[0]?.id ?? '');
  const hasInput = $derived(Boolean(body.trim()) || selectedAttachments.length > 0);

  const activeTenantId = $derived(getTenantId());
  const inboxLabel = $derived(
    tenantCount > 1 ? `Tenant: ${tenantSummary?.name ?? activeTenantId}` : 'All entities'
  );
  const selectedEntityLabel = $derived(
    entityPick === INBOX
      ? tenantCount > 1
        ? `All entities in ${tenantSummary?.name ?? activeTenantId}`
        : 'All entities'
      : entities.find((e) => e.id === entityPick)?.name ?? 'Unknown entity'
  );
  const activeEntityIds = $derived(
    new Set(
      tasks
        .filter((t) => t.status === 'in-progress' || t.status === 'queued')
        .map((t) => t.entity_id)
        .filter((id): id is string => !!id)
    )
  );
  const personalEntity = $derived(entities.find((e) => e.isPersonal) ?? null);
  const entityOptions = $derived([
    ...(personalEntity
      ? [{ id: personalEntity.id, label: 'Personal', active: false }]
      : []),
    { id: INBOX, label: 'All entities', active: false },
    ...entities
      .filter((entity) => !entity.isPersonal)
      .map((entity) => ({ id: entity.id, label: entity.name, active: activeEntityIds.has(entity.id) }))
  ]);
  const currentEntity = $derived(
    entityPick === INBOX ? null : entities.find((e) => e.id === entityPick) ?? null
  );
  const selectedEntityActive = $derived(entityPick !== INBOX && activeEntityIds.has(entityPick));
  // Compact review list: fresh (un-triaged) captures, scoped to the picked entity.
  const rawCaptures = $derived(
    captures
      .filter((c) => c.status === 'raw')
      .filter((c) => entityPick === INBOX || c.entity_id === entityPick)
      .slice(0, 6)
  );
  const activeTint = $derived(
    isHexColour(currentEntity?.colour) ? currentEntity!.colour! : DEFAULT_ENTITY_TINT
  );
  const tintStyle = $derived(`--entity-tint: ${activeTint};`);

  // Phase 9: heartbeat results land in a dedicated per-entity log. The chat
  // surface interleaves them as `tone: 'heartbeat'` bubbles tinted with the
  // active entity's accent (via .chat-bubble.heartbeat in ChatMessageList).
  type HeartbeatResultDto = {
    id: string;
    heartbeat_id: string;
    director_id: string;
    fired_at: string;
    summary: string;
  };
  let heartbeatResults = $state<HeartbeatResultDto[]>([]);

  function formatHeartbeatTime(iso: string): string {
    try {
      return new Date(iso).toLocaleString([], { dateStyle: 'short', timeStyle: 'short' });
    } catch {
      return iso;
    }
  }

  const heartbeatBubbles = $derived<ChatBubble[]>(
    heartbeatResults.map((r) => ({
      id: `hbr-${r.id}`,
      role: 'system',
      tone: 'heartbeat',
      text: r.summary,
      meta: `Heartbeat — ${r.director_id} director — ${formatHeartbeatTime(r.fired_at)}`
    }))
  );

  const combinedChatHistory = $derived<ChatBubble[]>(
    [...heartbeatBubbles, ...chatHistory].sort((a, b) => {
      // Heartbeat bubbles have ids like `hbr-<uuid>`; preserve their relative
      // ordering against chat by treating both as append-only — heartbeats
      // first (they are timeline events from outside the chat session), then
      // user/assistant turns. The chat history already carries its own order.
      const ah = a.id.startsWith('hbr-');
      const bh = b.id.startsWith('hbr-');
      if (ah && !bh) return -1;
      if (!ah && bh) return 1;
      return 0;
    })
  );

  async function refreshHeartbeatResults(entityId: string | null) {
    if (!entityId || entityId === INBOX) {
      heartbeatResults = [];
      return;
    }
    try {
      const rows = await api<HeartbeatResultDto[]>(
        `/api/entities/${encodeURIComponent(entityId)}/heartbeat-results`
      );
      heartbeatResults = rows ?? [];
    } catch (err) {
      console.error('heartbeat-results fetch failed', err);
    }
  }

  $effect(() => {
    const id = entityPick === INBOX ? null : entityPick;
    void refreshHeartbeatResults(id);
  });

  function pushChatBubble(bubble: ChatBubble) {
    chatHistory = [...chatHistory, bubble];
    persistChatHistory();
  }

  function captureHistoryPayload() {
    return chatHistory
      .filter((bubble) => (bubble.role === 'user' || bubble.role === 'assistant') && bubble.text.trim().length > 0)
      .map((bubble) => ({
        role: bubble.role,
        content: bubble.text
      }));
  }

  function chatStorageKey(entityId = entityPick) {
    return `consistent-world-chat:${activeTenantId}:${entityId}`;
  }

  function defaultChatHistoryFor(entityId = entityPick): ChatBubble[] {
    if (entityId !== INBOX) {
      const entity = entities.find((e) => e.id === entityId);
      const name = entity?.name ?? 'this entity';
      if (entity?.isPersonal) {
        return [
          {
            id: `welcome-${entityId}`,
            role: 'assistant',
            text: "This is your Personal Space — your private area. Dump anything here: loose thoughts, questions, half-formed ideas. It stays yours. When something clearly belongs to one of your entities I'll file it there and let you undo; when I'm not sure, I'll ask.",
            tone: 'default',
            meta: 'Personal Space'
          }
        ];
      }
      return [
        {
          id: `welcome-${entityId}`,
          role: 'assistant',
          text: `You're now working with ${name}. This is a separate chat session, so I'll keep the conversation focused on this entity's goals, tasks, memory, and operating context.`,
          tone: 'default',
          meta: 'Entity workspace'
        }
      ];
    }
    if (entities.length === 0) {
      return [
        {
          id: 'welcome',
          role: 'assistant',
          text: "Hey, I'm Consistent. To get started, create your first entity — describe a project, brand, product, or anything you want me to help manage. For example: \"My construction management app called QS Me\" or \"A towing marketplace startup\".",
          tone: 'question',
          meta: 'Create your first entity'
        }
      ];
    }
    return [
      {
        id: 'welcome-inbox',
        role: 'assistant',
        text: "You're in the workspace inbox. Use this when you're not sure which entity something belongs to, or switch to an entity when you want a focused session.",
        tone: 'default',
        meta: inboxLabel
      }
    ];
  }

  function persistChatHistory(entityId = entityPick) {
    if (typeof window === 'undefined') return;
    try {
      window.localStorage.setItem(
        chatStorageKey(entityId),
        JSON.stringify({ sessionId: chatSessionId, history: chatHistory })
      );
    } catch {
      // ignore
    }
  }

  function restoreChatHistoryFor(entityId = entityPick) {
    chatSessionId = null;
    draftUploadScope = newUploadScope();
    chatHistory = defaultChatHistoryFor(entityId);
    if (typeof window === 'undefined') return;
    try {
      const raw = window.localStorage.getItem(chatStorageKey(entityId));
      if (!raw) return;
      const parsed = JSON.parse(raw) as { sessionId?: string | null; history?: ChatBubble[] } | ChatBubble[];
      if (Array.isArray(parsed)) {
        if (parsed.length > 0) chatHistory = parsed;
        return;
      }
      if (parsed?.sessionId) chatSessionId = parsed.sessionId;
      if (Array.isArray(parsed?.history) && parsed.history.length > 0) chatHistory = parsed.history;
    } catch {
      // ignore
    }
  }

  function resetTransientChatState() {
    capturePreview = null;
    pendingCaptureAction = null;
    createdAction = null;
    lastAgentActions = [];
    statusLine = null;
    selectedAttachments = [];
    uploadedAttachmentRefs = [];
    body = '';
  }

  function activateChatContext(entityId: string) {
    persistChatHistory(entityPick);
    entityPick = entityId;
    entityDropdownOpen = false;
    resetTransientChatState();
    restoreChatHistoryFor(entityId);
    void refreshHeartbeatResults(entityId === INBOX ? null : entityId);
    try { localStorage.setItem(ENTITY_PICK_KEY, entityId); } catch {}
  }

  function speakReply(text: string) {
    if (typeof window === 'undefined') return;
    if (!voiceRepliesEnabled || !('speechSynthesis' in window)) return;
    window.speechSynthesis.cancel();
    const utterance = new SpeechSynthesisUtterance(text);
    utterance.rate = 1;
    utterance.pitch = 1;
    utterance.onstart = () => { voiceStatus = 'Speaking reply…'; };
    utterance.onend = () => { voiceStatus = null; };
    utterance.onerror = () => { voiceStatus = 'Voice playback unavailable.'; };
    window.speechSynthesis.speak(utterance);
  }

  function addAssistantBubble(
    text: string,
    tone: ChatBubble['tone'] = 'default',
    meta?: string,
    evidence?: ChatEvidence | null
  ) {
    const clean = text.trim();
    if (!clean) return;
    pushChatBubble({
      id: `assistant-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
      role: 'assistant',
      text: clean,
      tone,
      meta,
      evidence
    });
    speakReply(clean);
  }

  /** Replace one bubble's text in place while tokens stream in. Deliberately
   *  skips persistChatHistory: per-token localStorage writes are wasteful and
   *  the finalized turn is persisted once at the end. */
  function updateStreamingBubble(id: string, text: string) {
    const idx = chatHistory.findIndex((b) => b.id === id);
    if (idx === -1) return;
    const next = chatHistory.slice();
    next[idx] = { ...next[idx], text };
    chatHistory = next;
  }

  /** Swap the streamed draft for the authoritative final message (they differ
   *  when the turn looped through tools), then persist and speak once. */
  function finalizeStreamedBubble(
    id: string,
    text: string,
    meta?: string,
    evidence?: ChatEvidence | null
  ) {
    const idx = chatHistory.findIndex((b) => b.id === id);
    if (idx === -1) {
      addAssistantBubble(text, 'default', meta, evidence);
      return;
    }
    const next = chatHistory.slice();
    next[idx] = { ...next[idx], text, meta: meta ?? next[idx].meta, evidence };
    chatHistory = next;
    persistChatHistory();
    speakReply(text);
  }

  /**
   * One chat turn against the gateway: tokens are streamed into a live bubble
   * when the streaming endpoint is available, with a transparent fallback to
   * the blocking endpoint (older gateway, buffering proxy, transport error).
   * Returns the final payload plus the id of the already-rendered bubble, so
   * the caller knows whether it still needs to add one.
   */
  async function sendChatTurn(
    chatBody: Record<string, unknown>,
    bubbleMeta?: string
  ): Promise<{ response: CaptureChatResponse; streamedBubbleId: string | null }> {
    let bubbleId: string | null = null;
    let streamedText = '';
    let donePayload: CaptureChatResponse | null = null;
    try {
      await streamNdjson('/api/capture/chat/stream', chatBody, (line) => {
        const ev = line as { type?: string; sessionId?: string; text?: string; payload?: CaptureChatResponse };
        if (ev.type === 'meta' && ev.sessionId) {
          chatSessionId = ev.sessionId;
        } else if (ev.type === 'token' && ev.text) {
          streamedText += ev.text;
          if (bubbleId) {
            updateStreamingBubble(bubbleId, streamedText);
          } else {
            bubbleId = `assistant-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
            pushChatBubble({ id: bubbleId, role: 'assistant', text: streamedText, tone: 'default', meta: bubbleMeta });
          }
        } else if (ev.type === 'done' && ev.payload) {
          donePayload = ev.payload;
        }
      });
    } catch (err) {
      if (bubbleId === null && donePayload === null) {
        // Nothing rendered yet — the blocking endpoint can still save the turn.
        const response = await api<CaptureChatResponse>('/api/capture/chat', {
          method: 'POST',
          body: JSON.stringify(chatBody)
        });
        return { response, streamedBubbleId: null };
      }
      throw err;
    }
    if (donePayload) {
      return { response: donePayload, streamedBubbleId: bubbleId };
    }
    if (bubbleId !== null && streamedText.trim()) {
      // Stream closed without a final frame; keep what the user already read.
      return { response: { message: streamedText }, streamedBubbleId: bubbleId };
    }
    const response = await api<CaptureChatResponse>('/api/capture/chat', {
      method: 'POST',
      body: JSON.stringify(chatBody)
    });
    return { response, streamedBubbleId: null };
  }

  function handleChatActions(actions: ChatAction[] | undefined) {
    lastAgentActions = normalizeAgentActions(actions);
    if (!actions?.length || typeof window === 'undefined') return;
    const workspaceAction = actions.find((action) => (action.actionType ?? action.action_type) === 'create_workspace' && action.status !== 'failed');
    if (workspaceAction) {
      const workspaceId = workspaceAction.resultId ?? workspaceAction.result_id ?? null;
      if (workspaceId) setTenantId(workspaceId);
      window.dispatchEvent(new CustomEvent('consistent-world:workspaces-changed', { detail: { workspaceId } }));
    }
  }

  function normalizeAgentActions(actions: ChatAction[] | undefined): AgentActionView[] {
    return (actions ?? []).map((action) => {
      const actionType = action.actionType ?? action.action_type ?? 'action';
      const resultId = action.resultId ?? action.result_id ?? null;
      const status = action.status ?? 'completed';
      return {
        actionType,
        label: actionType.replaceAll('_', ' '),
        description: action.description ?? actionType.replaceAll('_', ' '),
        resultId,
        status,
        href: actionHref(actionType, resultId)
      };
    });
  }

  function actionHref(actionType: string, resultId: string | null): string | null {
    if (!resultId) return null;
    if (actionType === 'create_note' || actionType === 'create_task' || actionType === 'set_reminder') {
      return `/tasks?task_id=${encodeURIComponent(resultId)}`;
    }
    if (actionType === 'create_entity' || actionType === 'entity_update') {
      return `/entities/${encodeURIComponent(resultId)}`;
    }
    if (actionType === 'create_mission' || actionType === 'mission_update') {
      return `/missions/${encodeURIComponent(resultId)}`;
    }
    if (actionType === 'task_update') {
      return `/tasks?task_id=${encodeURIComponent(resultId)}`;
    }
    if (actionType.includes('heartbeat') && entityPick !== INBOX) {
      return `/entities/${encodeURIComponent(entityPick)}`;
    }
    if (actionType === 'create_workspace') return null;
    return null;
  }

  function deriveTitle(text: string, prefix: string): string {
    const raw = text.trim();
    if (!raw) return prefix || 'Untitled';
    const line = raw.split('\n')[0] ?? raw;
    const base = `${prefix}: ${line}`;
    return base.length > 220 ? base.slice(0, 217) + '...' : base;
  }

  function buildPrompt(): string {
    const raw = body.trim();
    const attachmentNames = selectedAttachments.map((f) => f.name);
    if (!raw && attachmentNames.length === 0) return '';
    if (!raw && attachmentNames.length > 0) return `Attachments provided:\n- ${attachmentNames.join('\n- ')}`;
    if (!raw) return '';
    return attachmentNames.length > 0 ? `${raw}\n\nAttachments:\n- ${attachmentNames.join('\n- ')}` : raw;
  }

  function autoResizeCaptureInput(event: Event) {
    const target = event.currentTarget;
    if (!(target instanceof HTMLTextAreaElement)) return;
    target.style.height = 'auto';
    const next = Math.min(Math.max(target.scrollHeight, 38), 220);
    target.style.height = `${next}px`;
  }

  function openAttachmentPicker(_kind: 'image' | 'video' | 'document' | 'folder') {
    attachmentMenuOpen = false;
  }

  function handleBodyInput(next: string) {
    body = next;
  }

  async function handleAttachmentChange(event: Event) {
    const target = event.currentTarget;
    if (!(target instanceof HTMLInputElement) || !target.files) return;
    const incoming = Array.from(target.files);
    if (incoming.length === 0) return;
    const merged = [...selectedAttachments];
    for (const file of incoming) {
      const exists = merged.some(
        (current) =>
          current.name === file.name &&
          current.size === file.size &&
          current.type === file.type &&
          current.lastModified === file.lastModified
      );
      if (!exists) merged.push(file);
    }
    selectedAttachments = merged;
    target.value = '';

    for (const file of incoming) {
      const exists = uploadedAttachmentRefs.some((ref) => ref.filename === file.name && ref.size_bytes === file.size);
      if (!exists) {
        try {
          const uploaded = await uploadChatAttachment(chatSessionId ?? draftUploadScope, file);
          uploadedAttachmentRefs = [...uploadedAttachmentRefs, {
            id: uploaded.id,
            filename: uploaded.filename,
            content_type: uploaded.content_type,
            size_bytes: uploaded.size_bytes
          }];
        } catch (err) {
          statusLine = err instanceof Error ? err.message : 'Failed to upload attachment.';
        }
      }
    }
  }

  function removeAttachment(index: number) {
    selectedAttachments = selectedAttachments.filter((_, i) => i !== index);
    uploadedAttachmentRefs = uploadedAttachmentRefs.filter((_, i) => i !== index);
  }

  async function toggleVoiceRecording() {
    if (isRecording && mediaRecorder) {
      mediaRecorder.stop();
      isRecording = false;
      statusLine = 'Voice recording saved.';
      return;
    }
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
      recorderStream = stream;
      recordingChunks = [];
      mediaRecorder = new MediaRecorder(stream);
      mediaRecorder.ondataavailable = (event) => {
        if (event.data.size > 0) recordingChunks.push(event.data);
      };
      mediaRecorder.onstop = () => {
        const ext = mediaRecorder?.mimeType.includes('ogg') ? 'ogg' : 'webm';
        const blob = new Blob(recordingChunks, { type: mediaRecorder?.mimeType || 'audio/webm' });
        const file = new File([blob], `voice-${new Date().toISOString().replace(/[:.]/g, '-')}.${ext}`, {
          type: blob.type
        });
        selectedAttachments = [...selectedAttachments, file];
        recorderStream?.getTracks().forEach((track) => track.stop());
        recorderStream = null;
        mediaRecorder = null;
      };
      mediaRecorder.start();
      isRecording = true;
      statusLine = 'Recording voice… tap mic again to stop.';
    } catch (err) {
      isRecording = false;
      statusLine = err instanceof Error ? err.message : 'Microphone access failed.';
    }
  }

  async function loadAll() {
    try {
      const [d, e, t, f, tenantRows] = await Promise.all([
        api<DirectorView[]>('/api/directors').catch(() => []),
        // All entities (the picker), and only the live tasks — that is all
        // `activeEntityIds` reads. A bare list is the oldest 50 of each.
        fetchAllPages<EntityRow>('/api/entities').catch(() => [] as EntityRow[]),
        Promise.all([
          fetchAllPages<TaskRow>('/api/tasks?status=in-progress'),
          fetchAllPages<TaskRow>('/api/tasks?status=queued')
        ])
          .then(([running, queued]) => [...running, ...queued])
          .catch(() => [] as TaskRow[]),
        api<PaginatedResponse<FeedEvent>>('/api/operator-feed?limit=16').then(r => r.items ?? []).catch(() => []),
        listTenants().catch(() => [])
      ]);
      directors = d ?? [];
      entities = e ?? [];
      tasks = t ?? [];
      feed = f ?? [];
      tenantSummary = (tenantRows ?? []).find((tenant) => tenant.id === activeTenantId) ?? null;
      tenantCount = (tenantRows ?? []).length;
      if (pendingInitialEntityId) {
        const exists = entities.some((row) => row.id === pendingInitialEntityId);
        if (exists) entityPick = pendingInitialEntityId;
        pendingInitialEntityId = null;
      } else if (entityPick === INBOX) {
        // Every user is an entity: default a fresh session to their private
        // Personal Space rather than the shared inbox.
        const personal = entities.find((row) => row.isPersonal);
        if (personal) entityPick = personal.id;
      }
      if (!chatRestored) {
        restoreChatHistoryFor(entityPick);
        chatRestored = true;
      }
      void loadCaptures();
      clearApiError();
    } catch {
      // keep home quiet when backend is unavailable
    }
  }

  async function loadCaptures() {
    try {
      const res = await listCaptures({
        status: 'raw',
        limit: 20,
        with_route: true,
        selected_entity_id: entityPick === INBOX ? undefined : entityPick,
      });
      captures = res.items ?? [];
    } catch {
      // keep home quiet when backend is unavailable
    }
  }

  /**
   * Persist every submitted user message as a raw capture *before* any
   * interpretation, source ingest, or CLI agent run. Best-effort: a capture
   * failure must never block the chat.
   */
  async function recordRawCapture(text: string) {
    const raw = text.trim();
    if (!raw) return;
    try {
      await createCapture({
        raw_text: raw,
        entity_id: entityPick === INBOX ? undefined : entityPick,
        source: 'chat',
        session_id: chatSessionId ?? undefined
      });
      void loadCaptures();
    } catch {
      // swallow: capture is a side-channel, not part of the send path
    }
  }

  async function handleDismissCapture(id: string) {
    captureBusyId = id;
    try {
      await dismissCapture(id);
      captures = captures.filter((c) => c.id !== id);
    } catch (err) {
      statusLine = parseApiError(err, 'Could not dismiss that capture.');
    } finally {
      captureBusyId = null;
    }
  }

  async function handlePromoteCapture(id: string) {
    captureBusyId = id;
    try {
      await promoteCaptureToTask(id);
      captures = captures.filter((c) => c.id !== id);
      statusLine = 'Created a task from that capture.';
      void loadAll();
    } catch (err) {
      statusLine = parseApiError(err, 'Could not create a task from that capture.');
    } finally {
      captureBusyId = null;
    }
  }

  // For an unresolved inbox capture, attach it to the currently selected entity
  // (when one is picked) so review actions have somewhere to file the note.
  function captureTargetEntityId(capture: RawCapture): string | undefined {
    return capture.entity_id ?? (entityPick === INBOX ? undefined : entityPick);
  }

  async function attachCaptureToEntity(capture: RawCapture, entityId: string) {
    if (!entityId) return;
    captureBusyId = capture.id;
    try {
      const updated = await attachCapture(capture.id, entityId);
      captures = captures.map((c) => (c.id === capture.id ? { ...c, ...updated } : c));
      statusLine = `Attached that capture to ${entityLabelById(entityId)}.`;
    } catch (err) {
      statusLine = parseApiError(err, 'Could not attach that capture.');
    } finally {
      captureBusyId = null;
    }
  }

  function handleManualAttach(capture: RawCapture, event: Event) {
    const entityId = (event.target as HTMLSelectElement).value;
    if (entityId) void attachCaptureToEntity(capture, entityId);
  }

  // File the last loose message under a suggested entity (creates a capture there).
  async function routingFileUnder(entityId: string, entityName?: string) {
    try {
      await createCapture({
        raw_text: lastRoutingText,
        entity_id: entityId,
        source: 'chat',
        session_id: chatSessionId ?? undefined
      });
      statusLine = `Filed under ${entityName ?? 'that entity'}.`;
      void loadCaptures();
    } catch (err) {
      statusLine = parseApiError(err, 'Could not file that.');
    }
    lastRouting = null;
  }

  // Undo an auto-file: move the created capture back to the Personal Space.
  async function routingUndo() {
    const token = lastRouting?.undoToken;
    const personal = personalEntity?.id;
    if (token && personal) {
      try {
        await attachCapture(token, personal);
        statusLine = 'Moved back to your Personal Space.';
        void loadCaptures();
      } catch (err) {
        statusLine = parseApiError(err, 'Could not undo that.');
      }
    }
    lastRouting = null;
  }

  function routingDismiss() {
    lastRouting = null;
  }

  async function handleSaveMemory(capture: RawCapture) {
    captureBusyId = capture.id;
    try {
      await promoteCaptureToMemory(capture.id, {
        entity_id: captureTargetEntityId(capture),
        kind: 'canonical',
      });
      captures = captures.filter((c) => c.id !== capture.id);
      statusLine = 'Saved that capture to memory.';
    } catch (err) {
      statusLine = parseApiError(err, 'Could not save that capture to memory.');
    } finally {
      captureBusyId = null;
    }
  }

  async function handleUpdateNarrative(capture: RawCapture) {
    const entityId = captureTargetEntityId(capture);
    if (!entityId) {
      statusLine = 'Pick an entity before updating its narrative.';
      return;
    }
    captureBusyId = capture.id;
    try {
      await updateCaptureNarrative(capture.id, { entity_id: entityId });
      captures = captures.filter((c) => c.id !== capture.id);
      statusLine = 'Added that capture to the entity narrative.';
    } catch (err) {
      statusLine = parseApiError(err, 'Could not update the narrative.');
    } finally {
      captureBusyId = null;
    }
  }

  async function resolveEntityId(): Promise<string | null> {
    if (entityPick === INBOX) return null;
    return entityPick;
  }

  function entityLabelById(id: string): string {
    return entities.find((e) => e.id === id)?.name ?? id;
  }

  function directorLabelById(id?: string | null): string | undefined {
    if (!id) return undefined;
    return directors.find((d) => d.id === id)?.name ?? id;
  }

  function captureSummary(intent: string, action: ActionIntent, entityLabel?: string): string {
    const target = entityLabel ? ` for ${entityLabel}` : '';
    switch (intent) {
      case 'create_task':
        return action === 'note'
          ? `Saved a note${target}.`
          : action === 'reminder'
            ? `Created a reminder${target}.`
            : `Created a task${target}.`;
      case 'create_entity':
      case 'create_space':
      case 'create_entity_and_space':
        return `Created ${entityLabel ?? 'a new IP'} and opened it for onboarding.`;
      case 'chat':
        return `Sent this to Operations${target ? ` about ${entityLabel}` : ''}.`;
      default:
        return `Processed your input${target}.`;
    }
  }

  function mergeDraftForCommit(rawText: string): CaptureDraft {
    if (!capturePreview) return {};
    const d = capturePreview.draft ?? {};
    const text = rawText.trim();
    return {
      entityName: d.entityName ?? undefined,
      entityType: d.entityType ?? undefined,
      entitySummary: d.entitySummary ?? undefined,
      spaceName: d.spaceName ?? undefined,
      taskTitle: d.taskTitle ?? deriveTitle(text, 'Task'),
      taskBody: d.taskBody ?? text,
      chatMessage: d.chatMessage ?? d.taskBody ?? text
    };
  }

  function cancelCapturePreview() {
    capturePreview = null;
    pendingCaptureAction = null;
    statusLine = null;
    addAssistantBubble('No problem. Adjust your message or pick a different entity and we can try again.', 'default', 'Capture cancelled');
  }

  async function runAiCapturePipeline(action: Extract<ActionIntent, 'note' | 'task' | 'reminder'>) {
    capturePreview = null;
    pendingCaptureAction = null;
    statusLine = null;
    createdAction = null;
    const prompt = buildPrompt();
    if (!prompt) {
      statusLine = 'Write or paste a message first.';
      return;
    }
    pendingCaptureAction = action;
    const entityId = await resolveEntityId();
    try {
      capturePreview = await api<CaptureInterpretResponse>('/api/capture/interpret', {
        method: 'POST',
        body: JSON.stringify({ text: prompt, entityId: entityId ?? undefined, requestedAction: action })
      });
      statusLine = capturePreview.intent === 'clarify'
        ? capturePreview.clarifyingQuestion || 'More detail needed — pick an entity or rephrase.'
        : 'I understood that and prepared the next step.';
      if (capturePreview.intent === 'clarify') {
        addAssistantBubble(
          capturePreview.clarifyingQuestion || 'I need a bit more detail before I can place this correctly.',
          'question',
          'Needs clarification'
        );
      } else {
        addAssistantBubble('I interpreted that and prepared the next step. Review it below and confirm if it looks right.', 'default', 'Draft ready');
      }
    } catch (err) {
      pendingCaptureAction = null;
      capturePreview = null;
      statusLine = parseApiError(err, 'Interpret failed. Check gateway and AI settings.');
      addAssistantBubble(statusLine, 'question', 'Interpret failed');
    }
  }

  async function confirmCaptureCommit() {
    if (!capturePreview || !pendingCaptureAction) return;
    const promptSnapshot = buildPrompt();
    const rawText = body.trim();
    const textBasis = promptSnapshot || rawText;
    const directorId = defaultDirectorId;
    if (!directorId) {
      statusLine = 'No director is available yet. Open Team setup first.';
      return;
    }
    const pick = capturePreview.intent;
    if (pick === 'clarify') {
      statusLine = capturePreview.clarifyingQuestion || 'Choose an entity or rephrase.';
      return;
    }
    const mergedDraft = mergeDraftForCommit(textBasis);
    const entityId =
      (capturePreview.entityMatch && capturePreview.entityMatch.trim()) ||
      (entityPick !== INBOX ? entityPick : '') ||
      undefined;
    const taskKind =
      pendingCaptureAction === 'note'
        ? 'note'
        : pendingCaptureAction === 'reminder'
          ? 'reminder'
          : 'task';
    submitting = true;
    try {
      const res = await api<CaptureCommitResponse>('/api/capture/commit', {
        method: 'POST',
        body: JSON.stringify({
          intent: pick,
          draft: mergedDraft,
          entityId,
          directorId,
          taskKind: pick === 'create_task' ? taskKind : undefined,
          captureIntent: pick === 'chat' ? pendingCaptureAction : undefined,
          clarifyingQuestion: capturePreview.clarifyingQuestion ?? undefined,
          candidateEntityIds: capturePreview.candidateEntityIds ?? []
        })
      });
      if (!res.ok) {
        statusLine = res.message ?? 'Nothing was committed.';
        addAssistantBubble(statusLine, 'question', 'Commit blocked');
        return;
      }
      const saved = pendingCaptureAction;
      capturePreview = null;
      pendingCaptureAction = null;
      body = '';
      selectedAttachments = [];
      const taskId = res.taskId ?? res.command?.task_id ?? undefined;
      const entId = res.entityId ?? res.command?.entity_id ?? entityId ?? undefined;
      const resolvedEntityLabel = entId ? entityLabelById(entId) : undefined;
      const resolvedDirectorLabel = directorLabelById(res.command?.director_id ?? directorId);
      const interpretedIntent = pick;
      createdAction = {
        intent: saved,
        taskId,
        entityId: entId || undefined,
        interpretedIntent,
        entityLabel: resolvedEntityLabel,
        directorLabel: resolvedDirectorLabel,
        summary: captureSummary(interpretedIntent, saved, resolvedEntityLabel),
        queued: Boolean(taskId)
      };
      if (pick === 'create_entity' || pick === 'create_space' || pick === 'create_entity_and_space') {
        statusLine = 'Created.';
        addAssistantBubble(createdAction?.summary ?? 'Created a new entity and added it to the tenant.', 'success', selectedEntityLabel);
        if (createdAction.entityId) await goto(`/entities/${createdAction.entityId}`);
      } else if (pick === 'create_task') {
        statusLine = createdAction?.summary ?? 'Task created.';
        addAssistantBubble(statusLine, 'success', createdAction?.entityLabel ?? selectedEntityLabel);
      } else {
        statusLine = createdAction?.summary ?? 'Sent.';
        addAssistantBubble(statusLine, 'success', createdAction?.directorLabel ?? 'Operations');
      }
      await loadAll();
      clearApiError();
    } catch (err) {
      statusLine = parseApiError(err, 'Commit failed. Try again.');
      addAssistantBubble(statusLine, 'question', 'Commit failed');
    } finally {
      submitting = false;
    }
  }

  async function runCreateTask(prefix: string, taskKind: 'note' | 'task' | 'reminder'): Promise<CreatedAction | null> {
    const entityId = await resolveEntityId();
    const task = await api<{ id: string; entity_id?: string | null }>('/api/tasks', {
      method: 'POST',
      body: JSON.stringify({
        title: deriveTitle(body, prefix),
        dept: 'Capture',
        entity_id: entityId ?? undefined,
        task_kind: taskKind,
        content: body.trim()
      })
    });
    return {
      intent: taskKind,
      taskId: task.id,
      entityId: (task.entity_id ?? entityId ?? undefined) || undefined
    };
  }

  async function runCreateMission(): Promise<CreatedAction | null> {
    const entityId = await resolveEntityId();
    if (!entityId) {
      statusLine = 'Missions require an entity. Pick one from the dropdown.';
      return null;
    }
    const mission = await api<{ id: string }>('/api/missions', {
      method: 'POST',
      body: JSON.stringify({
        entity_id: entityId,
        title: deriveTitle(body, 'Mission'),
        summary: body.trim().slice(0, 800),
        goal: body.trim().slice(0, 2000),
        priority: 'medium'
      })
    });
    await goto('/missions/' + mission.id);
    return { intent: 'mission', missionId: mission.id, entityId };
  }

  function resetComposerInput() {
    body = '';
    selectedAttachments = [];
    uploadedAttachmentRefs = [];
    const textarea = document.getElementById('capture-input') as HTMLTextAreaElement | null;
    if (textarea) {
      textarea.value = '';
      textarea.style.height = 'auto';
    }
  }

  async function sendConversationalChat() {
    statusLine = null;
    createdAction = null;
    lastAgentActions = [];
    lastRouting = null;
    const outgoing = buildPrompt() || body.trim();
    if (!outgoing) {
      statusLine = 'Write something first.';
      return;
    }

    const attachmentsForBubble = [...uploadedAttachmentRefs];
    // Capture history before the optimistic user bubble is appended. The
    // gateway also defends against duplicates, but keeping the payload clean
    // avoids charging the model for the same turn twice.
    const historyForRequest = captureHistoryPayload();

    pushChatBubble({
      id: `user-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
      role: 'user',
      text: outgoing,
      meta: entityPick === INBOX ? inboxLabel : selectedEntityLabel,
      attachments: attachmentsForBubble.length > 0 ? attachmentsForBubble : undefined
    });

    resetComposerInput();
    submitting = true;
    try {
      if (interpretWithAi) {
        capturePreview = null;
        pendingCaptureAction = null;
        const { response, streamedBubbleId } = await sendChatTurn(
          {
            message: outgoing,
            entityId: entityPick === INBOX ? undefined : entityPick,
            scope: entityPick === INBOX ? 'workspace' : 'entity',
            timezoneOffsetMinutes: typeof Date !== 'undefined' ? -new Date().getTimezoneOffset() : undefined,
            history: historyForRequest,
            sessionId: chatSessionId ?? undefined,
            workspaceContext: `workspace:${activeTenantId}`
          },
          entityPick === INBOX ? inboxLabel : selectedEntityLabel
        );
        chatSessionId = response.sessionId ?? response.session_id ?? chatSessionId;
        const reply = response.message?.trim() || 'I got that.';
        handleChatActions(response.actions);
        const replyMeta = response.entityContext ?? response.entity_context ?? undefined;
        const replyEvidence = response.evidence ?? null;
        if (streamedBubbleId) {
          finalizeStreamedBubble(streamedBubbleId, reply, replyMeta, replyEvidence);
        } else {
          addAssistantBubble(reply, 'default', replyMeta, replyEvidence);
        }
        lastRouting = response.routing ?? null;
        lastRoutingText = outgoing;
      } else {
        addAssistantBubble("I got that. AI interpretation is off right now, but I'm here and ready when you want to turn this into work.", 'default');
      }

      await loadAll();
      clearApiError();
    } catch (err) {
      statusLine = parseApiError(err, 'Chat reply failed. Check gateway and AI settings.');
      if (isNoProviderError(err)) {
        addAssistantBubble(
          "I don't have a working AI model to think with yet. Open AI settings (the gear icon in the sidebar) and connect one — Claude Code or Codex if you have a subscription, Ollama for fully local, or an API key. I'll be ready the moment one is connected.",
          'question',
          'Connect an AI model'
        );
      } else {
        addAssistantBubble(`I couldn't reply properly just then: ${statusLine}`, 'question');
      }
    } finally {
      submitting = false;
    }
  }

  /** A 502/503 from the chat endpoints means "no usable model", not "broken app"
   *  — first-run machines without Claude Code or Ollama land here. */
  function isNoProviderError(err: unknown): boolean {
    const msg = err instanceof Error ? err.message : '';
    return msg.includes('502') || msg.includes('503');
  }

  function firstSourceUrl(text: string): string | null {
    const match = text.match(/https?:\/\/[^\s<>"')]+/i);
    return match?.[0]?.replace(/[.,;:!?]+$/, '') ?? null;
  }

  async function ingestSourceFromChat(outgoing: string, sourceUrl: string): Promise<boolean> {
    pushChatBubble({
      id: `user-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
      role: 'user',
      text: outgoing,
      meta: entityPick === INBOX ? inboxLabel : selectedEntityLabel
    });
    resetComposerInput();
    submitting = true;
    const attaching = entityPick !== INBOX;
    addAssistantBubble(
      attaching
        ? 'I’ll connect that source to this entity and update the working narrative from it.'
        : 'I’ll read that source, create the entity, and build the first operating narrative from it.',
      'default',
      attaching ? selectedEntityLabel : 'Source import'
    );
    try {
      const result = await ingestEntitySource({
        url: sourceUrl,
        entityId: attaching ? entityPick : undefined
      });
      await loadAll();
      if (result.created && result.entity?.id) {
        activateChatContext(result.entity.id);
      }
      const entityName = result.entity?.name ?? 'the entity';
      addAssistantBubble(
        result.created
          ? `**${entityName}** is live. I imported the source, built the initial narrative, and selected it as the active chat context.`
          : `I attached that source to **${entityName}** and refreshed the entity narrative without overwriting existing confirmed fields.`,
        'success',
        result.created ? 'Entity created from source' : 'Source attached'
      );
      clearApiError();
      return true;
    } catch (err) {
      statusLine = parseApiError(err, 'I could not import that source. Check the URL or gateway settings.');
      addAssistantBubble(`I couldn't import that source yet: ${statusLine}`, 'question', 'Source import failed');
      return false;
    } finally {
      submitting = false;
    }
  }

  function buildAssistantQuestionForStep(step: number, proposedName: string, backendQuestion: string): string {
    if (step === 0) {
      return `Great — based on what you said, I'll set this up as **${proposedName}**.\n\nDoes that name fit, or would you like to refine it? You can type a new name, or just say "looks good" to keep it.`;
    }
    if (step === 1) {
      return `What's the main thing you want **${proposedName}** to accomplish over the next ~30 days?`;
    }
    if (step === 2) {
      return `Last one — what's the most active context right now for **${proposedName}**? Anything in flight, recent decisions, or open questions I should remember.`;
    }
    return backendQuestion || 'Tell me a bit more.';
  }

  function deriveFallbackEntityName(text: string): string {
    const line = text.split('\n').map((s) => s.trim()).find((s) => s.length > 0) ?? 'New entity';
    return line.slice(0, 80) || 'New entity';
  }

  async function startChatEntityCreation(firstMessage: string) {
    const trimmed = firstMessage.trim();
    if (!trimmed) return;
    addAssistantBubble(
      'Let me set this up as your first entity. One moment while I interpret what you described…',
      'creating-entity',
      'New entity'
    );
    let proposedName: string | null = null;
    let proposedSummary: string | null = null;
    try {
      const proposal = await interpretAsEntityProposal(trimmed);
      if (proposal) {
        proposedName = proposal.name;
        proposedSummary = proposal.summary;
      }
    } catch {
      // Fall through to heuristic naming
    }
    if (!proposedName) proposedName = deriveFallbackEntityName(trimmed);
    try {
      const start = await startEntityCreation({ entityName: proposedName });
      entityCreation = {
        sessionId: start.sessionId,
        proposedName,
        proposedSummary,
        step: 0,
        busy: false,
        proposedColour: start.proposedColour,
        palette: start.palette,
        colourBusy: false
      };
      addAssistantBubble(
        buildAssistantQuestionForStep(0, proposedName, start.question),
        'creating-entity',
        'Step 1 of 3 · Name'
      );
    } catch (err) {
      addAssistantBubble(
        parseApiError(err, 'I could not start entity creation. Check the gateway and try again.'),
        'question',
        'Setup failed'
      );
    }
  }

  async function submitEntityCreationAnswer(answer: string) {
    if (!entityCreation || entityCreation.busy) return;
    const flow = entityCreation;
    const trimmed = answer.trim();
    pushChatBubble({
      id: `user-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
      role: 'user',
      text: trimmed || '(skipped)',
      tone: 'creating-entity',
      meta: `Step ${flow.step + 1} of ${ENTITY_CREATION_TOTAL_STEPS}`
    });
    entityCreation = { ...flow, busy: true };
    try {
      // Step 0: user is confirming or overriding the proposed name.
      // The backend's first question is `profile`; we don't actually want to
      // ask the user for that — we synthesise it from the interpreted summary
      // (or a short label fallback) so the next backend step is `goals`.
      if (flow.step === 0) {
        const lower = trimmed.toLowerCase();
        const confirmPhrases = ['looks good', 'yes', 'yep', 'yeah', 'ok', 'okay', 'sure', 'good', ''];
        const isConfirm = confirmPhrases.includes(lower);
        let newName = flow.proposedName;
        let sessionId = flow.sessionId;
        if (!isConfirm && trimmed.length > 0) {
          newName = trimmed.slice(0, 120);
          // entity_name is locked at session start; restart with the new name.
          try {
            await cancelEntityCreation(flow.sessionId);
          } catch {
            // ignore
          }
          const restart = await startEntityCreation({ entityName: newName });
          sessionId = restart.sessionId;
          flow.proposedColour = restart.proposedColour;
          flow.palette = restart.palette;
        }
        const profileAnswer = flow.proposedSummary ?? `Entity called ${newName}.`;
        await answerEntityCreation({ sessionId, answer: profileAnswer });
        entityCreation = {
          sessionId,
          proposedName: newName,
          proposedSummary: flow.proposedSummary,
          step: 1,
          busy: false,
          proposedColour: flow.proposedColour,
          palette: flow.palette,
          colourBusy: false
        };
        addAssistantBubble(
          buildAssistantQuestionForStep(1, newName, ''),
          'creating-entity',
          'Step 2 of 3 · Goals'
        );
        return;
      }

      // Steps 1 and 2: pass the answer to the backend's current step (goals / active_context)
      const isSkip = trimmed.length === 0;
      await answerEntityCreation({
        sessionId: flow.sessionId,
        answer: trimmed,
        skip: isSkip
      });

      if (flow.step === 1) {
        entityCreation = { ...flow, step: 2, busy: false };
        addAssistantBubble(
          buildAssistantQuestionForStep(2, flow.proposedName, ''),
          'creating-entity',
          'Step 3 of 3 · Active context'
        );
        return;
      }

      // Step 2: show confirmation popup before finalising
      entityCreation = { ...flow, step: 2, busy: false };
      entityConfirmVisible = true;
      addAssistantBubble(
        `Here's what I've got for **${flow.proposedName}**. Confirm the details above and hit **Create** to make it official.`,
        'creating-entity',
        'Confirm entity'
      );
    } catch (err) {
      entityCreation = entityCreation ? { ...entityCreation, busy: false } : null;
      addAssistantBubble(
        parseApiError(err, 'Something went wrong while saving your entity. Try again or cancel to start over.'),
        'question',
        'Save failed'
      );
    }
  }

  async function pickEntityCreationColour(hex: string) {
    if (!entityCreation || entityCreation.colourBusy) return;
    if (entityCreation.proposedColour === hex) return;
    const flow = entityCreation;
    entityCreation = { ...flow, colourBusy: true };
    try {
      const resp = await updateEntityCreationColour({
        sessionId: flow.sessionId,
        colour: hex
      });
      if (entityCreation) {
        entityCreation = { ...entityCreation, proposedColour: resp.colour, colourBusy: false };
      }
    } catch {
      if (entityCreation) {
        entityCreation = { ...entityCreation, colourBusy: false };
      }
    }
  }

  async function cancelChatEntityCreation() {
    if (!entityCreation) return;
    const flow = entityCreation;
    entityCreation = null;
    entityConfirmVisible = false;
    try {
      await cancelEntityCreation(flow.sessionId);
    } catch {
      // ignore
    }
    addAssistantBubble(
      "Entity creation cancelled. Describe what you want to manage whenever you're ready.",
      'default',
      'Cancelled'
    );
  }

  async function confirmCreateEntity() {
    if (!entityCreation) return;
    const flow = entityCreation;
    entityConfirmVisible = false;
    entityCreation = { ...flow, busy: true };
    try {
      const result = await completeEntityCreation({
        sessionId: flow.sessionId,
        forceComplete: true
      });
      const newEntityId = result.entity?.id;
      entityCreation = null;
      await loadAll();
      if (newEntityId) {
        activateChatContext(newEntityId);
      }
      addAssistantBubble(
        `**${flow.proposedName}** is live and selected at the top of your chat. I'll use its goals and context in everything I do now. You can switch to a different entity anytime using the dropdown at the top, or keep it as the default.`,
        'success',
        flow.proposedName
      );
    } catch (err) {
      entityCreation = entityCreation ? { ...entityCreation, busy: false } : null;
      entityConfirmVisible = true;
      addAssistantBubble(
        parseApiError(err, 'Something went wrong creating the entity. Try again or cancel.'),
        'question',
        'Creation failed'
      );
    }
  }

  function parseExternalAgentCommand(text: string): { kind: 'claude' | 'opencode' | 'cursor-agent'; prompt: string } | null {
    const trimmed = text.trim();
    const match = trimmed.match(/^(?:\/|ask\s+|run\s+|send\s+to\s+)(claude|opencode|cursor|cursor-agent)\b[:,]?\s*([\s\S]*)$/i);
    if (!match) return null;
    const rawKind = match[1].toLowerCase();
    const prompt = (match[2] ?? '').trim();
    if (!prompt) return null;
    return {
      kind: rawKind === 'cursor' ? 'cursor-agent' : (rawKind as 'claude' | 'opencode' | 'cursor-agent'),
      prompt
    };
  }

  function sleep(ms: number) {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }

  function isTerminalAgentRunStatus(status: string) {
    const normalized = status.toLowerCase();
    return normalized === 'completed' || normalized === 'failed' || normalized === 'cancelled';
  }

  async function pollExternalAgentRun(entityId: string, runId: string, kind: string) {
    for (let attempt = 0; attempt < 180; attempt += 1) {
      await sleep(attempt === 0 ? 1500 : 5000);
      const run = await getEntityAgentRun(entityId, runId);
      if (!isTerminalAgentRunStatus(run.status)) continue;
      await loadAll();
      addAssistantBubble(
        `**${kind} finished with status ${run.status}.**\n\n${run.output || run.error || 'No output returned.'}`,
        run.status === 'completed' ? 'success' : 'question',
        run.taskId ? `Task ${run.taskId}` : 'External agent run'
      );
      return;
    }
    addAssistantBubble(
      `${kind} is still running. I saved the run and will show it in the entity task/run records when it finishes.`,
      'default',
      'External agent still running'
    );
  }

  async function runExternalAgentFromChat(outgoing: string, command: { kind: 'claude' | 'opencode' | 'cursor-agent'; prompt: string }) {
    pushChatBubble({
      id: `user-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
      role: 'user',
      text: outgoing,
      meta: entityPick === INBOX ? inboxLabel : selectedEntityLabel
    });
    resetComposerInput();
    if (entityPick === INBOX) {
      addAssistantBubble('Pick an entity first, then I can run that CLI agent inside the entity workspace.', 'question', 'Entity required');
      return;
    }
    submitting = true;
    addAssistantBubble(
      `Starting ${command.kind} from this entity's working folder. I’ll capture the output as a task and run record.`,
      'default',
      selectedEntityLabel
    );
    try {
      const run = await createEntityAgentRun(entityPick, {
        kind: command.kind,
        prompt: command.prompt,
        timeoutSecs: 900,
        title: command.prompt.slice(0, 100),
        createTask: true
      });
      await loadAll();
      addAssistantBubble(
        `${command.kind} run started. I’ll poll it in the background and record the result when it finishes.`,
        'default',
        run.taskId ? `Task ${run.taskId}` : `Run ${run.runId.slice(0, 8)}`
      );
      void pollExternalAgentRun(entityPick, run.runId, command.kind).catch((err) => {
        addAssistantBubble(
          `I started ${command.kind}, but polling failed: ${parseApiError(err, 'Could not poll external agent run.')}`,
          'question',
          'External agent polling'
        );
      });
      clearApiError();
    } catch (err) {
      statusLine = parseApiError(err, `Could not run ${command.kind}. Check the entity local path and CLI login.`);
      addAssistantBubble(`I couldn't run ${command.kind}: ${statusLine}`, 'question', 'External agent failed');
    } finally {
      submitting = false;
    }
  }

  async function submitPrimaryChat() {
    if (submitting || !hasInput) return;
    if (entityCreation) {
      const outgoing = body.trim();
      resetComposerInput();
      await submitEntityCreationAnswer(outgoing);
      return;
    }
    if (capturePreview) {
      await confirmCaptureCommit();
      return;
    }
    // Record the raw capture before any interpretation / source ingest / CLI run.
    void recordRawCapture(body.trim());
    const externalAgentCommand = parseExternalAgentCommand(body.trim());
    if (externalAgentCommand) {
      await runExternalAgentFromChat(body.trim(), externalAgentCommand);
      return;
    }
    const sourceUrl = firstSourceUrl(body.trim());
    if (sourceUrl) {
      await ingestSourceFromChat(body.trim(), sourceUrl);
      return;
    }
    // No entities yet → nudge user to create one, or kick off creation if
    // their message looks like a genuine entity description.
    if (entities.length === 0) {
      const outgoing = body.trim();
      if (!outgoing) {
        statusLine = "Describe what you'd like to manage and I'll set it up as your first entity.";
        return;
      }
      const lower = outgoing.toLowerCase().replace(/[^a-z\s]/g, "").trim();
      const greetings = [
        'hi', 'hey', 'hello', 'yo', 'sup', 'whats up', 'hiya',
        'good morning', 'good afternoon', 'good evening', 'howdy',
        'help', 'test', 'testing', 'ok', 'okay', 'thanks', 'thank you'
      ];
      const isGeneric = greetings.includes(lower) || lower.length < 3;
      if (isGeneric) {
        pushChatBubble({
          id: `user-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
          role: 'user',
          text: outgoing
        });
        resetComposerInput();
        addAssistantBubble(
          "Hey! Before we dive in, let's create your first entity — it could be a project, a brand, a product, or anything you want to manage. Describe it and I'll set it up for you, or head to the Entities page to create one there.",
          'question',
          'Create an entity to get started'
        );
        return;
      }
      pushChatBubble({
        id: `user-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
        role: 'user',
        text: outgoing,
        tone: 'creating-entity',
        meta: 'Starting your first entity'
      });
      resetComposerInput();
      await startChatEntityCreation(outgoing);
      return;
    }
    await sendConversationalChat();
  }

  function handleComposerKeydown(event: KeyboardEvent) {
    if (event.key !== 'Enter' || event.shiftKey || event.isComposing) return;
    event.preventDefault();
    void submitPrimaryChat();
  }

  function parseApiError(err: unknown, fallback: string): string {
    if (err instanceof Error && err.message.trim()) return err.message;
    return fallback;
  }

  async function performAction(action: ActionIntent) {
    statusLine = null;
    createdAction = null;
    if (!hasInput) {
      statusLine = 'Write or record something first.';
      addAssistantBubble(statusLine, 'question');
      return;
    }

    const outgoing = buildPrompt() || body.trim();
    if (outgoing) {
      pushChatBubble({
        id: `user-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
        role: 'user',
        text: outgoing,
        meta: entityPick === INBOX ? inboxLabel : selectedEntityLabel
      });
    }

    submitting = true;
    try {
      if (action === 'mission') {
        createdAction = await runCreateMission();
        statusLine = 'Mission created.';
        addAssistantBubble('I turned that into a mission. Open it to review the goal and the next suggested steps.', 'success', selectedEntityLabel);
      } else if (interpretWithAi) {
        await runAiCapturePipeline(action);
      } else if (action === 'note') {
        createdAction = await runCreateTask('Note', 'note');
        statusLine = 'Note captured.';
        addAssistantBubble('Saved that as a note and kept the context attached.', 'success', selectedEntityLabel);
      } else if (action === 'task') {
        createdAction = await runCreateTask('Task', 'task');
        statusLine = 'Task created.';
        addAssistantBubble('Created a task from that message.', 'success', selectedEntityLabel);
      } else {
        createdAction = await runCreateTask('Reminder', 'reminder');
        statusLine = 'Reminder scheduled.';
        addAssistantBubble('Scheduled that reminder so heartbeat can follow it up later.', 'success', selectedEntityLabel);
      }

      const holdInputForAiConfirm = interpretWithAi && action !== 'mission';
      if (!holdInputForAiConfirm) {
        body = '';
        selectedAttachments = [];
      }
      if (!holdInputForAiConfirm) {
        await loadAll();
        clearApiError();
      }
    } catch (err) {
      statusLine = parseApiError(err, 'Could not complete action. Check gateway and try again.');
      addAssistantBubble(statusLine, 'question', 'Execution error');
    } finally {
      submitting = false;
    }
  }

  function firstRunNext() {
    if (!firstRunCanAdvance) return;
    firstRunError = null;
    firstRunStep = Math.min(firstRunStep + 1, FIRST_RUN_QUESTIONS.length - 1);
  }

  function firstRunBack() {
    firstRunError = null;
    firstRunStep = Math.max(firstRunStep - 1, 0);
  }

  /** Tapping an option is itself the answer — no separate confirm step. */
  async function chooseWorkingStyle(choice: WorkingStyle) {
    answerFocusChoice = choice;
    await finishFirstRun();
  }

  async function finishFirstRun() {
    if (firstRunBusy) return;
    firstRunBusy = true;
    firstRunError = null;
    try {
      const profile = await submitOnboarding({
        identity: answerIdentity.trim() || undefined,
        intent: answerIntent.trim() || undefined,
        focus: answerFocusText.trim() || undefined,
        focusChoice: answerFocusChoice || undefined
      });
      if (!profile) {
        firstRunError = 'Could not save that. Check the gateway and try again.';
        return;
      }
      firstRunDismissed = true;
      // A focused operator starts on the one screen that matters today;
      // everyone else stays in chat with the full workspace around them.
      if (profile.workingStyle === 'linear') await goto('/today');
    } finally {
      firstRunBusy = false;
    }
  }

  /**
   * Skip records a neutral default: onboarded, working style left `unset`, so
   * nothing is hidden and we never ask again.
   */
  async function skipFirstRun() {
    if (firstRunBusy) return;
    firstRunBusy = true;
    firstRunError = null;
    try {
      const saved = await saveProfile({ markOnboarded: true });
      if (!saved) {
        firstRunError = 'Could not save that. Check the gateway and try again.';
        return;
      }
      firstRunDismissed = true;
    } finally {
      firstRunBusy = false;
    }
  }

  function pickEntity(id: string) {
    if (id === entityPick) {
      entityDropdownOpen = false;
      return;
    }
    activateChatContext(id);
  }

  function openCreateEntity() {
    entityDropdownOpen = false;
    goto('/entities?create=1');
  }

  onMount(() => {
    if (typeof window !== 'undefined') {
      const params = new URLSearchParams(window.location.search);
      const requestedEntityId = params.get('entity_id') || params.get('entity');
      if (requestedEntityId && requestedEntityId !== INBOX) {
        pendingInitialEntityId = requestedEntityId;
      } else {
        try {
          const saved = localStorage.getItem(ENTITY_PICK_KEY);
          if (saved && saved !== INBOX) pendingInitialEntityId = saved;
        } catch {}
      }
    }
    // Shared with the sidebar — whichever mounts first wins, the other reuses it.
    void ensureProfile();
    loadAll();
    const timer = setInterval(loadAll, 15000);
    // Phase 9: poll heartbeat results for the active entity every 30s so a
    // newly fired heartbeat shows up in the chat without a page refresh.
    const heartbeatTimer = setInterval(() => {
      const id = entityPick === INBOX ? null : entityPick;
      void refreshHeartbeatResults(id);
    }, 30000);
    return () => {
      clearInterval(timer);
      clearInterval(heartbeatTimer);
    };
  });
</script>

<!-- Left indexable: `/` is the site root at consistent.ceo, not a workspace internal. -->
<Seo
  title="Chat"
  description="Consistent CEO turns your businesses, projects, goals, approvals, and memories into a persistent, evidence-backed operating graph, and puts AI to work on it under explicit human oversight. Rust, self-hosted, open source."
  canonical="/"
/>

<div class="chat-page" style={tintStyle} class:tinted={currentEntity !== null}>
  <div class="context-bar" aria-label="Chat context">
    <button
      type="button"
      class="context-pill"
      onclick={() => (entityDropdownOpen = !entityDropdownOpen)}
      aria-haspopup="listbox"
      aria-expanded={entityDropdownOpen}
    >
      <span class="context-dot" class:pulse={selectedEntityActive} aria-hidden="true"></span>
      <span class="context-name">{currentEntity ? currentEntity.name : 'All entities'}</span>
      <span class="context-meta">
        {currentEntity
          ? [currentEntity.dept ?? 'Active', tenantCount > 1 ? tenantSummary?.name ?? activeTenantId : null]
              .filter(Boolean)
              .join(' · ')
          : inboxLabel}
      </span>
      <ChevronIcon size={14} />
    </button>

    <div class="context-actions">
      <button
        type="button"
        class="chip-toggle"
        class:active={voiceRepliesEnabled}
        onclick={() => {
          voiceRepliesEnabled = !voiceRepliesEnabled;
          if (!voiceRepliesEnabled && typeof window !== 'undefined' && 'speechSynthesis' in window) {
            window.speechSynthesis.cancel();
            voiceStatus = null;
          }
        }}
      >
        {voiceRepliesEnabled ? 'Voice on' : 'Voice off'}
      </button>
      <button
        type="button"
        class="chip-toggle"
        class:active={interpretWithAi}
        onclick={() => (interpretWithAi = !interpretWithAi)}
        title={interpretWithAi ? 'AI interpretation is on' : 'AI interpretation is off'}
      >
        {interpretWithAi ? '✨ AI on' : 'AI off'}
      </button>
    </div>

    {#if entityDropdownOpen}
      <div class="entity-dropdown" role="listbox">
        {#each entityOptions as option (option.id)}
          <button
            type="button"
            class="entity-option"
            class:selected={entityPick === option.id}
            role="option"
            aria-selected={entityPick === option.id}
            onclick={() => pickEntity(option.id)}
          >
            {#if option.active}<span class="entity-pulse-dot" aria-label="Active operations"></span>{/if}
            {option.label}
          </button>
        {/each}
        <button type="button" class="entity-option new" onclick={openCreateEntity}>+ New entity</button>
      </div>
    {/if}
  </div>

  <div class="messages-region">
    {#if showFirstRun}
      <section class="onboarding-card" aria-label="Welcome">
        <div class="onboarding-head">
          <h2>{FIRST_RUN_QUESTIONS[firstRunStep].title}</h2>
          <button type="button" class="btn-ghost" disabled={firstRunBusy} onclick={skipFirstRun}>
            Skip
          </button>
        </div>
        <p class="onboarding-copy">{FIRST_RUN_QUESTIONS[firstRunStep].help}</p>

        {#if firstRunStep === 0}
          <textarea
            class="first-run-input"
            rows="3"
            placeholder={FIRST_RUN_QUESTIONS[0].placeholder}
            bind:value={answerIdentity}
          ></textarea>
        {:else if firstRunStep === 1}
          <textarea
            class="first-run-input"
            rows="3"
            placeholder={FIRST_RUN_QUESTIONS[1].placeholder}
            bind:value={answerIntent}
          ></textarea>
        {:else}
          <div class="first-run-choices">
            <button
              type="button"
              class="first-run-choice"
              class:selected={answerFocusChoice === 'holistic'}
              disabled={firstRunBusy}
              onclick={() => chooseWorkingStyle('holistic')}
            >
              <span class="frc-title">Many things at once</span>
              <span class="frc-sub">
                Several entities in motion. You get the full workspace — every surface, nothing hidden.
              </span>
            </button>
            <button
              type="button"
              class="first-run-choice"
              class:selected={answerFocusChoice === 'linear'}
              disabled={firstRunBusy}
              onclick={() => chooseWorkingStyle('linear')}
            >
              <span class="frc-title">One thing, until it's done</span>
              <span class="frc-sub">
                A focused workspace: chat, today, tasks, missions. You can open the rest any time.
              </span>
            </button>
          </div>
          <textarea
            class="first-run-input"
            rows="2"
            placeholder={FIRST_RUN_QUESTIONS[2].placeholder}
            bind:value={answerFocusText}
          ></textarea>
        {/if}

        <div class="onboarding-actions">
          <span class="first-run-step">Step {firstRunStep + 1} of {FIRST_RUN_QUESTIONS.length}</span>
          {#if firstRunStep > 0}
            <button type="button" class="btn-ghost" disabled={firstRunBusy} onclick={firstRunBack}>
              Back
            </button>
          {/if}
          {#if firstRunStep < FIRST_RUN_QUESTIONS.length - 1}
            <button
              type="button"
              class="btn-primary"
              disabled={!firstRunCanAdvance || firstRunBusy}
              onclick={firstRunNext}
            >
              Next
            </button>
          {:else}
            <button
              type="button"
              class="btn-primary"
              disabled={!firstRunCanAdvance || firstRunBusy}
              onclick={finishFirstRun}
            >
              {firstRunBusy ? 'Setting up…' : 'Done'}
            </button>
          {/if}
        </div>

        {#if firstRunError}
          <p class="status-line" role="status">{firstRunError}</p>
        {/if}
      </section>
    {/if}

    {#if entityCreation}
      <div
        class="entity-creation-banner"
        role="status"
        style={`--creation-tint: ${entityCreation.proposedColour};`}
      >
        <div class="ecb-icon" aria-hidden="true" style="background: var(--creation-tint);">🛠</div>
        <div class="ecb-main">
          <div class="ecb-title">Creating entity: <strong>{entityCreation.proposedName}</strong></div>
          <div class="ecb-sub">
            Step {Math.min(entityCreation.step + 1, ENTITY_CREATION_TOTAL_STEPS)} of {ENTITY_CREATION_TOTAL_STEPS}
            · {entityCreationStepLabels[entityCreation.step] ?? 'Wrapping up'}
          </div>
          {#if entityCreation.palette && entityCreation.palette.length > 0}
            <div class="ecb-swatches" role="group" aria-label="Pick an accent colour">
              {#each entityCreation.palette as sw (sw.hex)}
                <button
                  type="button"
                  class="ecb-swatch"
                  class:active={sw.hex.toLowerCase() === entityCreation.proposedColour.toLowerCase()}
                  style={`background: ${sw.hex};`}
                  title={sw.name}
                  aria-label={`Use ${sw.name}`}
                  aria-pressed={sw.hex.toLowerCase() === entityCreation.proposedColour.toLowerCase()}
                  disabled={entityCreation.colourBusy}
                  onclick={() => void pickEntityCreationColour(sw.hex)}
                ></button>
              {/each}
            </div>
          {/if}
        </div>
        <button
          type="button"
          class="ecb-cancel"
          onclick={() => void cancelChatEntityCreation()}
          disabled={entityCreation.busy}
        >Cancel</button>
      </div>
    {/if}

    {#if entityConfirmVisible && entityCreation}
      <div class="entity-confirm-overlay" role="dialog" aria-modal="true" aria-label="Confirm entity creation">
        <div class="entity-confirm-card" style={`--confirm-tint: ${entityCreation.proposedColour};`}>
          <div class="ecc-header">
            <span class="ecc-dot" style={`background: ${entityCreation.proposedColour};`}></span>
            <span class="ecc-name">{entityCreation.proposedName}</span>
          </div>
          {#if entityCreation.proposedSummary}
            <p class="ecc-summary">{entityCreation.proposedSummary}</p>
          {/if}
          <p class="ecc-hint">This will create the entity, select it in your chat, and start using its context going forward.</p>
          <div class="ecc-actions">
            <button
              type="button"
              class="ecc-btn ecc-btn-cancel"
              onclick={() => void cancelChatEntityCreation()}
              disabled={entityCreation.busy}
            >Cancel</button>
            <button
              type="button"
              class="ecc-btn ecc-btn-confirm"
              style={`background: ${entityCreation.proposedColour};`}
              onclick={() => void confirmCreateEntity()}
              disabled={entityCreation.busy}
            >{entityCreation.busy ? 'Creating...' : 'Create entity'}</button>
          </div>
        </div>
      </div>
    {/if}

    <ChatMessageList chatHistory={combinedChatHistory} />

    {#if rawCaptures.length > 0}
      <section class="capture-inbox" aria-label="Raw capture inbox">
        <div class="capture-inbox-head">
          <span class="capture-inbox-label">Raw captures</span>
          <span class="capture-inbox-count">{rawCaptures.length}</span>
        </div>
        <ul class="capture-inbox-list">
          {#each rawCaptures as capture (capture.id)}
            <li class="capture-row">
              <div class="capture-row-main">
                <span class="capture-row-text">{capture.summary || capture.raw_text}</span>
                <span class="capture-row-meta">
                  {capture.source}{capture.provider ? ` · ${capture.provider}` : ''}{capture.entity_id
                    ? ` · ${entityLabelById(capture.entity_id)}`
                    : ''}
                </span>
                {#if !capture.entity_id && capture.route?.entityId}
                  <span class="capture-route" title={capture.route.reason}>
                    → Suggested: {capture.route.entityName ?? entityLabelById(capture.route.entityId)}
                    ({Math.round(capture.route.confidence * 100)}%)
                  </span>
                {/if}
              </div>
              <div class="capture-row-actions">
                {#if !capture.entity_id && capture.route?.entityId}
                  <button
                    type="button"
                    class="capture-btn capture-btn-attach"
                    disabled={captureBusyId === capture.id}
                    title={capture.route.reason}
                    onclick={() => void attachCaptureToEntity(capture, capture.route!.entityId!)}
                  >Attach → {capture.route.entityName ?? entityLabelById(capture.route.entityId)}</button>
                {/if}
                {#if !capture.entity_id && entities.length > 0}
                  <select
                    class="capture-attach-select"
                    disabled={captureBusyId === capture.id}
                    aria-label="Attach capture to entity"
                    onchange={(e) => handleManualAttach(capture, e)}
                  >
                    <option value="">Attach…</option>
                    {#each entities as ent (ent.id)}
                      <option value={ent.id}>{ent.name}</option>
                    {/each}
                  </select>
                {/if}
                <button
                  type="button"
                  class="capture-btn capture-btn-task"
                  disabled={captureBusyId === capture.id}
                  onclick={() => void handlePromoteCapture(capture.id)}
                >Create task</button>
                <button
                  type="button"
                  class="capture-btn"
                  disabled={captureBusyId === capture.id}
                  onclick={() => void handleSaveMemory(capture)}
                >Save memory</button>
                <button
                  type="button"
                  class="capture-btn"
                  disabled={captureBusyId === capture.id || !captureTargetEntityId(capture)}
                  title={captureTargetEntityId(capture)
                    ? 'Append to the entity narrative'
                    : 'Pick an entity to update its narrative'}
                  onclick={() => void handleUpdateNarrative(capture)}
                >Update narrative</button>
                <button
                  type="button"
                  class="capture-btn capture-btn-dismiss"
                  disabled={captureBusyId === capture.id}
                  onclick={() => void handleDismissCapture(capture.id)}
                >Dismiss</button>
              </div>
            </li>
          {/each}
        </ul>
      </section>
    {/if}

    {#if capturePreview || createdAction || statusLine || voiceStatus || lastAgentActions.length > 0 || lastRouting}
      <div class="inline-cards">
        {#if voiceStatus}
          <p class="status-line" role="status">{voiceStatus}</p>
        {/if}
        {#if statusLine}
          <p class="status-line" role="status">{statusLine}</p>
        {/if}
        {#if lastRouting && lastRouting.outcome !== 'personal'}
          <div class="routing-card" role="status">
            {#if lastRouting.outcome === 'auto_filed'}
              <span class="routing-text"
                >Filed to <strong>{lastRouting.entityName ?? 'an entity'}</strong>{lastRouting.reason
                  ? ` — ${lastRouting.reason}`
                  : ''}</span
              >
              <button class="routing-btn" type="button" onclick={() => void routingUndo()}>Undo</button>
            {:else if lastRouting.outcome === 'suggest'}
              <span class="routing-text"
                >This looks like it's about <strong>{lastRouting.entityName ?? 'an entity'}</strong>.</span
              >
              {#if lastRouting.entityId}
                <button
                  class="routing-btn primary"
                  type="button"
                  onclick={() => void routingFileUnder(lastRouting!.entityId!, lastRouting!.entityName ?? undefined)}
                  >File under {lastRouting.entityName ?? 'it'}</button
                >
              {/if}
              <button class="routing-btn" type="button" onclick={() => routingDismiss()}>Keep personal</button>
            {/if}
          </div>
        {/if}
        {#if lastAgentActions.length > 0}
          <div class="agent-actions-panel" aria-label="Agent actions">
            <div class="agent-actions-head">
              <span class="agent-actions-label">Agent actions</span>
              <span class="agent-actions-count">{lastAgentActions.length}</span>
            </div>
            <ul class="agent-action-list">
              {#each lastAgentActions as action, index (`${action.actionType}-${action.resultId ?? index}`)}
                <li class="agent-action-row" class:failed={action.status === 'failed'}>
                  <span class="agent-action-status" aria-hidden="true"></span>
                  <div class="agent-action-main">
                    <span class="agent-action-title">{action.description}</span>
                    <span class="agent-action-meta">
                      {action.label}
                      {#if action.resultId}
                        · {action.resultId}
                      {/if}
                    </span>
                  </div>
                  {#if action.href}
                    <a class="agent-action-link" href={action.href}>Open</a>
                  {/if}
                </li>
              {/each}
            </ul>
          </div>
        {/if}
        {#if capturePreview}
          <div class="capture-preview" role="region" aria-label="AI capture review">
            <div class="capture-preview-head">
              <span class="capture-preview-label">AI capture</span>
              <span class="capture-preview-intent">{capturePreview.intent.replaceAll('_', ' ')}</span>
              <span class="capture-preview-confidence">
                {Math.round(Math.min(1, Math.max(0, capturePreview.confidence)) * 100)}% match
              </span>
            </div>
            {#if capturePreview.clarifyingQuestion}
              <p class="capture-preview-question">{capturePreview.clarifyingQuestion}</p>
            {/if}
            {#if capturePreview.candidateEntityIds && capturePreview.candidateEntityIds.length > 0}
              <div class="capture-preview-candidates" role="group" aria-label="Matching entities">
                <span class="candidates-label">Candidates:</span>
                {#each capturePreview.candidateEntityIds as cid (cid)}
                  <button type="button" class="candidate-chip" onclick={() => pickEntity(cid)}>
                    {entityLabelById(cid)}
                  </button>
                {/each}
              </div>
            {/if}
            <ul class="capture-preview-draft" aria-label="Draft fields">
              {#if capturePreview.draft?.entityName}
                <li><span class="d-k">Entity</span> {capturePreview.draft.entityName}</li>
              {/if}
              {#if capturePreview.draft?.spaceName}
                <li><span class="d-k">Space</span> {capturePreview.draft.spaceName}</li>
              {/if}
              {#if capturePreview.draft?.taskTitle}
                <li><span class="d-k">Task</span> {capturePreview.draft.taskTitle}</li>
              {/if}
              {#if capturePreview.draft?.entitySummary}
                <li><span class="d-k">Summary</span> {capturePreview.draft.entitySummary}</li>
              {/if}
            </ul>
            <div class="capture-preview-actions">
              {#if capturePreview.intent !== 'clarify'}
                <button type="button" class="preview-confirm" disabled={submitting} onclick={() => void confirmCaptureCommit()}>
                  {submitting ? 'Working…' : 'Confirm'}
                </button>
              {/if}
              <button type="button" class="preview-cancel" disabled={submitting} onclick={cancelCapturePreview}>Cancel</button>
            </div>
          </div>
        {/if}

        {#if createdAction}
          <div class="capture-result" aria-label="Capture result">
            <div class="capture-result-head">
              <span class="capture-result-label">Captured</span>
              <span class="capture-result-intent">{createdAction.intent}</span>
            </div>
            {#if createdAction.summary}
              <p class="capture-result-summary">{createdAction.summary}</p>
            {/if}
            <div class="next-step-links" aria-label="Next steps">
              {#if createdAction.taskId}
                <a class="next-link" href={`/tasks?task_id=${createdAction.taskId}`}>Open created task</a>
              {/if}
              {#if createdAction.missionId}
                <a class="next-link" href={`/missions/${createdAction.missionId}`}>Open mission</a>
              {/if}
              {#if createdAction.entityId}
                <a class="next-link" href={`/entities/${createdAction.entityId}`}>Open entity</a>
              {/if}
              <a class="next-link" href="/tasks">View activity and tasks</a>
            </div>
          </div>
        {/if}
      </div>
    {/if}
  </div>

  <div class="composer-region">
    <div class="composer-wrap">
      <div class="action-row" class:armed={hasInput}>
        {#each ACTIONS as action (action.id)}
          <button
            type="button"
            class="action-pill"
            disabled={!hasInput || submitting}
            onclick={() => performAction(action.id)}
            title={action.hint}
          >
            {action.label}
          </button>
        {/each}
      </div>
      <ChatComposer
        body={body}
        attachmentMenuOpen={attachmentMenuOpen}
        selectedAttachments={selectedAttachments}
        isRecording={isRecording}
        submitting={submitting}
        placeholder={currentEntity ? `Message about ${currentEntity.name}…` : 'Message Consistent…'}
        onBodyInput={handleBodyInput}
        onTextareaInput={autoResizeCaptureInput}
        onTextareaKeydown={handleComposerKeydown}
        onToggleAttachmentMenu={() => (attachmentMenuOpen = !attachmentMenuOpen)}
        onOpenAttachmentPicker={openAttachmentPicker}
        onAttachmentChange={handleAttachmentChange}
        onRemoveAttachment={removeAttachment}
        onToggleVoiceRecording={() => void toggleVoiceRecording()}
        onSubmit={() => void submitPrimaryChat()}
      />
    </div>
  </div>
</div>

<style>
  .chat-page {
    --entity-tint: #c98a4b;
    display: grid;
    grid-template-rows: auto minmax(0, 1fr) auto;
    grid-template-areas:
      'context'
      'messages'
      'composer';
    height: 100%;
    min-height: 0;
    background:
      radial-gradient(900px 340px at 50% -10%, rgba(201, 138, 75, 0.06), transparent 62%),
      radial-gradient(1200px 540px at 50% 120%, rgba(201, 138, 75, 0.08), transparent 72%);
    transition: background var(--transition-base);
  }
  .chat-page.tinted {
    background:
      radial-gradient(900px 340px at 50% -10%, color-mix(in srgb, var(--entity-tint) 16%, transparent), transparent 62%),
      radial-gradient(1200px 540px at 50% 120%, color-mix(in srgb, var(--entity-tint) 12%, transparent), transparent 72%);
  }
  .chat-page.tinted .context-dot {
    background: var(--entity-tint);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--entity-tint) 22%, transparent);
  }
  .chat-page.tinted .context-dot.pulse {
    animation: entity-pulse-tinted 2s ease-in-out infinite;
  }
  @keyframes entity-pulse-tinted {
    0%, 100% { box-shadow: 0 0 0 3px color-mix(in srgb, var(--entity-tint) 22%, transparent); opacity: 1; }
    50% { box-shadow: 0 0 0 6px color-mix(in srgb, var(--entity-tint) 45%, transparent); opacity: 0.7; }
  }
  .chat-page.tinted :global(.chat-bubble.assistant),
  .chat-page.tinted :global(.chat-bubble.system) {
    border-color: color-mix(in srgb, var(--entity-tint) 42%, var(--chat-assistant-border));
  }

  .context-bar {
    grid-area: context;
    position: relative;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 10px 16px;
    border-bottom: 1px solid var(--border-subtle);
    flex-wrap: wrap;
  }

  .context-pill {
    display: inline-flex;
    align-items: center;
    gap: 10px;
    padding: 6px 12px;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: var(--surface-overlay);
    color: var(--text-primary);
    cursor: pointer;
    min-height: 38px;
    max-width: min(100%, 520px);
  }
  .context-pill:hover {
    border-color: rgba(201, 138, 75, 0.4);
  }
  .context-dot {
    width: 8px;
    height: 8px;
    border-radius: 999px;
    background: #7a8163;
    box-shadow: 0 0 0 3px rgba(122, 129, 99, 0.18);
    flex-shrink: 0;
  }
  .context-dot.pulse {
    animation: entity-pulse 2s ease-in-out infinite;
  }
  @keyframes entity-pulse {
    0%, 100% { box-shadow: 0 0 0 3px rgba(122, 129, 99, 0.18); opacity: 1; }
    50% { box-shadow: 0 0 0 6px rgba(122, 129, 99, 0.35); opacity: 0.7; }
  }
  .context-name {
    font-weight: 700;
    font-size: 13px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 200px;
  }
  .context-meta {
    font-size: 11px;
    color: var(--text-muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 280px;
  }

  .context-actions {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .chip-toggle {
    min-height: 32px;
    padding: 0 12px;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: var(--surface-overlay);
    color: var(--text-secondary);
    font-size: 11px;
    font-weight: 600;
    cursor: pointer;
    transition: border-color var(--transition-base), color var(--transition-base);
  }
  .chip-toggle.active {
    color: var(--text-primary);
    border-color: rgba(201, 138, 75, 0.55);
    background: var(--surface-accent-soft);
  }

  .entity-dropdown {
    position: absolute;
    top: calc(100% + 4px);
    left: 16px;
    right: auto;
    z-index: 40;
    min-width: 260px;
    max-height: 360px;
    overflow-y: auto;
    border: 1px solid var(--border);
    border-radius: 12px;
    background: var(--bg-card);
    box-shadow: 0 12px 28px rgba(0, 0, 0, 0.35);
    padding: 6px;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .entity-option {
    text-align: left;
    padding: 8px 10px;
    min-height: 36px;
    border-radius: 8px;
    border: 1px solid transparent;
    background: transparent;
    color: var(--text-primary);
    cursor: pointer;
    font-size: 13px;
  }
  .entity-option:hover {
    border-color: var(--border);
    background: var(--bg-secondary);
  }
  .entity-option.selected {
    background: var(--surface-accent-soft);
    color: var(--text-primary);
    border-color: rgba(201, 138, 75, 0.4);
  }
  .entity-option.new {
    color: var(--accent-cyan);
    font-weight: 700;
    border-top: 1px solid var(--border-subtle);
    margin-top: 4px;
    padding-top: 10px;
  }
  .entity-pulse-dot {
    display: inline-block;
    width: 6px;
    height: 6px;
    border-radius: 999px;
    background: #7a8163;
    margin-right: 6px;
    vertical-align: middle;
    animation: entity-pulse 2s ease-in-out infinite;
  }

  .messages-region {
    grid-area: messages;
    min-height: 0;
    min-width: 0;
    position: relative;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }

  .composer-region {
    grid-area: composer;
    padding: 12px 16px 18px;
    border-top: 1px solid var(--border-subtle);
    background: linear-gradient(180deg, transparent, var(--surface-sunken));
  }
  .composer-wrap {
    max-width: 820px;
    margin: 0 auto;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .action-row {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    opacity: 0.45;
    transition: opacity var(--transition-base);
  }
  .action-row.armed {
    opacity: 1;
  }
  .action-pill {
    min-height: 30px;
    padding: 4px 12px;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: var(--surface-overlay);
    color: var(--text-secondary);
    font-size: 12px;
    font-weight: 700;
    cursor: pointer;
    transition: background var(--transition-base), border-color var(--transition-base), color var(--transition-base);
  }
  .action-pill:hover:not(:disabled) {
    border-color: var(--accent-cyan);
    color: var(--text-primary);
  }
  .action-pill:disabled {
    cursor: not-allowed;
  }

  .inline-cards {
    padding: 0 16px 12px;
    max-width: 820px;
    margin: 0 auto;
    width: 100%;
    display: flex;
    flex-direction: column;
    gap: 10px;
    flex-shrink: 0;
    max-height: 40%;
    overflow-y: auto;
  }

  .onboarding-card {
    margin: 12px 16px 0;
    border: 1px solid var(--border);
    border-radius: 14px;
    background: var(--bg-card);
    padding: 14px;
    display: grid;
    gap: 8px;
    max-width: 820px;
    flex-shrink: 0;
  }

  .entity-creation-banner {
    display: flex;
    align-items: center;
    gap: 12px;
    margin: 12px 16px 0;
    padding: 10px 14px;
    border: 1px solid rgba(74, 61, 82, 0.6);
    border-radius: 12px;
    background: linear-gradient(180deg, rgba(74, 61, 82, 0.18), rgba(138, 90, 46, 0.08));
    color: var(--text-primary);
    max-width: 820px;
    flex-shrink: 0;
    box-shadow: 0 0 0 1px rgba(74, 61, 82, 0.15);
  }
  .ecb-icon {
    font-size: 18px;
    line-height: 1;
    width: 28px;
    height: 28px;
    border-radius: 999px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    color: #0a0908;
    box-shadow: 0 0 0 2px color-mix(in srgb, var(--creation-tint, #4a3d52) 30%, transparent);
  }
  .ecb-swatches {
    display: flex;
    gap: 6px;
    margin-top: 6px;
    flex-wrap: wrap;
  }
  .ecb-swatch {
    appearance: none;
    width: 18px;
    height: 18px;
    border-radius: 999px;
    border: 1px solid rgba(242, 237, 228, 0.25);
    padding: 0;
    cursor: pointer;
    transition: transform 0.08s ease, box-shadow 0.08s ease;
  }
  .ecb-swatch:hover:not(:disabled) {
    transform: scale(1.1);
  }
  .ecb-swatch.active {
    box-shadow: 0 0 0 2px var(--text-primary);
  }
  .ecb-swatch:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .ecb-main {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .ecb-title {
    font-size: 13px;
    color: var(--text-primary);
  }
  .ecb-sub {
    font-size: 11px;
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
  .ecb-cancel {
    appearance: none;
    background: transparent;
    color: var(--text-secondary);
    border: 1px solid rgba(74, 61, 82, 0.4);
    border-radius: 999px;
    padding: 5px 12px;
    font-size: 11px;
    cursor: pointer;
  }
  .ecb-cancel:hover:not(:disabled) {
    background: rgba(74, 61, 82, 0.15);
    color: var(--text-primary);
  }
  .ecb-cancel:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .entity-confirm-overlay {
    position: fixed;
    inset: 0;
    z-index: 100;
    display: flex;
    align-items: center;
    justify-content: center;
    background: rgba(0, 0, 0, 0.55);
    backdrop-filter: blur(4px);
  }
  .entity-confirm-card {
    background: var(--bg-card, #1a2332);
    border: 1px solid rgba(242, 237, 228, 0.12);
    border-radius: 16px;
    padding: 24px 28px;
    max-width: 400px;
    width: 90vw;
    box-shadow: 0 20px 60px rgba(0, 0, 0, 0.45);
  }
  .ecc-header {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-bottom: 10px;
  }
  .ecc-dot {
    width: 14px;
    height: 14px;
    border-radius: 999px;
    flex-shrink: 0;
  }
  .ecc-name {
    font-size: 17px;
    font-weight: 600;
    color: var(--text-primary);
  }
  .ecc-summary {
    font-size: 13px;
    color: var(--text-secondary);
    margin: 0 0 8px;
    line-height: 1.45;
  }
  .ecc-hint {
    font-size: 12px;
    color: var(--text-muted);
    margin: 0 0 18px;
    line-height: 1.4;
  }
  .ecc-actions {
    display: flex;
    justify-content: flex-end;
    gap: 10px;
  }
  .ecc-btn {
    appearance: none;
    border: none;
    border-radius: 10px;
    padding: 9px 18px;
    font-size: 13px;
    font-weight: 500;
    cursor: pointer;
    transition: opacity 0.15s ease, transform 0.1s ease;
  }
  .ecc-btn:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
  .ecc-btn-cancel {
    background: transparent;
    border: 1px solid var(--border);
    color: var(--text-secondary);
  }
  .ecc-btn-cancel:hover:not(:disabled) {
    background: var(--surface-hover);
    color: var(--text-primary);
  }
  .ecc-btn-confirm {
    color: #0a0908;
    font-weight: 600;
  }
  .ecc-btn-confirm:hover:not(:disabled) {
    opacity: 0.88;
    transform: translateY(-1px);
  }
  .onboarding-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
  }
  .onboarding-head h2 {
    margin: 0;
    font-size: 14px;
    color: var(--text-primary);
  }
  .onboarding-copy {
    margin: 0;
    font-size: 12px;
    color: var(--text-secondary);
  }
  .onboarding-actions {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
  }

  .first-run-step {
    margin-right: auto;
    font-size: 11px;
    font-weight: 600;
    letter-spacing: 0.02em;
    color: var(--text-tertiary, var(--text-secondary));
  }
  .first-run-input {
    width: 100%;
    resize: vertical;
    padding: 10px 12px;
    border-radius: 10px;
    border: 1px solid var(--border);
    background: var(--bg-secondary);
    color: var(--text-primary);
    font: inherit;
    font-size: 13px;
    line-height: 1.5;
  }
  .first-run-input:focus {
    outline: none;
    border-color: rgba(201, 138, 75, 0.55);
  }
  .first-run-choices {
    display: grid;
    gap: 8px;
    grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
  }
  .first-run-choice {
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
  .first-run-choice:hover:not(:disabled) {
    border-color: rgba(201, 138, 75, 0.55);
    background: var(--surface-hover);
  }
  .first-run-choice.selected {
    border-color: rgba(201, 138, 75, 0.7);
    background: var(--surface-accent-soft);
  }
  .first-run-choice:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .frc-title {
    font-size: 13px;
    font-weight: 700;
  }
  .frc-sub {
    font-size: 11px;
    line-height: 1.45;
    color: var(--text-secondary);
  }
  .btn-ghost,
  .btn-primary {
    min-height: 36px;
    padding: 0 14px;
    border-radius: 10px;
    font-size: 12px;
    font-weight: 700;
    cursor: pointer;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    text-decoration: none;
  }
  .btn-ghost {
    border: 1px solid transparent;
    background: transparent;
    color: var(--text-secondary);
  }
  .btn-primary {
    border: 1px solid rgba(201, 138, 75, 0.5);
    background: linear-gradient(180deg, rgba(201, 138, 75, 0.35), rgba(138, 90, 46, 0.55));
    color: #ffffff;
  }
  .btn-primary:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
  .status-line {
    margin: 0;
    color: var(--accent-cyan);
    font-size: 12px;
  }

  /* Smart-routing card (Personal Space auto-file / suggestion). Purple matches
     the Personal Space accent. */
  .routing-card {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px 10px;
    padding: 10px 12px;
    border-radius: 12px;
    border: 1px solid rgba(139, 92, 246, 0.35);
    background: rgba(139, 92, 246, 0.1);
    font-size: 13px;
  }
  .routing-text {
    flex: 1 1 auto;
    min-width: 140px;
  }
  .routing-btn {
    flex: 0 0 auto;
    padding: 5px 12px;
    border-radius: 999px;
    border: 1px solid rgba(139, 92, 246, 0.4);
    background: transparent;
    color: inherit;
    font-size: 12px;
    cursor: pointer;
  }
  .routing-btn:hover {
    background: rgba(139, 92, 246, 0.16);
  }
  .routing-btn.primary {
    background: rgba(139, 92, 246, 0.85);
    border-color: transparent;
    color: #fff;
  }
  .routing-btn.primary:hover {
    background: rgba(139, 92, 246, 1);
  }

  .capture-preview {
    padding: 12px 14px;
    border-radius: 12px;
    border: 1px solid rgba(201, 138, 75, 0.35);
    background: rgba(138, 90, 46, 0.12);
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .agent-actions-panel {
    padding: 12px 14px;
    border-radius: 12px;
    border: 1px solid color-mix(in srgb, var(--entity-tint) 38%, var(--border));
    background: color-mix(in srgb, var(--entity-tint) 10%, var(--bg-card));
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .agent-actions-head {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
  .agent-actions-label {
    font-weight: 800;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    font-size: 10px;
    color: var(--text-secondary);
  }
  .agent-actions-count {
    min-width: 20px;
    height: 20px;
    border-radius: 999px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    font-size: 11px;
    font-weight: 800;
    color: var(--text-primary);
    background: color-mix(in srgb, var(--entity-tint) 20%, transparent);
    border: 1px solid color-mix(in srgb, var(--entity-tint) 40%, transparent);
  }
  .agent-action-list {
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
    list-style: none;
  }
  .agent-action-row {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr) auto;
    align-items: center;
    gap: 9px;
    min-height: 38px;
    padding: 7px 8px;
    border: 1px solid var(--border-subtle);
    border-radius: 10px;
    background: color-mix(in srgb, var(--bg-card) 84%, transparent);
  }
  .agent-action-status {
    width: 8px;
    height: 8px;
    border-radius: 999px;
    background: #7a8163;
    box-shadow: 0 0 0 3px rgba(122, 129, 99, 0.14);
  }
  .agent-action-row.failed .agent-action-status {
    background: #c96a4b;
    box-shadow: 0 0 0 3px rgba(201, 106, 75, 0.14);
  }
  .agent-action-main {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .agent-action-title {
    font-size: 13px;
    font-weight: 700;
    color: var(--text-primary);
    overflow-wrap: anywhere;
  }
  .agent-action-meta {
    font-size: 11px;
    color: var(--text-muted);
    text-transform: capitalize;
    overflow-wrap: anywhere;
  }
  .agent-action-link {
    min-height: 28px;
    padding: 0 10px;
    border-radius: 999px;
    border: 1px solid var(--border);
    color: var(--accent-cyan);
    display: inline-flex;
    align-items: center;
    text-decoration: none;
    font-size: 11px;
    font-weight: 800;
  }
  .capture-inbox {
    margin: 8px 0 0;
    padding: 12px 14px;
    border-radius: 12px;
    border: 1px solid color-mix(in srgb, var(--entity-tint) 30%, var(--border));
    background: color-mix(in srgb, var(--entity-tint) 7%, var(--bg-card));
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .capture-inbox-head {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .capture-inbox-label {
    font-weight: 800;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    font-size: 10px;
    color: var(--text-secondary);
  }
  .capture-inbox-count {
    min-width: 20px;
    height: 20px;
    border-radius: 999px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    font-size: 11px;
    font-weight: 800;
    color: var(--text-primary);
    background: color-mix(in srgb, var(--entity-tint) 20%, transparent);
    border: 1px solid color-mix(in srgb, var(--entity-tint) 40%, transparent);
  }
  .capture-inbox-list {
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
    list-style: none;
  }
  .capture-row {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    align-items: center;
    gap: 9px;
    padding: 7px 8px;
    border: 1px solid var(--border-subtle);
    border-radius: 10px;
    background: color-mix(in srgb, var(--bg-card) 84%, transparent);
  }
  .capture-row-main {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .capture-row-text {
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow-wrap: anywhere;
  }
  .capture-row-meta {
    font-size: 11px;
    color: var(--text-muted);
    overflow-wrap: anywhere;
  }
  .capture-row-actions {
    display: inline-flex;
    flex-wrap: wrap;
    justify-content: flex-end;
    gap: 6px;
  }
  .capture-route {
    font-size: 11px;
    font-weight: 700;
    color: var(--accent-cyan);
    overflow-wrap: anywhere;
  }
  .capture-btn-attach {
    color: var(--accent-cyan);
    border-color: color-mix(in srgb, var(--accent-cyan) 45%, var(--border));
  }
  .capture-attach-select {
    min-height: 28px;
    padding: 0 8px;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
    font-size: 11px;
    font-weight: 700;
    cursor: pointer;
  }
  .capture-btn {
    min-height: 28px;
    padding: 0 10px;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
    font-size: 11px;
    font-weight: 800;
    cursor: pointer;
  }
  .capture-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .capture-btn-task {
    color: var(--accent-cyan);
    border-color: color-mix(in srgb, var(--accent-cyan) 45%, var(--border));
  }
  .capture-btn-dismiss:hover:not(:disabled) {
    color: var(--text-primary);
  }
  .capture-result {
    padding: 12px 14px;
    border-radius: 12px;
    border: 1px solid rgba(122, 129, 99, 0.35);
    background: rgba(20, 83, 45, 0.18);
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .capture-preview-head,
  .capture-result-head {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
    font-size: 12px;
  }
  .capture-preview-label,
  .capture-result-label {
    font-weight: 800;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    font-size: 10px;
    color: var(--text-secondary);
  }
  .capture-result-label {
    color: #7a8163;
  }
  .capture-preview-intent,
  .capture-result-intent {
    font-weight: 700;
    color: var(--text-primary);
    text-transform: capitalize;
  }
  .capture-preview-confidence {
    margin-left: auto;
    font-size: 11px;
    color: var(--text-muted);
  }
  .capture-preview-question,
  .capture-result-summary {
    margin: 0;
    font-size: 13px;
    line-height: 1.45;
    color: var(--text-primary);
  }
  .capture-preview-draft {
    margin: 0;
    padding-left: 1.1rem;
    font-size: 12px;
    color: var(--text-secondary);
    line-height: 1.4;
  }
  .capture-preview-draft .d-k {
    color: var(--text-muted);
    font-weight: 700;
    margin-right: 4px;
  }
  .capture-preview-candidates {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px;
  }
  .candidates-label {
    font-size: 11px;
    color: var(--text-muted);
  }
  .candidate-chip {
    min-height: 32px;
    padding: 0 12px;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: var(--bg-card);
    color: var(--accent-cyan);
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
  }
  .capture-preview-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }
  .preview-confirm {
    min-height: 40px;
    padding: 0 18px;
    border-radius: 10px;
    border: 1px solid rgba(201, 138, 75, 0.5);
    background: linear-gradient(180deg, rgba(201, 138, 75, 0.35), rgba(138, 90, 46, 0.55));
    color: #ffffff;
    font-size: 13px;
    font-weight: 800;
    cursor: pointer;
  }
  .preview-confirm:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
  .preview-cancel {
    min-height: 40px;
    padding: 0 16px;
    border-radius: 10px;
    border: 1px solid var(--border);
    background: var(--bg-secondary);
    color: var(--text-secondary);
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
  }

  .next-step-links {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }
  .next-link {
    min-height: 36px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--bg-secondary);
    color: var(--text-primary);
    text-decoration: none;
    font-size: 12px;
    font-weight: 700;
    padding: 0 12px;
  }
  .next-link:hover {
    border-color: var(--accent-cyan);
  }

  @media (max-width: 720px) {
    .context-bar {
      padding: 8px 12px;
    }
    .context-meta {
      display: none;
    }
    .composer-region {
      padding: 10px 10px 14px;
    }
  }
</style>
