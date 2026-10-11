//! Surreal-backed registry of the operator's devices (`devices` table).
//!
//! A device is one place the companion can reach the CEO: the phone, the
//! desktop shell, a browser tab. Registration is idempotent per
//! `(workspace_id, platform, install_id)` so reopening the app doesn't
//! accumulate rows.
//!
//! Two fields carry the delivery contract:
//!
//! - `push_token` — an Expo push token (`ExponentPushToken[...]`). Optional:
//!   a device that only foreground-polls registers without one, which is the
//!   default posture (see `[notifications] mobile_push` in config).
//! - `last_seen_feed_event_id` — per-device delivery cursor. The dispatcher
//!   only pushes events newer than this, so a device that was offline for a
//!   day gets the tail rather than a replay storm.
//!
//! ### SurrealDB v2 record-id handling
//!
//! Same round-trip dance as [`crate::director_store`] / [`crate::entity_docs_store`]:
//! Surreal stores the row id as a `RecordId`, so reads go through a
//! `DeviceRow` (RecordId + flattened rest) and writes through a
//! `DeviceUpsertBody` that intentionally omits `id`. The
//! `device_field_completeness_guard` below pins the projection so a new
//! `Device` field can't be silently dropped from persistence.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

/// A registered delivery target for companion notifications.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    pub workspace_id: String,
    /// `"ios"` | `"android"` | `"desktop"` | `"web"`.
    pub platform: String,
    /// Stable per-install identifier from the client; the idempotency key.
    pub install_id: String,
    /// Optional client-supplied display name for this device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Expo push token. `None` = poll-only device (no third party involved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_token: Option<String>,
    /// Cleared when Expo reports `DeviceNotRegistered`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Delivery cursor: the last feed event this device was notified about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_feed_event_id: Option<String>,
    pub created_at: String,
    pub last_seen_at: String,
}

fn default_true() -> bool {
    true
}

/// Handle onto the `devices` table.
pub struct SurrealDeviceStore {
    db: Arc<Surreal<Any>>,
}

/// Wire shape for a row read back from Surreal: the id arrives typed as a
/// `RecordId` separately from the rest of the fields (see the module header).
#[derive(Deserialize)]
struct DeviceRow {
    id: surrealdb::RecordId,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

// Surreal's `RecordId::to_string()` renders as `table:id` (backtick-quoted
// when the id needs escaping); this pulls just the id half back out.
fn record_id_key(rid: &surrealdb::RecordId) -> String {
    let s = rid.to_string();
    s.split_once(':')
        .map(|x| x.1)
        .map(|rest| rest.trim_matches('`').to_string())
        .unwrap_or(s)
}

// Re-merge the RecordId's key back into the flattened field map so it
// deserializes into `Device` as an ordinary `id: String` field.
fn row_to_device(row: DeviceRow) -> Result<Device, (String, serde_json::Error)> {
    let id = record_id_key(&row.id);
    let mut map = row.rest;
    map.insert("id".to_string(), serde_json::Value::String(id.clone()));
    serde_json::from_value::<Device>(serde_json::Value::Object(map)).map_err(|e| (id, e))
}

fn rows_to_devices(rows: Vec<DeviceRow>) -> Vec<Device> {
    rows.into_iter()
        .filter_map(|r| match row_to_device(r) {
            Ok(d) => Some(d),
            Err((rid, err)) => {
                tracing::warn!(device_id = %rid, error = %err, "SurrealDeviceStore: dropping malformed row");
                None
            }
        })
        .collect()
}

/// Write shape for an upsert: everything `Device` has except `id`, which is
/// supplied separately as the record key.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceUpsertBody {
    workspace_id: String,
    platform: String,
    install_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    push_token: Option<String>,
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_seen_feed_event_id: Option<String>,
    created_at: String,
    last_seen_at: String,
}

fn device_to_upsert_body(d: &Device) -> DeviceUpsertBody {
    DeviceUpsertBody {
        workspace_id: d.workspace_id.clone(),
        platform: d.platform.clone(),
        install_id: d.install_id.clone(),
        label: d.label.clone(),
        push_token: d.push_token.clone(),
        enabled: d.enabled,
        last_seen_feed_event_id: d.last_seen_feed_event_id.clone(),
        created_at: d.created_at.clone(),
        last_seen_at: d.last_seen_at.clone(),
    }
}

/// Not called at runtime: destructuring `Device` by name here means adding a
/// field without updating `DeviceUpsertBody`/`device_to_upsert_body` fails to
/// compile instead of silently dropping data.
#[allow(dead_code)]
fn device_field_completeness_guard(d: &Device) {
    let Device {
        id: _,
        workspace_id: _,
        platform: _,
        install_id: _,
        label: _,
        push_token: _,
        enabled: _,
        last_seen_feed_event_id: _,
        created_at: _,
        last_seen_at: _,
    } = d;
}

