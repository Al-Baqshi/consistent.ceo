//! Training-data plumbing for Consistent's own local model.
//!
//! Two jobs, both aimed at keeping fine-tuning data identical to what the
//! model sees in production:
//! - an opt-in recorder ([`maybe_trace`]) wrapped around every LLM client,
//!   which appends each real exchange (messages, tools, answer) to JSONL;
//! - renderers that turn small scenario files into heartbeat and chat prompts
//!   using the production prompt builders, plus an export of the live chat
//!   tool schemas.
//!
//! The Python side (`training/`) consumes these files; see training/README.md.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use consistent_world_common::types::ToolCall;
use consistent_world_llm::client::{
    ChatMessage, CompletionRequest, CompletionResponse, LlmProvider, Role, ToolDefinition,
};
use consistent_world_llm::streaming::{StreamEvent, TokenStream};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::capture::CaptureChatMessage;
use crate::director_runner::{BeatBrief, build_request_with_docs};
use crate::entity_heartbeat_defaults::BeatIntent;
use crate::prompt_assembly::{CanonicalInputs, assemble_system_prompt};
use crate::state::{Director, DirectorLesson, Entity, EntityDoc, Mission, MissionStatus};

/// Set to `1` to record every LLM exchange the gateway makes.
pub const TRACE_ENV: &str = "CONSISTENT_WORLD_TRAINING_TRACES";
/// Optional override for where traces are written.
pub const TRACE_DIR_ENV: &str = "CONSISTENT_WORLD_TRAINING_DIR";

const HEARTBEAT_PROMPT_PREFIX: &str = "A scheduled heartbeat just fired";

pub fn traces_enabled() -> bool {
    std::env::var(TRACE_ENV)
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

pub fn traces_dir() -> PathBuf {
    std::env::var(TRACE_DIR_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| PathBuf::from(crate::config::expand_tilde_path(&s)))
        .unwrap_or_else(|| {
            PathBuf::from(crate::config::expand_tilde_path(
                "~/.consistent-world/training/traces",
            ))
        })
}

/// Wrap `client` in the trace recorder when tracing is switched on.
pub(crate) fn maybe_trace(client: Box<dyn LlmProvider>) -> Box<dyn LlmProvider> {
    if traces_enabled() {
        Box::new(TracingProvider {
            inner: client,
            dir: traces_dir(),
        })
    } else {
        client
    }
}

struct TracingProvider {
    inner: Box<dyn LlmProvider>,
    dir: PathBuf,
}

// Serializes appends so concurrent beats never interleave half-lines.
static TRACE_WRITE: Mutex<()> = Mutex::new(());

/// What a recorded exchange was, inferred from the request itself so the
/// recorder needs no plumbing from call sites.
fn classify(request: &CompletionRequest) -> &'static str {
    let is_beat = request
        .messages
        .iter()
        .any(|m| m.role == Role::User && m.content.starts_with(HEARTBEAT_PROMPT_PREFIX));
    if is_beat {
        "heartbeat"
    } else if request.tools.as_ref().is_some_and(|t| !t.is_empty()) {
        "chat"
    } else {
        "other"
    }
}

fn trace_record(
    provider: &str,
    request: &CompletionRequest,
    content: Option<&str>,
    tool_calls: &[ToolCall],
    model: &str,
    latency_ms: u64,
) -> Value {
    json!({
        "ts": Utc::now().to_rfc3339(),
        "kind": classify(request),
        "provider": provider,
        "model": if model.is_empty() { request.model.as_str() } else { model },
        "latency_ms": latency_ms,
        "messages": request.messages,
        "tools": request.tools,
        "max_tokens": request.max_tokens,
        "temperature": request.temperature,
        "response": { "content": content, "tool_calls": tool_calls },
    })
}

fn append_trace(dir: &std::path::Path, record: &Value) {
    let path = dir.join(format!("{}.jsonl", Utc::now().format("%Y-%m-%d")));
    let line = match serde_json::to_string(record) {
        Ok(s) => s,
        Err(_) => return,
    };
    let _guard = TRACE_WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let result = std::fs::create_dir_all(dir).and_then(|_| {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(f, "{line}")
    });
    if let Err(e) = result {
        tracing::warn!(error = %e, path = %path.display(), "training trace write failed");
    }
}

