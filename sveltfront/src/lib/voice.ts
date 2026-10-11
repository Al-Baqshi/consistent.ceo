// Gemini Live voice client (browser).
//
// Mic (Web Audio → 16 kHz PCM16 mono) → WebSocket (/api/voice/session) → gateway.
// Playback: gateway emits PCM16 24 kHz mono binary frames; we enqueue them
// into an AudioWorklet-less ScriptProcessor fallback using AudioBufferSourceNode.

import { api, getAuthToken, getTenantId } from '$lib/api';
import { invoke, isTauri, listen } from '$lib/tauri';

const INPUT_RATE = 16_000;
const OUTPUT_RATE = 24_000;

export type VoiceStatus = 'idle' | 'connecting' | 'active' | 'error';

export interface VoiceEvents {
  onStatus?: (s: VoiceStatus, detail?: string) => void;
  onText?: (delta: string) => void;
  onTurnComplete?: () => void;
  onInterrupted?: () => void;
  onError?: (msg: string) => void;
  /** First-audio-out latency for the current turn, in ms. */
  onLatency?: (ms: number) => void;
}

export interface VoiceOptions {
  systemInstruction?: string;
  voice?: string;
  model?: string;
}

interface ServerControl {
  type: 'setupComplete' | 'text' | 'turnComplete' | 'interrupted' | 'error';
  delta?: string;
  message?: string;
}

export class VoiceSession {
  private ws: WebSocket | null = null;
  private audioCtx: AudioContext | null = null;
  private inputCtx: AudioContext | null = null;
  private micStream: MediaStream | null = null;
  private micSource: MediaStreamAudioSourceNode | null = null;
  private micProcessor: ScriptProcessorNode | null = null;
  private nextPlaybackTime = 0;
  private stopped = false;
  private tauriUnlisten: Array<() => void> = [];
  private useTauri = false;
  private turnStartedAt: number | null = null;
  private sawAudioThisTurn = false;

  constructor(private readonly events: VoiceEvents = {}) {}

  // `stop()` can land during any await in here — the ticket request, the socket
  // handshake, a mic permission prompt the user is still looking at. It releases
  // what existed when it ran; each step below re-checks `stopped` after its own
  // await and releases whatever it acquired late, so a stopped session can never
  // end up holding a live mic or socket.
  async start(opts: VoiceOptions = {}) {
    this.stopped = false;
    this.useTauri = isTauri();
    this.events.onStatus?.('connecting');

    try {
      if (this.useTauri) {
        await this.startTauri(opts);
      } else {
        await this.openSocket();
        if (this.stopped) return;
        this.sendConfig(opts);
        await this.openMic();
      }
      if (this.stopped) return;
      this.events.onStatus?.('active');
    } catch (err) {
      // A stop mid-handshake makes the pending step fail. That is the user
      // cancelling, not an error, and stop() has already reported 'idle'.
      if (this.stopped) return;
      const msg = err instanceof Error ? err.message : String(err);
      this.events.onStatus?.('error', msg);
      this.events.onError?.(msg);
      await this.stop();
    }
  }

  async stop() {
    this.stopped = true;
    try { this.micProcessor?.disconnect(); } catch (_) {}
    try { this.micSource?.disconnect(); } catch (_) {}
    this.micStream?.getTracks().forEach((t) => t.stop());
    this.micProcessor = null;
    this.micSource = null;
    this.micStream = null;
    try { await this.inputCtx?.close(); } catch (_) {}
    this.inputCtx = null;
    try { this.ws?.close(); } catch (_) {}
    this.ws = null;
    try { await this.audioCtx?.close(); } catch (_) {}
    this.audioCtx = null;
    this.nextPlaybackTime = 0;

    for (const un of this.tauriUnlisten.splice(0)) {
      try { un(); } catch (_) {}
    }
    if (this.useTauri) {
      try { await invoke('voice_stop'); } catch (_) {}
    }
    this.useTauri = false;

    this.events.onStatus?.('idle');
  }

