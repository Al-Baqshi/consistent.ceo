<script lang="ts">
  // Per-page head tags.
  //
  // Site-level metadata (description, OG, Twitter, JSON-LD) lives in `src/app.html`
  // so it survives in the static shell — most routes are SPA-rendered (`ssr = false`),
  // so a crawler never executes the code that would emit `svelte:head`. This component
  // deliberately does NOT re-emit those tags: doing so would duplicate them on the one
  // route that *is* prerendered (`/about`).
  //
  // `noindex` is the useful part for the workspace routes: a self-hosted instance
  // exposed to the internet should not have its console, settings, or data indexed.

  let {
    title,
    description,
    noindex = false,
    canonical
  }: {
    title: string;
    description?: string;
    noindex?: boolean;
    canonical?: string;
  } = $props();

  const SITE = 'Consistent CEO';
  const ORIGIN = 'https://consistent.ceo';

  const fullTitle = $derived(title === SITE ? SITE : `${title} — ${SITE}`);
</script>

<svelte:head>
  <title>{fullTitle}</title>
  {#if description}<meta name="description" content={description} />{/if}
  {#if noindex}<meta name="robots" content="noindex, nofollow" />{/if}
  {#if canonical}<link rel="canonical" href={`${ORIGIN}${canonical}`} />{/if}
</svelte:head>
