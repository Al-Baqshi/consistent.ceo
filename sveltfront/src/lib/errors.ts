/**
 * Global error banner: set when API fails, clear on dismiss or next success.
 * Use setApiError() after fetch; show <ErrorBanner /> in layout.
 */

import { writable } from 'svelte/store';

export const apiError = writable<string | null>(null);

export function setApiError(message: string) {
  apiError.set(message);
}

export function clearApiError() {
  apiError.set(null);
}
