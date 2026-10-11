import { getAuthToken, getGatewayUrl } from '@/api/client';
import { log } from '@/lib/logger';

/**
 * Full-duplex voice session against the gateway's `/api/voice/session`
 * WebSocket. The gateway is the brain: it injects the caller's entity
 * catalog into the session server-side, so spoken references like "the
 * coffee business" resolve to the right entity without the client doing
 * anything.
 *
 * Today the Expo app runs in TEXT modality (raw PCM playback needs a native
 * streamer module); `sendAudio` is already wired for when that lands — the
 * protocol is identical, only the response modality changes.
 */

export type VoiceSessionEvent =
  | { type: 'setupComplete' }
  | { type: 'text'; delta: string }
  | { type: 'inputTranscription'; delta: string }
  | { type: 'turnComplete' }
  | { type: 'interrupted' }
  | { type: 'error'; message: string };

export interface VoiceSessionConfig {
  /** Appended AFTER the gateway's entity-aware base instruction. */
  systemInstruction?: string;
  voice?: string;
  model?: string;
  /** Defaults to ['TEXT'] — the only modality the Expo shell can render today. */
  modalities?: string[];
}

export interface VoiceSessionHandlers {
  onEvent: (e: VoiceSessionEvent) => void;
  /** Raw PCM16 24kHz reply audio (only when modalities includes AUDIO). */
  onAudio?: (chunk: ArrayBuffer) => void;
  onClose?: () => void;
}

export class VoiceSession {
  private constructor(
    private readonly ws: WebSocket,
    private readonly handlers: VoiceSessionHandlers,
  ) {}

  static async connect(
    handlers: VoiceSessionHandlers,
    config: VoiceSessionConfig = {},
  ): Promise<VoiceSession> {
    const base = await getGatewayUrl();
    const token = (await getAuthToken()) ?? '';
    const wsBase = base.replace(/^http/, 'ws');

    // A one-time ticket rather than the bearer itself: React Native's
    // WebSocket cannot set headers, and a token in the query string is logged
    // by every proxy it passes.
    const ticketRes = await fetch(`${base}/api/voice/ticket`, {
      method: 'POST',
      headers: { authorization: `Bearer ${token}` },
    });
    if (!ticketRes.ok) {
      throw new Error(`voice ticket failed: ${ticketRes.status}`);
    }
    const { ticket } = (await ticketRes.json()) as { ticket: string };
    const url = `${wsBase}/api/voice/session?ticket=${encodeURIComponent(ticket)}`;

    const ws = new WebSocket(url);
    ws.binaryType = 'arraybuffer';
    const session = new VoiceSession(ws, handlers);

    return new Promise((resolve, reject) => {
      let settled = false;
      ws.onopen = () => {
        // Config must arrive before the first text/audio frame — the gateway
        // connects upstream lazily on the first content frame.
        ws.send(
          JSON.stringify({
            type: 'config',
            systemInstruction: config.systemInstruction,
            voice: config.voice,
            model: config.model,
            modalities: config.modalities ?? ['TEXT'],
          }),
        );
        settled = true;
        resolve(session);
      };
      ws.onerror = () => {
        if (!settled) {
          settled = true;
          reject(new Error('voice session failed to connect'));
        } else {
          handlers.onEvent({ type: 'error', message: 'connection error' });
        }
      };
      ws.onclose = () => {
        if (!settled) {
          settled = true;
          reject(new Error('voice session closed during connect'));
        }
        handlers.onClose?.();
      };
      ws.onmessage = (ev) => {
        if (typeof ev.data === 'string') {
          try {
            const parsed = JSON.parse(ev.data) as VoiceSessionEvent;
            handlers.onEvent(parsed);
          } catch {
            log('warn', 'api', `voice: unparseable control frame: ${ev.data.slice(0, 120)}`);
          }
        } else if (ev.data instanceof ArrayBuffer) {
          handlers.onAudio?.(ev.data);
        }
      };
    });
  }

  /** Speak to the companion via text (dictation or keyboard). */
  sendText(text: string): void {
    if (this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify({ type: 'text', text }));
    }
  }

  /** Stream a PCM16 16kHz microphone chunk. */
  sendAudio(chunk: ArrayBuffer): void {
    if (this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(chunk);
    }
  }

  /** Push-to-talk: done speaking — commit the buffered audio for a reply now. */
  sendAudioDone(): void {
    if (this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify({ type: 'audioDone' }));
    }
  }

  get open(): boolean {
    return this.ws.readyState === WebSocket.OPEN;
  }

  close(): void {
    try {
      this.ws.close();
    } catch {
      /* already closed */
    }
  }
}
