export type FeedDetailMap = Record<string, string>;

export type MissionLikeFeedEvent = {
  kind?: string;
  title?: string;
  detail?: string;
};

export function parseFeedDetail(detail?: string): FeedDetailMap {
  if (!detail) return {};
  const out: FeedDetailMap = {};
  for (const rawLine of detail.split('\n')) {
    const line = rawLine.trim();
    if (!line) continue;
    const idx = line.indexOf('=');
    if (idx <= 0) continue;
    const key = line.slice(0, idx).trim();
    const value = line.slice(idx + 1).trim();
    if (key && value) out[key] = value;
  }
  return out;
}

function humanizeCode(value?: string): string | undefined {
  return value ? value.replaceAll('_', ' ') : undefined;
}

export function humanizeAutorunBlockers(raw?: string): string | undefined {
  if (!raw) return undefined;
  const blockers = raw.split(',').map((item) => item.trim()).filter(Boolean);
  if (blockers.length === 0) return undefined;

  const mapped = blockers.map((blocker) => {
    switch (blocker) {
      case 'director_heartbeat_disabled':
        return 'all directors with this work are paused';
      case 'task_requires_human_approval':
        return 'the next task is waiting for human approval';
      case 'entity_heartbeat_off':
        return 'this entity has heartbeat task pickup turned off';
      case 'task_routed_to_other_director':
        return 'the task is routed to a different director than the ones currently able to run it';
      case 'director_has_no_agents':
        return 'a director exists but has no agents available';
      case 'no_directors_configured':
        return 'no directors are configured yet';
      case 'no_open_mission_tasks':
        return 'there is no open mission task to run';
      default:
        return humanizeCode(blocker) ?? blocker;
    }
  });

  if (mapped.length === 1) return mapped[0];
  if (mapped.length === 2) return `${mapped[0]} and ${mapped[1]}`;
  return `${mapped.slice(0, -1).join(', ')}, and ${mapped.at(-1)}`;
}

export function describeMissionFeedEvent(event: MissionLikeFeedEvent): string | null {
  const detail = parseFeedDetail(event.detail);
  const reason = detail.reason;

  if (reason === 'autorun_blocked') {
    const blockers = humanizeAutorunBlockers(detail.blockers);
    if (detail.action === 'mission_create') {
      return blockers
        ? `Mission created, but it did not start automatically because ${blockers}.`
        : 'Mission created, but it did not start automatically.';
    }
    return blockers
      ? `Mission could not keep running automatically because ${blockers}.`
      : 'Mission could not keep running automatically.';
  }

  if (reason === 'no_next_executable_task') {
    const branch = detail.branch;
    if (branch === 'bootstrap') {
      return 'Mission has no runnable work yet. Create the first concrete task to get it moving.';
    }
    if (branch === 'unblock') {
      return 'Mission has open work, but every current path is blocked. Queue an unblock task or fix the blocker.';
    }
    if (branch === 'user_input') {
      return 'Mission cannot continue until someone provides input or resolves the stalled decision.';
    }
    return 'Mission has no runnable next task right now. Add the next implementation step to continue.';
  }

  if (reason === 'inactive_no_progress') {
    return 'Mission has been quiet for a while with no progress, so it is now waiting for user input.';
  }

  if (reason === 'duplicate_open_next_step') {
    return 'No new follow-up was created because an open next-step task already exists.';
  }

  if (reason === 'all_tasks_completed') {
    return 'No follow-up was created because the current mission task set is already complete.';
  }

  if (reason === 'autocreate_cooldown') {
    return 'Auto-follow-up is temporarily cooling down before creating another task.';
  }

  if (reason === 'recommend_only_mode') {
    return 'Mission review produced a suggestion, but auto-create is off so no task was started.';
  }

  if (reason === 'entity_manual_automation') {
    return 'This entity is in manual automation mode, so the mission can suggest next steps but will not auto-run them.';
  }

  if (reason === 'insufficient_progress_for_next_phase') {
    return 'Mission is not far enough along yet to auto-create the next phase.';
  }

  return null;
}

export function extractFeedReason(detail?: string): string | undefined {
  const parsed = parseFeedDetail(detail);
  if (parsed.reason === 'autorun_blocked' && parsed.blockers) {
    return humanizeAutorunBlockers(parsed.blockers);
  }
  return humanizeCode(parsed.reason);
}
