import { api, fetchAllPages, getMissionMetrics } from '$lib/api';

type Mission = {
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
};

type Task = {
  id: string;
  title: string;
  status: 'backlog' | 'queued' | 'in-progress' | 'failed' | 'completed';
  progress: number;
  mission_id?: string;
  entity_id?: string;
  task_kind?: string;
  priority?: string;
  assigned_director_id?: string;
  assigned_agent_id?: string;
  result?: string;
};

type OperatorFeedKind =
  | 'task_created'
  | 'task_completed'
  | 'task_blocked'
  | 'approval_needed'
  | 'mission_created'
  | 'mission_stalled'
  | 'mission_missing_next_task'
  | 'mission_review_due'
  | 'mission_followup_suppressed'
  | 'mission_recommendation_accepted'
  | 'mission_recommendation_dismissed'
  | 'mission_snoozed'
  | 'mission_user_input_needed';

type MissionEvent = {
  id: string;
  timestamp: string;
  kind: OperatorFeedKind;
  taskId?: string;
  missionId?: string;
  title: string;
  detail?: string;
};

type DirectorView = {
  id: string;
  name: string;
  heartbeatEnabled: boolean;
};

export async function load({ params }: { params: { id: string } }) {
  const id = params.id;
  try {
    const [mission, tasks, events, metrics, directors] = await Promise.all([
      api<Mission>(`/api/missions/${id}`),
      // Filtered server-side and read to the end — unfiltered, this was the
      // workspace's oldest 50 tasks, so a newer mission showed none of its own.
      fetchAllPages<Task>(`/api/tasks?mission_id=${encodeURIComponent(id)}`).catch(() => [] as Task[]),
      api<MissionEvent[]>(`/api/operator-feed?mission_id=${encodeURIComponent(id)}&limit=80`).then(r => (r as any).items ?? r ?? []).catch(() => []),
      getMissionMetrics().catch(() => null),
      api<DirectorView[]>('/api/directors').catch(() => []),
    ]);
    const entityActivity = await api<MissionEvent[]>(
      `/api/operator-feed?entity_id=${encodeURIComponent(mission.entity_id)}&limit=60`
    ).then(r => (r as any).items ?? r ?? []).catch(() => []);
    return {
      mission,
      tasks: (tasks ?? []).filter((task: Task) => task.mission_id === id),
      events: events ?? [],
      entityActivity: (entityActivity ?? []).filter((event: MissionEvent) => event.missionId !== id),
      directors: directors ?? [],
      metrics,
      error: null,
    };
  } catch (err) {
    const message = err instanceof Error ? err.message : 'Failed to load mission';
    return {
      mission: null,
      tasks: [],
      events: [],
      entityActivity: [],
      directors: [],
      metrics: null,
      error: message,
    };
  }
}
