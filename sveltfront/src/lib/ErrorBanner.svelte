<script lang="ts">
  import { apiError, clearApiError } from '$lib/errors';
</script>

{#if $apiError}
  <div class="banner" role="alert">
    <span class="banner-text">{$apiError}</span>
    <button type="button" class="banner-dismiss" onclick={() => clearApiError()} aria-label="Dismiss">✕</button>
  </div>
{/if}

<style>
  .banner {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 8px 16px;
    background: rgba(201, 106, 75, 0.12);
    border: 1px solid rgba(201, 106, 75, 0.4);
    color: #c96a4b;
    font-size: 13px;
  }
  .banner-text { flex: 1; min-width: 0; }
  .banner-dismiss {
    background: none;
    border: none;
    color: inherit;
    padding: 4px;
    cursor: pointer;
    font-size: 14px;
    opacity: 0.9;
  }
  .banner-dismiss:hover { opacity: 1; }
</style>