#[async_trait]
impl LlmProvider for TracingProvider {
    async fn complete(&self, request: CompletionRequest) -> anyhow::Result<CompletionResponse> {
        let traced = request.clone();
        let response = self.inner.complete(request).await?;
        let record = trace_record(
            self.inner.provider_name(),
            &traced,
            response.content.as_deref(),
            &response.tool_calls,
            &response.model,
            response.latency_ms,
        );
        let dir = self.dir.clone();
        tokio::task::spawn_blocking(move || append_trace(&dir, &record));
        Ok(response)
    }

    async fn stream_complete(&self, request: CompletionRequest) -> anyhow::Result<TokenStream> {
        let traced = request.clone();
        let start = Instant::now();
        let stream = self.inner.stream_complete(request).await?;
        let provider = self.inner.provider_name();
        let dir = self.dir.clone();

        // Pass events through untouched while accumulating the answer; the
        // record is written once the stream finishes cleanly.
        let acc = Arc::new(Mutex::new(StreamAcc::default()));
        let acc_in = acc.clone();
        let tapped = stream.map(move |event| {
            let mut a = acc_in.lock().unwrap_or_else(|e| e.into_inner());
            match &event {
                StreamEvent::Token(t) => a.content.push_str(t),
                StreamEvent::ToolCallStart { id, name } => {
                    a.calls.push((id.clone(), name.clone(), String::new()))
                }
                StreamEvent::ToolCallDelta {
                    id,
                    arguments_chunk,
                } => {
                    if let Some(c) = a.calls.iter_mut().find(|c| &c.0 == id) {
                        c.2.push_str(arguments_chunk);
                    }
                }
                StreamEvent::Done if !a.written && !a.errored => {
                    a.written = true;
                    let calls: Vec<ToolCall> = a
                        .calls
                        .iter()
                        .map(|(id, name, args)| ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: serde_json::from_str(args)
                                .unwrap_or_else(|_| Value::String(args.clone())),
                        })
                        .collect();
                    let content = (!a.content.is_empty()).then_some(a.content.as_str());
                    let record = trace_record(
                        provider,
                        &traced,
                        content,
                        &calls,
                        "",
                        start.elapsed().as_millis() as u64,
                    );
                    let dir = dir.clone();
                    tokio::task::spawn_blocking(move || append_trace(&dir, &record));
                }
                StreamEvent::Error(_) => a.errored = true,
                _ => {}
            }
            event
        });
        Ok(Box::pin(tapped))
    }

    fn provider_name(&self) -> &'static str {
        self.inner.provider_name()
    }
}

#[derive(Default)]
struct StreamAcc {
    content: String,
    calls: Vec<(String, String, String)>,
    written: bool,
    errored: bool,
}

/// Record what the owner did with a reminder (done / snooze / drop …) next to
/// the traces, when tracing is on. `traces_to_sft.py --learn-from-outcomes`
/// joins these with check-in traces to keep the nudges that led to action,
/// so the local model learns from what actually worked for this person.
pub fn record_outcome(task: &crate::state::Task, action: &str, via: &str) {
    if !traces_enabled() {
        return;
    }
    let record = json!({
        "ts": Utc::now().to_rfc3339(),
        "kind": "reminder_outcome",
        "reminder_id": task.id,
        "what": crate::reminders::reminder_what(task),
        "action": action,
        "via": via,
        "nudges": task.metadata.as_ref().and_then(|m| m.get(crate::reminders::NUDGES_KEY)).cloned(),
        "first_fired_at": crate::state::task_meta_str(task, crate::reminders::FIRST_FIRED_KEY),
    });
    let dir = traces_dir().join("outcomes");
    std::thread::spawn(move || append_trace(&dir, &record));
}

// ---------------------------------------------------------------------------
// Scenario rendering
// ---------------------------------------------------------------------------

