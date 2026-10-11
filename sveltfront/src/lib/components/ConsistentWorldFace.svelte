<script lang="ts">
  import { onMount, onDestroy } from 'svelte';

  type Props = {
    signallingUrl?: string;
    onConnectedChange?: (connected: boolean) => void;
    onFaceEvent?: (event: { type: string; payload: unknown }) => void;
  };

  let {
    signallingUrl = 'ws://127.0.0.1:8888',
    onConnectedChange,
    onFaceEvent,
  }: Props = $props();

  let videoContainer: HTMLDivElement | null = $state(null);
  let isConnected = $state(false);
  let errorMessage = $state<string | null>(null);
  // Kept loosely typed: the Epic Games lib is optional at install time.
  let stream: {
    disconnect?: () => void;
    emitUIInteraction?: (m: unknown) => void;
    addResponseEventListener?: (event: string, handler: (data: string) => void) => void;
  } | null = null;

  async function loadPixelStreamingLib(): Promise<any | null> {
    try {
      // @ts-expect-error - optional peer dep; package may not be installed.
      return await import('@epicgames-ps/lib-pixelstreamingfrontend-ue5.6');
    } catch {
      errorMessage =
        'Pixel Streaming library not installed. Run: pnpm add @epicgames-ps/lib-pixelstreamingfrontend-ue5.6';
      return null;
    }
  }

  onMount(async () => {
    const lib = await loadPixelStreamingLib();
    if (!lib || !videoContainer) return;

    const { PixelStreaming, Config } = lib;

    const config = new Config({
      initialSettings: {
        AutoConnect: true,
        AutoPlayVideo: true,
        StartVideoMuted: false,
        HoveringMouse: true,
        ss: signallingUrl,
      },
    });

    stream = new PixelStreaming(config, {
      videoElementParent: videoContainer,
      onConnect: () => {
        isConnected = true;
        onConnectedChange?.(true);
      },
      onDisconnect: () => {
        isConnected = false;
        onConnectedChange?.(false);
      },
    });

    stream?.addResponseEventListener?.('tati_event', (data: string) => {
      try {
        const payload = JSON.parse(data);
        onFaceEvent?.({ type: payload?.type ?? 'unknown', payload });
      } catch {
        onFaceEvent?.({ type: 'raw', payload: data });
      }
    });
  });

  export function triggerExpression(name: string) {
    stream?.emitUIInteraction?.({ type: 'TRIGGER_EXPRESSION', name });
  }

  export function sendCommand(type: string, payload: Record<string, unknown> = {}) {
    stream?.emitUIInteraction?.({ type, ...payload });
  }

  onDestroy(() => {
    stream?.disconnect?.();
  });
</script>

<div class="tati-viewport-wrapper">
  {#if errorMessage}
    <div class="error-overlay">
      <p>{errorMessage}</p>
    </div>
  {:else if !isConnected}
    <div class="loading-overlay">
      <p>Initializing Consistent…</p>
    </div>
  {/if}

  <div bind:this={videoContainer} class="video-container"></div>
</div>

<style>
  .tati-viewport-wrapper {
    position: relative;
    width: 100%;
    height: 100%;
    background: #0a0908;
    border-radius: 12px;
    overflow: hidden;
  }

  .video-container {
    width: 100%;
    height: 100%;
  }

  :global(.tati-viewport-wrapper video) {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }

  .loading-overlay,
  .error-overlay {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    color: #f2ede4;
    background: rgba(0, 0, 0, 0.55);
    backdrop-filter: blur(4px);
    text-align: center;
    padding: 1rem;
  }

  .error-overlay p {
    color: #ff9090;
    font-size: 0.9rem;
    max-width: 32ch;
    line-height: 1.4;
  }
</style>
