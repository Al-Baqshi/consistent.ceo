import { create } from 'zustand';

import { api } from '@/api';
import { auth, type AuthUser } from '@/api/auth';
import { notify } from '@/lib/notifications';
import type {
  Approval,
  CompanionFeed,
  DailyReview,
  Entity,
  FeedMessage,
  Mission,
  Session,
  SystemHealth,
  TrustedDevice,
  Worker,
} from '@/api';

/**
 * Single app store. Lean by design — screens select the slice they need.
 * Async actions call the API layer (mock today, gateway later) and write
 * results back. Optimistic updates for approvals / pause so the UI feels
 * instant.
 */

interface AppState {
  // Identity — who this phone is signed in as. Null = signed out (or the
  // gateway can't be reached), which the UI must say plainly rather than
  // showing a plausible-looking name.
  user: AuthUser | null;
  userLoading: boolean;

  // Chat — one list on screen, two sources: the conversation this phone had
  // (user/assistant turns) and operator-feed events from the gateway
  // (system rows). Reloading events must never touch the conversation.
  feed: FeedMessage[];
  /** Gateway chat session from the last turn's `meta` line, reused next turn. */
  chatSessionId: string | undefined;
  quickActions: CompanionFeed['quickActions'];
  needsAttention: Approval[];
  feedLoading: boolean;
  /** False once a load falls back to the sample conversation. */
  feedLive: boolean;

  // Entities
  entities: Entity[];
  entitiesLoading: boolean;

  // Missions / workers
  missions: Mission[];
  workers: Worker[];

  // Control
  health: SystemHealth | null;
  devices: TrustedDevice[];
  sessions: Session[];

  // Review
  review: DailyReview | null;

  // Brain map: user-arranged planet positions (world coords)
  nodeOffsets: Record<string, { x: number; y: number }>;
  setNodeOffset: (id: string, pos: { x: number; y: number }) => void;

  // Actions
  loadUser: () => Promise<void>;
  signOut: () => Promise<void>;
  loadFeed: () => Promise<void>;
  pushMessage: (msg: FeedMessage) => void;
  replaceMessage: (id: string, msg: FeedMessage) => void;
  setChatSessionId: (id: string | undefined) => void;
  loadEntities: () => Promise<void>;
  loadMissions: () => Promise<void>;
  loadWorkers: () => Promise<void>;
  loadControl: () => Promise<void>;
  loadReview: () => Promise<void>;
  approve: (id: string) => Promise<void>;
  reject: (id: string) => Promise<void>;
  togglePinned: (id: string) => void;
  authenticate: (sessionId: string, approve: boolean) => Promise<void>;
}

/** Epoch millis for ordering; unparseable stamps sort as "now" (end of list). */
function stamp(m: FeedMessage): number {
  const t = Date.parse(m.at);
  return Number.isNaN(t) ? Date.now() : t;
}

/**
 * Fold a fresh operator-feed load into what is on screen.
 *
 * System rows are the gateway's — the new window replaces the old one. Every
 * other row is this phone's conversation and is kept as-is: opening Approvals
 * (which reloads the feed) used to replace the whole list and wipe the chat.
 * Incoming non-system rows (the demo sample conversation) are added once, by
 * id. The result is ordered by time, ties keeping their existing order so a
 * user turn and its reply never swap.
 */
function mergeFeed(current: FeedMessage[], incoming: FeedMessage[]): FeedMessage[] {
  const conversation = current.filter((m) => m.role !== 'system');
  const known = new Set(conversation.map((m) => m.id));
  const added = incoming.filter((m) => m.role !== 'system' && !known.has(m.id));
  const events = incoming.filter((m) => m.role === 'system');
  return [...conversation, ...added, ...events]
    .map((m, i) => ({ m, i, t: stamp(m) }))
    .sort((a, b) => a.t - b.t || a.i - b.i)
    .map(({ m }) => m);
}