/// The parts of an entity a prompt actually reads.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScenarioEntity {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub goals: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub active_context: Option<String>,
    #[serde(default)]
    pub operating_rules: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScenarioMission {
    pub title: String,
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub narrative: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScenarioDoc {
    pub title: String,
    pub content: String,
}

/// One heartbeat firing to render.
#[derive(Debug, Clone, Deserialize)]
pub struct BeatScenario {
    pub id: String,
    /// Seed director id/slug, e.g. `ops-coordinator`.
    pub director: String,
    pub entity: ScenarioEntity,
    #[serde(default)]
    pub mission: Option<ScenarioMission>,
    #[serde(default)]
    pub lessons: Vec<String>,
    #[serde(default)]
    pub docs: Vec<ScenarioDoc>,
    /// `setup` | `reminder` | `review` | `ideas` | `drive`; absent = hand-made beat.
    #[serde(default)]
    pub intent: Option<String>,
    #[serde(default)]
    pub focus: Option<String>,
    #[serde(default)]
    pub recent: Vec<String>,
    /// Loops beats: the open-items lines the dispatcher would list.
    #[serde(default)]
    pub open_items: Vec<String>,
    /// Free-form labels (e.g. `expect_skip`) passed through to the output for graders.
    #[serde(default)]
    pub meta: Option<Value>,
    #[serde(default)]
    pub memory: Vec<String>,
    #[serde(default)]
    pub fired_at: Option<DateTime<Utc>>,
}

/// One chat turn to render.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatScenario {
    pub id: String,
    #[serde(default)]
    pub entity: Option<ScenarioEntity>,
    #[serde(default)]
    pub mission: Option<ScenarioMission>,
    #[serde(default)]
    pub lessons: Vec<String>,
    #[serde(default)]
    pub docs: Vec<ScenarioDoc>,
    #[serde(default)]
    pub history: Vec<CaptureChatMessage>,
    pub message: String,
    #[serde(default)]
    pub ui_hint: Option<String>,
    /// Open reminders the chat sees (rendered by `reminders::chat_block`).
    #[serde(default)]
    pub open_reminders: Vec<ScenarioReminder>,
}

/// One open reminder in a chat scenario.
#[derive(Debug, Clone, Deserialize)]
pub struct ScenarioReminder {
    pub id: String,
    pub what: String,
    /// Times it already nudged; 0 = still scheduled.
    #[serde(default)]
    pub nudges: u32,
    /// Minutes until the next nudge (scheduled reminders).
    #[serde(default)]
    pub in_minutes: Option<i64>,
}

fn to_reminder_task(r: &ScenarioReminder, now: DateTime<Utc>) -> crate::state::Task {
    let due = now + chrono::Duration::minutes(r.in_minutes.unwrap_or(60));
    let mut meta = crate::reminders::schedule_metadata(due, Some(0));
    meta[crate::reminders::NUDGES_KEY] = json!(r.nudges);
    crate::state::Task {
        id: r.id.clone(),
        title: format!("Reminder: {}", r.what),
        task_kind: Some("reminder".into()),
        entity_id: Some(SCENARIO_ENTITY_ID.into()),
        metadata: Some(meta),
        ..Default::default()
    }
}

const SCENARIO_ENTITY_ID: &str = "ent_scenario";
const SCENARIO_TS: &str = "2026-01-01T00:00:00Z";

fn to_entity(e: &ScenarioEntity) -> Entity {
    Entity {
        id: SCENARIO_ENTITY_ID.to_string(),
        name: e.name.clone(),
        description: e.description.clone(),
        goals: e.goals.clone(),
        profile: e.profile.clone(),
        active_context: e.active_context.clone(),
        operating_rules: e.operating_rules.clone(),
        status: "active".to_string(),
        ..Default::default()
    }
}

fn to_mission(m: &ScenarioMission) -> Mission {
    Mission {
        id: "mis_scenario".to_string(),
        entity_id: SCENARIO_ENTITY_ID.to_string(),
        title: m.title.clone(),
        summary: String::new(),
        status: MissionStatus::Active,
        priority: "normal".to_string(),
        goal: m.goal.clone(),
        owner: None,
        created_by: None,
        source: None,
        created_at: SCENARIO_TS.to_string(),
        updated_at: SCENARIO_TS.to_string(),
        last_reviewed_at: None,
        review_cadence_hours: None,
        narrative: m.narrative.clone(),
        canonical_notes: m.notes.clone(),
        assigned_director_id: None,
        final_report: None,
        loop_config: None,
        loop_state: None,
    }
}

