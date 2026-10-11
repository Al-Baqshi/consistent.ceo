<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { fetchChatAttachmentUrl } from '$lib/api';

  // An inline image for a chat attachment. The bytes come through an
  // authenticated fetch (see `fetchChatAttachmentUrl`) rather than a bare
  // `src`, and the object URL is released when the element goes away.
  let {
    id,
    alt = '',
    class: className = '',
  }: {
    id: string;
    alt?: string;
    class?: string;
  } = $props();

  let src = $state<string | null>(null);
  let failed = $state(false);

  onMount(async () => {
    try {
      src = await fetchChatAttachmentUrl(id);
    } catch {
      failed = true;
    }
  });

  onDestroy(() => {
    if (src) URL.revokeObjectURL(src);
  });
</script>

{#if src}
  <img class={className} {src} {alt} />
{:else if failed}
  <div class="attachment-broken" title="This image could not be loaded">
    <span class="attachment-broken-icon">🖼️</span>
    <span class="attachment-broken-name">{alt || 'image'}</span>
  </div>
{:else}
  <div class={`attachment-loading ${className}`} aria-label="Loading image"></div>
{/if}

<style>
  .attachment-loading {
    min-width: 96px;
    min-height: 64px;
    border-radius: 8px;
    background: var(--bg-card);
    border: 1px solid var(--border);
    opacity: 0.6;
  }
  .attachment-broken {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 6px 10px;
    border-radius: 8px;
    border: 1px solid var(--border);
    background: var(--bg-card);
    color: var(--text-secondary);
    font-size: 12px;
  }
  .attachment-broken-name {
    max-width: 220px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
