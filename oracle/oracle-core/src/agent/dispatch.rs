//! Async multi-tool dispatcher (architecture §2.4).
//!
//! Executes a tool batch as a dependency DAG: independent calls run in
//! parallel, dependent calls unlock as their `$result.N` inputs resolve, and a
//! single turn-level cancellation aborts the whole tree. Each tool has a
//! per-call timeout so a hung tool can't wedge the loop.

use super::dag::{substitute, DependencyDag, ToolCall};
use super::AgentEvent;
use crate::tools::{ToolCtx, ToolOutcome, ToolRegistry};
use crate::Shared;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::warn;

const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-tool context budget: outputs larger than this are truncated with a note.
const RESULT_BYTE_BUDGET: usize = 2048;

pub struct Dispatcher {
    tools: ToolRegistry,
    shared: Arc<Shared>,
}

/// The ordered result table returned to the agent loop.
pub struct BatchResults {
    ordered: Vec<(u32, String, ResultView)>,
}

enum ResultView {
    Ok(Value),
    Err(Value),
}

impl BatchResults {
    /// Serialize all results into one compact observation string for the model.
    pub fn as_observation(&self) -> String {
        let mut items = Vec::new();
        for (id, name, rv) in &self.ordered {
            let body = match rv {
                ResultView::Ok(v) => serde_json::json!({"tool": name, "id": id, "ok": v}),
                ResultView::Err(e) => {
                    serde_json::json!({"tool": name, "id": id, "error": e})
                }
            };
            items.push(truncate(body.to_string(), RESULT_BYTE_BUDGET));
        }
        format!("[tool results]\n{}", items.join("\n"))
    }

    /// Map of id -> ok-value, for reference resolution / assertions in tests.
    pub fn ok_map(&self) -> HashMap<u32, Value> {
        self.ordered
            .iter()
            .filter_map(|(id, _, rv)| match rv {
                ResultView::Ok(v) => Some((*id, v.clone())),
                _ => None,
            })
            .collect()
    }
}

impl Dispatcher {
    pub fn new(tools: ToolRegistry, shared: Arc<Shared>) -> Self {
        Dispatcher { tools, shared }
    }