fn to_lessons(director_id: &str, lessons: &[String]) -> Vec<DirectorLesson> {
    lessons
        .iter()
        .enumerate()
        .map(|(i, l)| DirectorLesson {
            id: format!("les_{i}"),
            director_id: director_id.to_string(),
            scope: "global".to_string(),
            lesson: l.clone(),
            source: "observation".to_string(),
            severity: "soft".to_string(),
            embedding: None,
            created_at: SCENARIO_TS.to_string(),
            created_by: None,
        })
        .collect()
}

fn to_docs(docs: &[ScenarioDoc]) -> Vec<EntityDoc> {
    docs.iter()
        .enumerate()
        .map(|(i, d)| EntityDoc {
            id: format!("doc_{i}"),
            entity_id: SCENARIO_ENTITY_ID.to_string(),
            title: d.title.clone(),
            content: d.content.clone(),
            source: "manual".to_string(),
            url: None,
            enabled: true,
            embedding: None,
            created_at: SCENARIO_TS.to_string(),
            updated_at: SCENARIO_TS.to_string(),
            created_by: None,
        })
        .collect()
}

fn seed_directors() -> anyhow::Result<Vec<Director>> {
    crate::director_seed::parse_seed(crate::director_seed::EMBEDDED_SEED)
}

fn find_director<'a>(directors: &'a [Director], key: &str) -> Option<&'a Director> {
    directors
        .iter()
        .find(|d| d.id == key || d.slug == key)
        .or_else(|| {
            directors
                .iter()
                .find(|d| d.group_id.as_deref() == Some(key))
        })
}

fn messages_json(messages: &[ChatMessage]) -> Value {
    Value::Array(
        messages
            .iter()
            .map(|m| json!({ "role": m.role, "content": m.content }))
            .collect(),
    )
}

/// Render one heartbeat scenario into the exact request a beat sends.
pub fn render_beat(scenario: &BeatScenario, directors: &[Director]) -> anyhow::Result<Value> {
    let director = find_director(directors, &scenario.director)
        .ok_or_else(|| anyhow::anyhow!("unknown director '{}'", scenario.director))?;
    let entity = to_entity(&scenario.entity);
    let mission = scenario.mission.as_ref().map(to_mission);
    let lessons = to_lessons(&director.id, &scenario.lessons);
    let docs = to_docs(&scenario.docs);
    let brief = match scenario.intent.as_deref() {
        Some(tag) => Some(BeatBrief {
            intent: BeatIntent::from_tag(tag)
                .ok_or_else(|| anyhow::anyhow!("unknown beat intent '{tag}'"))?,
            focus: scenario.focus.clone(),
            recent: scenario.recent.clone(),
            open_items: scenario.open_items.clone(),
        }),
        None => None,
    };
    let fired_at = scenario.fired_at.unwrap_or_else(Utc::now);
    let request = build_request_with_docs(
        String::new(),
        director,
        &entity,
        mission.as_ref(),
        &lessons,
        brief.as_ref(),
        &scenario.memory,
        &docs,
        fired_at,
    );
    Ok(json!({
        "id": scenario.id,
        "kind": "heartbeat",
        "intent": scenario.intent.as_deref().unwrap_or("generic"),
        "director": director.id,
        "messages": messages_json(&request.messages),
        "max_tokens": request.max_tokens,
        "temperature": request.temperature,
        "meta": scenario.meta,
    }))
}

