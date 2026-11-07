import { Directory, DownloadTask, File, Paths } from 'expo-file-system';
import type { DownloadProgress } from 'expo-file-system';
import { Platform } from 'react-native';
import { create } from 'zustand';

import { FALLBACK_LOCAL_MODEL, getLocalModel } from '@/api/endpoints';
import type { LocalModelManifest } from '@/api/types';
import { log } from '@/lib/logger';

import { isLlamaAvailable, releaseLlama } from './llama';
import {
  getLocalModelState,
  setLocalModelState,
  clearLocalModelState,
} from './db';

/**
 * Consistent AI (offline): the GGUF model the phone runs itself when the
 * computer can't be reached. One button downloads it into the app's documents
 * folder — Wi-Fi only unless the operator says otherwise, with a free-space
 * check, progress, pause/resume (also across launches) and cancel.
 *
 * Model readiness state is persisted in SQLite (`local_model_state`) instead
 * of AsyncStorage, so the phone can query it without the gateway.
 */

type DeviceModule = typeof import('expo-device');
type NetworkModule = typeof import('expo-network');

function optional<T>(load: () => T): T | null {
  try {
    return load();
  } catch {
    return null;
  }
}

// Lazy so a dev client built before these were added keeps running.
const Device = Platform.OS === 'web' ? null : optional<DeviceModule>(() => require('expo-device'));
const Network = Platform.OS === 'web' ? null : optional<NetworkModule>(() => require('expo-network'));

/** Headroom on top of the model itself, so the phone isn't left completely full. */
const SPACE_MARGIN = 500_000_000;

export type LocalModelStatus = 'absent' | 'downloading' | 'paused' | 'ready' | 'error';

export type DownloadBlock = 'cellular' | 'offline' | 'space' | 'unsupported';

interface LocalModelState {
  manifest: LocalModelManifest;
  status: LocalModelStatus;
  bytesWritten: number;
  totalBytes: number;
  error: string | null;
  /** This build can run the model (llama.rn is linked). */
  runtimeAvailable: boolean;
  /** Device RAM in GB, when the OS says. */
  ramGb: number | null;
  /** Enough RAM for the manifest's `minRamGb`. Not a hard limit. */
  recommended: boolean;
  hydrated: boolean;
}

function ramGb(): number | null {
  const bytes = Device?.totalMemory;
  return bytes ? bytes / 1e9 : null;
}

function recommendedFor(manifest: LocalModelManifest, ram: number | null): boolean {
  // Marketing GB vs what the OS reports differ by a few percent.
  return ram == null || ram >= manifest.mobile.minRamGb * 0.92;
}

export const useLocalModel = create<LocalModelState>(() => ({
  manifest: FALLBACK_LOCAL_MODEL,
  status: 'absent',
  bytesWritten: 0,
  totalBytes: FALLBACK_LOCAL_MODEL.mobile.approxBytes,
  error: null,
  runtimeAvailable: false,
  ramGb: null,
  recommended: true,
  hydrated: false,
}));

const set = (patch: Partial<LocalModelState>) => useLocalModel.setState(patch);

function modelDir(): Directory {
  return new Directory(Paths.document, 'models');
}

function modelFile(manifest: LocalModelManifest): File {
  return new File(modelDir(), manifest.mobile.file);
}

function partFile(manifest: LocalModelManifest): File {
  return new File(modelDir(), `${manifest.mobile.file}.part`);
}

export function formatBytes(bytes: number): string {
  if (bytes >= 1e9) return `${(bytes / 1e9).toFixed(1)} GB`;
  if (bytes >= 1e6) return `${Math.round(bytes / 1e6)} MB`;
  return `${Math.max(0, Math.round(bytes / 1e3))} KB`;
}

/** Path to hand llama.rn, or null when the model isn't fully downloaded. */
export async function readyModelPath(): Promise<string | null> {
  if (Platform.OS === 'web') return null;
  const state = await getLocalModelState();
  const marker = state?.readyFile ?? null;
  if (!marker) return null;
  try {
    const file = new File(modelDir(), marker);
    return file.exists ? file.uri : null;
  } catch {
    return null;
  }
}

/** Downloaded, and this build can run it. */
export async function isLocalModelUsable(): Promise<boolean> {
  return isLlamaAvailable() && (await readyModelPath()) != null;
}

/** Read the manifest (gateway, else built-in) and what's on disk. */
export async function refreshLocalModel(): Promise<void> {
  const ram = ramGb();
  set({ runtimeAvailable: isLlamaAvailable(), ramGb: ram });
  const manifest = await getLocalModel();
  const ready = await readyModelPath();
  const state = await getLocalModelState();
  const paused = state?.paused ?? false;
  const current = useLocalModel.getState();
  set({
    manifest,
    recommended: recommendedFor(manifest, ram),
    totalBytes: current.status === 'downloading' ? current.totalBytes : manifest.mobile.approxBytes,
    hydrated: true,
  });
  if (current.status === 'downloading') return;
  if (ready) {
    set({ status: 'ready', error: null });
    await setLocalModelState({ status: 'ready', error: null });
  } else if (paused) {
    let written = 0;
    try {
      const part = partFile(manifest);
      written = part.exists ? part.size : 0;
    } catch {
      /* unknown */
    }
    set({ status: 'paused', bytesWritten: written });
    await setLocalModelState({ status: 'paused', bytesWritten: written });
  } else if (current.status !== 'error') {
    set({ status: 'absent', bytesWritten: 0 });
    await setLocalModelState({ status: 'absent', bytesWritten: 0 });
  }
}

