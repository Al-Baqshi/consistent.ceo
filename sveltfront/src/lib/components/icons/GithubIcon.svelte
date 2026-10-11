<script lang="ts">
  let { size = 20, class: className = '' }: { size?: number; class?: string } = $props();
</script>

<svg xmlns="http://www.w3.org/2000/svg" width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" class={className} aria-hidden="true">
  <path d="M9 19c-4 1.5-4-2-6-2m12 5v-3.5a3 3 0 0 0-.9-2.3c3-.3 6.2-1.5 6.2-6.8a5.3 5.3 0 0 0-1.5-3.7 4.95 4.95 0 0 0-.1-3.7S17 .5 14 2.5a13.4 13.4 0 0 0-7 0C4 .5 2.8.8 2.8.8a4.9 4.9 0 0 0-.1 3.7A5.3 5.3 0 0 0 1.2 8.2c0 5.3 3.2 6.5 6.2 6.8a3 3 0 0 0-.9 2.3V21" />
</svg>
