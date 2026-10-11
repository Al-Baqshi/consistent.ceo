<script lang="ts">
  let { size = 20, class: className = '' }: { size?: number; class?: string } = $props();
</script>

<svg xmlns="http://www.w3.org/2000/svg" width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" class={className} aria-hidden="true">
  <path d="M12 3v3" />
  <rect x="4" y="6" width="16" height="12" rx="3" />
  <path d="M9 12h.01" />
  <path d="M15 12h.01" />
  <path d="M9 16c.8.6 1.8 1 3 1s2.2-.4 3-1" />
  <path d="M2 13h2M20 13h2" />
</svg>
