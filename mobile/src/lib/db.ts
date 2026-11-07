import { SQLiteDatabase } from 'expo-sqlite';

/**
 * Single SQLite connection for the whole app. All local data — reminders,
 * entities, outbox entries, key-value state — lives here instead of
 * AsyncStorage so the phone can query, index and transact without the gateway.
 */

let db: SQLiteDatabase | null = null;

export async function getDb(): Promise<SQLiteDatabase> {
  if (db) return db;
  const { openDatabaseSync } = require('expo-sqlite');
  const database = openDatabaseSync('consistent-world.db');
  await migrate(database);
  db = database;
  return database;
}

export async function closeDb(): Promise<void> {
  if (db) {
    await db.closeAsync();
    db = null;
  }
}

/** Idempotent schema: create tables once, add columns safely. */
async function migrate(database: SQLiteDatabase): Promise<void> {
  await database.execAsync(`
    PRAGMA journal_mode = WAL;

    CREATE TABLE IF NOT EXISTS kv (
      key   TEXT PRIMARY KEY,
      value TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS reminders (
      id                 TEXT PRIMARY KEY,
      title              TEXT NOT NULL,
      what               TEXT NOT NULL,
      kind               TEXT NOT NULL DEFAULT 'reminder',
      entity_id          TEXT,
      entity_name        TEXT,
      state              TEXT NOT NULL DEFAULT 'scheduled',
      fire_at            TEXT,
      follow_ups         TEXT NOT NULL DEFAULT '[]',
      first_fired_at     TEXT,
      nudges             INTEGER NOT NULL DEFAULT 0,
      created_at         TEXT NOT NULL,
      delegated_task_id  TEXT,
      delegated_approval_id TEXT,
      client_id          TEXT,
      is_local           INTEGER NOT NULL DEFAULT 0,
      live               INTEGER NOT NULL DEFAULT 0
    );

    CREATE TABLE IF NOT EXISTS reminder_outbox (
      id        INTEGER PRIMARY KEY AUTOINCREMENT,
      type      TEXT NOT NULL CHECK (type IN ('create','action')),
      client_id TEXT,
      input     TEXT NOT NULL,
      queued_at INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS entities (
      id          TEXT PRIMARY KEY,
      name        TEXT NOT NULL,
      type        TEXT NOT NULL,
      summary     TEXT NOT NULL DEFAULT '',
      stage       TEXT,
      weight      REAL NOT NULL DEFAULT 0.5,
      pinned      INTEGER NOT NULL DEFAULT 0,
      colour      TEXT,
      file_count  INTEGER NOT NULL DEFAULT 0,
      personal    INTEGER NOT NULL DEFAULT 0,
      related_ids TEXT NOT NULL DEFAULT '[]',
      stats       TEXT NOT NULL DEFAULT '{}',
      last_active_at TEXT,
      is_local    INTEGER NOT NULL DEFAULT 0,
      live        INTEGER NOT NULL DEFAULT 0
    );

    CREATE TABLE IF NOT EXISTS entity_outbox (
      id        INTEGER PRIMARY KEY AUTOINCREMENT,
      type      TEXT NOT NULL CHECK (type IN ('create','update','delete')),
      client_id TEXT,
      input     TEXT NOT NULL,
      queued_at INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS local_model_state (
      key             TEXT PRIMARY KEY,
      ready_file      TEXT,
      paused          INTEGER NOT NULL DEFAULT 0,
      bytes_written   INTEGER NOT NULL DEFAULT 0,
      total_bytes     INTEGER NOT NULL DEFAULT 0,
      status          TEXT NOT NULL DEFAULT 'absent',
      error           TEXT,
      ram_gb          REAL,
      recommended     INTEGER NOT NULL DEFAULT 1,
      hydrated        INTEGER NOT NULL DEFAULT 0,
      runtime_available INTEGER NOT NULL DEFAULT 0
    );

    CREATE INDEX IF NOT EXISTS idx_reminders_state ON reminders(state);
    CREATE INDEX IF NOT EXISTS idx_reminders_fire_at ON reminders(fire_at);
    CREATE INDEX IF NOT EXISTS idx_reminders_is_local ON reminders(is_local);
    CREATE INDEX IF NOT EXISTS idx_entities_type ON entities(type);
    CREATE INDEX IF NOT EXISTS idx_entities_is_local ON entities(is_local);
  `);
}

/** ── Raw row types (snake_case, matching SQLite columns) ── */

