import type {
  Approval,
  DailyReview,
  Entity,
  EntityDetail,
  FeedMessage,
  Mission,
  QuickAction,
  SystemHealth,
  TrustedDevice,
  Worker,
} from '../types';

/**
 * The demo-mode sample world: a small design studio (Linden Studio), its two
 * ventures — a coffee roastery (Harbor Coffee Co.) and a note-taking SaaS side
 * project (Fieldnote) — and a founder, Alex Rivera. Every name, address and
 * storyline here is invented; none of it may name a real person or company,
 * because it is what App Review and the screenshots see (Guideline 2.3.9).
 * Agent groups follow `agents_seed.json`.
 */

// Relative time helpers (minutes / hours / days ago) as ISO strings.
const now = Date.now();
const min = (m: number) => new Date(now - m * 60_000).toISOString();
const hr = (h: number) => new Date(now - h * 3_600_000).toISOString();
const day = (d: number) => new Date(now - d * 86_400_000).toISOString();

// ── Entities ───────────────────────────────────────────────────────────
export const entities: Entity[] = [
  {
    id: 'linden',
    name: 'Linden Studio',
    type: 'company',
    summary: 'Small design studio — the company behind the ventures.',
    stage: 'Operating',
    weight: 1,
    pinned: true,
    lastActiveAt: min(12),
    relatedIds: ['fieldnote', 'harbor', 'alex', 'ops-coordinator'],
    stats: { files: 34, notes: 21, missions: 4, memories: 58 },
  },
  {
    id: 'fieldnote',
    name: 'Fieldnote',
    type: 'project',
    summary: 'Note-taking SaaS side project. Onboarding + beta readiness in build.',
    stage: 'Build',
    weight: 0.9,
    pinned: true,
    lastActiveAt: min(40),
    relatedIds: ['linden', 'backend-engineer', 'legal-ops-analyst', 'beta-mission'],
    stats: { files: 27, notes: 14, missions: 3, memories: 33 },
  },
  {
    id: 'harbor',
    name: 'Harbor Coffee Co.',
    type: 'project',
    summary: 'Small-batch coffee roastery. Bean catalog + grower pipeline.',
    stage: 'Launch prep',
    weight: 0.72,
    pinned: false,
    lastActiveAt: hr(3),
    relatedIds: ['linden', 'content-producer', 'catalog-mission'],
    stats: { files: 19, notes: 9, missions: 2, memories: 20 },
  },
  {
    id: 'alex',
    name: 'Alex Rivera',
    type: 'person',
    summary: 'Founder. Primary decision maker.',
    weight: 0.85,
    pinned: true,
    lastActiveAt: min(2),
    relatedIds: ['linden', 'fieldnote', 'harbor'],
    stats: { files: 5, notes: 40, missions: 0, memories: 61 },
  },
  {
    id: 'beta-mission',
    name: 'Beta readiness',
    type: 'mission',
    summary: 'Audit + harden Fieldnote before the private beta.',
    stage: 'Running',
    weight: 0.6,
    pinned: false,
    lastActiveAt: min(18),
    relatedIds: ['fieldnote', 'backend-engineer', 'debugging-engineer'],
    stats: { files: 3, notes: 6, missions: 1, memories: 8 },
  },
  {
    id: 'catalog-mission',
    name: 'Catalog import',
    type: 'mission',
    summary: 'Normalize 140 bean listings into the Harbor Coffee catalog.',
    stage: 'Review',
    weight: 0.5,
    pinned: false,
    lastActiveAt: hr(1),
    relatedIds: ['harbor', 'ops-coordinator'],
    stats: { files: 2, notes: 3, missions: 1, memories: 5 },
  },
  {
    id: 'backend-engineer',
    name: 'Backend Engineer',
    type: 'agent',
    summary: 'Builds APIs, service logic, and integration layers.',
    weight: 0.55,
    pinned: false,
    lastActiveAt: min(6),
    relatedIds: ['fieldnote', 'beta-mission'],
    stats: { files: 0, notes: 2, missions: 2, memories: 12 },
  },
  {
    id: 'content-producer',
    name: 'Content Producer',
    type: 'agent',
    summary: 'Creates briefs, copy drafts, and content structures.',
    weight: 0.45,
    pinned: false,
    lastActiveAt: hr(2),
    relatedIds: ['harbor'],
    stats: { files: 0, notes: 4, missions: 1, memories: 9 },
  },
  {
    id: 'legal-ops-analyst',
    name: 'Legal Ops Analyst',
    type: 'agent',
    summary: 'Flags legal/risk review needs; prepares structured briefs.',
    weight: 0.4,
    pinned: false,
    lastActiveAt: hr(5),
    relatedIds: ['fieldnote'],
    stats: { files: 0, notes: 3, missions: 0, memories: 7 },
  },
  {
    id: 'ops-coordinator',
    name: 'Ops Coordinator',
    type: 'agent',
    summary: 'Keeps systems organized; routes and unblocks work.',
    weight: 0.5,
    pinned: false,
    lastActiveAt: min(30),
    relatedIds: ['linden', 'catalog-mission'],
    stats: { files: 0, notes: 5, missions: 1, memories: 14 },
  },
  {
    id: 'beta-plan',
    name: 'Beta launch plan',
    type: 'file',
    summary: 'Living doc — the path to the Fieldnote private beta.',
    weight: 0.35,
    pinned: false,
    lastActiveAt: day(1),
    relatedIds: ['fieldnote', 'linden'],
    stats: { files: 1, notes: 2, missions: 0, memories: 3 },
  },
  {
    id: 'brand-voice',
    name: 'Brand voice',
    type: 'memory',
    summary: 'Warm, precise, never hype. Applies across all ventures.',
    weight: 0.3,
    pinned: false,
    lastActiveAt: day(2),
    relatedIds: ['linden', 'harbor', 'content-producer'],
    stats: { files: 0, notes: 1, missions: 0, memories: 1 },
  },
];

