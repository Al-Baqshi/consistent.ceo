import { create } from 'zustand';

import { kvGet, kvRemove, kvSet } from './db';

/**
 * Consent to send chat and voice to AI (App Review 5.1.2(i)).
 *
 * Nothing the operator types or says leaves the phone for an AI service until
 * they have read where it goes and said yes: chat `send()`, the entity "Ask
 * about…" composer and both voice entry points await `ensure()` from
 * `useAiConsentGate` first. The answer is revocable from Settings → Privacy.
 *
 * Stored in SQLite rather than AsyncStorage so the phone can query it without
 * the gateway. SQLite lives in the app's documents folder and is wiped when
 * the app is deleted — a reinstall asks again instead of inheriting a yes the
 * person may not remember giving. Only a yes is persisted; "Not now" lasts
 * until the next attempt.
 */

export const PREF_AI_CONSENT = 'cw.pref.aiConsent.v1';

type ConsentStore = {
  /** null until read from storage. */
  granted: boolean | null;
  load: () => Promise<boolean>;
  set: (granted: boolean) => Promise<void>;
};

export const useAiConsentStore = create<ConsentStore>((set, get) => ({
  granted: null,
  async load() {
    const known = get().granted;
    if (known !== null) return known;
    let granted = false;
    try {
      granted = (await kvGet(PREF_AI_CONSENT)) === '1';
    } catch {
      granted = false;
    }
    set({ granted });
    return granted;
  },
  async set(granted) {
    set({ granted });
    try {
      if (granted) await kvSet(PREF_AI_CONSENT, '1');
      else await kvRemove(PREF_AI_CONSENT);
    } catch {
      /* best-effort: the in-memory answer still gates this session */
    }
  },
}));

/** Whether the operator has agreed to AI data sharing. Reads storage once. */
export function hasAiConsent(): Promise<boolean> {
  return useAiConsentStore.getState().load();
}

export function setAiConsent(granted: boolean): Promise<void> {
  return useAiConsentStore.getState().set(granted);
}
