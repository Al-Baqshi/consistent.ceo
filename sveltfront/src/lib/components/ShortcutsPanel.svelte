<script lang="ts">
  // Wispr-Flow-style shortcut editor for the desktop shell's global hotkeys.
  // Renders only inside the Tauri webview — in a plain browser there is no
  // shortcut backend, so the parent gates on isTauri().
  import { onMount } from 'svelte';
  import { invoke } from '$lib/tauri';

  type Binding = { accelerator: string; hold: boolean };
  type ShortcutsConfig = { toggleWindow: Binding; dictate: Binding; command: Binding };
  type ActionKey = keyof ShortcutsConfig;

  type Row = {
    key: ActionKey;
    name: string;
    hint: string;
    /** Whether hold-to-talk makes sense for this action. */
    holdable: boolean;
  };

  const rows: Row[] = [
    {
      key: 'dictate',
      name: 'Dictation',
      hint: 'Speak and the words are typed where your cursor is.',
      holdable: true
    },
    {
      key: 'command',
      name: 'Command mode',
      hint: 'Speak and the assistant acts on it — reminders, notes, tasks.',
      holdable: true
    },
    {
      key: 'toggleWindow',
      name: 'Show / hide Consistent CEO',
      hint: 'Bring the workspace window up from anywhere.',
      holdable: false
    }
  ];

  const isMac =
    typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform ?? '');

  let config = $state<ShortcutsConfig | null>(null);
  let recording = $state<ActionKey | null>(null);
  /** Modifiers currently held while recording, for live feedback. */
  let liveMods = $state<string[]>([]);
  let error = $state('');
  let saved = $state('');

  async function load() {
    try {
      config = await invoke<ShortcutsConfig>('shortcuts_get');
    } catch (err) {
      console.error('shortcuts_get failed', err);
      error = 'Could not load shortcuts from the desktop shell.';
    }
  }

  /** "CmdOrCtrl+Shift+KeyD" → ["⌘", "⇧", "D"] (mac) / ["Ctrl", "Shift", "D"]. */
  function chips(accelerator: string): string[] {
    return accelerator.split('+').map((token) => {
      const t = token.trim();
      const upper = t.toUpperCase();
      if (['CMDORCTRL', 'CMDORCONTROL', 'COMMANDORCONTROL', 'COMMANDORCTRL'].includes(upper))
        return isMac ? '⌘' : 'Ctrl';
      if (['CMD', 'COMMAND', 'SUPER', 'META'].includes(upper)) return isMac ? '⌘' : 'Win';
      if (['CTRL', 'CONTROL'].includes(upper)) return isMac ? '⌃' : 'Ctrl';
      if (['ALT', 'OPTION'].includes(upper)) return isMac ? '⌥' : 'Alt';
      if (upper === 'SHIFT') return isMac ? '⇧' : 'Shift';
      if (upper.startsWith('KEY')) return t.slice(3).toUpperCase();
      if (upper.startsWith('DIGIT')) return t.slice(5);
      if (upper === 'SPACE') return 'Space';
      return t;
    });
  }

  function modsFromEvent(e: KeyboardEvent): string[] {
    const mods: string[] = [];
    // Prefer the cross-platform token so one config file works on every OS.
    if (isMac ? e.metaKey : e.ctrlKey) mods.push('CmdOrCtrl');
    if (isMac && e.ctrlKey) mods.push('Ctrl');
    if (!isMac && e.metaKey) mods.push('Super');
    if (e.altKey) mods.push('Alt');
    if (e.shiftKey) mods.push('Shift');
    return mods;
  }

  const MODIFIER_CODES = new Set([
    'MetaLeft', 'MetaRight', 'ControlLeft', 'ControlRight',
    'AltLeft', 'AltRight', 'ShiftLeft', 'ShiftRight', 'CapsLock', 'Fn'
  ]);

  function onRecordKey(e: KeyboardEvent) {
    if (!recording) return;
    e.preventDefault();
    e.stopPropagation();

    if (e.code === 'Escape') {
      stopRecording();
      return;
    }
    const mods = modsFromEvent(e);
    if (MODIFIER_CODES.has(e.code)) {
      // Only modifiers so far — show them live and keep waiting for a key.
      liveMods = mods;
      return;
    }
    const isFnKey = /^F\d{1,2}$/.test(e.code);
    if (mods.length === 0 && !isFnKey) {
      error = 'Add a modifier (⌘ / Ctrl / Alt / Shift) — a bare key would block typing everywhere.';
      return;
    }
    void commit(recording, [...mods, e.code].join('+'));
  }

  function startRecording(key: ActionKey) {
    recording = key;
    liveMods = [];
    error = '';
    saved = '';
    window.addEventListener('keydown', onRecordKey, true);
    window.addEventListener('keyup', onRecordUp, true);
  }

  function onRecordUp(e: KeyboardEvent) {
    if (!recording) return;
    liveMods = modsFromEvent(e);
  }

  function stopRecording() {
    recording = null;
    liveMods = [];
    window.removeEventListener('keydown', onRecordKey, true);
    window.removeEventListener('keyup', onRecordUp, true);
  }

  async function commit(key: ActionKey, accelerator: string) {
    const hold = config?.[key]?.hold ?? false;
    stopRecording();
    try {
      config = await invoke<ShortcutsConfig>('shortcuts_set', {
        action: key,
        accelerator,
        hold
      });
      error = '';
      saved = key;
      setTimeout(() => (saved = ''), 2000);
    } catch (err) {
      error = String(err);
    }
  }

  async function setHold(key: ActionKey, hold: boolean) {
    if (!config) return;
    try {
      config = await invoke<ShortcutsConfig>('shortcuts_set', {
        action: key,
        accelerator: config[key].accelerator,
        hold
      });
      error = '';
    } catch (err) {
      error = String(err);
    }
  }

  async function reset() {
    stopRecording();
    try {
      config = await invoke<ShortcutsConfig>('shortcuts_reset');
      error = '';
    } catch (err) {
      error = String(err);
    }
  }

  onMount(() => {
    load();
    return stopRecording;
  });