export const entityById = (id: string) => entities.find((e) => e.id === id);

// ── Entity detail enrichment ───────────────────────────────────────────
export function buildEntityDetail(id: string): EntityDetail | null {
  const e = entityById(id);
  if (!e) return null;
  const related = e.relatedIds
    .map(entityById)
    .filter((x): x is Entity => Boolean(x));
  return {
    ...e,
    goals:
      e.type === 'project' || e.type === 'company'
        ? 'Ship the next milestone without breaking trust with early users.'
        : undefined,
    operatingRules:
      e.type === 'project' || e.type === 'company'
        ? 'No hallucinated claims. Escalate anything risky. Keep it local-first.'
        : undefined,
    notes: [
      {
        id: `${id}-n1`,
        entityId: id,
        title: 'Current focus',
        body: `${e.name}: ${e.summary} Next checkpoint is the milestone review.`,
        createdAt: hr(4),
        backlinks: e.relatedIds.slice(0, 2),
      },
      {
        id: `${id}-n2`,
        entityId: id,
        title: 'Open threads',
        body: 'Waiting on a grower confirmation and a terms review pass.',
        createdAt: day(1),
        backlinks: e.relatedIds.slice(0, 1),
      },
    ],
    files: [
      {
        id: `${id}-f1`,
        entityId: id,
        name: 'overview.md',
        kind: 'doc',
        size: '4.2 KB',
        updatedAt: hr(6),
      },
      {
        id: `${id}-f2`,
        entityId: id,
        name: 'roadmap.pdf',
        kind: 'doc',
        size: '128 KB',
        updatedAt: day(2),
      },
      {
        id: `${id}-f3`,
        entityId: id,
        name: 'kickoff.m4a',
        kind: 'audio',
        size: '2.1 MB',
        updatedAt: day(3),
      },
    ],
    missions: missions.filter((m) => m.entityId === id),
    memories: [
      { id: `${id}-m1`, text: `${e.name} prefers weekly check-ins over daily standups.`, at: day(1) },
      { id: `${id}-m2`, text: 'Decisions should be reversible where possible.', at: day(4) },
      { id: `${id}-m3`, text: 'Keep the operator in the loop for anything user-facing.', at: day(6) },
    ],
    activity: [
      { id: `${id}-a1`, entityId: id, actor: 'Backend Engineer', action: 'pushed an update to the sign-up flow', at: min(18) },
      { id: `${id}-a2`, entityId: id, actor: 'You', action: 'added a note "Current focus"', at: hr(4) },
      { id: `${id}-a3`, entityId: id, actor: 'System', action: 'ran a heartbeat check', at: hr(7) },
    ],
    related,
    live: false,
  };
}

