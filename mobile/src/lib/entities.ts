import type { Entity } from '@/api/types';
import { http } from '@/api/client';
import { log } from '@/lib/logger';
import {
  clearEntityOutbox,
  enqueueEntityOutbox,
  getAllEntityOutbox,
  getAllEntities,
  kvGet,
  kvSet,
  replaceAllEntities,
  upsertEntity,
  deleteEntityDb,
  DbEntity,
} from './db';

/**
 * Offline-first entities on the phone.
 *
 * Every entity the user creates on this phone lands in SQLite first —
 * the app works without a desktop. Changes are queued in an outbox and
 * replayed to the gateway once it is reachable (pairing, or the computer
 * comes back online).
 *
 * Locally-created entities carry `local:` ids until the gateway assigns real
 * ones. They live alongside gateway-synced entities in the same cache, so
 * reminders can point at them by `entityId` immediately.
 *
 * Storage: SQLite (`entities`, `entity_outbox` tables) instead of
 * AsyncStorage, so the phone can query, index and transact locally.
 */

export type EntityChange =
  | { type: 'create'; clientId: string; input: CreateEntityInput; queuedAt: number }
  | { type: 'update'; id: string; input: Partial<Entity>; queuedAt: number }
  | { type: 'delete'; id: string; queuedAt: number };

export interface CreateEntityInput {
  name: string;
  type: 'person' | 'company' | 'project' | 'mission' | 'agent' | 'file' | 'memory';
  summary?: string;
  stage?: string;
  weight?: number;
  colour?: string;
  relatedIds?: string[];
  pinned?: boolean;
  clientId: string;
}

interface EntitiesState {
  items: Entity[];
  /** False while demo fixtures stand in for a computer. */
  live: boolean;
  loaded: boolean;
  syncing: boolean;
  /** The last sync couldn't reach the gateway; the list is what the phone knows. */
  offline: boolean;
  /** Changes waiting to reach the gateway. */
  pending: number;
  lastSyncAt: number;
}

export const entitiesState = {
  items: [] as Entity[],
  live: false,
  loaded: false,
  syncing: false,
  offline: false,
  pending: 0,
  lastSyncAt: 0,
};

// ── Storage (SQLite) ──────────────────────────────────────────────────

async function readCache(): Promise<{ items: Entity[]; live: boolean }> {
  const rows = await getAllEntities();
  const liveRaw = await kvGet('cw.entities.live');
  return { items: rows.map(dbToEntity), live: liveRaw === '1' };
}

async function writeCache(items: Entity[], live: boolean): Promise<void> {
  await replaceAllEntities(items.map((e) => entityToDb(e, live)), live);
  await kvSet('cw.entities.live', live ? '1' : '0');
}

async function readOutbox(): Promise<EntityChange[]> {
  const rows = await getAllEntityOutbox();
  const entries: EntityChange[] = [];
  for (const row of rows) {
    const input = JSON.parse(row.input) as Record<string, unknown>;
    if (row.type === 'create') {
      entries.push({
        type: 'create',
        clientId: row.clientId!,
        input: input as unknown as CreateEntityInput,
        queuedAt: row.queuedAt,
      });
    } else if (row.type === 'update') {
      entries.push({
        type: 'update',
        id: String(row.id),
        input: input as unknown as Partial<Entity>,
        queuedAt: row.queuedAt,
      });
    } else {
      entries.push({
        type: 'delete',
        id: String(row.id),
        queuedAt: row.queuedAt,
      });
    }
  }
  return entries;
}

async function writeOutbox(box: EntityChange[]): Promise<void> {
  await clearEntityOutbox();
  for (const entry of box) {
    await enqueueEntityOutbox({
      type: entry.type,
      clientId: entry.type === 'create' ? entry.clientId : null,
      input: entry.type === 'delete' ? '{}' : JSON.stringify(entry.input),
      queuedAt: entry.queuedAt,
    });
  }
  entitiesState.pending = (await getAllEntityOutbox()).length;
}

async function enqueue(entry: EntityChange): Promise<void> {
  await enqueueEntityOutbox({
    type: entry.type,
    clientId: entry.type === 'create' ? entry.clientId : null,
    input: entry.type === 'delete' ? '{}' : JSON.stringify(entry.input),
    queuedAt: entry.queuedAt,
  });
  entitiesState.pending = (await getAllEntityOutbox()).length;
}

