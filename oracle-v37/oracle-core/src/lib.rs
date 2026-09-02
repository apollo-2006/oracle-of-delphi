//! `oracle-core`: the orchestrator process. Ties together the agent loop, tool
//! registry, dual-layer memory, connectors, and the HUD gateway.
//!
//! Everything here is designed to run fully offline for tests and the demo REPL
//! (mock LLM, hashing embedder, stub connector tools), with real backends
//! (llama-server, ONNX embeddings, live Google/HA) swapping in behind traits.

pub mod agent;
pub mod ambient;
pub mod audio;
pub mod briefing;
pub mod browser;
pub mod config;
pub mod confirm;
pub mod connectors;
pub mod consolidate;
pub mod gateway;
pub mod idle;
pub mod lifecycle;
pub mod llm;
pub mod memory;
pub mod observ;
pub mod paths;
pub mod proactive;
pub mod screen;
pub mod security;
pub mod supervisor;
pub mod tiers;
pub mod tools;
pub mod workwindow;

use memory::{HashEmbedder, KnowledgeGraph, MemoryStore};
use proactive::routines::RoutineStore;
use std::sync::Arc;

/// The stream type the actd client uses on this platform: a Unix domain socket
/// on unix, a named pipe on Windows.
#[cfg(unix)]
pub type ActdStream = tokio::net::UnixStream;
#[cfg(windows)]
pub type ActdStream = tokio::net::windows::named_pipe::NamedPipeClient;
#[cfg(not(any(unix, windows)))]
pub type ActdStream = tokio::net::TcpStream;

/// Process-wide shared handles, passed to every tool via `ToolCtx`.
pub struct Shared {
    pub memory: MemoryStore,
    pub graph: KnowledgeGraph,
    /// Standing orders the user has asked for. Same SQLite file as memory.
    pub routines: RoutineStore,
    /// What happened on this machine recently, for the away briefing. In memory
    /// only: a briefing is about the last few hours, not history.
    pub events: briefing::EventLog,
    pub ha: connectors::homeassistant::EntityMirror,
    /// Live Google client, present only when Workspace auth is configured and
    /// a sealed token was loaded. `None` → the Gmail/Calendar tools return a
    /// clear "not authorized" error instead of fabricating data.
    pub google: Option<Arc<connectors::google_api::GoogleClient>>,
    /// Connected actuator-daemon client. `None` → the `os.*` tools return a
    /// clear "actd not connected" error instead of pretending.
    pub actd: Option<Arc<connectors::actd_client::ActdClient<ActdStream>>>,
    /// Where irreversible actions go for the user's decree. Defaults to
    /// [`confirm::DenyConfirmer`] (safe) until an interactive one is attached.
    pub confirmer: Arc<dyn confirm::Confirmer>,
    /// The managed web browser (Chrome via CDP) — Delphi's eyes/hands on the web.
    /// Lazily launches Chrome on first use, so it's free to always hold one.
    pub browser: Arc<browser::BrowserHandle>,
}

impl Shared {
    /// Production constructor: open the DB at `db_path` with the offline hash
    /// embedder. Retrieval is lexical; see [`Shared::open_with_embedder`].
    pub fn open(db_path: &str) -> anyhow::Result<Self> {
        Self::open_with_embedder(db_path, Box::new(HashEmbedder::default()))
    }

    /// Open with a chosen embedder — in production, the BGE sidecar.
    ///
    /// The embedder is a constructor argument rather than a setter because it
    /// determines the vector space every row is written into; swapping it after
    /// rows exist is a migration, not a configuration change.
    pub fn open_with_embedder(
        db_path: &str,
        embedder: Box<dyn memory::Embedder>,
    ) -> anyhow::Result<Self> {
        let memory = MemoryStore::open(db_path, embedder)?;
        let graph = KnowledgeGraph::open(db_path, KnowledgeGraph::default_vocab())?;
        let routines = RoutineStore::open(db_path)?;
        Ok(Shared {
            events: briefing::EventLog::new(),
            memory,
            graph,
            routines,
            ha: connectors::homeassistant::EntityMirror::new(),
            google: None,
            actd: None,
            confirmer: Arc::new(confirm::DenyConfirmer),
            browser: Arc::new(browser::BrowserHandle::new(
                browser::BrowserConfig::default(),
            )),
        })
    }

    /// Override the browser config (from oracle.toml [browser]), builder style.
    pub fn with_browser(mut self, cfg: browser::BrowserConfig) -> Self {
        self.browser = Arc::new(browser::BrowserHandle::new(cfg));
        self
    }