  /** Keep a Tauri listener, unless the session stopped while it registered —
   *  then drop it at once. Returns whether the session is still live. */
  private async keepListener(pending: Promise<() => void>): Promise<boolean> {
    const unlisten = await pending;
    if (this.stopped) {
      try { unlisten(); } catch (_) {}
      return false;
    }
    this.tauriUnlisten.push(unlisten);
    return true;
  }

  private async startTauri(opts: VoiceOptions) {
    const token = getAuthToken() ?? '';
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const gatewayWsUrl = `${proto}//${location.host}/api/voice/session`;

    const live =
      (await this.keepListener(
        listen<number[] | Uint8Array>('voice-audio', (payload) => {
          const bytes = payload instanceof Uint8Array ? payload : new Uint8Array(payload);
          this.playPcm16(bytes);
        }),
      )) &&
      (await this.keepListener(
        listen<string>('voice-control', (payload) => {
          this.handleControlJson(payload);
        }),
      )) &&
      (await this.keepListener(
        listen<string>('voice-error', (msg) => {
          this.events.onError?.(msg);
          this.events.onStatus?.('error', msg);
        }),
      )) &&
      (await this.keepListener(
        listen<null>('voice-stopped', () => {
          if (!this.stopped) {
            this.events.onStatus?.('idle');
          }
        }),
      ));
    if (!live) return;

    await invoke('voice_start', {
      args: {
        gatewayWsUrl,
        authToken: token || null,
        systemInstruction: opts.systemInstruction ?? null,
      },
    });
    if (this.stopped) {
      // stop() ran while the native side was starting, so its voice_stop may
      // have arrived first. The native capture is running now; stop it again.
      try { await invoke('voice_stop'); } catch (_) {}
    }
  }

  private async openSocket() {
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    // A one-time ticket, not the bearer itself: `WebSocket` cannot set headers,
    // and a token in the query string lands in access logs and Referer.
    const { ticket } = await api<{ ticket: string }>('/api/voice/ticket', { method: 'POST' });
    if (this.stopped) return;
    const qs = new URLSearchParams();
    qs.set('ticket', ticket);
    qs.set('workspace_id', getTenantId());
    const url = `${proto}//${location.host}/api/voice/session?${qs.toString()}`;

    const ws = new WebSocket(url);
    ws.binaryType = 'arraybuffer';
    // Held from the start, not from `open`, so a stop() during the handshake
    // can close it; closing a connecting socket fails the wait below.
    this.ws = ws;

    await new Promise<void>((resolve, reject) => {
      ws.addEventListener('open', () => resolve(), { once: true });
      ws.addEventListener('error', () => reject(new Error('voice websocket failed')), { once: true });
      ws.addEventListener('close', () => reject(new Error('voice websocket closed')), { once: true });
    });
    if (this.stopped) {
      try { ws.close(); } catch (_) {}
      return;
    }

    ws.addEventListener('message', (e) => this.handleMessage(e));
    ws.addEventListener('close', () => {
      if (!this.stopped) {
        this.events.onStatus?.('error', 'disconnected');
        this.events.onError?.('voice connection closed');
      }
    });
  }

  private sendConfig(opts: VoiceOptions) {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    this.ws.send(JSON.stringify({
      type: 'config',
      systemInstruction: opts.systemInstruction,
      voice: opts.voice,
      model: opts.model,
    }));
  }

