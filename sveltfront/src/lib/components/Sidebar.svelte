<script lang="ts">
  import { onMount } from 'svelte';
  import { page } from '$app/stores';
  import type { Component } from 'svelte';
  import HomeIcon from '$lib/components/icons/HomeIcon.svelte';
  import SunIcon from '$lib/components/icons/SunIcon.svelte';
  import TasksIcon from '$lib/components/icons/TasksIcon.svelte';
  import EntitiesIcon from '$lib/components/icons/EntitiesIcon.svelte';
  import MissionsIcon from '$lib/components/icons/MissionsIcon.svelte';
  import ActivityIcon from '$lib/components/icons/ActivityIcon.svelte';
  import AgentsIcon from '$lib/components/icons/AgentsIcon.svelte';
  import OperationsIcon from '$lib/components/icons/OperationsIcon.svelte';
  import BookIcon from '$lib/components/icons/BookIcon.svelte';
  import GearIcon from '$lib/components/icons/GearIcon.svelte';
  import VaultIcon from '$lib/components/icons/VaultIcon.svelte';
  import SparkleIcon from '$lib/components/icons/SparkleIcon.svelte';
  import DownloadIcon from '$lib/components/icons/DownloadIcon.svelte';
  import GithubIcon from '$lib/components/icons/GithubIcon.svelte';
  import {
    ensureProfile,
    filterNav,
    isFocusedEnvironment,
    saveProfile,
    userProfile
  } from '$lib/userProfile';

  type NavEntry = {
    label: string;
    href: string;
    icon: Component<{ size?: number; class?: string }>;
    external?: boolean;
    /**
     * Survives the focused (linear) environment. A single-track operator wants
     * to do the thing they came for, not tour the workspace — so only these
     * stay visible until they ask for everything.
     */
    focused?: boolean;
  };

  const topNav: NavEntry[] = [
    { label: 'Chat', href: '/', icon: HomeIcon, focused: true },
    { label: 'Today', href: '/today', icon: SunIcon, focused: true },
    { label: 'Tasks', href: '/tasks', icon: TasksIcon, focused: true },
    { label: 'Entities', href: '/entities', icon: EntitiesIcon },
    { label: 'Missions', href: '/missions', icon: MissionsIcon, focused: true },
    { label: 'Skills', href: '/skills', icon: SparkleIcon, focused: true },
    { label: 'Activity', href: '/activity', icon: ActivityIcon },
    { label: 'Agents', href: '/agents', icon: AgentsIcon },
    { label: 'Operations', href: '/dashboard', icon: OperationsIcon },
    { label: 'Vault', href: '/vault', icon: VaultIcon },
    { label: 'Knowledge hub', href: '/about', icon: BookIcon }
  ];

  // Until the profile loads, `$userProfile` is null and nothing is hidden — the
  // nav must never flicker items away a moment after paint.
  const focusedEnvironment = $derived(isFocusedEnvironment($userProfile));
  const visibleTopNav = $derived(
    filterNav(topNav, $userProfile?.workingStyle ?? 'unset', $userProfile?.showAllNav ?? false)
  );
  let revealBusy = $state(false);

  /** The escape hatch. Persisted server-side, so it holds on every device. */
  async function showEverything() {
    revealBusy = true;
    try {
      await saveProfile({ showAllNav: true });
    } finally {
      revealBusy = false;
    }
  }

  const bottomNav: NavEntry[] = [
    { label: 'Get the app', href: '/download', icon: DownloadIcon },
    { label: 'AI settings', href: '/settings', icon: GearIcon },
    // `consistent-world-agnosticAI` was a rename left behind: that repo does not
    // exist, so this link 404'd for everyone. The real one is private, so it
    // still only resolves for signed-in collaborators.
    { label: 'GitHub', href: 'https://github.com/Al-Baqshi/Consistent-world', icon: GithubIcon, external: true }
  ];

  let { onNavigate }: { onNavigate?: () => void } = $props();

  const STORAGE_KEY = 'consistent-world-sidebar-expanded';
  let expanded = $state(false);

  onMount(() => {
    void ensureProfile();
    if (typeof window === 'undefined') return;
    try {
      const raw = localStorage.getItem(STORAGE_KEY);
      if (raw === '1') expanded = true;
      else if (raw === '0') expanded = false;
      else expanded = window.innerWidth >= 1200;
    } catch (_) {
      expanded = false;
    }
  });

  function toggleExpanded() {
    expanded = !expanded;
    if (typeof window !== 'undefined') {
      try { localStorage.setItem(STORAGE_KEY, expanded ? '1' : '0'); } catch (_) {}
    }
  }

  function isActive(href: string, pathname: string): boolean {
    if (href === '/') return pathname === '/';
    return pathname === href || pathname.startsWith(href + '/');
  }
</script>

