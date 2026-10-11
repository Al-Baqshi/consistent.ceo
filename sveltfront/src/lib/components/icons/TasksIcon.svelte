<script lang="ts">
  let { size = 20, class: className = '' }: { size?: number; class?: string } = $props();
</script>

<svg xmlns="http://www.w3.org/2000/svg" width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" class={className} aria-hidden="true">
  <rect x="3" y="4" width="18" height="16" rx="2" />
  <path d="m8 10 2 2 4-4" />
  <path d="M14 16h4" />
  <path d="M8 16h1" />
</svg>
