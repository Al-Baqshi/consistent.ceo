// Skills on the phone: browse, read, and switch on or off.
//
// Writing and importing skills happens on the computer (the dashboard), where
// there is a keyboard and a file system; the phone is the remote control. In
// demo mode (no paired gateway) the built-in Consistent CEO playbook is shown
// read-only so the screen is explorable — nothing is invented beyond the names
// and descriptions the gateway itself ships.
import { APIError, http, isDemoGateway, tryGateway } from './client';

export type SkillScope = 'builtin' | 'global' | 'entity';

export type SkillSummary = {
  id: string;
  slug: string;
  name: string;
  title: string;
  description: string;
  scope: SkillScope;
  entityId?: string;
  enabled: boolean;
  editable: boolean;
  active: boolean;
  shadowedBy?: string;
  files: string[];
  updatedAt?: string;
  warnings?: string[];
};

export type SkillDetail = SkillSummary & { body: string; raw: string };

export type SkillsList = {
  skills: SkillSummary[];
  entities: { id: string; name: string }[];
  demo: boolean;
};

const DEMO: [string, string, string][] = [
  ['ceo-daily-brief', 'CEO daily brief', 'A one-screen morning brief across every venture: what needs a decision today, what is at risk, what moved.'],
  ['venture-weekly-review', 'Venture weekly review', 'The weekly operating review for one venture — shipped, slipped, numbers, decisions, next three priorities.'],
  ['decision-memo', 'Decision memo', 'Turns a messy choice into a one-page memo with real options, trade-offs and a recommendation.'],
  ['investor-update', 'Investor update', 'Drafts a concise monthly update for angels and VCs: highlights, metrics, lowlights, asks.'],
  ['mission-planner', 'Mission planner', 'Turns a goal into a verifiable mission with a definition of done, milestones and first tasks.'],
  ['meeting-to-actions', 'Meeting to actions', 'Turns notes or a transcript into decisions, owners and tasks filed under the right venture.'],
  ['delegation-brief', 'Delegation brief', 'Writes a brief a teammate, contractor or agent can execute without coming back with questions.'],
  ['customer-signal-synthesis', 'Customer signal synthesis', 'Clusters customer feedback into ranked themes with quotes and recommended actions.'],
  ['skill-author', 'Skill author', 'Turns a workflow you just walked through into a reusable skill for one venture or all of them.'],
];

function demoSkills(): SkillsList {
  return {
    demo: true,
    entities: [],
    skills: DEMO.map(([slug, title, description]) => ({
      id: `builtin:${slug}`,
      slug,
      name: slug,
      title,
      description,
      scope: 'builtin' as const,
      enabled: true,
      editable: false,
      active: true,
      files: [],
    })),
  };
}

export async function listSkills(): Promise<SkillsList> {
  const real = await tryGateway('skills', () =>
    http.get<{ skills: SkillSummary[]; entities: { id: string; name: string }[] }>('/api/skills'),
  );
  if (real) return { ...real, demo: false };
  return demoSkills();
}

export async function getSkill(id: string): Promise<SkillDetail | null> {
  if (await isDemoGateway()) return null;
  return http.get<SkillDetail>(`/api/skills/${encodeURIComponent(id)}`);
}

export async function setSkillEnabled(id: string, enabled: boolean): Promise<SkillSummary> {
  return http.post<SkillSummary>(`/api/skills/${encodeURIComponent(id)}/enabled`, { enabled });
}

/** A sentence for the user from a failed skills call. */
export function skillsErrorMessage(e: unknown): string {
  if (e instanceof APIError) {
    if (e.status === 401) return "You're signed out — sign in again to change skills.";
    if (e.status === 404) return 'That skill is gone — pull to refresh.';
    if (e.status === 503) return "Your computer isn't ready yet — try again in a moment.";
    return 'The skill couldn’t be changed. Try again.';
  }
  return "Can't reach your computer right now.";
}
