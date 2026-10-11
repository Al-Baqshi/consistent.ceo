<script lang="ts">
  import { page } from '$app/state';
</script>

<div style="padding: 2rem; font-family: monospace;">
  <h1>Error {page.status}</h1>
  <pre>{page.error?.message ?? 'Unknown error'}</pre>
</div>
