// Runtime Tauri bridge — dependency-free.
//
// Relies on `withGlobalTauri: true` in tauri.conf.json, which exposes
// `window.__TAURI__` inside the desktop webview. In a plain browser the
// global is undefined, so callers must branch via `isTauri()`.

type InvokeArgs = Record<string, unknown>;

interface TauriGlobal {
  core?: {
    invoke<T>(cmd: string, args?: InvokeArgs): Promise<T>;
  };
  event?: {
    listen<P = unknown>(
      event: string,
      handler: (payload: { payload: P }) => void,
    ): Promise<() => void>;
  };
}

function tauriGlobal(): TauriGlobal | null {
  if (typeof window === 'undefined') return null;
  const w = window as unknown as { __TAURI__?: TauriGlobal };
  return w.__TAURI__ ?? null;
}

export function isTauri(): boolean {
  return tauriGlobal() !== null;
}

export async function invoke<T>(cmd: string, args: InvokeArgs = {}): Promise<T> {
  const t = tauriGlobal();
  if (!t?.core?.invoke) {
    throw new Error('Tauri bridge not available');
  }
  return t.core.invoke<T>(cmd, args);
}

export async function listen<P = unknown>(
  event: string,
  handler: (payload: P) => void,
): Promise<() => void> {
  const t = tauriGlobal();
  if (!t?.event?.listen) {
    throw new Error('Tauri event bridge not available');
  }
  return t.event.listen<P>(event, (e) => handler(e.payload));
}