    /// Attach an interactive confirmer (builder style).
    pub fn with_confirmer(mut self, confirmer: Arc<dyn confirm::Confirmer>) -> Self {
        self.confirmer = confirmer;
        self
    }

    /// Attach a Google client (builder style, used from `main` after loading the
    /// sealed token).
    pub fn with_google(mut self, google: Option<connectors::google_api::GoogleClient>) -> Self {
        self.google = google.map(Arc::new);
        self
    }

    /// Attach a connected actd client (builder style).
    pub fn with_actd(
        mut self,
        actd: Option<connectors::actd_client::ActdClient<ActdStream>>,
    ) -> Self {
        self.actd = actd.map(Arc::new);
        self
    }

    /// Test/demo constructor: unique temp DB so parallel tests don't collide.
    pub fn for_test() -> Self {
        let path = format!("/tmp/oracle-core-{}.db", uuid::Uuid::new_v4());
        Self::open(&path).expect("open test db")
    }
}

/// Build the tool registry: memory + KG + Google/IoT + OS-control tools.
pub fn demo_registry() -> tools::ToolRegistry {
    let mut reg = tools::ToolRegistry::new();
    tools::builtin::register_all(&mut reg);
    tools::os_tools::register_all(&mut reg);
    tools::browser_tools::register_all(&mut reg);
    reg
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::agent::{Agent, AgentConfig, AgentEvent};
    use crate::llm::MockLlm;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// End-to-end: the scripted demo request drives a real tool DAG (parallel
    /// fan-out + one dependent draft) and produces a spoken summary — all with
    /// no GPU, no model, no network. Without Google configured, the workspace
    /// tools dispatch and return "not authorized" (the DAG mechanics — parallel
    /// fan-out, `$result.N` dependency gating — are what this exercises); the
    /// local light tool still succeeds.
    #[tokio::test]
    async fn full_turn_runs_tool_dag_and_speaks() {
        let shared = Arc::new(Shared::for_test());
        let agent = Agent::new(
            Arc::new(MockLlm::demo()),
            demo_registry(),
            shared,
            AgentConfig::default(),
        );
        let (tx, mut rx) = mpsc::channel(256);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(async move {
            agent
                .run_turn(
                    "check unread from my advisor, find 30 min tomorrow afternoon, draft a reply, dim my lights"
                        .into(),
                    tx,
                    cancel,
                )
                .await
        });

        let mut said = String::new();
        let mut tools_started = 0;
        let mut tools_done = 0;
        let mut finished_cancelled = None;
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::Say(s) => said.push_str(&s),
                AgentEvent::ToolStarted { .. } => tools_started += 1,
                AgentEvent::ToolFinished { ok, .. } => {
                    if ok {
                        tools_done += 1
                    }
                }
                AgentEvent::Finished { cancelled } => finished_cancelled = Some(cancelled),
            }
        }
        handle.await.unwrap().unwrap();

        // The protocol calls one tool per step: gmail.search (errors without
        // Google) then home_assistant.light (succeeds locally), then a spoken
        // summary — so at least two tools start, at least one succeeds, and the
        // turn ends with the summary.
        assert!(
            tools_started >= 2,
            "tools should dispatch across steps (got {tools_started})"
        );
        assert!(tools_done >= 1, "at least the light tool should succeed");
        assert!(said.contains("advisor"), "spoke a summary: {said}");
        assert_eq!(finished_cancelled, Some(false));
    }

    /// Barge-in: cancelling mid-turn ends it promptly as cancelled.
    #[tokio::test]
    async fn barge_in_cancels_turn() {
        let shared = Arc::new(Shared::for_test());
        let agent = Agent::new(
            Arc::new(MockLlm::saying(
                "this is a long spoken answer that the user interrupts partway through",
            )),
            demo_registry(),
            shared,
            AgentConfig::default(),
        );
        let (tx, mut rx) = mpsc::channel(256);
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();

        let handle = tokio::spawn(async move {
            agent
                .run_turn("tell me a long story".into(), tx, cancel2)
                .await
        });

        // Cancel almost immediately (barge-in).
        cancel.cancel();

        let mut finished_cancelled = None;
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::Finished { cancelled } = ev {
                finished_cancelled = Some(cancelled);
            }
        }
        handle.await.unwrap().unwrap();
        assert_eq!(finished_cancelled, Some(true));
    }
}