export function newClientId(): string {
  return `m-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

export function isLocalEntityId(id: string): boolean {
  return id.startsWith('local:');
}

function entityToDb(e: Entity, live: boolean): DbEntity {
  return {
    id: e.id,
    name: e.name,
    type: e.type,
    summary: e.summary,
    stage: e.stage ?? null,
    weight: e.weight,
    pinned: e.pinned,
    colour: e.colour ?? null,
    fileCount: e.fileCount ?? 0,
    personal: e.personal ?? false,
    relatedIds: e.relatedIds,
    stats: e.stats,
    lastActiveAt: e.lastActiveAt ?? null,
    isLocal: isLocalEntityId(e.id),
    live,
  };
}

function dbToEntity(r: DbEntity): Entity {
  return {
    id: r.id,
    name: r.name,
    type: r.type as Entity['type'],
    summary: r.summary,
    stage: r.stage ?? undefined,
    weight: r.weight,
    pinned: r.pinned,
    colour: r.colour ?? undefined,
    fileCount: r.fileCount,
    personal: r.personal,
    relatedIds: r.relatedIds,
    stats: r.stats,
    lastActiveAt: r.lastActiveAt ?? undefined,
  };
}

function localItemFor(clientId: string, input: Omit<CreateEntityInput, 'clientId'>): Entity {
  return {
    id: `local:${clientId}`,
    name: input.name,
    type: input.type,
    summary: input.summary ?? '',
    stage: input.stage,
    weight: input.weight ?? 0.5,
    pinned: input.pinned ?? false,
    colour: input.colour,
    fileCount: 0,
    personal: false,
    relatedIds: input.relatedIds ?? [],
    stats: { files: 0, notes: 0, missions: 0, memories: 0 },
  };
}

// ── Sync ─────────────────────────────────────────────────────────────────

function isUnreachable(e: unknown): boolean {
  return !(e instanceof Error) || true; // http throws APIError; check below
}

/** Replay queued changes to the gateway. False when still unreachable. */
async function replayOutbox(): Promise<boolean> {
  const remaining = await readOutbox();
  if (remaining.length === 0) return true;
  while (remaining.length > 0) {
    const entry = remaining[0];
    try {
      if (entry.type === 'create') {
        await http.post<Entity>('/api/entities', { ...entry.input, clientId: entry.clientId });
      } else if (entry.type === 'update') {
        await http.post<Entity>(`/api/entities/${encodeURIComponent(entry.id)}`, entry.input);
      } else {
        await http.del(`/api/entities/${encodeURIComponent(entry.id)}`);
      }
      remaining.shift();
      await writeOutbox(remaining);
    } catch (e) {
      const apiErr = e as { status?: number };
      if (apiErr.status === 401 || apiErr.status === 503) {
        await writeOutbox(remaining);
        return false;
      }
      log('warn', 'system', `entities: gateway refused queued ${entry.type}: ${String(e)}`);
      remaining.shift();
      await writeOutbox(remaining);
    }
  }
  await clearEntityOutbox();
  return true;
}

/**
 * Pull gateway entities, replay the local outbox, and merge into the local
 * cache. Local-only entities (no gateway match) are kept — nothing is deleted
 * silently. Returns the merged list.
 */
export async function syncEntities(): Promise<Entity[]> {
  entitiesState.syncing = true;
  try {
    const reached = await replayOutbox();
    let gatewayItems: Entity[] = [];
    if (reached) {
      try {
        gatewayItems = await http.list<Entity>('/api/entities?limit=200');
      } catch {
        gatewayItems = [];
      }
    }
    const cache = await readCache();
    const outbox = await readOutbox();
    const local = outbox
      .filter((e) => e.type === 'create')
      .map((e) => localItemFor(e.clientId, e.input));

    // Merge: gateway entities win on id. Local entities keep their `local:`
    // ids and coexist until a merge screen resolves duplicates.
    const byId = new Map<string, Entity>();
    for (const g of gatewayItems) byId.set(g.id, g);
    for (const l of local) {
      if (!byId.has(l.id)) byId.set(l.id, l);
    }
    // Also fold in any previously-cached entities not in either list.
    for (const c of cache.items) {
      if (!byId.has(c.id)) byId.set(c.id, c);
    }

    const items = Array.from(byId.values());
    await writeCache(items, reached);
    entitiesState.items = items;
    entitiesState.live = reached;
    entitiesState.loaded = true;
    entitiesState.offline = !reached;
    entitiesState.lastSyncAt = Date.now();
    entitiesState.pending = (await readOutbox()).length;
    return items;
  } catch (e) {
    log('warn', 'system', `entities: sync failed: ${String(e)}`);
    return entitiesState.items;
  } finally {
    entitiesState.syncing = false;
  }
}

// ── Creating / updating / deleting ───────────────────────────────────

/**
 * Create an entity on the phone right now, without the gateway. Queued and
 * POSTed (idempotent by `clientId`) as soon as the gateway answers.
 */
export async function createEntityLocal(input: Omit<CreateEntityInput, 'clientId'>): Promise<Entity> {
  const clientId = newClientId();
  const full: CreateEntityInput = { ...input, clientId };
  const item = localItemFor(clientId, input);
  await enqueue({ type: 'create', clientId, input: full, queuedAt: Date.now() });
  await upsertLocal(item);
  void syncEntities();
  return item;
}

/**
 * Update an entity on the phone right now. Queued for the gateway.
 * Local-only entities are updated in place; synced entities keep their id.
 */
export async function updateEntityLocal(id: string, input: Partial<Entity>): Promise<void> {
  const cache = await readCache();
  const current = cache.items.find((r) => r.id === id);
  if (!current) return;
  const next = { ...current, ...input };
  await enqueue({ type: 'update', id, input: next, queuedAt: Date.now() });
  await upsertLocal(next);
  void syncEntities();
}

/**
 * Delete an entity on the phone right now. Queued for the gateway.
 * Local-only entities are removed immediately; synced entities are queued.
 */
export async function deleteEntityLocal(id: string): Promise<void> {
  await enqueue({ type: 'delete', id, queuedAt: Date.now() });
  await removeLocal(id);
  void syncEntities();
}

/** Persist an entity to the local cache (upsert). */
async function upsertLocal(item: Entity): Promise<void> {
  const cache = await readCache();
  const rest = cache.items.filter((r) => r.id !== item.id);
  await writeCache([item, ...rest], cache.live);
  entitiesState.items = [item, ...rest];
}

/** Remove an entity from the local cache. */
async function removeLocal(id: string): Promise<void> {
  const cache = await readCache();
  const rest = cache.items.filter((r) => r.id !== id);
  await writeCache(rest, cache.live);
  entitiesState.items = rest;
}

/** Mark the cache as loaded from storage (call on app start). */
export async function hydrateEntities(): Promise<void> {
  const cache = await readCache();
  entitiesState.items = cache.items;
  entitiesState.live = cache.live;
  entitiesState.loaded = true;
  const box = await readOutbox();
  entitiesState.pending = box.length;
}

/** Read-only snapshot of the current local entity list. */
export function getEntitiesLocal(): Entity[] {
  return [...entitiesState.items];
}

/** Look up one entity by id from the local cache (incl. `local:` ids). */
export function getEntityLocal(id: string): Entity | undefined {
  return entitiesState.items.find((e) => e.id === id);
}

/** After pairing, the gateway may assign real ids to `local:` entities.
 * Remap the local id → gateway id across cache and outbox. */
export async function markEntitySynced(localId: string, gatewayId: string): Promise<void> {
  if (!isLocalEntityId(localId)) return;
  const cache = await readCache();
  const items = cache.items.map((e) => (e.id === localId ? { ...e, id: gatewayId } : e));
  await writeCache(items, cache.live);
  entitiesState.items = items;

  const box = await readOutbox();
  const remapped = box.map((e) => {
    if (e.type === 'update' && e.id === localId) return { ...e, id: gatewayId };
    if (e.type === 'delete' && e.id === localId) return { ...e, id: gatewayId };
    return e;
  });
  await writeOutbox(remapped);
  entitiesState.pending = remapped.length;
}

/** Number of changes waiting to reach the gateway. */
export function getPendingCount(): number {
  return entitiesState.pending;
}

/** Whether the last sync couldn't reach the gateway. */
export function isEntitiesOffline(): boolean {
  return entitiesState.offline;
}

/** Whether entities are live (synced with a real gateway). */
export function isEntitiesLive(): boolean {
  return entitiesState.live;
}

/** Clear all local entities and outbox (useful for testing / reset). */
export async function clearEntitiesLocal(): Promise<void> {
  await writeCache([], false);
  await clearEntityOutbox();
  entitiesState.items = [];
  entitiesState.live = false;
  entitiesState.pending = 0;
  entitiesState.offline = false;
  entitiesState.loaded = true;
}