<aside class="rail" class:expanded aria-label="Primary navigation">
  <a class="brand" href="/" aria-label="Consistent CEO home" onclick={() => onNavigate?.()}>
    <img src="/logo-consistent-ceo.png" alt="" aria-hidden="true" />
    <span class="brand-label">Consistent CEO</span>
  </a>

  <button
    type="button"
    class="rail-toggle-btn"
    onclick={toggleExpanded}
    aria-label={expanded ? 'Collapse sidebar' : 'Expand sidebar'}
    aria-pressed={expanded}
    title={expanded ? 'Collapse sidebar' : 'Expand sidebar'}
  >
    <svg
      xmlns="http://www.w3.org/2000/svg"
      width="18"
      height="18"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="1.75"
      stroke-linecap="round"
      stroke-linejoin="round"
      aria-hidden="true"
    >
      {#if expanded}
        <path d="M15 6l-6 6 6 6" />
      {:else}
        <path d="M9 6l6 6-6 6" />
      {/if}
    </svg>
    <span class="rail-label">{expanded ? 'Collapse' : 'Expand'}</span>
  </button>

  <nav class="rail-group">
    {#each visibleTopNav as item (item.href)}
      <a
        class="rail-item"
        class:active={isActive(item.href, $page.url.pathname)}
        href={item.href}
        title={item.label}
        aria-label={item.label}
        onclick={() => onNavigate?.()}
        target={item.external ? '_blank' : undefined}
        rel={item.external ? 'noreferrer' : undefined}
      >
        <item.icon size={20} />
        <span class="rail-label">{item.label}</span>
      </a>
    {/each}

    {#if focusedEnvironment}
      <button
        type="button"
        class="rail-item rail-reveal"
        onclick={showEverything}
        disabled={revealBusy}
        title="Show everything"
        aria-label="Show everything"
      >
        <svg
          xmlns="http://www.w3.org/2000/svg"
          width="20"
          height="20"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="1.75"
          stroke-linecap="round"
          stroke-linejoin="round"
          aria-hidden="true"
        >
          <circle cx="5" cy="6" r="1.5" />
          <circle cx="5" cy="12" r="1.5" />
          <circle cx="5" cy="18" r="1.5" />
          <path d="M10 6h9M10 12h9M10 18h9" />
        </svg>
        <span class="rail-label">{revealBusy ? 'Opening…' : 'Show everything'}</span>
      </button>
    {/if}
  </nav>

  <nav class="rail-group rail-bottom">
    {#each bottomNav as item (item.href)}
      <a
        class="rail-item"
        class:active={!item.external && isActive(item.href, $page.url.pathname)}
        href={item.href}
        title={item.label}
        aria-label={item.label}
        onclick={() => onNavigate?.()}
        target={item.external ? '_blank' : undefined}
        rel={item.external ? 'noreferrer' : undefined}
      >
        <item.icon size={20} />
        <span class="rail-label">{item.label}</span>
      </a>
    {/each}
  </nav>
</aside>

<style>
  .rail {
    height: 100%;
    width: var(--sidebar-width);
    display: flex;
    flex-direction: column;
    /* The nav needs ~630px to lay out; the window can be 600px. Scroll the rail
       rather than let it silently clip the bottom entries. */
    min-height: 0;
    overflow-y: auto;
    padding: 10px 0;
    background: var(--bg-secondary);
    border-right: 1px solid var(--border);
    transition: width 0.2s ease;
  }

  .brand {
    display: flex;
    align-items: center;
    gap: 10px;
    min-height: 42px;
    margin: 0 8px 8px;
    padding: 0 10px;
    color: var(--text-primary);
    text-decoration: none;
    border-radius: 10px;
    overflow: hidden;
  }
  .brand:hover {
    background: var(--surface-hover);
  }
  .brand img {
    width: 28px;
    height: 28px;
    flex: 0 0 28px;
    object-fit: contain;
  }
  .brand-label {
    display: none;
    font-size: 0.84rem;
    font-weight: 650;
    letter-spacing: -0.01em;
    white-space: nowrap;
  }
  .rail.expanded .brand-label {
    display: inline;
  }
  .rail.expanded {
    width: 200px;
  }

  .rail-toggle-btn {
    display: flex;
    align-items: center;
    gap: 10px;
    margin: 0 8px 6px;
    padding: 0 10px;
    height: 36px;
    flex-shrink: 0;
    border-radius: 10px;
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
    cursor: pointer;
    justify-content: center;
    transition: background var(--transition-base), color var(--transition-base);
  }
  .rail.expanded .rail-toggle-btn {
    justify-content: flex-start;
  }
  .rail-toggle-btn:hover {
    color: var(--text-primary);
    background: var(--surface-hover);
  }

  .rail-group {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 0 8px;
    /* Keep full height once the rail scrolls; otherwise flex squashes the rows
       into an unreadable stack instead of overflowing. */
    flex-shrink: 0;
  }
  .rail-bottom {
    margin-top: auto;
  }

  .rail-item {
    position: relative;
    display: flex;
    align-items: center;
    gap: 10px;
    height: 44px;
    flex-shrink: 0;
    padding: 0 12px;
    border-radius: 10px;
    color: var(--text-secondary);
    text-decoration: none;
    transition: background var(--transition-base), color var(--transition-base);
    justify-content: center;
  }
  .rail.expanded .rail-item {
    justify-content: flex-start;
  }
  .rail-item:hover {
    color: var(--text-primary);
    background: var(--surface-hover);
  }
  .rail-item.active {
    color: var(--text-primary);
    background: var(--surface-accent-soft);
    box-shadow: inset 0 0 0 1px rgba(201, 138, 75, 0.35);
  }
  .rail-item.active::before {
    content: '';
    position: absolute;
    left: -8px;
    top: 10px;
    bottom: 10px;
    width: 3px;
    border-radius: 3px;
    background: var(--accent-cyan);
  }

  /* The way out of the focused environment. Deliberately quieter than a real
     nav entry — it's an escape hatch, not a destination. */
  .rail-reveal {
    border: 1px dashed var(--border);
    background: transparent;
    cursor: pointer;
    font: inherit;
    margin-top: 6px;
  }
  .rail-reveal:disabled {
    opacity: 0.6;
    cursor: default;
  }

  .rail-label {
    display: none;
    font-size: 13px;
    font-weight: 600;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .rail.expanded .rail-label {
    display: inline;
  }
</style>