// ── Missions ───────────────────────────────────────────────────────────
export const missions: Mission[] = [
  {
    id: 'beta-mission',
    title: 'Get Fieldnote ready for beta',
    status: 'running',
    entityId: 'fieldnote',
    entityName: 'Fieldnote',
    agentId: 'backend-engineer',
    agentName: 'Backend Engineer',
    progress: 0.62,
    dept: 'Full Stack Development',
    updatedAt: min(18),
  },
  {
    id: 'catalog-mission',
    title: 'Import & normalize the bean catalog',
    status: 'review',
    entityId: 'harbor',
    entityName: 'Harbor Coffee Co.',
    agentId: 'ops-coordinator',
    agentName: 'Ops Coordinator',
    progress: 0.88,
    dept: 'Operations',
    updatedAt: hr(1),
  },
  {
    id: 'legal-mission',
    title: 'Draft Fieldnote terms & privacy brief',
    status: 'blocked',
    entityId: 'fieldnote',
    entityName: 'Fieldnote',
    agentId: 'legal-ops-analyst',
    agentName: 'Legal Ops Analyst',
    progress: 0.3,
    dept: 'Law',
    updatedAt: hr(5),
  },
  {
    id: 'content-mission',
    title: 'Launch page copy — 3 variations',
    status: 'queued',
    entityId: 'harbor',
    entityName: 'Harbor Coffee Co.',
    agentId: 'content-producer',
    agentName: 'Content Producer',
    progress: 0,
    dept: 'Content & Creative',
    updatedAt: hr(2),
  },
  {
    id: 'summary-mission',
    title: 'Weekly cross-venture summary',
    status: 'done',
    entityId: 'linden',
    entityName: 'Linden Studio',
    agentId: 'ops-coordinator',
    agentName: 'Ops Coordinator',
    progress: 1,
    dept: 'Operations',
    updatedAt: day(1),
  },
];

// ── Workers ────────────────────────────────────────────────────────────
export const workers: Worker[] = [
  { id: 'backend-engineer', name: 'Backend Engineer', role: 'Builds APIs & integration layers', groupId: 'full-stack-development', status: 'running', currentMissionId: 'beta-mission', load: 0.7 },
  { id: 'ops-coordinator', name: 'Ops Coordinator', role: 'Routes & unblocks work', groupId: 'operations', status: 'running', currentMissionId: 'catalog-mission', load: 0.45 },
  { id: 'legal-ops-analyst', name: 'Legal Ops Analyst', role: 'Legal & risk review', groupId: 'law', status: 'paused', currentMissionId: 'legal-mission', load: 0.2 },
  { id: 'content-producer', name: 'Content Producer', role: 'Briefs, copy & structure', groupId: 'content-creative', status: 'idle', load: 0 },
  { id: 'debugging-engineer', name: 'Debugging Engineer', role: 'Traces failures to root cause', groupId: 'full-stack-development', status: 'idle', load: 0 },
];

// ── Approvals ──────────────────────────────────────────────────────────
export const approvals: Approval[] = [
  {
    id: 'appr-1',
    title: 'Run the Fieldnote data migration',
    detail: 'Backend Engineer wants to run the production data migration as part of beta readiness. It can’t be rolled back once beta users sign in.',
    risk: 'high',
    requestedBy: 'Backend Engineer',
    createdAt: min(14),
    status: 'pending',
  },
  {
    id: 'appr-2',
    title: 'Publish the Harbor Coffee catalog draft',
    detail: 'Ops Coordinator normalized 140 bean listings and is ready to publish the draft catalog for your review.',
    risk: 'low',
    requestedBy: 'Ops Coordinator',
    createdAt: min(52),
    status: 'pending',
  },
  {
    id: 'appr-3',
    title: 'Send grower follow-up email',
    detail: 'Draft email to three coffee growers asking for updated harvest dates. Will send from hello@harborcoffee.example.',
    risk: 'medium',
    requestedBy: 'Ops Coordinator',
    createdAt: hr(2),
    status: 'pending',
  },
];

