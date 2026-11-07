<script lang="ts">
  // Open reminders on the home view: what is due, what is next, and one-tap
  // Done / Snooze / Tomorrow / Drop. Everything here is `/api/reminders`
  // (crates/gateway/src/control_api/reminders_api.rs); the gateway keeps
  // nudging anything left open, so this list is the way to make it stop.
  import { onMount, tick } from 'svelte';
  import {
    createReminder,
    listReminders,
    reminderAction,
    type ReminderActionInput,
    type ReminderView
  } from '$lib/api';
  import {
    DEFAULT_WHEN,
    OPEN_REMINDERS_EVENT,
    REMINDERS_CHANGED_EVENT,
    REMINDERS_HASH,
    friendlyTime,
    splitReminderText
  } from '$lib/reminders';

  let {
    entityId = null,
    /** Changes whenever a new `scheduled_task_fired` event is seen. */
    signal = null
  }: { entityId?: string | null; signal?: string | null } = $props();

  const REFRESH_MS = 30_000;
  const COLLAPSED_KEY = 'consistent-world-reminders-collapsed';

  let items = $state<ReminderView[]>([]);
  let loaded = $state(false);
  let collapsed = $state(false);
  let busyId = $state<string | null>(null);
  let error = $state<string | null>(null);
  let note = $state<string | null>(null);
  let newWhat = $state('');
  let newWhen = $state('');
  let adding = $state(false);
  let now = $state(new Date());
  let highlight = $state(false);
  let root = $state<HTMLElement | null>(null);

  const sorted = $derived(
    [...items].sort((a, b) => {
      // Due first (it is already nagging), then by next nudge, parked last.
      const rank = (r: ReminderView) => (r.state === 'due' ? 0 : r.state === 'parked' ? 2 : 1);
      const byRank = rank(a) - rank(b);
      if (byRank !== 0) return byRank;
      if (a.fireAt && b.fireAt) return a.fireAt.localeCompare(b.fireAt);
      if (a.fireAt) return -1;
      if (b.fireAt) return 1;
      return b.createdAt.localeCompare(a.createdAt);
    })
  );
  const dueCount = $derived(items.filter((r) => r.state === 'due').length);

  async function refresh() {
    try {
      const res = await listReminders('open');
      items = (res.items ?? []).filter((r) => r.state !== 'done' && r.state !== 'cancelled');
      error = null;
    } catch {
      if (!loaded) error = 'Could not load reminders.';
    } finally {
      loaded = true;
      now = new Date();
    }
  }

  function errText(err: unknown, fallback: string): string {
    return err instanceof Error && err.message.trim() ? err.message : fallback;
  }

  async function act(r: ReminderView, input: ReminderActionInput, done: string) {
    busyId = r.id;
    error = null;
    try {
      const updated = await reminderAction(r.id, input);
      if (updated.state === 'done' || updated.state === 'cancelled') {
        items = items.filter((x) => x.id !== r.id);
      } else {
        items = items.map((x) => (x.id === r.id ? updated : x));
      }
      note = done;
    } catch (err) {
      error = errText(err, 'That did not work. Try again.');
    } finally {
      busyId = null;
    }
  }

  async function add(event: SubmitEvent) {
    event.preventDefault();
    const typed = newWhat.trim();
    if (!typed || adding) return;
    adding = true;
    error = null;
    note = null;
    try {
      let what = typed;
      let when = newWhen.trim();
      if (!when) {
        const split = splitReminderText(typed);
        what = split.what;
        when = split.when ?? '';
      }
      const created = await createReminder({
        what,
        when: when || DEFAULT_WHEN,
        entityId: entityId ?? undefined
      });
      if (!created.fireAt) {
        // The gateway didn't understand the time; never leave it unscheduled.
        const fixed = await reminderAction(created.id, { action: 'reschedule', when: DEFAULT_WHEN });
        items = [...items.filter((x) => x.id !== fixed.id), fixed];
        note = `Couldn't read "${when}", so I'll remind you ${DEFAULT_WHEN}.`;
      } else {
        items = [...items.filter((x) => x.id !== created.id), created];
        note = when
          ? `I'll remind you ${friendlyTime(created.fireAt)}.`
          : `No time given, so I'll remind you ${friendlyTime(created.fireAt)}.`;
      }
      newWhat = '';
      newWhen = '';
    } catch (err) {
      error = errText(err, 'Could not add that reminder.');
    } finally {
      adding = false;
    }
  }

  async function reveal() {
    collapsed = false;
    await tick();
    root?.scrollIntoView({ behavior: 'smooth', block: 'start' });
    highlight = true;
    setTimeout(() => (highlight = false), 1600);
    void refresh();
  }

  function toggle() {
    collapsed = !collapsed;
    try { localStorage.setItem(COLLAPSED_KEY, collapsed ? '1' : '0'); } catch {}
  }

  $effect(() => {
    if (signal) void refresh();
  });

  onMount(() => {
    try { collapsed = localStorage.getItem(COLLAPSED_KEY) === '1'; } catch {}
    void refresh().then(() => {
      if (window.location.hash === REMINDERS_HASH) void reveal();
    });
    const onChanged = () => void refresh();
    const onOpen = () => void reveal();
    const onHash = () => {
      if (window.location.hash === REMINDERS_HASH) void reveal();
    };
    window.addEventListener(REMINDERS_CHANGED_EVENT, onChanged);
    window.addEventListener(OPEN_REMINDERS_EVENT, onOpen);
    window.addEventListener('hashchange', onHash);
    const timer = setInterval(refresh, REFRESH_MS);
    const clock = setInterval(() => (now = new Date()), 60_000);
    return () => {
      window.removeEventListener(REMINDERS_CHANGED_EVENT, onChanged);
      window.removeEventListener(OPEN_REMINDERS_EVENT, onOpen);
      window.removeEventListener('hashchange', onHash);
      clearInterval(timer);
      clearInterval(clock);
    };
  });