interface DbReminderRow {
  id: string;
  title: string;
  what: string;
  kind: string;
  entity_id: string | null;
  entity_name: string | null;
  state: string;
  fire_at: string | null;
  follow_ups: string;
  first_fired_at: string | null;
  nudges: number;
  created_at: string;
  delegated_task_id: string | null;
  delegated_approval_id: string | null;
  client_id: string | null;
  is_local: number;
  live: number;
}

interface DbEntityRow {
  id: string;
  name: string;
  type: string;
  summary: string;
  stage: string | null;
  weight: number;
  pinned: number;
  colour: string | null;
  file_count: number;
  personal: number;
  related_ids: string;
  stats: string;
  last_active_at: string | null;
  is_local: number;
  live: number;
}

interface DbReminderOutboxRow {
  id: number;
  type: string;
  client_id: string | null;
  input: string;
  queued_at: number;
}

interface DbEntityOutboxRow {
  id: number;
  type: string;
  client_id: string | null;
  input: string;
  queued_at: number;
}

interface DbLocalModelRow {
  key: string;
  ready_file: string | null;
  paused: number;
  bytes_written: number;
  total_bytes: number;
  status: string;
  error: string | null;
  ram_gb: number | null;
  recommended: number;
  hydrated: number;
  runtime_available: number;
}

/** ── Domain types ── */

export interface DbReminder {
  id: string;
  title: string;
  what: string;
  kind: string;
  entityId: string | null;
  entityName: string | null;
  state: string;
  fireAt: string | null;
  followUps: string[];
  firstFiredAt: string | null;
  nudges: number;
  createdAt: string;
  delegatedTaskId: string | null;
  delegatedApprovalId: string | null;
  clientId: string | null;
  isLocal: boolean;
  live: boolean;
}

export interface DbEntity {
  id: string;
  name: string;
  type: string;
  summary: string;
  stage: string | null;
  weight: number;
  pinned: boolean;
  colour: string | null;
  fileCount: number;
  personal: boolean;
  relatedIds: string[];
  stats: { files: number; notes: number; missions: number; memories: number };
  lastActiveAt: string | null;
  isLocal: boolean;
  live: boolean;
}

export interface DbOutboxEntry {
  id: number;
  type: 'create' | 'action';
  clientId: string | null;
  input: string;
  queuedAt: number;
}

export interface DbEntityOutboxEntry {
  id: number;
  type: 'create' | 'update' | 'delete';
  clientId: string | null;
  input: string;
  queuedAt: number;
}

export interface DbLocalModelState {
  readyFile: string | null;
  paused: boolean;
  bytesWritten: number;
  totalBytes: number;
  status: string;
  error: string | null;
  ramGb: number | null;
  recommended: boolean;
  hydrated: boolean;
  runtimeAvailable: boolean;
}

/** ── Key-value store (replaces AsyncStorage for small state) ── */

export async function kvGet(key: string): Promise<string | null> {
  const database = await getDb();
  const result = await database.getAllAsync<{ value: string }>(
    'SELECT value FROM kv WHERE key = ?',
    [key],
  );
  return result.length > 0 ? result[0].value : null;
}

export async function kvSet(key: string, value: string): Promise<void> {
  const database = await getDb();
  await database.runAsync(
    'INSERT INTO kv(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value',
    [key, value],
  );
}

export async function kvRemove(key: string): Promise<void> {
  const database = await getDb();
  await database.runAsync('DELETE FROM kv WHERE key = ?', [key]);
}

/** ── Reminders ── */

export async function getAllReminders(): Promise<DbReminder[]> {
  const database = await getDb();
  const rows = await database.getAllAsync<DbReminderRow>(
    'SELECT * FROM reminders ORDER BY created_at DESC',
  );
  return rows.map((r) => ({
    id: r.id,
    title: r.title,
    what: r.what,
    kind: r.kind,
    entityId: r.entity_id,
    entityName: r.entity_name,
    state: r.state,
    fireAt: r.fire_at,
    followUps: JSON.parse(r.follow_ups ?? '[]'),
    firstFiredAt: r.first_fired_at,
    nudges: r.nudges,
    createdAt: r.created_at,
    delegatedTaskId: r.delegated_task_id,
    delegatedApprovalId: r.delegated_approval_id,
    clientId: r.client_id,
    isLocal: r.is_local === 1,
    live: r.live === 1,
  }));
}

