<script lang="ts">
  let { size = 20, class: className = '' }: { size?: number; class?: string } = $props();
</script>

<svg xmlns="http://www.w3.org/2000/svg" width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" class={className} aria-hidden="true">
  <path d="M12 3v4" />
  <path d="M12 17v4" />
  <path d="M3 12h4" />
  <path d="M17 12h4" />
  <path d="M6.2 6.2 8.9 8.9" />
  <path d="M15.1 15.1l2.7 2.7" />
  <path d="M6.2 17.8 8.9 15.1" />
  <path d="M15.1 8.9l2.7-2.7" />
</svg>
