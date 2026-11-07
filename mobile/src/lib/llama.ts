import { Platform, TurboModuleRegistry } from 'react-native';
import type { LlamaContext } from 'llama.rn';

import { log } from '@/lib/logger';

/**
 * llama.rn, loaded only when its native module is actually in this build.
 * Expo Go, web and dev clients built before it was added simply report
 * "unavailable" instead of crashing at import.
 */

type LlamaModule = typeof import('llama.rn');

let mod: LlamaModule | null | undefined;

export function getLlama(): LlamaModule | null {
  if (mod !== undefined) return mod;
  mod = null;
  if (Platform.OS === 'web') return mod;
  try {
    if (TurboModuleRegistry.get('RNLlama') != null) mod = require('llama.rn') as LlamaModule;
  } catch (e) {
    log('warn', 'system', `llama.rn unavailable: ${String(e)}`);
  }
  return mod;
}

export function isLlamaAvailable(): boolean {
  return getLlama() != null;
}

let context: { path: string; ctx: Promise<LlamaContext> } | null = null;

/** One loaded model at a time; loading takes seconds, so it is kept warm. */
export function llamaContext(path: string): Promise<LlamaContext> {
  const llama = getLlama();
  if (!llama) return Promise.reject(new Error('On-device AI needs the latest app build.'));
  if (context?.path === path) return context.ctx;
  void releaseLlama();
  const ctx = llama.initLlama({
    model: path,
    n_ctx: 4096,
    n_gpu_layers: Platform.OS === 'ios' ? 99 : 0,
    use_mlock: false,
  });
  context = { path, ctx };
  ctx.catch(() => {
    if (context?.ctx === ctx) context = null;
  });
  return ctx;
}

export async function releaseLlama(): Promise<void> {
  const current = context;
  context = null;
  if (!current) return;
  try {
    await (await current.ctx).release();
  } catch {
    /* never loaded */
  }
}
