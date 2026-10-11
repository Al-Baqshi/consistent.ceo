<script lang="ts">
  let { size = 20, class: className = '' }: { size?: number; class?: string } = $props();
</script>

<svg xmlns="http://www.w3.org/2000/svg" width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" class={className} aria-hidden="true">
  <rect x="9" y="3" width="6" height="12" rx="3" />
  <path d="M5 11a7 7 0 0 0 14 0" />
  <path d="M12 18v3" />
</svg>
