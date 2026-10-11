import {
  requestRecordingPermissionsAsync,
  setAudioModeAsync,
  useAudioStream,
} from 'expo-audio';
import * as Speech from 'expo-speech';
import { useCallback, useEffect, useRef, useState } from 'react';

import { isDemoGateway } from '@/api/client';
import { log } from '@/lib/logger';
import { VoiceSession } from '@/lib/voiceSession';

/**
 * Half-duplex push-to-talk against the gateway's live voice session.
 *
 * Mic audio streams to the gateway in real time (PCM16 16kHz over the voice
 * WebSocket); the reply streams back as text and is spoken aloud with the
 * device TTS. Half-duplex on purpose: the mic is off while the companion
 * speaks, which avoids TTS echo and the iOS audio-session category conflict
 * (recording puts the session in .record, which mutes playback).
 *
 * The session stays open across turns so a conversation doesn't reconnect
 * per utterance; it closes when the caller stops the talk flow.
 */

export type VoiceTalkState =
  | 'idle'
  | 'connecting'
  | 'listening'
  | 'thinking'
  | 'speaking'
  | 'error';

export interface VoiceTalkCallbacks {
  /** Cumulative transcript of what the user said this turn. */
  onUserTranscript?: (text: string) => void;
  /** Cumulative reply text so far this turn. */
  onAssistantDelta?: (text: string) => void;
  /** Final reply text for the turn. */
  onAssistantDone?: (text: string) => void;
  onError?: (message: string) => void;
}

export interface VoiceTalkOptions extends VoiceTalkCallbacks {
  /** Appended after the gateway's entity-aware base instruction. */
  systemInstruction?: string;
  /** When true the reply is not spoken aloud (text still streams). */
  muted?: boolean;
}

const TARGET_RATE = 16_000;
/** No reply this long after "done speaking" → treat the turn as failed. */
const REPLY_WATCHDOG_MS = 15_000;

// Never mention credits or buying anything here (App Review 3.1.1): a refused
// session just means the computer isn't taking voice right now.
const CONNECT_ERROR =
  'Voice needs a connection to your computer. Check that Consistent CEO is open on it, and the connection in Settings.';

const DEMO_ERROR =
  'Voice conversations run through your computer. Pair this phone with Consistent CEO on your computer to talk.';

/** Linear-interpolation downsample to 16kHz — safety net for the rare device
 * that can't deliver the requested rate natively. */
function downsampleTo16k(data: ArrayBuffer, fromRate: number): ArrayBuffer {
  if (fromRate === TARGET_RATE) return data;
  const src = new Int16Array(data);
  const outLen = Math.floor((src.length * TARGET_RATE) / fromRate);
  const out = new Int16Array(outLen);
  const ratio = fromRate / TARGET_RATE;
  for (let i = 0; i < outLen; i++) {
    const pos = i * ratio;
    const i0 = Math.floor(pos);
    const i1 = Math.min(i0 + 1, src.length - 1);
    const frac = pos - i0;
    out[i] = (src[i0] * (1 - frac) + src[i1] * frac) | 0;
  }
  return out.buffer;
}

