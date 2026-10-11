type MissionStatus = 'planned' | 'active' | 'blocked' | 'user-input-needed' | 'completed';

export type MissionStateInput = {
  missionStatus?: MissionStatus;
  hasNextTask: boolean;
  nextTaskTitle?: string;
  allBlocked: boolean;
  allCompleted: boolean;
  latestEventKind?: string;
  latestSuppressionReason?: string;
};

export function deriveCurrentMissionState(input: MissionStateInput): string {
  // Stable precedence to avoid contradictory labels across screens.
  if (input.latestEventKind === 'mission_snoozed') return 'Review snoozed';
  if (input.missionStatus === 'user-input-needed') return 'Waiting on user input';
  if (input.allBlocked) return 'Blocked by open dependency task';
  if (input.hasNextTask) return `Next action: ${input.nextTaskTitle ?? 'execute queued next task'}`;
  if (input.allCompleted) return 'No follow-up needed';
  if (input.latestEventKind === 'mission_followup_suppressed') {
    const reason = (input.latestSuppressionReason ?? '').toLowerCase();
    if (reason.includes('all_tasks_completed')) {
      return 'No follow-up created because mission is already complete';
    }
    if (reason.includes('duplicate_open_next_step')) {
      return 'No new task created because an open next-step task already exists';
    }
    if (reason.includes('autocreate_cooldown')) return 'No review action due yet';
    return 'No follow-up created (suppressed by policy or state)';
  }
  return 'Review due / no clear next action';
}