/// Render one chat scenario into the exact opening request a chat turn sends.
pub fn render_chat(scenario: &ChatScenario, tools: &[ToolDefinition]) -> Value {
    let entity = scenario.entity.as_ref().map(to_entity);
    let mission = scenario.mission.as_ref().map(to_mission);
    let lessons = to_lessons("operations", &scenario.lessons);
    let docs = to_docs(&scenario.docs);
    // Mirrors chat_agent::build_canonical_block: no entity, no canonical block.
    let canonical = entity.as_ref().map(|e| {
        assemble_system_prompt(&CanonicalInputs {
            soul: None,
            director: None,
            entity: Some(e),
            mission: mission.as_ref(),
            lessons: &lessons,
            docs: &docs,
        })
    });
    // Mirrors chat_agent::open_reminders_block (rides the skills slot).
    let reminder_tasks: Vec<crate::state::Task> = scenario
        .open_reminders
        .iter()
        .map(|r| to_reminder_task(r, Utc::now()))
        .collect();
    let refs: Vec<&crate::state::Task> = reminder_tasks.iter().collect();
    let reminders_block = crate::reminders::chat_block(&refs, Utc::now());
    let messages = crate::chat_agent::build_initial_messages(
        &scenario.history,
        &scenario.message,
        entity.as_ref(),
        scenario.ui_hint.as_deref(),
        canonical.as_deref().filter(|s| !s.trim().is_empty()),
        reminders_block.as_deref(),
    );
    json!({
        "id": scenario.id,
        "kind": "chat",
        "messages": messages_json(&messages),
        "tools": tools,
        // chat_agent.rs sends every chat round at this temperature.
        "temperature": 0.65,
    })
}

/// The chat tool definitions exactly as a local gateway offers them.
pub async fn chat_tool_definitions(local_files: bool) -> anyhow::Result<Vec<ToolDefinition>> {
    use consistent_world_memory::store::{MemoryStore, SurrealMemoryStore};
    let memory: Arc<dyn MemoryStore> = Arc::new(SurrealMemoryStore::connect_memory().await?);
    let control = Arc::new(crate::state::ControlState::new("ollama", "http://x", "m"));
    let mut state = crate::state::ServerState::new("training".into(), None, memory)
        .with_control_state(control.clone());
    // Tool descriptions are static; the roots only need to exist long enough
    // for the file tools to be registered.
    let scratch = std::env::temp_dir().join(format!("cw-training-{}", uuid::Uuid::new_v4()));
    if local_files {
        let mut files = crate::local_files::LocalFiles::new(
            scratch.join("Desktop"),
            scratch.join("Consistent"),
        );
        files.ensure_roots()?;
        state = state.with_local_files(Arc::new(files));
    }
    let tools =
        crate::chat_tools::build_workspace_tools(state, control, "default", None, "user", None);
    let defs = crate::chat_agent::tool_defs(&tools);
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(defs)
}

fn read_jsonl<T: serde::de::DeserializeOwned>(path: &str) -> anyhow::Result<Vec<T>> {
    let raw = std::fs::read_to_string(path)?;
    raw.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| serde_json::from_str(l).map_err(|e| anyhow::anyhow!("{path}:{}: {e}", i + 1)))
        .collect()
}

fn write_jsonl(path: &str, rows: &[Value]) -> anyhow::Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = String::new();
    for r in rows {
        out.push_str(&serde_json::to_string(r)?);
        out.push('\n');
    }
    std::fs::write(path, out)?;
    Ok(())
}