</script>

<section class="shortcuts" aria-label="Shortcuts">
  <h2 class="section-title">Shortcuts</h2>
  <p class="section-hint">
    Choose the keys that start dictation and voice commands — they work in any app, even when
    Consistent CEO is hidden.
  </p>

  {#if error}
    <p class="shortcut-error" role="alert">{error}</p>
  {/if}

  {#if config}
    <div class="shortcut-list">
      {#each rows as row (row.key)}
        <div class="shortcut-row">
          <div class="shortcut-text">
            <span class="shortcut-name">{row.name}</span>
            <span class="shortcut-hint">{row.hint}</span>
            {#if row.holdable}
              <div class="mode-toggle" role="radiogroup" aria-label="{row.name} activation">
                <button
                  type="button"
                  class="mode-btn"
                  class:mode-active={!config[row.key].hold}
                  role="radio"
                  aria-checked={!config[row.key].hold}
                  onclick={() => setHold(row.key, false)}
                >
                  Hands-free
                </button>
                <button
                  type="button"
                  class="mode-btn"
                  class:mode-active={config[row.key].hold}
                  role="radio"
                  aria-checked={config[row.key].hold}
                  onclick={() => setHold(row.key, true)}
                >
                  Push to talk
                </button>
              </div>
              <span class="mode-hint">
                {config[row.key].hold
                  ? 'Hold the shortcut while you speak; release to finish.'
                  : 'Press once to start, press again to stop.'}
              </span>
            {/if}
          </div>

          <button
            type="button"
            class="keys-btn"
            class:keys-recording={recording === row.key}
            onclick={() => (recording === row.key ? stopRecording() : startRecording(row.key))}
            aria-label="Change {row.name} shortcut"
          >
            {#if recording === row.key}
              {#if liveMods.length > 0}
                {#each chips(liveMods.join('+')) as chip}
                  <kbd class="key-chip">{chip}</kbd>
                {/each}
                <span class="recording-hint">…</span>
              {:else}
                <span class="recording-hint">Press shortcut — Esc cancels</span>
              {/if}
            {:else}
              {#each chips(config[row.key].accelerator) as chip}
                <kbd class="key-chip">{chip}</kbd>
              {/each}
              <svg class="pencil" viewBox="0 0 16 16" width="12" height="12" aria-hidden="true">
                <path
                  d="M11.5 1.5l3 3L5 14H2v-3L11.5 1.5z"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="1.4"
                  stroke-linejoin="round"
                />
              </svg>
            {/if}
            {#if saved === row.key}
              <span class="saved-tick" aria-hidden="true">✓</span>
            {/if}
          </button>
        </div>
      {/each}
    </div>

    <div class="shortcut-actions">
      <button type="button" class="reset-btn" onclick={reset}>Reset to default</button>
    </div>
  {:else if !error}
    <p class="section-hint">Loading shortcuts…</p>
  {/if}
</section>

<style>
  .shortcuts {
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 16px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .section-title {
    margin: 0;
    font-size: 1rem;
    font-weight: 700;
    color: var(--text-primary);
  }
  .section-hint {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--text-muted);
    line-height: 1.4;
  }
  .shortcut-error {
    margin: 0;
    font-size: 0.8125rem;
    color: #c96a4b;
  }
  .shortcut-list {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .shortcut-row {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
    padding: 12px;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 10px;
  }
  .shortcut-text {
    display: flex;
    flex-direction: column;
    gap: 4px;
    min-width: 0;
  }
  .shortcut-name {
    font-size: 0.875rem;
    font-weight: 600;
    color: var(--text-primary);
  }
  .shortcut-hint {
    font-size: 0.8125rem;
    color: var(--text-secondary);
    line-height: 1.35;
  }
  .mode-toggle {
    display: inline-flex;
    margin-top: 6px;
    border: 1px solid var(--border);
    border-radius: 8px;
    overflow: hidden;
    width: fit-content;
  }
  .mode-btn {
    min-height: 30px;
    padding: 0 10px;
    font-size: 0.75rem;
    font-weight: 600;
    border: none;
    background: transparent;
    color: var(--text-secondary);
    cursor: pointer;
  }
  .mode-btn + .mode-btn {
    border-left: 1px solid var(--border);
  }
  .mode-active {
    background: rgba(201, 138, 75, 0.16);
    color: var(--text-primary);
  }
  .mode-hint {
    font-size: 0.75rem;
    color: var(--text-muted);
  }
  .keys-btn {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    flex-shrink: 0;
    min-height: 40px;
    padding: 0 12px;
    border-radius: 8px;
    border: 1px solid var(--border);
    background: var(--bg-secondary);
    color: var(--text-secondary);
    cursor: pointer;
  }
  .keys-recording {
    border-color: rgba(201, 138, 75, 0.6);
    box-shadow: 0 0 0 2px rgba(201, 138, 75, 0.18);
  }
  .key-chip {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    min-width: 24px;
    height: 24px;
    padding: 0 6px;
    border: 1px solid var(--border);
    border-bottom-width: 2px;
    border-radius: 6px;
    background: var(--bg-secondary);
    color: var(--text-primary);
    font-size: 0.75rem;
    font-weight: 700;
    font-family: inherit;
  }
  .recording-hint {
    font-size: 0.75rem;
    color: var(--text-muted);
  }
  .saved-tick {
    color: #7a8163;
    font-weight: 700;
    font-size: 0.8125rem;
  }
  .pencil {
    color: var(--text-muted);
  }
  .shortcut-actions {
    display: flex;
    justify-content: center;
  }
  .reset-btn {
    min-height: 38px;
    padding: 0 14px;
    border-radius: 8px;
    font-size: 0.8125rem;
    font-weight: 600;
    border: 1px solid var(--border);
    background: var(--bg-card);
    color: var(--text-secondary);
    cursor: pointer;
  }
</style>
