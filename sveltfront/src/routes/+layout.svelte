<script lang="ts">
  import '../app.css';
  import { onMount } from 'svelte';
  import ErrorBanner from '$lib/ErrorBanner.svelte';
  import Sidebar from '$lib/components/Sidebar.svelte';
  import TenantSwitcher from '$lib/components/TenantSwitcher.svelte';
  import SunIcon from '$lib/components/icons/SunIcon.svelte';
  import MoonIcon from '$lib/components/icons/MoonIcon.svelte';
  import BellIcon from '$lib/components/icons/BellIcon.svelte';
  import { goto } from '$app/navigation';
  import { page } from '$app/stores';
  import { API_BASE, api, getAuthToken, getTenantId, listTenants, setTenantId, type TenantSummary, type PaginatedResponse } from '$lib/api';
  import { feedKindLabel, feedKindNeedsHuman, feedKindTone } from '$lib/feedKinds';

  let { children } = $props();

  /** The sign-in page renders bare: no sidebar, no workspace switcher. */
  const isLoginRoute = $derived($page.url.pathname.startsWith('/login'));

  const THEME_KEY = 'consistent-world-theme';
  const NOTIFY_BROWSER_KEY = 'consistent-world-notify-browser';
  const NOTIFY_SOUND_KEY = 'consistent-world-notify-sound';
  type Theme = 'dark' | 'light';
  let theme = $state<Theme>('dark');
  let mounted = $state(false);
  let tenants = $state<TenantSummary[]>([]);
  let activeTenantId = $state('default');
  /** The claimed account's username, shown on the tenant pill. `null` while
   *  unknown, and for a machine-owner account that has never been given
   *  credentials — its username is a principal id, not a person. */
  let accountUsername = $state<string | null>(null);
  let tenantLoading = $state(false);
  let tenantError = $state<string | null>(null);
  let railOpen = $state(false);
  /** The running gateway's version, from `/health`. Read rather than written
   *  into the page, so the bar names what is installed after an update. */
  let gatewayVersion = $state<string | null>(null);

  async function loadGatewayVersion() {
    try {
      const res = await fetch(`${API_BASE}/health`);
      const body = (await res.json()) as { version?: unknown };
      if (typeof body.version === 'string') gatewayVersion = body.version;
    } catch (_) {
      // Nothing to show is better than a wrong number.
    }
  }

  function setTheme(next: Theme) {
    theme = next;
    if (typeof document !== 'undefined') {
      document.documentElement.setAttribute('data-theme', next);
      try { localStorage.setItem(THEME_KEY, next); } catch (_) {}
    }
  }

  function toggleTheme() {
    setTheme(theme === 'dark' ? 'light' : 'dark');
  }

  async function loadTenants() {
    tenantLoading = true;
    tenantError = null;
    try {
      const rows = await listTenants();
      tenants = rows;
      if (!rows.some((t) => t.id === activeTenantId)) {
        activeTenantId = rows[0]?.id ?? activeTenantId;
      }
    } catch (error) {
      tenantError = error instanceof Error ? error.message : 'Failed to load tenants';
    } finally {
      tenantLoading = false;
    }
  }

  async function changeTenant(event: Event) {
    const target = event.currentTarget;
    if (!(target instanceof HTMLSelectElement)) return;
    const nextId = target.value || 'default';
    activeTenantId = nextId;
    setTenantId(nextId);
    notifications = [];
    unreadCount = 0;
    lastSeenNotificationId = null;
    await Promise.all([loadNotifications(), loadTenants()]);
  }

  function handleWorkspacesChanged(event: Event) {
    const detail = event instanceof CustomEvent ? event.detail : null;
    const workspaceId = typeof detail?.workspaceId === 'string' ? detail.workspaceId.trim() : '';
    if (workspaceId) {
      activeTenantId = workspaceId;
      setTenantId(workspaceId);
      notifications = [];
      unreadCount = 0;
      lastSeenNotificationId = null;
    }
    void loadTenants();
  }

  type FeedEvent = {
    id: string;
    timestamp: string;
    kind: string;
    title: string;
    detail?: string;
    taskId?: string;
    missionId?: string;
    entityId?: string;
    connectionId?: string;
    approvalId?: string;
  };
  let notificationsOpen = $state(false);
  let notifications = $state<FeedEvent[]>([]);
  let unreadCount = $state(0);
  let lastSeenNotificationId = $state<string | null>(null);
  let browserNotificationsEnabled = $state(false);
  let soundNotificationsEnabled = $state(false);
  let notificationPermission = $state<'default' | 'denied' | 'granted'>('default');
  let feedTimer: ReturnType<typeof setInterval> | undefined;

  // Labels and tones for every `OperatorFeedKind` live in `$lib/feedKinds`,
  // shared with the task board and activity pages.

  /** Short "3m ago" stamps; the absolute time stays on the tooltip. */
  function relativeTime(iso: string): string {
    const then = new Date(iso).getTime();
    if (Number.isNaN(then)) return '';
    const diff = Date.now() - then;
    if (diff < 45_000) return 'just now';
    const mins = Math.round(diff / 60_000);
    if (mins < 60) return `${mins}m ago`;
    const hours = Math.round(mins / 60);
    if (hours < 24) return `${hours}h ago`;
    const days = Math.round(hours / 24);
    if (days < 7) return `${days}d ago`;
    return new Date(then).toLocaleDateString();
  }

  function playNotificationChime() {
    if (typeof window === 'undefined' || !soundNotificationsEnabled) return;
    const AudioCtx = window.AudioContext || (window as typeof window & { webkitAudioContext?: typeof AudioContext }).webkitAudioContext;
    if (!AudioCtx) return;
    const ctx = new AudioCtx();
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    osc.type = 'sine';
    osc.frequency.value = 880;
    gain.gain.value = 0.0001;
    osc.connect(gain);
    gain.connect(ctx.destination);
    const now = ctx.currentTime;
    gain.gain.exponentialRampToValueAtTime(0.04, now + 0.01);
    gain.gain.exponentialRampToValueAtTime(0.0001, now + 0.18);
    osc.start(now);
    osc.stop(now + 0.2);
    osc.onended = () => void ctx.close();
  }

  /**
   * True when this page is running inside the Tauri desktop shell rather than
   * a plain browser. The shell polls the same feed natively (see
   * `desktop/src-tauri/src/companion.rs`) and raises OS notifications that
   * survive the window being closed — so emitting a web Notification here too
   * would show the operator every alert twice.
   */
  function inDesktopShell(): boolean {
    return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
  }

  function sendBrowserNotification(item: FeedEvent) {
    if (typeof window === 'undefined' || !browserNotificationsEnabled || notificationPermission !== 'granted') return;
    if (inDesktopShell()) return;
    if (document.visibilityState === 'visible' && notificationsOpen) return;
    new Notification(feedKindLabel(item.kind), {
      body: item.title,
      tag: `feed-${item.id}`,
      silent: true
    });
  }

  async function requestBrowserNotifications(forcePrompt = false) {
    if (typeof window === 'undefined' || !('Notification' in window)) return false;
    notificationPermission = Notification.permission;
    if (notificationPermission === 'granted') return true;
    if (notificationPermission === 'denied' && !forcePrompt) return false;
    const permission = await Notification.requestPermission();
    notificationPermission = permission;
    browserNotificationsEnabled = permission === 'granted';
    try { localStorage.setItem(NOTIFY_BROWSER_KEY, browserNotificationsEnabled ? '1' : '0'); } catch (_) {}
    return permission === 'granted';
  }

  async function loadNotifications() {
    try {
      const res = await api<PaginatedResponse<FeedEvent>>('/api/operator-feed?limit=25');
      const nextItems = res.items ?? [];
      const previousTopId = notifications[0]?.id ?? lastSeenNotificationId;
      notifications = nextItems;
      if (notifications.length === 0) {
        unreadCount = 0;
        return;
      }
      if (!lastSeenNotificationId) {
        lastSeenNotificationId = notifications[0].id;
        unreadCount = 0;
        return;
      }
      const index = notifications.findIndex((n) => n.id === lastSeenNotificationId);
      const newItems = index === -1 ? notifications : notifications.slice(0, index);
      unreadCount = newItems.length;
      // Every event still lands in the bell list, but only the ones a person
      // has to look at or act on (an approval, a failure, an overdue task, the
      // morning brief…) earn an interruption. A chime per "task created" would
      // teach the operator to ignore the chime.
      // Only rows that arrived since the *previous poll* can interrupt; the
      // unread set above persists until the bell is opened, and re-chiming it
      // every 5s would be noise.
      const sincePreviousPoll = (() => {
        if (!previousTopId) return [];
        const at = notifications.findIndex((n) => n.id === previousTopId);
        return at === -1 ? notifications : notifications.slice(0, at);
      })();
      const urgent = sincePreviousPoll.find((n) => feedKindNeedsHuman(n.kind));
      if (urgent) {
        sendBrowserNotification(urgent);
        playNotificationChime();
      }
    } catch (_) {
      // keep quiet if feed fails
    }
  }

  async function toggleBrowserNotifications() {
    if (browserNotificationsEnabled) {
      browserNotificationsEnabled = false;
      try { localStorage.setItem(NOTIFY_BROWSER_KEY, '0'); } catch (_) {}
      return;
    }
    await requestBrowserNotifications(true);
  }

  function toggleSoundNotifications() {
    soundNotificationsEnabled = !soundNotificationsEnabled;
    try { localStorage.setItem(NOTIFY_SOUND_KEY, soundNotificationsEnabled ? '1' : '0'); } catch (_) {}
    if (soundNotificationsEnabled) playNotificationChime();
  }

  function handleWindowKeydown(event: KeyboardEvent) {
    if (event.key === 'Escape' && notificationsOpen) notificationsOpen = false;
  }

  function toggleNotifications() {
    notificationsOpen = !notificationsOpen;
    if (notificationsOpen && notifications.length > 0) {
      lastSeenNotificationId = notifications[0].id;
      unreadCount = 0;
    }
  }

  /** Best-effort: a failure here must never raise the global banner or change
   *  what the pill showed before. */
  async function loadAccountName() {
    try {
      const status = await api<{ hasPassword: boolean; username: string }>('/api/auth/account', {
        silent: true
      });
      accountUsername = status.hasPassword && status.username.trim() ? status.username : null;
    } catch (_) {
      accountUsername = null;
    }
  }

  onMount(() => {
    // The app needs a session before any of its data loads. `/login` is the one
    // page that renders without one — it is where you go to get one, and it
    // starts none of the polling below (which would only 401 in a loop).
    if (isLoginRoute) return;
    if (!getAuthToken()) {
      goto('/login');
      return;
    }
    activeTenantId = getTenantId();
    void loadAccountName();
    void loadGatewayVersion();
    let stored: Theme | null = null;
    try { stored = localStorage.getItem(THEME_KEY) as Theme | null; } catch (_) {}
    const initial: Theme = stored === 'light' || stored === 'dark' ? stored : 'dark';
    setTheme(initial);
    try {
      browserNotificationsEnabled = localStorage.getItem(NOTIFY_BROWSER_KEY) === '1';
      soundNotificationsEnabled = localStorage.getItem(NOTIFY_SOUND_KEY) === '1';
    } catch (_) {}
    if (typeof window !== 'undefined' && 'Notification' in window) {
      notificationPermission = Notification.permission;
      if (browserNotificationsEnabled && notificationPermission !== 'granted') {
        browserNotificationsEnabled = false;
      }
    }
    mounted = true;
    loadTenants();
    window.addEventListener('consistent-world:workspaces-changed', handleWorkspacesChanged);
    loadNotifications();
    feedTimer = setInterval(loadNotifications, 5000);
    return () => {
      window.removeEventListener('consistent-world:workspaces-changed', handleWorkspacesChanged);
      if (feedTimer) clearInterval(feedTimer);
    };
  });
