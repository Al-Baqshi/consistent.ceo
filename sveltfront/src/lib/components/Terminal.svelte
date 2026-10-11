<script lang="ts">
  // Embedded terminal (xterm.js) bound to one desktop PTY session.
  //
  // Renders a real terminal only inside the Tauri webview; on web / mobile it
  // shows a compact placeholder. xterm touches `window` at import time, so the
  // library, the fit addon and its stylesheet are all loaded dynamically inside
  // onMount — nothing here references `window` / `document` at module scope.
  //
  // The parent composes any toolbar; this component exposes `start()`,
  // `restart()`, `kill()`, `focus()`, `write()`, `isRunning()` and
  // `sessionId()` via `bind:this`.
  import { onMount } from 'svelte';
  import type { ITheme, Terminal as XTerm } from '@xterm/xterm';
  import type { FitAddon } from '@xterm/addon-fit';
  import {
    onPtyExit,
    onPtyOutput,
    ptyAvailable,
    ptyKill,
    ptyResize,
    ptySpawn,
    ptyWrite
  } from '$lib/pty';

  interface Props {
    /** argv to run (via the login shell). Takes precedence over `script`. */
    command?: string[];
    /** Shell string run via `$SHELL -l -c`. Neither → interactive login shell. */
    script?: string;
    cwd?: string;
    /** Spawn as soon as the terminal is ready. Default true. */
    autostart?: boolean;
    /** Visual height in rows; width always fills the container. Default 18. */
    rows?: number;
    /** Once per session, when the process ends (null = killed / failed to spawn). */
    onexit?: (code: number | null) => void;
    /** Once per distinct `https://` URL seen in the output of a session. */
    onurl?: (url: string) => void;
    /** Session id, right after a successful spawn. */
    onready?: (id: string) => void;
    class?: string;
  }

  let {
    command,
    script,
    cwd,
    autostart = true,
    rows = 18,
    onexit,
    onurl,
    onready,
    class: className = ''
  }: Props = $props();

  const available = ptyAvailable();

  const FONT_SIZE = 13;
  const RESIZE_DEBOUNCE_MS = 50;
  /** How long `kill()` waits for the backend's `pty-exit` before giving up. */
  const KILL_EXIT_GRACE_MS = 1000;
  const EARLY_CHUNK_CAP = 256;
  const URL_TAIL_CAP = 4096;
  const URL_RE = /https:\/\/[^\s\x00-\x1f\x7f"'<>`]+/g;
  const TRAILING_PUNCT_RE = /[.,;:!?'")\]}>]+$/;

  let mountEl = $state<HTMLDivElement | null>(null);

  let term: XTerm | null = null;
  let fit: FitAddon | null = null;
  let id: string | null = null;
  let running = false;
  let destroyed = false;
  let startPromise: Promise<void> | null = null;
  let unlisteners: Array<() => void> = [];
  let resizeTimer: ReturnType<typeof setTimeout> | null = null;
  let lastCols = 0;
  let lastRows = 0;

  // Resolves once the terminal exists (or the component is torn down), so a
  // parent may call start() before the dynamic import has finished.
  let resolveReady: () => void = () => {};
  const ready = new Promise<void>((resolve) => {
    resolveReady = resolve;
  });

  // Events for a session can race the `pty_spawn` response over the IPC.
  // While a spawn is in flight, stash events for ids we don't know yet and
  // replay the ones that turn out to be ours.
  let spawning = false;
  let earlyOutput = new Map<string, Uint8Array[]>();
  let earlyExit = new Map<string, number | null>();
  let earlyChunks = 0;

  let exitWaiters = new Map<string, () => void>();

  // URL detection over the decoded output stream.
  let urlDecoder: TextDecoder | null = null;
  let urlTail = '';
  let seenUrls = new Set<string>();

  function currentRows(): number {
    return Math.max(1, Math.floor(rows));
  }

  function cssVar(cs: CSSStyleDeclaration, name: string, fallback: string): string {
    return cs.getPropertyValue(name).trim() || fallback;
  }

  function withAlpha(color: string, alpha: number): string {
    const hex = color.match(/^#([0-9a-f]{3}|[0-9a-f]{6})$/i);
    if (hex) {
      let h = hex[1];
      if (h.length === 3) h = h.split('').map((c) => c + c).join('');
      const r = parseInt(h.slice(0, 2), 16);
      const g = parseInt(h.slice(2, 4), 16);
      const b = parseInt(h.slice(4, 6), 16);
      return `rgba(${r}, ${g}, ${b}, ${alpha})`;
    }
    const rgb = color.match(/^rgba?\(\s*([\d.]+)\s*,\s*([\d.]+)\s*,\s*([\d.]+)/i);
    if (rgb) return `rgba(${rgb[1]}, ${rgb[2]}, ${rgb[3]}, ${alpha})`;
    return color;
  }

  function readTheme(): ITheme {
    const cs = getComputedStyle(document.documentElement);
    const background = cssVar(cs, '--bg-card', cssVar(cs, '--surface-sunken', '#16130f'));
    const foreground = cssVar(cs, '--text-primary', '#f2ede4');
    const accent = cssVar(cs, '--accent-cyan', foreground);
    const muted = cssVar(cs, '--text-muted', foreground);
    return {
      background,
      foreground,
      cursor: accent,
      cursorAccent: background,
      selectionBackground: withAlpha(accent, 0.35),
      selectionInactiveBackground: withAlpha(accent, 0.2),
      scrollbarSliderBackground: withAlpha(muted, 0.3),
      scrollbarSliderHoverBackground: withAlpha(muted, 0.45),
      scrollbarSliderActiveBackground: withAlpha(muted, 0.6)
    };
  }

  function readMonoFont(): string {
    const cs = getComputedStyle(document.documentElement);
    return cssVar(
      cs,
      '--font-mono',
      "'JetBrains Mono', 'SF Mono', Menlo, Monaco, 'Cascadia Code', Consolas, monospace"
    );
  }

  function applyTheme(): void {
    if (!term) return;
    term.options.theme = readTheme();
  }

  /** Fit columns to the container; rows come from the prop. */
  function refit(): void {
    if (!term || !fit) return;
    const dims = fit.proposeDimensions();
    const cols =
      dims && Number.isFinite(dims.cols) && dims.cols > 0 ? Math.floor(dims.cols) : term.cols;
    const r = currentRows();
    if (cols !== term.cols || r !== term.rows) term.resize(cols, r);
    if (running && id && (cols !== lastCols || r !== lastRows)) {
      lastCols = cols;
      lastRows = r;
      void ptyResize(id, cols, r).catch(() => {});
    }
  }

  function scheduleRefit(): void {
    if (resizeTimer !== null) clearTimeout(resizeTimer);
    resizeTimer = setTimeout(() => {
      resizeTimer = null;
      if (!destroyed) refit();
    }, RESIZE_DEBOUNCE_MS);
  }

  function emitUrl(raw: string): void {
    const url = raw.replace(TRAILING_PUNCT_RE, '');
    if (url.length <= 'https://'.length || seenUrls.has(url)) return;
    seenUrls.add(url);
    onurl?.(url);
  }

  /** Longest suffix of `s` that is a strict prefix of "https://". */
  function partialSchemePrefix(s: string): string {
    for (let n = Math.min(7, s.length); n > 0; n--) {
      const suffix = s.slice(-n);
      if ('https://'.startsWith(suffix)) return suffix;
    }
    return '';
  }

  function scanText(text: string, final: boolean): void {
    const buf = urlTail + text;
    URL_RE.lastIndex = 0;
    let m: RegExpExecArray | null;
    while ((m = URL_RE.exec(buf)) !== null) {
      const end = m.index + m[0].length;
      if (end === buf.length && !final && m[0].length < URL_TAIL_CAP) {
        // Might continue in the next chunk — hold it until a delimiter shows up.
        urlTail = buf.slice(m.index);
        return;
      }
      emitUrl(m[0]);
    }
    urlTail = final ? '' : partialSchemePrefix(buf);
  }

  function scanBytes(bytes: Uint8Array): void {
    if (!onurl) return;
    if (!urlDecoder) urlDecoder = new TextDecoder('utf-8');
    scanText(urlDecoder.decode(bytes, { stream: true }), false);
  }

  function flushUrlScan(): void {
    if (!urlDecoder) return;
    const rest = urlDecoder.decode();
    scanText(rest, true);
    urlDecoder = null;
  }

  function resetUrlScan(): void {
    urlDecoder = null;
    urlTail = '';
    seenUrls = new Set();
  }

  function handleOutput(evId: string, bytes: Uint8Array): void {
    if (id !== null && evId === id) {
      term?.write(bytes);
      scanBytes(bytes);
      return;
    }
    if (spawning && earlyChunks < EARLY_CHUNK_CAP) {
      const list = earlyOutput.get(evId) ?? [];
      list.push(bytes);
      earlyOutput.set(evId, list);
      earlyChunks++;
    }
  }

  function handleExit(evId: string, code: number | null): void {
    if (id !== null && evId === id) {
      endSession(evId, code);
      return;
    }
    if (spawning) earlyExit.set(evId, code);
  }

  /** Idempotent per session: the first caller wins, later ones are ignored. */
  function endSession(sid: string, code: number | null): void {
    if (id !== sid) return;
    id = null;
    running = false;
    flushUrlScan();
    const label = code === null ? '[process exited]' : `[process exited with code ${code}]`;
    term?.write(`\r\n\x1b[2m${label}\x1b[0m\r\n`);
    exitWaiters.get(sid)?.();
    exitWaiters.delete(sid);
    onexit?.(code);
  }

  function waitForExit(sid: string, ms: number): Promise<void> {
    return new Promise((resolve) => {
      const timer = setTimeout(done, ms);
      function done() {
        clearTimeout(timer);
        exitWaiters.delete(sid);
        resolve();
      }
      exitWaiters.set(sid, done);
    });
  }

  function writeError(message: string): void {
    term?.write(`\r\n\x1b[31m${message}\x1b[0m\r\n`);
  }

  async function doStart(): Promise<void> {
    await ready;
    if (destroyed || !term || !fit || running) return;

    refit();
    const cols = term.cols;
    const r = term.rows;

    spawning = true;
    earlyOutput = new Map();
    earlyExit = new Map();
    earlyChunks = 0;
    resetUrlScan();

    try {
      const newId = await ptySpawn({ command, script, cwd, cols, rows: r });
      if (destroyed) {
        void ptyKill(newId).catch(() => {});
        return;
      }
      id = newId;
      running = true;
      lastCols = cols;
      lastRows = r;

      for (const chunk of earlyOutput.get(newId) ?? []) {
        term.write(chunk);
        scanBytes(chunk);
      }
      onready?.(newId);
      if (earlyExit.has(newId)) endSession(newId, earlyExit.get(newId) ?? null);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      writeError(message);
      onexit?.(null);
    } finally {
      spawning = false;
      earlyOutput = new Map();
      earlyExit = new Map();
      earlyChunks = 0;
    }
  }

  /** Spawn the session. No-op when one is already running or starting. */
  export function start(): Promise<void> {
    if (!available) return Promise.resolve();
    if (startPromise) return startPromise;
    startPromise = doStart().finally(() => {
      startPromise = null;
    });
    return startPromise;
  }

  /** Kill the current session, if any. Resolves once it has ended. */
  export async function kill(): Promise<void> {
    if (startPromise) await startPromise;
    const sid = id;
    if (!sid) return;
    const exited = waitForExit(sid, KILL_EXIT_GRACE_MS);
    try {
      await ptyKill(sid);
    } catch (_) {
      // idempotent on the backend; a failure here means the session is gone
    }
    await exited;
    // Backend never confirmed — close the session locally so start() can run.
    endSession(sid, null);
  }

  export async function restart(): Promise<void> {
    await kill();
    await start();
  }

  export function focus(): void {
    term?.focus();
  }

  /** Send keystrokes to the running process. No-op when nothing is running. */
  export function write(data: string): void {
    if (!running || !id) return;
    void ptyWrite(id, data).catch(() => {});
  }

  export function isRunning(): boolean {
    return running;
  }

  export function sessionId(): string | null {
    return id;
  }

  async function init(): Promise<void> {
    try {
      const [xterm, fitAddon] = await Promise.all([
        import('@xterm/xterm'),
        import('@xterm/addon-fit'),
        import('@xterm/xterm/css/xterm.css')
      ]);
      if (destroyed || !mountEl) return;

      const t = new xterm.Terminal({
        convertEol: true,
        cursorBlink: true,
        fontFamily: readMonoFont(),
        fontSize: FONT_SIZE,
        rows: currentRows(),
        scrollback: 2000,
        macOptionIsMeta: true,
        theme: readTheme()
      });
      const f = new fitAddon.FitAddon();
      t.loadAddon(f);
      t.open(mountEl);
      t.onData((data) => {
        if (running && id) void ptyWrite(id, data).catch(() => {});
      });
      term = t;
      fit = f;
      refit();

      const unOut = await onPtyOutput(handleOutput);
      if (destroyed) {
        unOut();
        return;
      }
      unlisteners.push(unOut);
      const unExit = await onPtyExit(handleExit);
      if (destroyed) {
        unExit();
        return;
      }
      unlisteners.push(unExit);
    } catch (err) {
      console.error('terminal init failed', err);
      writeError(err instanceof Error ? err.message : String(err));
      return;
    } finally {
      resolveReady();
    }

    if (autostart) void start();
  }

  function teardown(): void {
    destroyed = true;
    resolveReady();
    if (resizeTimer !== null) {
      clearTimeout(resizeTimer);
      resizeTimer = null;
    }
    for (const un of unlisteners.splice(0)) {
      try {
        un();
      } catch (_) {}
    }
    for (const done of exitWaiters.values()) done();
    exitWaiters.clear();
    if (id) {
      void ptyKill(id).catch(() => {});
      id = null;
      running = false;
    }
    term?.dispose();
    term = null;
    fit = null;
  }

  onMount(() => {
    if (!available) return;

    const themeObserver = new MutationObserver(() => applyTheme());
    themeObserver.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ['data-theme']
    });

    const resizeObserver = new ResizeObserver(() => scheduleRefit());
    if (mountEl) resizeObserver.observe(mountEl);

    void init();

    return () => {
      themeObserver.disconnect();
      resizeObserver.disconnect();
      teardown();
    };
  });

  // Follow later changes to the `rows` prop.
  $effect(() => {
    const r = currentRows();
    if (term && term.rows !== r) refit();
  });
</script>

{#if available}
  <div class="terminal {className}">
    <div class="terminal-mount" bind:this={mountEl}></div>
  </div>
{:else}
  <div class="terminal terminal-unavailable {className}">
    <p class="terminal-note">The terminal is available in the desktop app.</p>
  </div>
{/if}

<style>
  .terminal {
    box-sizing: border-box;
    min-width: 0;
    overflow: hidden;
    padding: 8px;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: var(--radius-lg, 8px);
    transition: border-color var(--transition-base, 150ms ease);
  }
  .terminal:focus-within {
    border-color: var(--border-focus, var(--border));
  }
  .terminal-mount {
    width: 100%;
    min-width: 0;
    overflow: hidden;
  }
  .terminal-unavailable {
    padding: 12px;
  }
  .terminal-note {
    margin: 0;
    font-size: 0.8125rem;
    line-height: 1.4;
    color: var(--text-muted);
  }
</style>