/// `consistent-world training render-beats`.
pub fn render_beats_file(input: &str, output: &str) -> anyhow::Result<usize> {
    let directors = seed_directors()?;
    let scenarios: Vec<BeatScenario> = read_jsonl(input)?;
    let rows = scenarios
        .iter()
        .map(|s| render_beat(s, &directors).map_err(|e| anyhow::anyhow!("{}: {e}", s.id)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    write_jsonl(output, &rows)?;
    Ok(rows.len())
}

/// `consistent-world training render-chats`.
pub async fn render_chats_file(input: &str, output: &str) -> anyhow::Result<usize> {
    let tools = chat_tool_definitions(true).await?;
    let scenarios: Vec<ChatScenario> = read_jsonl(input)?;
    let rows: Vec<Value> = scenarios.iter().map(|s| render_chat(s, &tools)).collect();
    write_jsonl(output, &rows)?;
    Ok(rows.len())
}

/// `consistent-world training context`: the directors and tools the scenario
/// generator needs to know about, in one JSON file.
pub async fn export_context(output: &str) -> anyhow::Result<()> {
    let directors: Vec<Value> = seed_directors()?
        .into_iter()
        .map(|d| json!({ "id": d.id, "name": d.name, "group": d.group_id, "role": d.role, "mission": d.description }))
        .collect();
    let tools = chat_tool_definitions(true).await?;
    let ctx = json!({ "directors": directors, "chat_tools": tools });
    if let Some(parent) = std::path::Path::new(output).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output, serde_json::to_string_pretty(&ctx)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beat() -> BeatScenario {
        serde_json::from_value(json!({
            "id": "b1",
            "director": "ops-coordinator",
            "entity": { "name": "Acme Bakery", "goals": "Open second store by June" },
            "mission": { "title": "Lease", "goal": "Sign a lease", "notes": ["Landlord wants 6 months deposit"] },
            "intent": "reminder",
            "focus": "What is stalled today.",
            "recent": ["Lease draft still with lawyer."],
            "memory": ["Owner prefers mornings"],
            "fired_at": "2026-10-17T08:00:00Z"
        }))
        .unwrap()
    }

    #[test]
    fn beat_render_matches_production_prompt_shape() {
        let directors = seed_directors().unwrap();
        let row = render_beat(&beat(), &directors).unwrap();
        let msgs = row["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        let system = msgs[0]["content"].as_str().unwrap();
        assert!(system.contains("## Active entity (canonical)"));
        assert!(system.contains("Landlord wants 6 months deposit"));
        assert!(system.contains("## Entity memory\n- Owner prefers mornings"));
        let user = msgs[1]["content"].as_str().unwrap();
        assert!(user.starts_with(
            "A scheduled heartbeat just fired at 2026-10-17 08:00:00 UTC for the entity \"Acme Bakery\"."
        ));
        assert!(user.contains("1. Lease draft still with lawyer."));
        assert!(user.ends_with("No greeting, no sign-off, no markdown formatting."));
        assert_eq!(row["max_tokens"], 400);
    }

    #[test]
    fn unknown_director_or_intent_is_an_error() {
        let directors = seed_directors().unwrap();
        let mut b = beat();
        b.director = "nobody".into();
        assert!(render_beat(&b, &directors).is_err());
        let mut b = beat();
        b.intent = Some("dance".into());
        assert!(render_beat(&b, &directors).is_err());
    }

    #[tokio::test]
    async fn chat_render_carries_real_tools_and_entity_context() {
        let tools = chat_tool_definitions(true).await.unwrap();
        assert!(tools.iter().any(|t| t.name == "set_reminder"));
        assert!(tools.iter().any(|t| t.name == "write_local_file"));
        let s: ChatScenario = serde_json::from_value(json!({
            "id": "c1",
            "entity": { "name": "Acme Bakery" },
            "history": [{ "role": "user", "content": "hey" }, { "role": "assistant", "content": "hi" }],
            "message": "remind me to call the landlord in 2 hours"
        }))
        .unwrap();
        let row = render_chat(&s, &tools);
        let msgs = row["messages"].as_array().unwrap();
        assert!(
            msgs[0]["content"]
                .as_str()
                .unwrap()
                .starts_with("You are Consistent CEO")
        );
        assert_eq!(msgs.last().unwrap()["role"], "user");
        assert_eq!(msgs.len(), 5);
    }

    #[test]
    fn classify_spots_beats_and_tool_turns() {
        let mk = |content: &str, tools: bool| CompletionRequest {
            model: "m".into(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: content.into(),
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: tools.then(|| {
                vec![ToolDefinition {
                    name: "x".into(),
                    description: String::new(),
                    parameters: json!({}),
                }]
            }),
            max_tokens: None,
            temperature: None,
            stream: false,
        };
        assert_eq!(
            classify(&mk("A scheduled heartbeat just fired at …", false)),
            "heartbeat"
        );
        assert_eq!(classify(&mk("hi", true)), "chat");
        assert_eq!(classify(&mk("hi", false)), "other");
    }

    #[test]
    fn traces_append_one_line_per_record() {
        let dir = tempfile::tempdir().unwrap();
        append_trace(dir.path(), &json!({"a": 1}));
        append_trace(dir.path(), &json!({"a": 2}));
        let file = std::fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let text = std::fs::read_to_string(file.path()).unwrap();
        assert_eq!(text.lines().count(), 2);
    }
}
