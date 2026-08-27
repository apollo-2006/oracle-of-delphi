//! Built-in tools wired into the demo registry.
//!
//! The Google/HA tools here are thin, offline-safe stand-ins that return
//! deterministic shapes so the agent loop is testable without live credentials;
//! the real network clients live in `crate::connectors` and swap in behind the
//! same tool names. Memory + KG tools are fully real (they hit SQLite).

use super::{SideEffect, ToolCtx, ToolError, ToolOutcome, TypedTool};
use crate::memory::graph::KgQuery;
use crate::memory::EpisodeKind;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

// ---------------------------------------------------------------------------
// Memory: remember / recall / forget (real SQLite)
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct RememberArgs {
    /// The fact or note to store in long-term memory.
    pub text: String,
}
pub struct Remember;
#[async_trait]
impl TypedTool for Remember {
    type Args = RememberArgs;
    const NAME: &'static str = "memory.remember";
    const DESCRIPTION: &'static str = "Store a durable fact or note in long-term memory.";
    const SIDE_EFFECT: SideEffect = SideEffect::Reversible;
    async fn run(&self, a: RememberArgs, ctx: &ToolCtx) -> ToolOutcome {
        match ctx
            .shared
            .memory
            .insert(EpisodeKind::Conversation, &a.text, 0.7)
        {
            Ok(id) => ToolOutcome::Ok(json!({ "stored_id": id })),
            Err(e) => ToolOutcome::Err(ToolError::transient(&e.to_string())),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct RecallArgs {
    /// What to search memory for.
    pub query: String,
    /// Max results (1-10).
    #[serde(default = "default_k")]
    pub limit: u32,
}
fn default_k() -> u32 {
    5
}
pub struct Recall;
#[async_trait]
impl TypedTool for Recall {
    type Args = RecallArgs;
    const NAME: &'static str = "memory.recall";
    const DESCRIPTION: &'static str = "Search long-term memory for relevant past items.";
    async fn run(&self, a: RecallArgs, ctx: &ToolCtx) -> ToolOutcome {
        let limit = a.limit.clamp(1, 10) as usize;
        match ctx.shared.memory.retrieve(&a.query, limit) {
            Ok(items) => {
                let out: Vec<_> = items
                    .iter()
                    .map(|it| {
                        let date = chrono::DateTime::from_timestamp(it.episode.t_unix, 0)
                            .map(|d| d.format("%Y-%m-%d").to_string())
                            .unwrap_or_default();
                        json!({"date": date, "text": it.episode.text, "score": it.score})
                    })
                    .collect();
                // reinforce what we surfaced (forgetting curve)
                for it in &items {
                    let _ = ctx.shared.memory.reinforce(it.episode.id, 0.05);
                }
                ToolOutcome::Ok(json!({ "results": out }))
            }
            Err(e) => ToolOutcome::Err(ToolError::transient(&e.to_string())),
        }
    }
    fn validate(a: &RecallArgs) -> Result<(), ToolError> {
        if a.query.trim().is_empty() {
            return Err(ToolError::invalid(
                "query",
                "empty query",
                "provide search terms",
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct KgArgs {
    /// One of: "neighbors" or "assert".
    pub op: String,
    pub entity: Option<String>,
    pub rel: Option<String>,
    pub obj: Option<String>,
    #[serde(default = "default_depth")]
    pub depth: u8,
}
fn default_depth() -> u8 {
    2
}
pub struct KgTool;
#[async_trait]
impl TypedTool for KgTool {
    type Args = KgArgs;
    const NAME: &'static str = "kg.query";
    const DESCRIPTION: &'static str =
        "Query or assert facts in the knowledge graph. op=neighbors|assert.";
    const SIDE_EFFECT: SideEffect = SideEffect::Reversible;
    async fn run(&self, a: KgArgs, ctx: &ToolCtx) -> ToolOutcome {
        let q = match a.op.as_str() {
            "neighbors" => {
                let Some(entity) = a.entity else {
                    return ToolOutcome::Err(ToolError::invalid(
                        "entity",
                        "neighbors requires entity",
                        "set entity",
                    ));
                };
                KgQuery::Neighbors {
                    entity,
                    rel: a.rel,
                    depth: a.depth,
                }
            }
            "assert" => match (a.entity, a.rel, a.obj) {
                (Some(subj), Some(rel), Some(obj)) => KgQuery::Assert {
                    subj,
                    rel,
                    obj,
                    provenance: format!("turn:{}", ctx.turn_id),
                },
                _ => {
                    return ToolOutcome::Err(ToolError::invalid(
                        "obj",
                        "assert requires entity, rel, obj",
                        "provide all three",
                    ))
                }
            },
            other => {
                return ToolOutcome::Err(ToolError::invalid(
                    "op",
                    &format!("unknown op '{other}'"),
                    "use neighbors or assert",
                ))
            }
        };
        match ctx.shared.graph.query(q) {
            Ok(edges) => {
                let out: Vec<_> = edges
                    .iter()
                    .map(|e| json!({"src": e.src, "rel": e.rel, "dst": e.dst}))
                    .collect();
                ToolOutcome::Ok(json!({ "edges": out }))
            }
            Err(msg) => ToolOutcome::Err(ToolError::invalid("rel", &msg, "use a known relation")),
        }
    }
}

// ---------------------------------------------------------------------------
// Workspace + IoT stand-ins (deterministic; real clients in connectors::)
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct GmailSearchArgs {
    /// Gmail search query, e.g. "from:advisor is:unread".
    pub query: String,
}
pub struct GmailSearch;
#[async_trait]
impl TypedTool for GmailSearch {
    type Args = GmailSearchArgs;
    const NAME: &'static str = "gmail.search";
    const DESCRIPTION: &'static str =
        "Search the user's real Gmail (query is Gmail search syntax, e.g. 'from:alice is:unread'). Returns matching messages.";
    async fn run(&self, a: GmailSearchArgs, c: &ToolCtx) -> ToolOutcome {
        let Some(google) = c.shared.google.clone() else {
            return not_authorized();
        };
        let now = chrono::Utc::now().timestamp();
        match google.gmail_search(&a.query, 8, now).await {
            Ok(msgs) => {
                let top_sender = msgs.first().map(|m| m.from.clone()).unwrap_or_default();
                // Each message BODY/snippet is third-party text → fence it as
                // untrusted data before it reaches the model (injection defense).
                let threads: Vec<_> = msgs
                    .iter()
                    .map(|m| {
                        json!({
                            "id": m.id,
                            "thread_id": m.thread_id,
                            "from": m.from,
                            "subject": m.subject,
                            "unread": m.unread,
                            "snippet": crate::security::wrap_untrusted(
                                &format!("email:{}", m.from), &m.snippet),
                        })
                    })
                    .collect();
                ToolOutcome::Ok(json!({
                    "count": msgs.len(),
                    "top_sender": top_sender,
                    "threads": threads,
                    "query": a.query
                }))
            }
            Err(e) => ToolOutcome::Err(ToolError::transient(&format!("gmail: {e}"))),
        }
    }
}

/// Standard error when Google isn't authorized, with the exact fix.
fn not_authorized() -> ToolOutcome {
    ToolOutcome::Err(ToolError {
        status: super::ToolErrorKind::Denied,
        field: None,
        reason: "Google Workspace is not connected".into(),
        hint: Some("run `oracle-core auth --credentials <credentials.json> --account <email>` and set [google] credentials_path in the config".into()),
    })
}

#[derive(Deserialize, JsonSchema)]
pub struct FreeSlotsArgs {
    /// ISO date (YYYY-MM-DD) in the user's timezone.
    pub date: String,
    /// Meeting length in minutes (5-480).
    pub duration_min: u32,
    /// Optional day window: "morning" | "afternoon" | "evening".
    pub window: Option<String>,
}
pub struct FreeSlots;
#[async_trait]
impl TypedTool for FreeSlots {
    type Args = FreeSlotsArgs;
    const NAME: &'static str = "calendar.free_slots";
    const DESCRIPTION: &'static str =
        "Find free slots of a given length on a date, from the user's real Google Calendar.";
    async fn run(&self, a: FreeSlotsArgs, c: &ToolCtx) -> ToolOutcome {
        let Some(google) = c.shared.google.clone() else {
            return not_authorized();
        };
        let now = chrono::Utc::now().timestamp();
        // Query the whole day; compute busy intervals in minutes-since-midnight.
        let time_min = format!("{}T00:00:00Z", a.date);
        let time_max = format!("{}T23:59:59Z", a.date);
        let events = match google.calendar_events(&time_min, &time_max, now).await {
            Ok(e) => e,
            Err(e) => return ToolOutcome::Err(ToolError::transient(&format!("calendar: {e}"))),
        };
        let busy: Vec<crate::connectors::google_api::Slot> = events
            .iter()
            .filter_map(|e| {
                Some(crate::connectors::google_api::Slot {
                    start_min: rfc3339_to_minutes(&e.start)?,
                    end_min: rfc3339_to_minutes(&e.end)?,
                })
            })
            .collect();
        let (win_start, win_end) = match a.window.as_deref() {
            Some("morning") => (8 * 60, 12 * 60),
            Some("evening") => (17 * 60, 21 * 60),
            // "afternoon" or unspecified → default afternoon window.
            _ => (12 * 60, 18 * 60),
        };
        let slots =
            crate::connectors::google_api::free_slots(&busy, win_start, win_end, a.duration_min);
        let out: Vec<_> = slots
            .iter()
            .map(|s| json!({"start": min_to_hhmm(s.start_min), "end": min_to_hhmm(s.end_min)}))
            .collect();
        ToolOutcome::Ok(json!({
            "date": a.date,
            "slots": out,
            "window": a.window,
            "busy_events": events.len()
        }))
    }
    fn validate(a: &FreeSlotsArgs) -> Result<(), ToolError> {
        if !(5..=480).contains(&a.duration_min) {
            return Err(ToolError::invalid(
                "duration_min",
                "out of range 5..=480",
                "pick a duration between 5 and 480 minutes",
            ));
        }
        if chrono::NaiveDate::parse_from_str(&a.date, "%Y-%m-%d").is_err() {
            return Err(ToolError::invalid(
                "date",
                "not an ISO date",
                "use YYYY-MM-DD",
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct DraftArgs {
    /// Recipient email address.
    pub to: String,
    /// Subject line.
    pub subject: String,
    /// Plain-text body.
    pub body: String,
}
pub struct CreateDraft;
#[async_trait]
impl TypedTool for CreateDraft {
    type Args = DraftArgs;
    const NAME: &'static str = "gmail.create_draft";
    const DESCRIPTION: &'static str =
        "Create a real Gmail draft (does NOT send it — the user reviews and sends).";
    const SIDE_EFFECT: SideEffect = SideEffect::Reversible;
    async fn run(&self, a: DraftArgs, c: &ToolCtx) -> ToolOutcome {
        let Some(google) = c.shared.google.clone() else {
            return not_authorized();
        };
        let now = chrono::Utc::now().timestamp();
        match google
            .gmail_create_draft(&a.to, &a.subject, &a.body, now)
            .await
        {
            Ok(id) => ToolOutcome::Ok(json!({"draft_id": id, "to": a.to, "subject": a.subject})),
            Err(e) => ToolOutcome::Err(ToolError::transient(&format!("gmail draft: {e}"))),
        }
    }
}

/// Parse an RFC3339 datetime to minutes-since-midnight in its own offset.
fn rfc3339_to_minutes(s: &str) -> Option<u32> {
    let dt = chrono::DateTime::parse_from_rfc3339(s).ok()?;
    use chrono::Timelike;
    Some(dt.hour() * 60 + dt.minute())
}

fn min_to_hhmm(m: u32) -> String {
    format!("{:02}:{:02}", m / 60, m % 60)
}

#[derive(Deserialize, JsonSchema)]
pub struct LightArgs {
    pub room: String,
    /// Brightness percentage 0-100.
    pub brightness_pct: u8,
}
pub struct Light;
#[async_trait]
impl TypedTool for Light {
    type Args = LightArgs;
    const NAME: &'static str = "home_assistant.light";
    const DESCRIPTION: &'static str = "Set brightness of a room's lights.";
    const SIDE_EFFECT: SideEffect = SideEffect::Reversible;
    async fn run(&self, a: LightArgs, _c: &ToolCtx) -> ToolOutcome {
        ToolOutcome::Ok(json!({"room":a.room,"brightness_pct":a.brightness_pct.min(100),"ok":true}))
    }
    fn validate(a: &LightArgs) -> Result<(), ToolError> {
        if a.brightness_pct > 100 {
            return Err(ToolError::invalid(
                "brightness_pct",
                "must be 0..=100",
                "use a percentage",
            ));
        }
        Ok(())
    }
}

/// Register every built-in tool into a registry.
pub fn register_all(reg: &mut super::ToolRegistry) {
    reg.register(Remember)
        .register(Recall)
        .register(KgTool)
        .register(GmailSearch)
        .register(FreeSlots)
        .register(CreateDraft)
        .register(Light);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Shared;
    use std::sync::Arc;

    fn ctx() -> ToolCtx {
        ToolCtx {
            turn_id: uuid::Uuid::new_v4(),
            shared: Arc::new(Shared::for_test()),
        }
    }

    #[tokio::test]
    async fn remember_then_recall_roundtrips() {
        let c = ctx();
        let r = Remember
            .run(
                RememberArgs {
                    text: "my thesis advisor is Dr. Chen".into(),
                },
                &c,
            )
            .await;
        assert!(matches!(r, ToolOutcome::Ok(_)));
        let hit = Recall
            .run(
                RecallArgs {
                    query: "who is my advisor".into(),
                    limit: 3,
                },
                &c,
            )
            .await;
        match hit {
            ToolOutcome::Ok(v) => assert!(!v["results"].as_array().unwrap().is_empty()),
            _ => panic!("recall failed"),
        }
    }

    #[tokio::test]
    async fn free_slots_validates_duration() {
        use crate::tools::{Tool, Typed};
        let c = ctx();
        // Exercise the full validation ladder via the object-safe adapter.
        let bad = Typed(super::FreeSlots)
            .dispatch(json!({"date":"2026-08-18","duration_min":9999}), &c)
            .await;
        assert!(matches!(bad, ToolOutcome::Err(_)));
    }

    #[tokio::test]
    async fn kg_assert_and_neighbors_via_tool() {
        let c = ctx();
        KgTool
            .run(
                KgArgs {
                    op: "assert".into(),
                    entity: Some("User".into()),
                    rel: Some("advisor".into()),
                    obj: Some("Dr. Chen".into()),
                    depth: 1,
                },
                &c,
            )
            .await;
        let out = KgTool
            .run(
                KgArgs {
                    op: "neighbors".into(),
                    entity: Some("User".into()),
                    rel: None,
                    obj: None,
                    depth: 1,
                },
                &c,
            )
            .await;
        match out {
            ToolOutcome::Ok(v) => {
                assert!(v["edges"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["dst"] == "Dr. Chen"))
            }
            _ => panic!("kg neighbors failed"),
        }
    }
}
