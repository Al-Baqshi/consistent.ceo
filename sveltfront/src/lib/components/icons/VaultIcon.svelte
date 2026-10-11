<script lang="ts">
  let { size = 20, class: className = '' }: { size?: number; class?: string } = $props();
</script>

<svg xmlns="http://www.w3.org/2000/svg" width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" class={className} aria-hidden="true">
  <rect x="3" y="10" width="18" height="11" rx="2" />
  <path d="M7 10V7a5 5 0 0 1 10 0v3" />
  <circle cx="12" cy="15.5" r="1.5" />
  <path d="M12 17v1.5" />
</svg>
