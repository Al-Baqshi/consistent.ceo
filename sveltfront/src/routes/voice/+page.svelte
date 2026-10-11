<script lang="ts">
  import { onDestroy } from 'svelte';
  import { VoiceSession, type VoiceStatus } from '$lib/voice';
  import Seo from '$lib/components/Seo.svelte';

  let status = $state<VoiceStatus>('idle');
  let statusDetail = $state<string | null>(null);
  let transcript = $state('');
  let errorMsg = $state<string | null>(null);
  let lastLatencyMs = $state<number | null>(null);
  let session: VoiceSession | null = null;

  const systemInstruction =
    'You are Consistent, a warm and concise voice assistant. Keep replies short.';

  function newSession() {
    return new VoiceSession({
      onStatus: (s, detail) => {
        status = s;
        statusDetail = detail ?? null;
        if (s !== 'error') errorMsg = null;
      },
      onText: (delta) => { transcript += delta; },
      onTurnComplete: () => { transcript += '\n\n'; },
      onInterrupted: () => {},
      onError: (msg) => { errorMsg = msg; },
      onLatency: (ms) => { lastLatencyMs = Math.round(ms); },
    });
  }

  async function toggle() {
    if (status === 'idle' || status === 'error') {
      transcript = '';
      errorMsg = null;
      lastLatencyMs = null;
      session = newSession();
      await session.start({ systemInstruction });
    } else {
      await session?.stop();
      session = null;
    }
  }

  onDestroy(() => {
    session?.stop();
  });

  const label = $derived(
    status === 'connecting' ? 'Connecting…'
    : status === 'active' ? 'Listening'
    : status === 'error' ? `Error${statusDetail ? ` · ${statusDetail}` : ''}`
    : 'Tap to start'
  );

  const buttonLabel = $derived(
    status === 'active' || status === 'connecting' ? 'Stop' : 'Start'
  );
</script>

<Seo title="Voice" noindex />

<div class="page">
  <header>
    <h1>Consistent — Voice</h1>
    <span class="status" class:active={status === 'active'} class:connecting={status === 'connecting'} class:error={status === 'error'}>
      {label}
    </span>
  </header>

  <button class="orb" class:active={status === 'active'} onclick={toggle} aria-label={buttonLabel}>
    <span class="orb-inner">
      {status === 'active' ? '◉' : '●'}
    </span>
  </button>

  <div class="controls">
    <button class="primary" onclick={toggle}>{buttonLabel}</button>
  </div>

  {#if errorMsg}
    <p class="error">{errorMsg}</p>
  {/if}

  {#if lastLatencyMs !== null}
    <p class="latency">First audio out: <strong>{lastLatencyMs} ms</strong></p>
  {/if}

  <section class="transcript" aria-live="polite">
    {#if transcript}
      <pre>{transcript}</pre>
    {:else}
      <p class="muted">Consistent’s reply will stream here while speaking.</p>
    {/if}
  </section>
</div>

<style>
  .page {
    max-width: 720px;
    margin: 0 auto;
    padding: 2rem 1.25rem 4rem;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 1.5rem;
    color: #f2ede4;
  }
  header {
    width: 100%;
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 1rem;
  }
  h1 {
    font-size: 1.35rem;
    font-weight: 600;
    margin: 0;
  }
  .status {
    font-size: 0.8rem;
    padding: 0.25rem 0.7rem;
    border-radius: 999px;
    background: rgba(242, 237, 228, 0.06);
    color: #b3aa9b;
  }
  .status.connecting { color: #e0a55e; background: rgba(224, 165, 94, 0.12); }
  .status.active     { color: #7a8163; background: rgba(122, 129, 99, 0.15); }
  .status.error      { color: #c96a4b; background: rgba(201, 106, 75, 0.15); }

  .orb {
    width: 200px;
    height: 200px;
    border-radius: 50%;
    border: none;
    background: radial-gradient(circle at 30% 30%, #c98a4b, #3b2db5);
    box-shadow: 0 18px 60px rgba(201, 138, 75, 0.35);
    color: #fff;
    font-size: 2.2rem;
    cursor: pointer;
    transition: transform 0.15s ease, box-shadow 0.3s ease;
  }
  .orb:hover { transform: scale(1.02); }
  .orb.active {
    animation: pulse 1.4s ease-in-out infinite;
    box-shadow: 0 0 0 0 rgba(201, 138, 75, 0.6);
  }
  .orb-inner { display: inline-block; }

  @keyframes pulse {
    0%, 100% { box-shadow: 0 0 0 0 rgba(201, 138, 75, 0.6); }
    50%      { box-shadow: 0 0 0 28px rgba(201, 138, 75, 0); }
  }

  .controls { display: flex; gap: 0.75rem; }
  .primary {
    background: #c98a4b;
    color: #fff;
    border: none;
    padding: 0.6rem 1.4rem;
    border-radius: 999px;
    font-weight: 600;
    cursor: pointer;
  }
  .primary:hover { filter: brightness(1.08); }

  .transcript {
    width: 100%;
    min-height: 140px;
    background: rgba(242, 237, 228, 0.04);
    border: 1px solid rgba(242, 237, 228, 0.08);
    border-radius: 14px;
    padding: 1rem 1.1rem;
  }
  .transcript pre {
    margin: 0;
    white-space: pre-wrap;
    font-family: inherit;
    font-size: 0.95rem;
    line-height: 1.5;
  }
  .muted { color: #7d7568; margin: 0; font-size: 0.9rem; }
  .error {
    margin: 0;
    color: #c96a4b;
    font-size: 0.88rem;
  }
  .latency {
    margin: 0;
    font-size: 0.82rem;
    color: #b3aa9b;
  }
  .latency strong { color: #f2ede4; }
</style>