  private async openMic() {
    const stream = await navigator.mediaDevices.getUserMedia({
      audio: { echoCancellation: true, noiseSuppression: true, channelCount: 1 },
    });
    if (this.stopped) {
      // Stopped while the permission prompt was up: hand the mic straight back.
      stream.getTracks().forEach((t) => t.stop());
      return;
    }
    this.micStream = stream;

    const AudioContextCtor = (window.AudioContext ||
      (window as unknown as { webkitAudioContext: typeof AudioContext }).webkitAudioContext);
    const inputCtx = new AudioContextCtor();
    this.inputCtx = inputCtx;
    const sourceRate = inputCtx.sampleRate;
    const source = inputCtx.createMediaStreamSource(stream);
    this.micSource = source;

    const processor = inputCtx.createScriptProcessor(4096, 1, 1);
    this.micProcessor = processor;
    const ratio = sourceRate / INPUT_RATE;
    let carry: Float32Array = new Float32Array(0);

    processor.onaudioprocess = (e) => {
      if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
      const input = e.inputBuffer.getChannelData(0);
      const combined = new Float32Array(carry.length + input.length);
      combined.set(carry);
      combined.set(input, carry.length);

      const outLen = Math.floor(combined.length / ratio);
      const out = new Int16Array(outLen);
      for (let i = 0; i < outLen; i++) {
        const srcIdx = i * ratio;
        const lo = Math.floor(srcIdx);
        const hi = Math.min(lo + 1, combined.length - 1);
        const frac = srcIdx - lo;
        const sample = combined[lo] * (1 - frac) + combined[hi] * frac;
        const clamped = Math.max(-1, Math.min(1, sample));
        out[i] = clamped < 0 ? clamped * 0x8000 : clamped * 0x7fff;
      }
      const consumed = Math.floor(outLen * ratio);
      carry = combined.slice(consumed);

      if (this.turnStartedAt === null) {
        this.turnStartedAt = performance.now();
        this.sawAudioThisTurn = false;
      }
      this.ws.send(out.buffer);
    };

    source.connect(processor);
    processor.connect(inputCtx.destination); // required for Safari; output is silenced below
    const silent = inputCtx.createGain();
    silent.gain.value = 0;
    processor.connect(silent).connect(inputCtx.destination);
  }

  private handleMessage(e: MessageEvent) {
    if (typeof e.data === 'string') {
      this.handleControlJson(e.data);
      return;
    }

    if (e.data instanceof ArrayBuffer) {
      this.playPcm16(new Uint8Array(e.data));
    }
  }

  private handleControlJson(raw: string) {
    try {
      const ctrl = JSON.parse(raw) as ServerControl;
      switch (ctrl.type) {
        case 'setupComplete':
          break;
        case 'text':
          if (ctrl.delta) this.events.onText?.(ctrl.delta);
          break;
        case 'turnComplete':
          this.turnStartedAt = null;
          this.sawAudioThisTurn = false;
          this.events.onTurnComplete?.();
          break;
        case 'interrupted':
          this.nextPlaybackTime = 0;
          this.events.onInterrupted?.();
          break;
        case 'error':
          this.events.onError?.(ctrl.message ?? 'unknown voice error');
          break;
      }
    } catch (_) {
      // ignore malformed control frames
    }
  }

  private ensureOutputCtx(): AudioContext {
    if (!this.audioCtx) {
      const AudioContextCtor = (window.AudioContext ||
        (window as unknown as { webkitAudioContext: typeof AudioContext }).webkitAudioContext);
      this.audioCtx = new AudioContextCtor({ sampleRate: OUTPUT_RATE });
    }
    return this.audioCtx;
  }

  private playPcm16(bytes: Uint8Array) {
    const ctx = this.ensureOutputCtx();
    const frameCount = bytes.byteLength / 2;
    if (frameCount === 0) return;
    if (!this.sawAudioThisTurn && this.turnStartedAt !== null) {
      const latency = performance.now() - this.turnStartedAt;
      this.sawAudioThisTurn = true;
      this.events.onLatency?.(latency);
    }
    const buffer = ctx.createBuffer(1, frameCount, OUTPUT_RATE);
    const channel = buffer.getChannelData(0);
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    for (let i = 0; i < frameCount; i++) {
      const s = view.getInt16(i * 2, true);
      channel[i] = s < 0 ? s / 0x8000 : s / 0x7fff;
    }
    const src = ctx.createBufferSource();
    src.buffer = buffer;
    src.connect(ctx.destination);
    const startAt = Math.max(ctx.currentTime, this.nextPlaybackTime);
    src.start(startAt);
    this.nextPlaybackTime = startAt + buffer.duration;
  }
}
