# Disaster recovery: restoring from a backup

The gateway exports a full SurrealQL dump to the `BACKUP_S3_*` bucket daily
(`crates/gateway/src/backup.rs`), retaining the newest 14 objects under the
`surreal/` prefix. This is the last line of defense if a server deployment's
database volume is lost. Backups are off unless `BACKUP_S3_*` is set; a desktop
install keeps its database in `~/.consistent-world/db` and is backed up with the
rest of the home folder. On-demand backups: `POST /api/admin/backup` (needs `GATEWAY_AUTH_TOKEN`).

This procedure has been run once against production, restoring into a
throwaway container and comparing row counts against the live database —
they matched exactly.

## 1. Pick a backup object

List objects (any S3-compatible client works; the bucket uses SigV4 auth,
virtual-host addressing `https://{bucket}.{endpoint_host}/{key}`). Keys sort
lexicographically by their embedded timestamp, so the last one is newest:
`surreal/main-{YYYYMMDDTHHMMSSZ}.surql`.

Credentials: `BACKUP_S3_ACCESS_KEY_ID` / `BACKUP_S3_SECRET_ACCESS_KEY` /
`BACKUP_S3_ENDPOINT` / `BACKUP_S3_BUCKET` / `BACKUP_S3_REGION` (the gateway
service's environment).

## 2. Download it

GET the object with the same SigV4 scheme (see `s3_request` in `backup.rs`
for the exact signing steps — host/x-amz-content-sha256/x-amz-date signed
headers, no query-string signing needed for a plain GET).

## 3. Restore into a target SurrealDB

Start a SurrealDB instance matching the pinned version
(`surrealdb/surrealdb:v2.3.10` — must match `surrealdb` in `Cargo.toml`):

```bash
docker run -d --name restore --user root --pass root -p 18001:8000 \
  surrealdb/surrealdb:v2.3.10 start --user root --pass root memory
```

Import via the HTTP `/import` endpoint — **do not use `surreal import` (the
CLI subcommand)**: it does not honor the `OPTION IMPORT;` pragma at the top
of the export and fails with `table 'X' already exists` on graph/edge
tables. The REST endpoint honors it correctly. The endpoint also 415s
without an explicit `Accept: application/json` header:

```bash
curl -X POST http://localhost:18001/import \
  -u root:root \
  -H "Surreal-NS: consistent-world" -H "Surreal-DB: main" \
  -H "Content-Type: application/octet-stream" \
  -H "Accept: application/json" \
  --data-binary @main-<timestamp>.surql
```

## 4. Verify before cutting over

Compare row counts against the source (or your own knowledge of expected
data) for the tables that matter most: `control_tasks`, `control_snapshots`,
`director_lessons`, `entities`. Only once counts (and ideally a content
spot-check) match should this replace a live volume — e.g. by pointing
`SURREAL_URL` at the restored instance, or by restoring into the existing
volume-backed service after stopping it.
