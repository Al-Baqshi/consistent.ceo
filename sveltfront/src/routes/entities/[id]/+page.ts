import { api, fetchAllPages } from '$lib/api';

/**
 * Dependency key for everything this page loads. The fetches below go through
 * `api()` (plain `fetch`), so SvelteKit cannot track them by URL; actions on
 * the page call `invalidate(_ENTITY_DEPENDENCY)` to re-run this load.
 */
export const _ENTITY_DEPENDENCY = 'app:entity';

export type Task = {
  id: string;
  title: string;
  status: string;
  progress: number;
  entity_id?: string;
  mission_id?: string;
  task_kind?: string;
  assigned_director_id?: string;
  assigned_agent_id?: string;
  result?: string;
  requires_human_approval?: boolean;
};

export type Mission = {
  id: string;
  entity_id: string;
  title: string;
  summary: string;
  status: 'planned' | 'active' | 'blocked' | 'user-input-needed' | 'completed';
  priority: string;
  goal: string;
  owner?: string;
  created_by?: string;
  source?: string;
  created_at: string;
  updated_at: string;
  last_reviewed_at?: string;
  review_cadence_hours?: number;
};

export type MissionPolicy = {
  starterTaskAutoQueueingEnabled: boolean;
  followupTaskProposalEnabled: boolean;
  followupAutoCreateEnabled: boolean;
  defaultReviewCadenceHours: number;
};

export type TaskKindRecommendation = {
  taskKind: string;
  directorId?: string;
  provider?: string;
  model?: string;
  mode?: string;
  generationTier?: string;
  reviewTier?: string;
  fallbackTiers?: string[];
};

export type Relationship = { type: string; target: string; status: string; notes?: string };

export type EnrichmentDraft = {
  profile?: string;
  goals?: string;
  activeContext?: string;
  operatingRules?: string;
  notes?: string;
  contentPipeline?: string;
};

export type EntityEnrichment = {
  status: string;
  error?: string;
  inputSnapshot?: { name?: string; description?: string; department?: string; owner?: string };
  draft?: EnrichmentDraft;
  generatedBy?: string;
  model?: string;
  generatedAt?: string;
  draftVersion?: number;
};

export type EntitySetup = {
  whatItIs?: string;
  primaryGoal?: string;
  successCriteria?: string;
  primaryDomain?: string;
  primaryEmail?: string;
  repoPath?: string;
  backendStack?: string;
  frontendStack?: string;
  databaseStack?: string;
};

export type EntityReadiness = {
  percent: number;
  label: string;
  nextQuestion?: string;
  missingFields?: string[];
};

export type EntityFlowDiagrams = {
  previous?: string | null;
  current?: string | null;
  future?: string | null;
  updatedAt?: string | null;
};

export type Entity = {
  id: string;
  name: string;
  status: string;
  /** True for the user's private, auto-provisioned Personal Space. */
  isPersonal?: boolean;
  type?: string;
  dept: string;
  fileCount: number;
  path?: string;
  /** The operator-chosen local folder (`state::Entity.local_path`). */
  localPath?: string;
  description?: string;
  stage?: string;
  domains?: string[];
  markets?: string[];
  profile?: string;
  goals?: string;
  activeContext?: string;
  operatingRules?: string;
  notes?: string;
  contentPipeline?: string;
  relationships?: Relationship[];
  enrichment?: EntityEnrichment;
  resources?: {
    id: string;
    kind: string;
    root: string;
    readAllowed?: boolean;
    writeAllowed?: boolean;
    execAllowed?: boolean;
  }[];
  executionPolicies?: {
    allowedCapabilities?: string[];
    allowedExecutionLevels?: string[];
    approvalRequiredCapabilities?: string[];
  } | null;
  defaultWorkspaceRoot?: string;
  memoryScope?: string;
  preferredModelTier?: string;
  allowedAgentGroups?: string[];
  setup?: EntitySetup;
  readiness?: EntityReadiness;
  entityRuntime?: {
    automationMode?: 'manual' | 'suggest' | 'auto_run';
    heartbeatMode?: 'off' | 'suggest' | 'execute';
    defaultChatSessionId?: string;
    quietHoursLocal?: { start: string; end: string };
  };
  flowDiagrams?: EntityFlowDiagrams;
};

export type EntityConnection = {
  id: string;
  entityId: string;
  kind: string;
  provider: string;
  label: string;
  status: string;
  capabilityState?: Record<string, unknown>;
  config?: Record<string, unknown>;
  secretRef?: string;
  externalAccountId?: string;
  externalHandle?: string;
  scopes?: string[];
  lastCheckedAt?: string;
  lastError?: string;
  createdBy?: string;
  createdAt: string;
  updatedAt: string;
};