// ── Companion feed ─────────────────────────────────────────────────────
export const quickActions: QuickAction[] = [
  { id: 'qa-ask', label: 'Ask about projects', icon: '◆', intent: 'ask' },
  { id: 'qa-capture', label: 'Capture a thought', icon: '✎', intent: 'capture' },
  { id: 'qa-approve', label: 'Approvals', icon: '✓', intent: 'approve' },
  { id: 'qa-mission', label: 'Start a mission', icon: '➤', intent: 'mission' },
  { id: 'qa-summary', label: 'Summarize today', icon: '✦', intent: 'summary' },
  { id: 'qa-entity', label: 'Open an entity', icon: '⬡', intent: 'entity' },
  { id: 'qa-auth', label: 'Authenticate session', icon: '⚿', intent: 'auth' },
];

export const feedMessages: FeedMessage[] = [
  {
    id: 'fm-1',
    role: 'system',
    variant: 'heartbeat',
    text: 'Heartbeat · Backend Engineer advanced Fieldnote beta readiness to 62%. Two checks remain.',
    at: min(18),
  },
  {
    id: 'fm-2',
    role: 'assistant',
    text: "Morning. Two things want you: a high-risk data migration on Fieldnote, and the Harbor Coffee catalog is ready to publish. Want me to walk you through the migration first?",
    at: min(16),
    followUps: ['Walk me through the migration', 'Show the catalog', "What's the risk?"],
  },
  {
    id: 'fm-3',
    role: 'assistant',
    variant: 'default',
    text: 'This needs your approval before I proceed:',
    at: min(14),
    approval: approvals[0],
  },
  {
    id: 'fm-4',
    role: 'user',
    text: 'How is Harbor Coffee doing this week?',
    at: min(9),
  },
  {
    id: 'fm-5',
    role: 'assistant',
    text: 'Harbor Coffee is in launch prep. The bean catalog import is at 88% and in review, launch copy is queued, and the grower pipeline is waiting on two harvest-date confirmations. Nothing is blocked on you except publishing the catalog.',
    at: min(8),
    followUps: ['Open Harbor Coffee', 'Publish the catalog', 'Draft the grower email'],
  },
  {
    id: 'fm-6',
    role: 'system',
    variant: 'status',
    text: 'Ops Coordinator finished the weekly cross-venture summary. It’s in Review.',
    at: min(4),
  },
];

// ── Daily review ───────────────────────────────────────────────────────
export const dailyReview: DailyReview = {
  date: new Date(now).toISOString(),
  live: false,
  headline: 'Steady day. One high-risk decision waiting, everything else moving.',
  sections: [
    {
      id: 'rv-sum',
      kind: 'summary',
      title: 'Today',
      items: [
        { id: 's1', text: '3 missions advanced, 1 completed, 1 blocked on the terms review.' },
        { id: 's2', text: 'Fieldnote beta readiness reached 62%.' },
        { id: 's3', text: 'Harbor Coffee catalog import ready to publish.' },
      ],
    },
    {
      id: 'rv-done',
      kind: 'completed',
      title: 'Completed',
      items: [
        { id: 'c1', text: 'Weekly cross-venture summary', meta: 'Ops Coordinator', entityId: 'linden' },
        { id: 'c2', text: 'Normalized 140 bean listings', meta: 'Ops Coordinator', entityId: 'harbor' },
      ],
    },
    {
      id: 'rv-q',
      kind: 'questions',
      title: 'Open questions',
      items: [
        { id: 'q1', text: 'Approve the Fieldnote data migration?', entityId: 'fieldnote' },
        { id: 'q2', text: 'Which launch-copy direction for Harbor Coffee?', entityId: 'harbor' },
      ],
    },
    {
      id: 'rv-missed',
      kind: 'missed',
      title: 'Missed / stale',
      items: [
        { id: 'm1', text: 'Terms & privacy brief has been blocked 5h waiting on inputs.', entityId: 'fieldnote' },
      ],
    },
    {
      id: 'rv-next',
      kind: 'next',
      title: 'Suggested next steps',
      items: [
        { id: 'n1', text: 'Approve or defer the data migration to unblock beta.' },
        { id: 'n2', text: 'Publish the Harbor Coffee catalog draft.' },
        { id: 'n3', text: 'Unblock the terms brief by answering its three questions.' },
      ],
    },
    {
      id: 'rv-updates',
      kind: 'updates',
      title: 'Recent entity updates',
      items: [
        { id: 'u1', text: 'Fieldnote — sign-up flow updated', meta: '18m ago', entityId: 'fieldnote' },
        { id: 'u2', text: 'Harbor Coffee — catalog moved to review', meta: '1h ago', entityId: 'harbor' },
      ],
    },
    {
      id: 'rv-hb',
      kind: 'heartbeats',
      title: 'Worker heartbeats',
      items: [
        { id: 'h1', text: 'Backend Engineer · running · beta readiness 62%', meta: '18m' },
        { id: 'h2', text: 'Ops Coordinator · running · catalog review', meta: '30m' },
        { id: 'h3', text: 'Legal Ops Analyst · paused · awaiting inputs', meta: '5h' },
      ],
    },
  ],
};