export async function upsertReminder(r: DbReminder): Promise<void> {
  const database = await getDb();
  await database.runAsync(
    `INSERT INTO reminders(
      id, title, what, kind, entity_id, entity_name, state, fire_at,
      follow_ups, first_fired_at, nudges, created_at,
      delegated_task_id, delegated_approval_id, client_id, is_local, live
    ) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
     ON CONFLICT(id) DO UPDATE SET
      title=excluded.title, what=excluded.what, kind=excluded.kind,
      entity_id=excluded.entity_id, entity_name=excluded.entity_name,
      state=excluded.state, fire_at=excluded.fire_at,
      follow_ups=excluded.follow_ups, first_fired_at=excluded.first_fired_at,
      nudges=excluded.nudges, created_at=excluded.created_at,
      delegated_task_id=excluded.delegated_task_id,
      delegated_approval_id=excluded.delegated_approval_id,
      client_id=excluded.client_id, is_local=excluded.is_local, live=excluded.live`,
    [
      r.id, r.title, r.what, r.kind, r.entityId, r.entityName, r.state,
      r.fireAt, JSON.stringify(r.followUps), r.firstFiredAt, r.nudges,
      r.createdAt, r.delegatedTaskId, r.delegatedApprovalId, r.clientId,
      r.isLocal ? 1 : 0, r.live ? 1 : 0,
    ],
  );
}

export async function deleteReminder(id: string): Promise<void> {
  const database = await getDb();
  await database.runAsync('DELETE FROM reminders WHERE id = ?', [id]);
}

export async function replaceAllReminders(items: DbReminder[], live: boolean): Promise<void> {
  const database = await getDb();
  await database.execAsync('BEGIN IMMEDIATE');
  try {
    await database.runAsync('DELETE FROM reminders');
    for (const r of items) {
      await upsertReminder({ ...r, live });
    }
    await database.execAsync('COMMIT');
  } catch {
    await database.execAsync('ROLLBACK');
    throw new Error('Failed to replace reminders');
  }
}

/** ── Reminder outbox ── */

export async function getAllOutbox(): Promise<DbOutboxEntry[]> {
  const database = await getDb();
  const rows = await database.getAllAsync<DbReminderOutboxRow>(
    'SELECT * FROM reminder_outbox ORDER BY queued_at ASC',
  );
  return rows.map((r) => ({
    id: r.id,
    type: r.type as 'create' | 'action',
    clientId: r.client_id,
    input: r.input,
    queuedAt: r.queued_at,
  }));
}

export async function enqueueOutbox(entry: Omit<DbOutboxEntry, 'id'>): Promise<void> {
  const database = await getDb();
  await database.runAsync(
    'INSERT INTO reminder_outbox(type, client_id, input, queued_at) VALUES(?,?,?,?)',
    [entry.type, entry.clientId ?? null, entry.input, entry.queuedAt],
  );
}

export async function removeOutboxEntry(id: number): Promise<void> {
  const database = await getDb();
  await database.runAsync('DELETE FROM reminder_outbox WHERE id = ?', [id]);
}

export async function clearOutbox(): Promise<void> {
  const database = await getDb();
  await database.runAsync('DELETE FROM reminder_outbox');
}

/** ── Entities ── */

export async function getAllEntities(): Promise<DbEntity[]> {
  const database = await getDb();
  const rows = await database.getAllAsync<DbEntityRow>(
    'SELECT * FROM entities ORDER BY name ASC',
  );
  return rows.map((r) => ({
    id: r.id,
    name: r.name,
    type: r.type,
    summary: r.summary,
    stage: r.stage,
    weight: r.weight,
    pinned: r.pinned === 1,
    colour: r.colour,
    fileCount: r.file_count,
    personal: r.personal === 1,
    relatedIds: JSON.parse(r.related_ids ?? '[]'),
    stats: JSON.parse(r.stats ?? '{}'),
    lastActiveAt: r.last_active_at,
    isLocal: r.is_local === 1,
    live: r.live === 1,
  }));
}

export async function upsertEntity(e: DbEntity): Promise<void> {
  const database = await getDb();
  await database.runAsync(
    `INSERT INTO entities(
      id, name, type, summary, stage, weight, pinned, colour,
      file_count, personal, related_ids, stats, last_active_at, is_local, live
    ) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
     ON CONFLICT(id) DO UPDATE SET
      name=excluded.name, type=excluded.type, summary=excluded.summary,
      stage=excluded.stage, weight=excluded.weight, pinned=excluded.pinned,
      colour=excluded.colour, file_count=excluded.file_count,
      personal=excluded.personal, related_ids=excluded.related_ids,
      stats=excluded.stats, last_active_at=excluded.last_active_at,
      is_local=excluded.is_local, live=excluded.live`,
    [
      e.id, e.name, e.type, e.summary, e.stage, e.weight, e.pinned ? 1 : 0,
      e.colour, e.fileCount, e.personal ? 1 : 0, JSON.stringify(e.relatedIds),
      JSON.stringify(e.stats), e.lastActiveAt, e.isLocal ? 1 : 0, e.live ? 1 : 0,
    ],
  );
}