</script>

<svelte:window onkeydown={handleWindowKeydown} />

{#if isLoginRoute}
  {@render children()}
{:else}
<div class="shell">
  <button class="rail-toggle" onclick={() => (railOpen = !railOpen)} aria-label="Toggle navigation">
    <span></span><span></span><span></span>
  </button>

  <div class="rail-wrap" class:open={railOpen}>
    <Sidebar onNavigate={() => (railOpen = false)} />
  </div>

  {#if railOpen}
    <button class="rail-backdrop" aria-label="Close navigation" onclick={() => (railOpen = false)}></button>
  {/if}

  <div class="main">
    <header class="topbar" class:menu-open={notificationsOpen}>
      <div class="topbar-left">
        <!-- Only a gateway several people share has workspaces to switch
             between; on a personal install the pill would only say "default". -->
        {#if tenants.length > 1}
          <TenantSwitcher
            workspaces={tenants}
            activeId={activeTenantId}
            username={accountUsername}
            loading={tenantLoading}
            error={tenantError}
            onChange={changeTenant}
          />
        {/if}
      </div>
      <div class="topbar-center">
        <span class="brand">Consistent CEO</span>
        {#if gatewayVersion}
          <span class="version">v{gatewayVersion}</span>
        {/if}
      </div>
      <div class="topbar-right">
        <div class="theme-switch" class:is-dark={theme === 'dark'} class:pending={!mounted} role="group" aria-label="Colour theme">
          <span class="theme-thumb" aria-hidden="true"></span>
          <button
            class="theme-opt"
            class:active={mounted && theme === 'light'}
            type="button"
            aria-pressed={mounted && theme === 'light'}
            onclick={() => setTheme('light')}
            title="Light mode"
          >
            <SunIcon size={15} />
            <span class="visually-hidden">Light mode</span>
          </button>
          <button
            class="theme-opt"
            class:active={mounted && theme === 'dark'}
            type="button"
            aria-pressed={mounted && theme === 'dark'}
            onclick={() => setTheme('dark')}
            title="Dark mode"
          >
            <MoonIcon size={15} />
            <span class="visually-hidden">Dark mode</span>
          </button>
        </div>
        <div class="notification-wrap">
          <button
            class="icon-btn"
            class:open={notificationsOpen}
            onclick={toggleNotifications}
            aria-label="Notifications"
            aria-expanded={notificationsOpen}
            title="Notifications"
          >
            <BellIcon size={18} />
            {#if unreadCount > 0}
              <span class="notif-count">{unreadCount > 99 ? '99+' : unreadCount}</span>
            {/if}
          </button>
          {#if notificationsOpen}
            <button class="notification-backdrop" aria-label="Close notifications" onclick={() => (notificationsOpen = false)}></button>
            <div class="notification-dropdown" role="dialog" aria-label="Recent activity">
              <div class="notification-head">
                <span class="notification-heading">Recent activity</span>
                {#if notifications.length > 0}
                  <span class="notification-total">{notifications.length}</span>
                {/if}
              </div>
              <div class="notification-list">
                {#if notifications.length === 0}
                  <div class="notification-empty">
                    <BellIcon size={20} />
                    <span class="notification-empty-title">No events yet</span>
                    <span class="notification-empty-hint">Task, mission and connection activity lands here.</span>
                  </div>
                {:else}
                  {#each notifications as n}
                    <div class="notification-item">
                      <span class="notification-dot" data-tone={feedKindTone(n.kind)} aria-hidden="true"></span>
                      <div class="notification-body">
                        <div class="notification-title">{n.title}</div>
                        <div class="notification-meta">
                          <span class="notification-kind">{feedKindLabel(n.kind)}</span>
                          <span class="notification-sep" aria-hidden="true">·</span>
                          <time datetime={n.timestamp} title={new Date(n.timestamp).toLocaleString()}>{relativeTime(n.timestamp)}</time>
                        </div>
                      </div>
                    </div>
                  {/each}
                {/if}
              </div>
              <div class="notification-foot">
                <button
                  class="notification-toggle"
                  class:on={browserNotificationsEnabled}
                  type="button"
                  aria-pressed={browserNotificationsEnabled}
                  onclick={toggleBrowserNotifications}
                >
                  <span class="notification-toggle-dot" aria-hidden="true"></span>
                  Browser
                </button>
                <button
                  class="notification-toggle"
                  class:on={soundNotificationsEnabled}
                  type="button"
                  aria-pressed={soundNotificationsEnabled}
                  onclick={toggleSoundNotifications}
                >
                  <span class="notification-toggle-dot" aria-hidden="true"></span>
                  Sound
                </button>
              </div>
            </div>
          {/if}
        </div>
      </div>
    </header>

    <ErrorBanner />

    <main class="content">
      {@render children()}
    </main>
  </div>
</div>
{/if}

<style>
  .shell {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr);
    /* Pin the single row to the viewport. Left implicit, the row is auto-sized,
       so a rail taller than the window grows the row past 100vh — and because
       the shell clips, the bottom of `.content` ends up below the screen with
       its scrollbar out of reach. `minmax(0, 1fr)` keeps both columns exactly
       viewport-height, so `.content` is the only thing that scrolls. */
    grid-template-rows: minmax(0, 1fr);
    height: 100vh;
    width: 100vw;
    overflow: hidden;
    background:
      radial-gradient(860px 360px at 50% -18%, rgba(201, 138, 75, 0.12), transparent 70%),
      var(--bg-primary);
  }

  .rail-wrap {
    height: 100%;
    display: block;
  }

  .main {
    display: flex;
    flex-direction: column;
    min-width: 0;
    height: 100%;
  }

  .topbar {
    position: relative;
    flex-shrink: 0;
    display: grid;
    /* Equal side columns keep the title centred, including when the left one
       is empty (a personal install has no workspace switcher). */
    grid-template-columns: 1fr auto 1fr;
    align-items: center;
    gap: 12px;
    padding: 0 16px;
    height: var(--topbar-height);
    border-bottom: 1px solid var(--border);
    background: var(--panel-gradient);
    box-shadow: 0 1px 0 rgba(238, 217, 174, 0.04), 0 12px 28px rgba(10, 9, 8, 0.24);
    backdrop-filter: blur(16px);
  }
  /* Page-level overlays sit at z-index 100 and would otherwise paint over the
     notification panel. Lift the whole bar only while the panel is open, so
     modals still cover the bar the rest of the time. */
  .topbar.menu-open {
    z-index: var(--z-overlay);
  }
  .topbar-left {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .topbar-center {
    display: flex;
    align-items: baseline;
    justify-content: center;
    gap: 8px;
  }
  .brand {
    font-size: 15px;
    font-weight: 700;
    color: var(--text-primary);
    background: var(--gradient-text);
    -webkit-background-clip: text;
    background-clip: text;
    -webkit-text-fill-color: transparent;
    letter-spacing: 0;
  }
  .version {
    font-size: 10px;
    color: var(--text-muted);
  }
  .topbar-right {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 8px;
  }

  .icon-btn {
    position: relative;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    min-width: 36px;
    min-height: 36px;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: var(--surface-overlay);
    color: var(--text-secondary);
    cursor: pointer;
    transition: background var(--transition-base), border-color var(--transition-base), color var(--transition-base);
  }
  .icon-btn:hover {
    color: var(--text-primary);
    background: var(--surface-hover);
    border-color: var(--border-focus);
  }
  .icon-btn:focus-visible,
  .theme-opt:focus-visible,
  .notification-toggle:focus-visible {
    outline: 2px solid var(--border-focus);
    outline-offset: 2px;
  }
  .icon-btn.open {
    color: var(--text-primary);
    background: var(--surface-accent-soft);
    border-color: var(--border-focus);
  }

  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    margin: -1px;
    padding: 0;
    overflow: hidden;
    clip: rect(0 0 0 0);
    white-space: nowrap;
    border: 0;
  }

  /* Both destinations stay on screen with the active one lit, so the current
     theme is readable at a glance instead of inferred from a lone icon. */
  .theme-switch {
    position: relative;
    display: inline-flex;
    align-items: center;
    gap: 2px;
    padding: 3px;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: var(--surface-sunken);
    box-shadow: inset 0 1px 2px rgba(10, 9, 8, 0.22);
  }
  .theme-thumb {
    position: absolute;
    top: 3px;
    left: 3px;
    width: 32px;
    height: 28px;
    border-radius: 999px;
    background: var(--gradient-primary-button);
    box-shadow: 0 2px 8px rgba(201, 138, 75, 0.38);
    transition: transform var(--transition-base), opacity var(--transition-base);
    pointer-events: none;
  }
  .theme-switch.is-dark .theme-thumb {
    transform: translateX(34px);
  }
  /* Until the stored theme is read, nothing is highlighted rather than the
     wrong half. */
  .theme-switch.pending .theme-thumb {
    opacity: 0;
  }
  .theme-opt {
    position: relative;
    z-index: 1;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 32px;
    height: 28px;
    padding: 0;
    border: 0;
    border-radius: 999px;
    background: transparent;
    color: var(--text-muted);
    cursor: pointer;
    transition: color var(--transition-base);
  }
  .theme-opt:hover {
    color: var(--text-primary);
  }
  .theme-opt.active {
    color: #fff;
  }
  .notif-count {
    position: absolute;
    top: -4px;
    right: -4px;
    min-width: 16px;
    height: 16px;
    padding: 0 4px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: 999px;
    font-size: 10px;
    font-weight: 700;
    color: #fff;
    background: var(--gradient-primary-button);
    box-shadow: 0 0 0 2px rgba(10, 9, 8, 0.7), 0 0 14px rgba(201, 138, 75, 0.32);
  }

  .notification-wrap {
    position: relative;
  }
  /* Catches the click that dismisses the panel without stealing it from the
     bell itself. */
  .notification-backdrop {
    position: fixed;
    inset: 0;
    z-index: 1;
    border: 0;
    padding: 0;
    background: transparent;
    cursor: default;
  }
  .notification-dropdown {
    position: absolute;
    right: 0;
    top: calc(100% + 10px);
    z-index: 2;
    display: flex;
    flex-direction: column;
    width: min(calc(100vw - 24px), 344px);
    max-height: min(70vh, 520px);
    overflow: hidden;
    border: 1px solid var(--border);
    border-radius: var(--radius-xl);
    /* Opaque on purpose: a translucent panel let the page bleed through the
       rows and made the feed unreadable. */
    background: var(--bg-card);
    box-shadow: var(--shadow-lg), 0 0 0 1px rgba(201, 138, 75, 0.1);
  }
  .notification-head {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 11px 14px;
    border-bottom: 1px solid var(--border-subtle);
  }
  .notification-heading {
    font-size: var(--text-sm);
    font-weight: var(--font-semibold);
    letter-spacing: 0.02em;
    color: var(--text-primary);
  }
  .notification-total {
    margin-left: auto;
    min-width: 20px;
    padding: 1px 6px;
    border-radius: 999px;
    background: var(--surface-accent-soft);
    color: var(--accent-orange);
    font-size: var(--text-2xs);
    font-weight: var(--font-bold);
    text-align: center;
  }
  .notification-list {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 4px;
  }
  .notification-empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 4px;
    padding: 28px 20px 32px;
    text-align: center;
    color: var(--text-muted);
  }
  .notification-empty-title {
    font-size: var(--text-sm);
    font-weight: var(--font-semibold);
    color: var(--text-secondary);
  }
  .notification-empty-hint {
    font-size: var(--text-xs);
    line-height: var(--leading-normal);
    max-width: 24ch;
  }
  .notification-item {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    padding: 9px 10px;
    border-radius: var(--radius-lg);
    transition: background var(--transition-base);
  }
  .notification-item:hover {
    background: var(--surface-hover);
  }
  .notification-dot {
    flex-shrink: 0;
    width: 7px;
    height: 7px;
    margin-top: 5px;
    border-radius: 999px;
    background: var(--accent-cyan);
  }
  .notification-dot[data-tone='alert'] { background: var(--accent-red); }
  .notification-dot[data-tone='warn']  { background: var(--accent-orange); }
  .notification-dot[data-tone='good']  { background: var(--accent-green); }
  .notification-body {
    min-width: 0;
  }
  .notification-title {
    font-size: var(--text-sm);
    line-height: var(--leading-normal);
    color: var(--text-primary);
    overflow-wrap: anywhere;
  }
  .notification-meta {
    display: flex;
    align-items: center;
    gap: 5px;
    margin-top: 2px;
    font-size: var(--text-2xs);
    color: var(--text-muted);
  }
  .notification-kind {
    text-transform: uppercase;
    font-weight: var(--font-bold);
    letter-spacing: 0.05em;
    color: var(--text-secondary);
  }
  .notification-foot {
    display: flex;
    gap: 6px;
    padding: 8px 10px;
    border-top: 1px solid var(--border-subtle);
    background: var(--surface-sunken);
  }
  .notification-toggle {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    min-height: 28px;
    padding: 0 10px;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-muted);
    font-size: var(--text-xs);
    font-weight: var(--font-medium);
    cursor: pointer;
    transition: color var(--transition-base), border-color var(--transition-base), background var(--transition-base);
  }
  .notification-toggle:hover {
    color: var(--text-primary);
    border-color: var(--border-focus);
  }
  .notification-toggle-dot {
    width: 6px;
    height: 6px;
    border-radius: 999px;
    background: var(--text-muted);
    transition: background var(--transition-base), box-shadow var(--transition-base);
  }
  .notification-toggle.on {
    color: var(--text-primary);
    border-color: rgba(201, 138, 75, 0.5);
    background: var(--surface-accent-soft);
  }
  .notification-toggle.on .notification-toggle-dot {
    background: var(--accent-orange);
    box-shadow: 0 0 8px rgba(224, 165, 94, 0.7);
  }

  .content {
    flex: 1;
    min-height: 0;
    overflow: auto;
  }

  .rail-toggle {
    display: none;
    position: fixed;
    top: 10px;
    left: 10px;
    z-index: 61;
    width: 38px;
    height: 38px;
    border-radius: 10px;
    border: 1px solid var(--border);
    background: var(--bg-secondary);
    flex-direction: column;
    gap: 4px;
    align-items: center;
    justify-content: center;
    cursor: pointer;
  }
  .rail-toggle span {
    display: block;
    width: 18px;
    height: 2px;
    background: var(--text-secondary);
    border-radius: 2px;
  }
  .rail-backdrop {
    display: none;
    position: fixed;
    inset: 0;
    z-index: 55;
    background: rgba(10, 9, 8, 0.78);
    border: 0;
    cursor: pointer;
  }

  @media (max-width: 720px) {
    .shell {
      grid-template-columns: minmax(0, 1fr);
    }
    .rail-wrap {
      position: fixed;
      top: 0;
      left: 0;
      bottom: 0;
      z-index: 60;
      transform: translateX(-100%);
      transition: transform 0.2s ease;
    }
    .rail-wrap.open {
      transform: translateX(0);
    }
    .rail-toggle {
      display: flex;
    }
    .rail-backdrop {
      display: block;
    }
    .topbar {
      padding-left: 56px;
    }
    .topbar-center .brand {
      font-size: 14px;
    }
  }
</style>