let task: DownloadTask | null = null;

const onProgress = ({ bytesWritten, totalBytes }: DownloadProgress) => {
  set({
    bytesWritten,
    totalBytes: totalBytes > 0 ? totalBytes : useLocalModel.getState().manifest.mobile.approxBytes,
  });
  void setLocalModelState({
    bytesWritten,
    totalBytes: totalBytes > 0 ? totalBytes : useLocalModel.getState().manifest.mobile.approxBytes,
    status: 'downloading',
  });
};

/** Why a download shouldn't start right now; null when it's fine. */
async function precheck(manifest: LocalModelManifest, allowCellular: boolean): Promise<DownloadBlock | null> {
  if (Platform.OS === 'web') return 'unsupported';
  if (Network) {
    try {
      const net = await Network.getNetworkStateAsync();
      if (net.isConnected === false) return 'offline';
      const wired =
        net.type === Network.NetworkStateType.WIFI || net.type === Network.NetworkStateType.ETHERNET;
      if (!allowCellular && !wired) return 'cellular';
    } catch {
      /* unknown network — let the download itself decide */
    }
  }
  try {
    const part = partFile(manifest);
    const have = part.exists ? part.size : 0;
    if (Paths.availableDiskSpace < manifest.mobile.approxBytes - have + SPACE_MARGIN) return 'space';
  } catch {
    /* unknown free space */
  }
  return null;
}

async function finish(result: Promise<File | null>, manifest: LocalModelManifest): Promise<void> {
  try {
    const file = await result;
    if (!file) return; // paused
    const final = modelFile(manifest);
    if (final.exists) final.delete();
    await file.move(final);
    await setLocalModelState({
      readyFile: manifest.mobile.file,
      paused: false,
      status: 'ready',
      error: null,
      bytesWritten: final.size,
      totalBytes: final.size,
    });
    task = null;
    set({ status: 'ready', error: null, bytesWritten: final.size, totalBytes: final.size });
    log('info', 'system', 'local model: downloaded');
  } catch (e) {
    if (useLocalModel.getState().status !== 'downloading') return; // cancelled on purpose
    task = null;
    await setLocalModelState({ status: 'error', error: String(e) });
    log('warn', 'system', `local model: download failed: ${String(e)}`);
    set({ status: 'error', error: 'The download stopped. Check your connection and try again.' });
  }
}

/**
 * Start (or continue) the download. Resolves once it has started — or with
 * the reason it didn't, so the UI can ask (e.g. "use mobile data?").
 */
export async function startModelDownload({
  allowCellular = false,
}: { allowCellular?: boolean } = {}): Promise<DownloadBlock | null> {
  const state = useLocalModel.getState();
  if (state.status === 'downloading' || state.status === 'ready') return null;
  const manifest = state.manifest;
  const block = await precheck(manifest, allowCellular);
  if (block) return block;

  modelDir().create({ intermediates: true, idempotent: true });
  set({ status: 'downloading', error: null });
  await setLocalModelState({ status: 'downloading', error: null });

  if (task && task.state === 'paused') {
    void finish(task.resumeAsync(), manifest);
    return null;
  }
  const saved = await getLocalModelState();
  const paused = saved?.paused ?? false;
  if (paused) {
    // We no longer persist the Expo DownloadTask savable JSON in SQLite, so a
    // paused download cannot be resumed mid-stream. Delete any partial file
    // and start fresh on the next launch.
    try {
      const part = partFile(manifest);
      if (part.exists) part.delete();
    } catch {
      /* already gone */
    }
    await setLocalModelState({ paused: false, status: 'absent', bytesWritten: 0 });
    set({ status: 'absent', bytesWritten: 0 });
  }
  const part = partFile(manifest);
  if (part.exists) part.delete();
  set({ bytesWritten: 0, totalBytes: manifest.mobile.approxBytes });
  await setLocalModelState({ bytesWritten: 0, totalBytes: manifest.mobile.approxBytes });
  task = File.createDownloadTask(manifest.mobile.url, part, { onProgress, sessionType: 'background' });
  void finish(task.downloadAsync(), manifest);
  return null;
}

export async function pauseModelDownload(): Promise<void> {
  if (!task || task.state !== 'active') return;
  try {
    await task.pauseAsync();
    await setLocalModelState({ paused: true, status: 'paused' });
    set({ status: 'paused' });
  } catch (e) {
    log('warn', 'system', `local model: pause failed: ${String(e)}`);
  }
}

export async function cancelModelDownload(): Promise<void> {
  const current = task;
  task = null;
  set({ status: 'absent', bytesWritten: 0, error: null });
  await setLocalModelState({ status: 'absent', bytesWritten: 0, error: null });
  current?.cancel();
  try {
    const part = partFile(useLocalModel.getState().manifest);
    if (part.exists) part.delete();
  } catch {
    /* already gone */
  }
}

export async function deleteLocalModel(): Promise<void> {
  await releaseLlama();
  await cancelModelDownload();
  await clearLocalModelState();
  try {
    const dir = modelDir();
    if (dir.exists) dir.delete();
  } catch (e) {
    log('warn', 'system', `local model: delete failed: ${String(e)}`);
  }
  set({ status: 'absent', bytesWritten: 0 });
}