</script>

<section id="reminders" class="reminders" class:highlight bind:this={root} aria-label="Reminders">
  <button type="button" class="head" onclick={toggle} aria-expanded={!collapsed}>
    <span class="label">Reminders</span>
    {#if items.length > 0}<span class="count">{items.length}</span>{/if}
    {#if dueCount > 0}<span class="badge due">{dueCount} due</span>{/if}
    <span class="chev" class:open={!collapsed} aria-hidden="true">›</span>
  </button>

  {#if !collapsed}
    {#if sorted.length > 0}
      <ul class="list">
        {#each sorted as r (r.id)}
          <li class="row" class:is-due={r.state === 'due'}>
            <div class="main">
              <span class="what">{r.what || r.title}</span>
              <span class="meta">
                {#if r.state === 'due'}<span class="badge due">Due</span>{/if}
                {#if r.state === 'parked'}<span class="badge parked">Parked</span>{/if}
                {#if r.state === 'delegated'}<span class="badge parked">On your computer</span>{/if}
                <span title={r.fireAt ? new Date(r.fireAt).toLocaleString() : ''}>
                  {r.state === 'delegated'
                    ? 'Waiting for your approval, then it runs on its own'
                    : r.state === 'parked' && !r.fireAt
                    ? 'Waiting for you'
                    : r.state === 'due'
                      ? r.fireAt ? `next nudge ${friendlyTime(r.fireAt, now)}` : 'waiting for you'
                      : friendlyTime(r.fireAt, now)}
                </span>
                {#if r.entityName}<span>· {r.entityName}</span>{/if}
              </span>
            </div>
            <div class="actions">
              <button type="button" class="btn primary" disabled={busyId === r.id}
                onclick={() => act(r, { action: 'done' }, 'Done. No more nudges for that one.')}>Done</button>
              <button type="button" class="btn" disabled={busyId === r.id}
                onclick={() => act(r, { action: 'snooze', minutes: 60 }, 'Snoozed for an hour.')}>Snooze 1h</button>
              <button type="button" class="btn" disabled={busyId === r.id}
                onclick={() => act(r, { action: 'reschedule', when: 'tomorrow 9am' }, 'Moved to tomorrow 9am.')}>Tomorrow 9am</button>
              {#if r.state !== 'delegated'}
                <button type="button" class="btn" disabled={busyId === r.id}
                  title="Hand this to your computer. It waits for your approval, then runs on its own."
                  onclick={() => act(r, { action: 'delegate' }, 'Handed to your computer. Approve it in Approvals and it runs on its own.')}>Do it for me</button>
              {/if}
              <button type="button" class="btn quiet" disabled={busyId === r.id}
                onclick={() => act(r, { action: 'cancel' }, 'Dropped.')}>Drop</button>
            </div>
          </li>
        {/each}
      </ul>
    {:else if loaded && !error}
      <p class="empty">Nothing open. Anything you ask me to remember shows up here until it's done.</p>
    {/if}

    <form class="add" onsubmit={add}>
      <input class="add-what" placeholder="Remind me to…" bind:value={newWhat} aria-label="What to be reminded about" />
      <input class="add-when" placeholder="When? e.g. tomorrow 9am" bind:value={newWhen} aria-label="When" />
      <button type="submit" class="btn primary" disabled={!newWhat.trim() || adding}>{adding ? 'Adding…' : 'Add'}</button>
    </form>

    {#if error}
      <p class="msg bad" role="status">{error}</p>
    {:else if note}
      <p class="msg" role="status">{note}</p>
    {/if}
  {/if}
</section>

<style>
  .reminders {
    margin: 8px 16px 0;
    padding: 10px 14px;
    border-radius: 12px;
    border: 1px solid color-mix(in srgb, var(--entity-tint, #c98a4b) 30%, var(--border));
    background: color-mix(in srgb, var(--entity-tint, #c98a4b) 6%, var(--bg-card));
    display: flex;
    flex-direction: column;
    gap: 8px;
    flex-shrink: 0;
    max-height: 42vh;
    overflow-y: auto;
    scroll-margin-top: 8px;
    transition: box-shadow 0.3s ease;
  }
  .reminders.highlight {
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--entity-tint, #c98a4b) 45%, transparent);
  }
  .head {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 0;
    border: 0;
    background: none;
    color: inherit;
    cursor: pointer;
    text-align: left;
  }
  .label {
    font-weight: 800;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    font-size: 10px;
    color: var(--text-secondary);
  }
  .count {
    min-width: 20px;
    height: 20px;
    border-radius: 999px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    font-size: 11px;
    font-weight: 800;
    color: var(--text-primary);
    background: color-mix(in srgb, var(--entity-tint, #c98a4b) 20%, transparent);
    border: 1px solid color-mix(in srgb, var(--entity-tint, #c98a4b) 40%, transparent);
  }
  .chev {
    margin-left: auto;
    color: var(--text-muted);
    transition: transform 0.15s ease;
  }
  .chev.open { transform: rotate(90deg); }
  .badge {
    padding: 0 7px;
    border-radius: 999px;
    font-size: 10px;
    font-weight: 800;
    line-height: 18px;
    text-transform: uppercase;
    letter-spacing: 0.03em;
  }
  .badge.due {
    color: #c96a4b;
    background: rgba(201, 106, 75, 0.14);
    border: 1px solid rgba(201, 106, 75, 0.45);
  }
  .badge.parked {
    color: var(--text-muted);
    border: 1px solid var(--border);
  }
  .list {
    margin: 0;
    padding: 0;
    list-style: none;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .row {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    align-items: center;
    gap: 9px;
    padding: 7px 8px;
    border: 1px solid var(--border-subtle);
    border-radius: 10px;
    background: color-mix(in srgb, var(--bg-card) 84%, transparent);
  }
  .row.is-due { border-color: rgba(201, 106, 75, 0.4); }
  .main {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .what {
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
    overflow-wrap: anywhere;
  }
  .meta {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px;
    font-size: 11px;
    color: var(--text-muted);
  }
  .actions {
    display: inline-flex;
    flex-wrap: wrap;
    justify-content: flex-end;
    gap: 6px;
  }
  .btn {
    min-height: 28px;
    padding: 0 10px;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
    font-size: 11px;
    font-weight: 800;
    cursor: pointer;
    white-space: nowrap;
  }
  .btn.primary {
    color: var(--accent-cyan);
    border-color: color-mix(in srgb, var(--accent-cyan) 45%, var(--border));
  }
  .btn.quiet:hover:not(:disabled) { color: var(--text-primary); }
  .btn:disabled { opacity: 0.5; cursor: default; }
  .add {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  .add input {
    min-height: 30px;
    padding: 0 10px;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: var(--surface-overlay, transparent);
    color: var(--text-primary);
    font-size: 12px;
  }
  .add-what { flex: 2 1 200px; }
  .add-when { flex: 1 1 140px; }
  .empty,
  .msg {
    margin: 0;
    font-size: 12px;
    color: var(--text-muted);
  }
  .msg.bad { color: #c96a4b; }
  @media (max-width: 640px) {
    .row { grid-template-columns: 1fr; }
    .actions { justify-content: flex-start; }
  }
</style>