impl SurrealDeviceStore {
    pub fn new(db: Arc<Surreal<Any>>) -> Arc<Self> {
        Arc::new(Self { db })
    }

    /// Define the `devices` table and its lookup indexes. Safe to call
    /// repeatedly (each `DEFINE` is idempotent).
    pub async fn init_schema(&self) -> Result<()> {
        let mut res = self
            .db
            .query(
                r#"
                DEFINE TABLE devices SCHEMALESS;
                DEFINE FIELD workspaceId ON devices TYPE option<string>;
                DEFINE FIELD installId ON devices TYPE option<string>;
                DEFINE FIELD enabled ON devices TYPE option<bool>;
                DEFINE INDEX devices_workspace_idx ON devices FIELDS workspaceId;
                DEFINE INDEX devices_install_idx ON devices FIELDS workspaceId, installId;
                "#,
            )
            .await?;
        for (_idx, err) in res.take_errors() {
            tracing::warn!("SurrealDeviceStore schema warning: {}", err);
        }
        Ok(())
    }

    pub async fn upsert(&self, device: &Device) -> Result<()> {
        let body = device_to_upsert_body(device);
        let _: Option<DeviceRow> = self
            .db
            .upsert(("devices", device.id.as_str()))
            .content(body)
            .await
            .map_err(|e| anyhow::anyhow!("upsert device {}: {}", device.id, e))?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> Result<Option<Device>> {
        let row: Option<DeviceRow> = self
            .db
            .select(("devices", id))
            .await
            .map_err(|e| anyhow::anyhow!("select device {}: {}", id, e))?;
        Ok(row.and_then(|r| match row_to_device(r) {
            Ok(d) => Some(d),
            Err((rid, err)) => {
                tracing::warn!(device_id = %rid, error = %err, "SurrealDeviceStore: dropping malformed row");
                None
            }
        }))
    }

    /// Idempotency lookup: one row per `(workspace_id, install_id)`.
    pub async fn find_by_install(
        &self,
        workspace_id: &str,
        install_id: &str,
    ) -> Result<Option<Device>> {
        let wid = workspace_id.to_string();
        let iid = install_id.to_string();
        let mut res = self
            .db
            .query("SELECT * FROM devices WHERE workspaceId = $wid AND installId = $iid LIMIT 1")
            .bind(("wid", wid))
            .bind(("iid", iid))
            .await
            .map_err(|e| anyhow::anyhow!("query device by install: {}", e))?;
        let rows: Vec<DeviceRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take device by install: {}", e))?;
        Ok(rows_to_devices(rows).into_iter().next())
    }

    pub async fn list_by_workspace(&self, workspace_id: &str) -> Result<Vec<Device>> {
        let wid = workspace_id.to_string();
        let mut res = self
            .db
            .query("SELECT * FROM devices WHERE workspaceId = $wid")
            .bind(("wid", wid))
            .await
            .map_err(|e| anyhow::anyhow!("query devices by workspace: {}", e))?;
        let rows: Vec<DeviceRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take devices: {}", e))?;
        Ok(rows_to_devices(rows))
    }

    pub async fn list_all(&self) -> Result<Vec<Device>> {
        let rows: Vec<DeviceRow> = self
            .db
            .select("devices")
            .await
            .map_err(|e| anyhow::anyhow!("select devices: {}", e))?;
        Ok(rows_to_devices(rows))
    }

    /// Every device that can receive a push right now: enabled, with a token.
    pub async fn list_pushable(&self, workspace_id: &str) -> Result<Vec<Device>> {
        Ok(self
            .list_by_workspace(workspace_id)
            .await?
            .into_iter()
            .filter(|d| d.enabled && d.push_token.is_some())
            .collect())
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        let _: Option<DeviceRow> = self
            .db
            .delete(("devices", id))
            .await
            .map_err(|e| anyhow::anyhow!("delete device {}: {}", id, e))?;
        Ok(())
    }

    /// Advance the delivery cursor after a successful notification.
    pub async fn set_cursor(&self, id: &str, feed_event_id: &str) -> Result<()> {
        if let Some(mut d) = self.get(id).await? {
            d.last_seen_feed_event_id = Some(feed_event_id.to_string());
            d.last_seen_at = chrono::Utc::now().to_rfc3339();
            self.upsert(&d).await?;
        }
        Ok(())
    }

    /// Expo reported the token is dead — stop pushing to it, keep the row so
    /// the device can re-register with a fresh token.
    pub async fn disable_token(&self, id: &str) -> Result<()> {
        if let Some(mut d) = self.get(id).await? {
            d.push_token = None;
            d.enabled = false;
            self.upsert(&d).await?;
        }
        Ok(())
    }
}
