/**
 * User-level profile: who you are, what you want, and **how you work**.
 *
 * The server (`crates/gateway/src/user_profile.rs`) infers a working style from
 * a three-question first run. This module loads it once, shares it as a store,
 * and turns it into the one thing that matters: which environment you get.
 *
 * - `holistic` — the full environment. Every nav surface, multiple entities.
 * - `linear`   — a focused environment. Chat, Today, Tasks, Missions; land on
 *                `/today`; steer toward one active entity.
 * - `unset`    — we don't know, so we hide nothing. Same as holistic.
 *
 * Requests here deliberately do **not** go through `$lib/api`: that helper
 * raises the global error banner on any failure, and this is a best-effort
 * enhancement. A gateway without auth, an expired session, or an older build
 * must all degrade silently to "show everything", never to a red banner.
 */

import { writable, get } from 'svelte/store';
import { getAuthToken, getTenantId } from '$lib/api';

const API_BASE = import.meta.env.VITE_consistent_world_API_BASE || '';

export type WorkingStyle = 'unset' | 'holistic' | 'linear';

export type OnboardingAnswers = {
  identity?: string;
  intent?: string;
  focus?: string;
  focusChoice?: string;
  autonomy?: string;
};

export type UserProfile = {
  displayName?: string;
  role?: string;
  primaryIntent?: string;
  workingStyle: WorkingStyle;
  onboardedAt?: string;
  showAllNav: boolean;
  answers: OnboardingAnswers;
};

export type MeResponse = {
  userId: string;
  username: string;
  workspaceId: string;
  email?: string;
  phone?: string;
  profile: UserProfile;
};

/** What a client assumes when the server has nothing to say: hide nothing. */
export const DEFAULT_PROFILE: UserProfile = {
  workingStyle: 'unset',
  showAllNav: false,
  answers: {}
};

/**
 * The loaded profile, or `null` while unknown. `null` is *not* "no profile" —
 * it means "not answered yet", and every consumer treats it as the full
 * environment so nothing flickers away mid-load.
 */
export const userProfile = writable<UserProfile | null>(null);

/** True once a load attempt has settled, successfully or not. */
export const profileLoaded = writable(false);

let inflight: Promise<UserProfile | null> | null = null;

async function request<T>(path: string, init: RequestInit = {}): Promise<T | null> {
  if (typeof window === 'undefined') return null;
  try {
    const headers: Record<string, string> = {
      'Content-Type': 'application/json',
      ...((init.headers as Record<string, string>) ?? {})
    };
    const token = getAuthToken();
    if (token) headers['Authorization'] = `Bearer ${token}`;
    headers['x-workspace-id'] = getTenantId();

    const res = await fetch(`${API_BASE}${path}`, { ...init, headers });
    if (!res.ok) return null;
    if (res.status === 204) return null;
    return (await res.json()) as T;
  } catch {
    // Offline, gateway down, older build without the route — all the same
    // outcome: we simply don't know the user's style.
    return null;
  }
}

/** Load the profile once per page load. Concurrent callers share the request. */
export function ensureProfile(force = false): Promise<UserProfile | null> {
  if (!force) {
    const cached = get(userProfile);
    if (cached) return Promise.resolve(cached);
    if (inflight) return inflight;
  }
  inflight = request<MeResponse>('/api/me')
    .then((me) => {
      const profile = me?.profile ?? null;
      userProfile.set(profile);
      return profile;
    })
    .finally(() => {
      profileLoaded.set(true);
      inflight = null;
    });
  return inflight;
}

export type ProfilePatch = {
  displayName?: string;
  role?: string;
  primaryIntent?: string;
  workingStyle?: WorkingStyle;
  showAllNav?: boolean;
  markOnboarded?: boolean;
};

/** Partial update. Returns the saved profile, or `null` if the write failed. */
export async function saveProfile(patch: ProfilePatch): Promise<UserProfile | null> {
  const updated = await request<UserProfile>('/api/me/profile', {
    method: 'PUT',
    body: JSON.stringify(patch)
  });
  if (updated) userProfile.set(updated);
  return updated;
}

/** Single submit of the whole first-run flow. The server classifies the style. */
export async function submitOnboarding(
  answers: OnboardingAnswers,
  displayName?: string
): Promise<UserProfile | null> {
  const updated = await request<UserProfile>('/api/me/onboarding', {
    method: 'POST',
    body: JSON.stringify({ answers, displayName })
  });
  if (updated) userProfile.set(updated);
  return updated;
}

/** Has this user been through (or skipped) first run? */
export function isOnboarded(profile: UserProfile | null): boolean {
  return Boolean(profile?.onboardedAt);
}

/**
 * Is the focused (linear) environment in effect right now?
 *
 * Pure. `showAllNav` is the escape hatch — a linear user who wants everything
 * back flips it and is never trapped.
 */
export function isFocusedEnvironment(profile: UserProfile | null): boolean {
  if (!profile) return false;
  return profile.workingStyle === 'linear' && !profile.showAllNav;
}

/**
 * The nav filter, as a pure function of (entries, style, override).
 *
 * Linear keeps only what a single-track operator needs — the entries marked
 * `focused` (Chat, Today, Tasks, Missions). Holistic and unset keep everything.
 * Nothing is ever removed from the *bottom* nav, so settings — and therefore
 * the way back — is always one click away.
 */
export function filterNav<T extends { focused?: boolean }>(
  entries: T[],
  style: WorkingStyle,
  showAll: boolean
): T[] {
  if (showAll || style !== 'linear') return entries;
  return entries.filter((e) => e.focused === true);
}

/** Where a user lands after first run. Linear starts on the one-screen day. */
export function landingRouteFor(style: WorkingStyle): string {
  return style === 'linear' ? '/today' : '/';
}

/** Human label for a working style, used in settings and confirmations. */
export function workingStyleLabel(style: WorkingStyle): string {
  switch (style) {
    case 'holistic':
      return 'Holistic';
    case 'linear':
      return 'Focused';
    default:
      return 'Not set';
  }
}