    /// Run a batch. `out` receives per-tool HUD events; the return value is the
    /// ordered result table for feeding back into the model.
    pub async fn run(
        &self,
        turn_id: uuid::Uuid,
        calls: Vec<ToolCall>,
        out: &mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> BatchResults {
        let dag = DependencyDag::from(&calls);
        if dag.has_cycle() {
            warn!("tool batch has a dependency cycle; refusing to dispatch");
            return BatchResults {
                ordered: calls
                    .iter()
                    .map(|c| {
                        (
                            c.id,
                            c.name.clone(),
                            ResultView::Err(serde_json::json!({
                                "status": "invalid_args",
                                "reason": "dependency cycle among tool calls"
                            })),
                        )
                    })
                    .collect(),
            };
        }

        let mut completed: HashSet<u32> = HashSet::new();
        let mut ok_results: HashMap<u32, Value> = HashMap::new();
        let mut ordered: Vec<(u32, String, ResultView)> = Vec::new();
        let mut running: JoinSet<(u32, String, ToolOutcome)> = JoinSet::new();
        let mut launched: HashSet<u32> = HashSet::new();

        // Launch helper: spawn everything currently ready & not yet launched.
        macro_rules! launch_ready {
            () => {{
                for call in dag.ready(&completed) {
                    if launched.contains(&call.id) {
                        continue;
                    }
                    // Resolve $result references from what's completed so far.
                    let resolved = match substitute(&call.args, &ok_results) {
                        Ok(v) => v,
                        Err(e) => {
                            // Can't resolve — record as error, mark complete.
                            ordered.push((
                                call.id,
                                call.name.clone(),
                                ResultView::Err(serde_json::json!({
                                    "status": "invalid_args", "reason": e
                                })),
                            ));
                            completed.insert(call.id);
                            launched.insert(call.id);
                            continue;
                        }
                    };
                    launched.insert(call.id);
                    let _ = out
                        .send(AgentEvent::ToolStarted {
                            id: call.id,
                            name: call.name.clone(),
                        })
                        .await;
                    let tool = self.tools.get(&call.name);
                    let ctx = ToolCtx {
                        turn_id,
                        shared: self.shared.clone(),
                    };
                    let child = cancel.child_token();
                    let name = call.name.clone();
                    let id = call.id;
                    running.spawn(async move {
                        let outcome = match tool {
                            None => ToolOutcome::Err(crate::tools::ToolError {
                                status: crate::tools::ToolErrorKind::NotFound,
                                field: None,
                                reason: format!("no such tool: {name}"),
                                hint: Some("check the tool manifest".into()),
                            }),
                            Some(t) => {
                                tokio::select! {
                                    biased;
                                    _ = child.cancelled() => ToolOutcome::Err(
                                        crate::tools::ToolError::transient("cancelled")
                                    ),
                                    r = tokio::time::timeout(
                                        DEFAULT_TOOL_TIMEOUT, t.dispatch(resolved, &ctx)
                                    ) => match r {
                                        Ok(o) => o,
                                        Err(_) => ToolOutcome::Err(
                                            crate::tools::ToolError::transient("tool timed out")
                                        ),
                                    }
                                }
                            }
                        };
                        (id, name, outcome)
                    });
                }
            }};
        }

        launch_ready!();

        while let Some(joined) = running.join_next().await {
            let (id, name, outcome) = match joined {
                Ok(t) => t,
                Err(e) => {
                    warn!("tool task panicked: {e}");
                    continue;
                }
            };
            let ok = matches!(outcome, ToolOutcome::Ok(_));
            match outcome {
                ToolOutcome::Ok(v) => {
                    ok_results.insert(id, v.clone());
                    ordered.push((id, name.clone(), ResultView::Ok(v)));
                }
                ToolOutcome::Err(e) => {
                    ordered.push((
                        id,
                        name.clone(),
                        ResultView::Err(serde_json::to_value(&e).unwrap()),
                    ));
                }
            }
            completed.insert(id);
            let _ = out.send(AgentEvent::ToolFinished { id, name, ok }).await;

            if cancel.is_cancelled() {
                running.abort_all();
                break;
            }
            // Newly-unlocked calls become ready now.
            launch_ready!();
        }

        // Stable output order by id for deterministic observations.
        ordered.sort_by_key(|(id, _, _)| *id);
        BatchResults { ordered }
    }
}

fn truncate(mut s: String, budget: usize) -> String {
    if s.len() > budget {
        // Truncate on a char boundary.
        let mut cut = budget;
        while !s.is_char_boundary(cut) && cut > 0 {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str("…[truncated; use read_more]");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolError, TypedTool};
    use async_trait::async_trait;

    // A tool that echoes its args and records call order via a shared counter.
    #[derive(serde::Deserialize, schemars::JsonSchema)]
    struct EchoArgs {
        tag: String,
    }
    struct Echo;
    #[async_trait]
    impl TypedTool for Echo {
        type Args = EchoArgs;
        const NAME: &'static str = "echo";
        const DESCRIPTION: &'static str = "echo tag";
        async fn run(&self, a: EchoArgs, _c: &ToolCtx) -> ToolOutcome {
            ToolOutcome::Ok(serde_json::json!({ "echoed": a.tag }))
        }
    }

    #[derive(serde::Deserialize, schemars::JsonSchema)]
    struct FailArgs {}
    struct Fail;
    #[async_trait]
    impl TypedTool for Fail {
        type Args = FailArgs;
        const NAME: &'static str = "fail";
        const DESCRIPTION: &'static str = "always fails";
        async fn run(&self, _a: FailArgs, _c: &ToolCtx) -> ToolOutcome {
            ToolOutcome::Err(ToolError::transient("boom"))
        }
    }

    fn shared() -> Arc<Shared> {
        Arc::new(Shared::for_test())
    }

    #[tokio::test]
    async fn parallel_batch_all_complete() {
        let mut reg = ToolRegistry::new();
        reg.register(Echo);
        let d = Dispatcher::new(reg, shared());
        let (tx, mut rx) = mpsc::channel(64);
        let calls = vec![
            ToolCall {
                id: 1,
                name: "echo".into(),
                args: serde_json::json!({"tag":"a"}),
            },
            ToolCall {
                id: 2,
                name: "echo".into(),
                args: serde_json::json!({"tag":"b"}),
            },
        ];
        let res = d
            .run(uuid::Uuid::new_v4(), calls, &tx, CancellationToken::new())
            .await;
        drop(tx);
        let mut started = 0;
        while let Some(ev) = rx.recv().await {
            if matches!(ev, AgentEvent::ToolStarted { .. }) {
                started += 1;
            }
        }
        assert_eq!(started, 2);
        let ok = res.ok_map();
        assert_eq!(ok[&1]["echoed"], "a");
        assert_eq!(ok[&2]["echoed"], "b");
    }

    #[tokio::test]
    async fn dependent_call_receives_substituted_value() {
        let mut reg = ToolRegistry::new();
        reg.register(Echo);
        let d = Dispatcher::new(reg, shared());
        let (tx, _rx) = mpsc::channel(64);
        // call 2 depends on call 1's "echoed" field
        let calls = vec![
            ToolCall {
                id: 1,
                name: "echo".into(),
                args: serde_json::json!({"tag":"seed"}),
            },
            ToolCall {
                id: 2,
                name: "echo".into(),
                args: serde_json::json!({"tag":"$result.1.echoed"}),
            },
        ];
        let res = d
            .run(uuid::Uuid::new_v4(), calls, &tx, CancellationToken::new())
            .await;
        assert_eq!(res.ok_map()[&2]["echoed"], "seed");
    }

    #[tokio::test]
    async fn failed_tool_becomes_error_observation() {
        let mut reg = ToolRegistry::new();
        reg.register(Fail);
        let d = Dispatcher::new(reg, shared());
        let (tx, _rx) = mpsc::channel(64);
        let calls = vec![ToolCall {
            id: 1,
            name: "fail".into(),
            args: serde_json::json!({}),
        }];
        let res = d
            .run(uuid::Uuid::new_v4(), calls, &tx, CancellationToken::new())
            .await;
        let obs = res.as_observation();
        assert!(obs.contains("error"));
        assert!(obs.contains("boom"));
    }

    #[tokio::test]
    async fn unknown_tool_is_not_found_error() {
        let reg = ToolRegistry::new();
        let d = Dispatcher::new(reg, shared());
        let (tx, _rx) = mpsc::channel(64);
        let calls = vec![ToolCall {
            id: 1,
            name: "ghost".into(),
            args: serde_json::json!({}),
        }];
        let res = d
            .run(uuid::Uuid::new_v4(), calls, &tx, CancellationToken::new())
            .await;
        assert!(res.as_observation().contains("no such tool"));
    }
}