export const useAppStore = create<AppState>((set, get) => ({
  user: null,
  userLoading: false,

  feed: [],
  chatSessionId: undefined,
  quickActions: [],
  needsAttention: [],
  feedLoading: false,
  feedLive: true,

  entities: [],
  entitiesLoading: false,

  missions: [],
  workers: [],

  health: null,
  devices: [],
  sessions: [],

  review: null,

  nodeOffsets: {},
  setNodeOffset(id, pos) {
    set((s) => ({ nodeOffsets: { ...s.nodeOffsets, [id]: pos } }));
  },

  async loadUser() {
    set({ userLoading: true });
    if (!(await auth.isSignedIn())) {
      set({ user: null, userLoading: false });
      return;
    }
    try {
      set({ user: await auth.me(), userLoading: false });
    } catch {
      // Expired token or unreachable gateway — treat as signed out rather
      // than showing a stale identity.
      set({ user: null, userLoading: false });
    }
  },

  async signOut() {
    await auth.logout();
    set({ user: null });
  },

  async loadFeed() {
    set({ feedLoading: true });
    try {
      const feed = await api.getCompanionFeed();
      set((s) => ({
        feed: mergeFeed(s.feed, feed.messages),
        quickActions: feed.quickActions,
        needsAttention: feed.needsAttention,
        feedLive: feed.live,
        feedLoading: false,
      }));
    } catch {
      // Real gateway configured but unreachable — keep whatever is on screen
      // (the conversation so far) and let the banner say why nothing new came.
      set({ feedLoading: false });
    }
  },

  pushMessage(msg) {
    set((s) => ({ feed: [...s.feed, msg] }));
  },

  replaceMessage(id, msg) {
    set((s) => ({ feed: s.feed.map((m) => (m.id === id ? msg : m)) }));
  },

  setChatSessionId(id) {
    set({ chatSessionId: id });
  },

  async loadEntities() {
    set({ entitiesLoading: true });
    try {
      const entities = await api.listEntities();
      set({ entities, entitiesLoading: false });
    } catch {
      set({ entitiesLoading: false });
    }
  },

  async loadMissions() {
    try {
      const missions = await api.listMissions();
      set({ missions });
    } catch {
      // Unreachable real gateway — keep the current list.
    }
  },

  async loadWorkers() {
    try {
      const workers = await api.listWorkers();
      set({ workers });
    } catch {
      // Unreachable real gateway — keep the current list.
    }
  },

  async loadControl() {
    // Each read fails soft on its own: with a real gateway down,
    // `getSystemHealth` still resolves (as "offline") and one rejection must
    // not blank the whole Control tab.
    const [health, devices, sessions, missions, workers] = await Promise.all([
      api.getSystemHealth(),
      api.listTrustedDevices().catch(() => get().devices),
      api.listPendingSessions(),
      api.listMissions().catch(() => get().missions),
      api.listWorkers().catch(() => get().workers),
    ]);
    set({ health, devices, sessions, missions, workers });
  },

  async loadReview() {
    try {
      const review = await api.getDailyReview();
      set({ review });
    } catch {
      // Unreachable real gateway — keep the last review (or the empty state).
    }
  },

  async approve(id) {
    const approval = get().needsAttention.find((a) => a.id === id);
    // optimistic
    set((s) => ({
      needsAttention: s.needsAttention.filter((a) => a.id !== id),
      feed: s.feed.map((m) =>
        m.approval?.id === id
          ? { ...m, approval: { ...m.approval, status: 'approved' as const } }
          : m,
      ),
    }));
    try {
      await api.approveAction(id);
    } catch {
      // The decision never reached the gateway — put the row back and say so.
      if (approval) set((s) => ({ needsAttention: [approval, ...s.needsAttention] }));
      notify('Not approved', "Couldn't reach your computer — the approval is still pending.", {
        seconds: 1,
      });
      return;
    }
    // Real local notification (with sound) so approvals feel consequential.
    notify('Approved ✓', approval?.title ?? 'Action approved — executing now.', { seconds: 1 });
  },

  async reject(id) {
    const approval = get().needsAttention.find((a) => a.id === id);
    set((s) => ({
      needsAttention: s.needsAttention.filter((a) => a.id !== id),
      feed: s.feed.map((m) =>
        m.approval?.id === id
          ? { ...m, approval: { ...m.approval, status: 'rejected' as const } }
          : m,
      ),
    }));
    try {
      await api.rejectAction(id);
    } catch {
      if (approval) set((s) => ({ needsAttention: [approval, ...s.needsAttention] }));
      notify('Not rejected', "Couldn't reach your computer — the approval is still pending.", {
        seconds: 1,
      });
    }
  },

  togglePinned(id) {
    set((s) => ({
      entities: s.entities.map((e) =>
        e.id === id ? { ...e, pinned: !e.pinned } : e,
      ),
    }));
  },

  /**
   * Approve or deny a sign-in on the gateway. The row leaves the list at once,
   * but only stays gone if the write landed — a refused or unreachable call
   * puts it back rather than letting the phone claim it decided something.
   */
  async authenticate(sessionId, approve) {
    const previous = get().sessions;
    // The code came with the request and is on screen; the gateway compares it,
    // so it has to travel back with the approval.
    const code = previous.find((x) => x.id === sessionId)?.code;
    set({ sessions: previous.filter((x) => x.id !== sessionId) });
    try {
      await api.authenticateSession(sessionId, approve, code);
    } catch {
      set({ sessions: previous });
      throw new Error(
        approve
          ? "That approval didn't reach your computer — the sign-in is still waiting."
          : "That denial didn't reach your computer — the sign-in is still waiting.",
      );
    }
  },
}));
