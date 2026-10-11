<script lang="ts">
  let { size = 20, class: className = '' }: { size?: number; class?: string } = $props();
</script>

<svg xmlns="http://www.w3.org/2000/svg" width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" class={className} aria-hidden="true">
  <path d="M19 11h-2a2 2 0 1 0-4 0V9H9v4H7a2 2 0 1 0 0 4h2v2h4a2 2 0 1 1 4 0v-2h2a2 2 0 1 0 0-4" />
</svg>
