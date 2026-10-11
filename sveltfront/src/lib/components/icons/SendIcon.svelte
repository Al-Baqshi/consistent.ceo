<script lang="ts">
  let { size = 24, class: className = '' }: { size?: number; class?: string } = $props();
</script>

<svg xmlns="http://www.w3.org/2000/svg" width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" class={className} aria-hidden="true">
  <circle cx="12" cy="12" r="10" />
  <path d="M16 12l-4-4-4 4" />
  <path d="M12 16V8" />
</svg>