export type EntityConnectionEvent = {
  id: string;
  timestamp: string;
  entityId: string;
  connectionId: string;
  kind: string;
  detail?: string;
};

export type OperatorFeedEvent = {
  id: string;
  timestamp: string;
  kind: string;
  title: string;
  detail?: string;
  taskId?: string;
  missionId?: string;
  entityId?: string;
  connectionId?: string;
  approvalId?: string;
};

export type PendingApprovalRow = {
  id: string;
  createdAt: string;
  updatedAt: string;
  entityId: string;
  connectionId?: string;
  taskId?: string;
  action: string;
  status: string;
  detail?: string;
  createdBy?: string;
};

export type EntityMemoryPreview = {
  shortTerm: string[];
  longTerm: string[];
};

export type ScheduledTaskRow = {
  id: string;
  cronExpression: string;
  nextRun: string;
  agentId: string;
  taskDescription: string;
  entityId?: string;
  connectionId?: string;
};

export type AgentRoleLite = {
  id: string;
  name?: string;
};

export type EntityFile = {
  id: string;
  entity_id: string;
  filename: string;
  content_type: string;
  size_bytes: number;
  uploaded_at: string;
  path: string;
};

/// One sourced fact about the entity's world — a customer, a lead, a deal, a
/// content item, a connected account, a file, a metric, an event. `kind` is
/// open-ended; the eight above are documented, not enforced. `source` +
/// `sourceRef` together are where this row came from and what makes syncing
/// it idempotent — see `docs/integrations.md` on the gateway.
export type EntityRecord = {
  id: string;
  entityId: string;
  kind: string;
  source: string;
  sourceRef: string;
  fields: Record<string, unknown>;
  evidence?: string;
  fetchedAt: string;
  createdAt: string;
  updatedAt: string;
  createdBy?: string;
};

export async function load({
  params,
  depends
}: {
  params: { id: string };
  depends: (...deps: `${string}:${string}`[]) => void;
}) {
  depends(_ENTITY_DEPENDENCY);
  const id = params.id;
  const scoped = encodeURIComponent(id);
  try {
    const [
      entity,
      tasks,
      missions,
      missionPolicy,
      connections,
      connectionEvents,
      records,
      operatorFeed,
      pendingApprovals,
      memoryPreview,
      scheduleTasks,
      agents
    ] = await Promise.all([
      api<Entity>(`/api/entities/${id}`),
      // Scoped to this entity and read to the end: a bare `/api/tasks` returns
      // the workspace's oldest 50, which left newer entities showing no tasks.
      fetchAllPages<Task>(`/api/tasks?entity_id=${scoped}`).catch(() => [] as Task[]),
      fetchAllPages<Mission>(`/api/missions?entity_id=${scoped}`).catch(() => [] as Mission[]),
      api<MissionPolicy>('/api/missions/policy').catch(() => ({
        starterTaskAutoQueueingEnabled: true,
        followupTaskProposalEnabled: true,
        followupAutoCreateEnabled: true,
        defaultReviewCadenceHours: 24
      })),
      api<EntityConnection[]>(`/api/entities/${id}/connections`).catch(() => []),
      api<EntityConnectionEvent[]>(`/api/entities/${id}/connection-events`).catch(() => []),
      api<EntityRecord[]>(`/api/entities/${id}/records`).catch(() => []),
      api<OperatorFeedEvent[]>(`/api/operator-feed?entity_id=${encodeURIComponent(id)}&limit=80`).then(r => (r as any).items ?? r ?? []).catch(() => []),
      api<PendingApprovalRow[]>(
        `/api/approvals?entity_id=${encodeURIComponent(id)}&status=pending`
      ).catch(() => []),
      api<EntityMemoryPreview>(`/api/entities/${id}/memory-preview`).catch(() => ({
        shortTerm: [],
        longTerm: []
      })),
      api<ScheduledTaskRow[]>('/api/schedule').catch(() => []),
      api<AgentRoleLite[]>('/api/agents').catch(() => [])
    ]);
    return {
      entity,
      tasks,
      missions,
      missionPolicy,
      connections,
      connectionEvents,
      records,
      operatorFeed,
      pendingApprovals,
      memoryPreview,
      scheduleTasks,
      agents,
      error: null
    };
  } catch (err) {
    const message = err instanceof Error ? err.message : 'Failed to load entity';
    return {
      entity: null,
      tasks: [],
      missions: [],
      missionPolicy: null,
      connections: [],
      connectionEvents: [],
      records: [],
      operatorFeed: [],
      pendingApprovals: [],
      memoryPreview: { shortTerm: [], longTerm: [] },
      scheduleTasks: [],
      agents: [],
      error: message
    };
  }
}