// ── System / control ───────────────────────────────────────────────────
/**
 * The offline shape only. Counts are deliberately absent rather than invented:
 * with no gateway there is nothing to count, and `live: false` tells the Control
 * screen to render em-dashes instead of numbers.
 */
export const systemHealth: SystemHealth = {
  gatewayConnected: false,
  gatewayUrl: 'http://127.0.0.1:18800',
  desktopPaired: false,
  paused: false,
  live: false,
};

export const trustedDevices: TrustedDevice[] = [
  { id: 'dev-phone', name: 'iPhone 16 Pro', platform: 'iOS', lastSeen: min(0), current: true, trusted: true },
  { id: 'dev-desktop', name: 'Studio Mac', platform: 'macOS · desktop', lastSeen: min(3), current: false, trusted: true },
  { id: 'dev-ipad', name: 'iPad Pro', platform: 'iPadOS', lastSeen: hr(20), current: false, trusted: true },
];

// Sign-in requests have no fixture on purpose. Approving a device is a security
// decision, so the phone shows only what `GET /api/auth/approvals` returns —
// never a sample one. See `listPendingSessions`.

// ── Calendar / schedule ────────────────────────────────────────────────
// Items spread across the current month so the calendar has life.
const dayAt = (offsetDays: number, hour: number) => {
  const d = new Date(now + offsetDays * 86_400_000);
  d.setHours(hour, 0, 0, 0);
  return d.toISOString();
};

export const scheduleItems = [
  { id: 'cal-1', title: 'Beta readiness checkpoint', at: dayAt(0, 15), kind: 'mission' as const, agentName: 'Backend Engineer', entityId: 'fieldnote', entityName: 'Fieldnote' },
  { id: 'cal-2', title: 'Publish the Harbor Coffee catalog draft', at: dayAt(0, 17), kind: 'scheduled' as const, agentName: 'Ops Coordinator', entityId: 'harbor', entityName: 'Harbor Coffee Co.' },
  { id: 'cal-3', title: 'Daily review', at: dayAt(0, 20), kind: 'review' as const },
  { id: 'cal-4', title: 'Grower follow-up emails', at: dayAt(1, 9), kind: 'scheduled' as const, agentName: 'Ops Coordinator', entityId: 'harbor', entityName: 'Harbor Coffee Co.' },
  { id: 'cal-5', title: 'Terms & privacy brief due', at: dayAt(2, 12), kind: 'mission' as const, agentName: 'Legal Ops Analyst', entityId: 'fieldnote', entityName: 'Fieldnote' },
  { id: 'cal-6', title: 'Weekly cross-venture summary', at: dayAt(4, 8), kind: 'scheduled' as const, agentName: 'Ops Coordinator', entityId: 'linden', entityName: 'Linden Studio' },
  { id: 'cal-7', title: 'Fieldnote beta go / no-go', at: dayAt(6, 14), kind: 'mission' as const, entityId: 'fieldnote', entityName: 'Fieldnote' },
  { id: 'cal-8', title: 'Launch copy review — 3 variants', at: dayAt(8, 11), kind: 'mission' as const, agentName: 'Content Producer', entityId: 'harbor', entityName: 'Harbor Coffee Co.' },
  { id: 'cal-9', title: 'Migration rehearsal', at: dayAt(-2, 10), kind: 'scheduled' as const, entityId: 'fieldnote', entityName: 'Fieldnote' },
  { id: 'cal-10', title: 'Catalog import kickoff', at: dayAt(-5, 9), kind: 'mission' as const, agentName: 'Ops Coordinator', entityId: 'harbor', entityName: 'Harbor Coffee Co.' },
];