export async function deleteEntityDb(id: string): Promise<void> {
  const database = await getDb();
  await database.runAsync('DELETE FROM entities WHERE id = ?', [id]);
}

export async function replaceAllEntities(items: DbEntity[], live: boolean): Promise<void> {
  const database = await getDb();
  await database.execAsync('BEGIN IMMEDIATE');
  try {
    await database.runAsync('DELETE FROM entities');
    for (const e of items) {
      await upsertEntity({ ...e, live });
    }
    await database.execAsync('COMMIT');
  } catch {
    await database.execAsync('ROLLBACK');
    throw new Error('Failed to replace entities');
  }
}

/** ── Entity outbox ── */

export async function getAllEntityOutbox(): Promise<DbEntityOutboxEntry[]> {
  const database = await getDb();
  const rows = await database.getAllAsync<DbEntityOutboxRow>(
    'SELECT * FROM entity_outbox ORDER BY queued_at ASC',
  );
  return rows.map((r) => ({
    id: r.id,
    type: r.type as 'create' | 'update' | 'delete',
    clientId: r.client_id,
    input: r.input,
    queuedAt: r.queued_at,
  }));
}

export async function enqueueEntityOutbox(entry: Omit<DbEntityOutboxEntry, 'id'>): Promise<void> {
  const database = await getDb();
  await database.runAsync(
    'INSERT INTO entity_outbox(type, client_id, input, queued_at) VALUES(?,?,?,?)',
    [entry.type, entry.clientId ?? null, entry.input, entry.queuedAt],
  );
}

export async function removeEntityOutboxEntry(id: number): Promise<void> {
  const database = await getDb();
  await database.runAsync('DELETE FROM entity_outbox WHERE id = ?', [id]);
}

export async function clearEntityOutbox(): Promise<void> {
  const database = await getDb();
  await database.runAsync('DELETE FROM entity_outbox');
}

/** ── Local model state ── */

export async function getLocalModelState(): Promise<DbLocalModelState | null> {
  const database = await getDb();
  const rows = await database.getAllAsync<DbLocalModelRow>(
    'SELECT * FROM local_model_state LIMIT 1',
  );
  if (rows.length === 0) return null;
  const r = rows[0];
  return {
    readyFile: r.ready_file,
    paused: r.paused === 1,
    bytesWritten: r.bytes_written,
    totalBytes: r.total_bytes,
    status: r.status,
    error: r.error,
    ramGb: r.ram_gb,
    recommended: r.recommended === 1,
    hydrated: r.hydrated === 1,
    runtimeAvailable: r.runtime_available === 1,
  };
}

export async function setLocalModelState(partial: Partial<DbLocalModelState>): Promise<void> {
  const database = await getDb();
  const current = await getLocalModelState();
  const row: DbLocalModelState = {
    readyFile: partial.readyFile ?? current?.readyFile ?? null,
    paused: partial.paused ?? current?.paused ?? false,
    bytesWritten: partial.bytesWritten ?? current?.bytesWritten ?? 0,
    totalBytes: partial.totalBytes ?? current?.totalBytes ?? 0,
    status: partial.status ?? current?.status ?? 'absent',
    error: partial.error ?? current?.error ?? null,
    ramGb: partial.ramGb ?? current?.ramGb ?? null,
    recommended: partial.recommended ?? current?.recommended ?? true,
    hydrated: partial.hydrated ?? current?.hydrated ?? false,
    runtimeAvailable: partial.runtimeAvailable ?? current?.runtimeAvailable ?? false,
  };
  await database.runAsync(
    `INSERT INTO local_model_state(
      key, ready_file, paused, bytes_written, total_bytes, status, error,
      ram_gb, recommended, hydrated, runtime_available
    ) VALUES('singleton',?,?,?,?,?,?,?,?,?,?)
     ON CONFLICT(key) DO UPDATE SET
      ready_file=excluded.ready_file, paused=excluded.paused,
      bytes_written=excluded.bytes_written, total_bytes=excluded.total_bytes,
      status=excluded.status, error=excluded.error,
      ram_gb=excluded.ram_gb, recommended=excluded.recommended,
      hydrated=excluded.hydrated, runtime_available=excluded.runtime_available`,
    [
      row.readyFile, row.paused ? 1 : 0, row.bytesWritten, row.totalBytes,
      row.status, row.error, row.ramGb, row.recommended ? 1 : 0,
      row.hydrated ? 1 : 0, row.runtimeAvailable ? 1 : 0,
    ],
  );
}

export async function clearLocalModelState(): Promise<void> {
  const database = await getDb();
  await database.runAsync('DELETE FROM local_model_state');
}