export function useVoiceTalk(options: VoiceTalkOptions) {
  const [state, setState] = useState<VoiceTalkState>('idle');
  const [errorMessage, setErrorMessage] = useState<string | null>(null);

  const stateRef = useRef<VoiceTalkState>('idle');
  const sessionRef = useRef<VoiceSession | null>(null);
  const optionsRef = useRef(options);
  optionsRef.current = options;

  const userTextRef = useRef('');
  const replyTextRef = useRef('');
  const sentAudioRef = useRef(false);
  const watchdogRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const transition = useCallback((next: VoiceTalkState) => {
    stateRef.current = next;
    setState(next);
  }, []);

  const clearWatchdog = () => {
    if (watchdogRef.current) {
      clearTimeout(watchdogRef.current);
      watchdogRef.current = null;
    }
  };

  const { stream } = useAudioStream({
    sampleRate: TARGET_RATE,
    channels: 1,
    encoding: 'int16',
    onBuffer: (buffer) => {
      if (stateRef.current !== 'listening') return;
      const session = sessionRef.current;
      if (!session?.open) return;
      const pcm = downsampleTo16k(buffer.data, buffer.sampleRate || TARGET_RATE);
      if (pcm.byteLength > 0) {
        sentAudioRef.current = true;
        session.sendAudio(pcm);
      }
    },
  });
  const streamRef = useRef(stream);
  streamRef.current = stream;

  const stopStream = useCallback(() => {
    try {
      streamRef.current.stop();
    } catch {
      /* not streaming */
    }
  }, []);

  const fail = useCallback(
    (message: string) => {
      clearWatchdog();
      stopStream();
      Speech.stop();
      transition('error');
      setErrorMessage(message);
      optionsRef.current.onError?.(message);
    },
    [stopStream, transition],
  );

  const beginListening = useCallback(async () => {
    await setAudioModeAsync({ allowsRecording: true, playsInSilentMode: true });
    await streamRef.current.start();
    transition('listening');
  }, [transition]);

  /** Reply finished (spoken or muted) — go back to listening for the next turn. */
  const resumeAfterReply = useCallback(() => {
    if (stateRef.current !== 'speaking' && stateRef.current !== 'thinking') return;
    userTextRef.current = '';
    replyTextRef.current = '';
    sentAudioRef.current = false;
    beginListening().catch((e) => {
      log('warn', 'api', `voice talk: mic restart failed: ${String(e)}`);
      fail('Microphone restart failed — close and reopen the talk strip.');
    });
  }, [beginListening, fail]);

  const handleTurnComplete = useCallback(() => {
    clearWatchdog();
    const reply = replyTextRef.current.trim();
    optionsRef.current.onAssistantDone?.(reply);
    if (!reply || optionsRef.current.muted) {
      resumeAfterReply();
      return;
    }
    transition('speaking');
    // Free the audio session from .record so TTS is audible.
    setAudioModeAsync({ allowsRecording: false, playsInSilentMode: true })
      .catch(() => undefined)
      .then(() => {
        Speech.speak(reply, {
          onDone: resumeAfterReply,
          onStopped: resumeAfterReply,
          onError: resumeAfterReply,
        });
      });
  }, [resumeAfterReply, transition]);

  const connect = useCallback(async (): Promise<VoiceSession> => {
    if (sessionRef.current?.open) return sessionRef.current;
    const session = await VoiceSession.connect(
      {
        onEvent: (e) => {
          switch (e.type) {
            case 'inputTranscription':
              userTextRef.current += e.delta;
              optionsRef.current.onUserTranscript?.(userTextRef.current);
              break;
            case 'text':
              // Server VAD can start a reply before the user taps done.
              if (stateRef.current === 'listening') {
                stopStream();
                transition('thinking');
              }
              clearWatchdog();
              replyTextRef.current += e.delta;
              optionsRef.current.onAssistantDelta?.(replyTextRef.current);
              break;
            case 'turnComplete':
              if (stateRef.current === 'thinking' || stateRef.current === 'listening') {
                if (stateRef.current === 'listening') stopStream();
                handleTurnComplete();
              }
              break;
            case 'error':
              log('warn', 'api', `voice talk: gateway error: ${e.message}`);
              fail(e.message || CONNECT_ERROR);
              break;
            default:
              break;
          }
        },
        onClose: () => {
          sessionRef.current = null;
          if (stateRef.current === 'listening' || stateRef.current === 'thinking') {
            fail('Voice session closed unexpectedly — tap the mic to try again.');
          }
        },
      },
      {
        systemInstruction: optionsRef.current.systemInstruction,
        modalities: ['TEXT'],
      },
    );
    sessionRef.current = session;
    return session;
  }, [fail, handleTurnComplete, stopStream, transition]);

  /** Open the session (if needed) and start streaming the mic. */
  const start = useCallback(async () => {
    if (stateRef.current !== 'idle' && stateRef.current !== 'error') return;
    setErrorMessage(null);
    transition('connecting');
    // Demo mode has no computer to talk to: say so without asking for the
    // microphone, which would be a permission prompt for nothing.
    if (await isDemoGateway()) {
      fail(DEMO_ERROR);
      return;
    }
    const perm = await requestRecordingPermissionsAsync();
    if (!perm.granted) {
      fail('Microphone permission is required to talk to your companion.');
      return;
    }
    try {
      await connect();
      userTextRef.current = '';
      replyTextRef.current = '';
      sentAudioRef.current = false;
      await beginListening();
    } catch (e) {
      log('warn', 'api', `voice talk: connect failed: ${String(e)}`);
      fail(CONNECT_ERROR);
    }
  }, [beginListening, connect, fail, transition]);

  /** User finished speaking — commit the audio and wait for the reply. */
  const endUtterance = useCallback(() => {
    if (stateRef.current !== 'listening') return;
    stopStream();
    if (!sentAudioRef.current) {
      // Nothing was said — stay in the conversation, keep listening.
      beginListening().catch(() => transition('idle'));
      return;
    }
    sessionRef.current?.sendAudioDone();
    transition('thinking');
    clearWatchdog();
    watchdogRef.current = setTimeout(() => {
      if (stateRef.current === 'thinking') {
        fail('No reply from the voice service — try again.');
      }
    }, REPLY_WATCHDOG_MS);
  }, [beginListening, fail, stopStream, transition]);

  /** Tear the whole talk flow down. */
  const stop = useCallback(() => {
    clearWatchdog();
    stopStream();
    Speech.stop();
    sessionRef.current?.close();
    sessionRef.current = null;
    setAudioModeAsync({ allowsRecording: false, playsInSilentMode: true }).catch(() => undefined);
    setErrorMessage(null);
    transition('idle');
  }, [stopStream, transition]);

  useEffect(() => () => stop(), [stop]);

  return { state, errorMessage, start, endUtterance, stop };
}
