//! The consolidation pass end to end, against a real SQLite store and graph.
//!
//! The model is stubbed — CI has no small tier — but everything either side of
//! it is real: pending-episode selection, the graph assertion path with its
//! vocabulary check, marking, and the interaction with retention. Those are the
//! parts that rot silently; the model call is the part that needs hardware.

use std::sync::Arc;

use futures::stream::{self, BoxStream};
use oracle_core::config::ConsolidateConfig;
use oracle_core::consolidate::{self, run_pass};
use oracle_core::llm::{Llm, LlmDelta, LlmRequest, StopReason};
use oracle_core::memory::graph::KgQuery;
use oracle_core::memory::EpisodeKind;
use oracle_core::Shared;
use tokio_util::sync::CancellationToken;

/// An LLM that replies with a fixed string, and records the prompt it saw.
struct Canned {
    reply: String,
    seen: std::sync::Mutex<Option<LlmRequest>>,
}

impl Canned {
    fn new(reply: &str) -> Arc<Self> {
        Arc::new(Canned {
            reply: reply.to_string(),
            seen: std::sync::Mutex::new(None),
        })
    }
}

#[async_trait::async_trait]
impl Llm for Canned {
    async fn generate(
        &self,
        req: LlmRequest,
        _cancel: CancellationToken,
    ) -> anyhow::Result<BoxStream<'static, LlmDelta>> {
        *self.seen.lock().unwrap() = Some(req);
        let deltas = vec![
            LlmDelta::Text(self.reply.clone()),
            LlmDelta::Done {
                stop_reason: StopReason::Stop,
            },
        ];
        Ok(Box::pin(stream::iter(deltas)))
    }
}

/// An LLM that always fails, standing in for a sidecar that is down.
struct Broken;

#[async_trait::async_trait]
impl Llm for Broken {
    async fn generate(
        &self,
        _req: LlmRequest,
        _cancel: CancellationToken,
    ) -> anyhow::Result<BoxStream<'static, LlmDelta>> {
        anyhow::bail!("the small tier is not answering")
    }
}

fn cfg() -> ConsolidateConfig {
    ConsolidateConfig {
        enabled: true,
        poll_secs: 1,
        batch_size: 10,
        max_tokens: 200,
        from_observations: true,
    }
}

#[tokio::test]
async fn a_fact_survives_the_episode_it_came_from() {
    // The whole point: the episode expires, the knowledge does not.
    let shared = Arc::new(Shared::for_test());
    shared
        .memory
        .insert(
            EpisodeKind::Conversation,
            "my advisor is Dr Chen and she wants the draft by Friday",
            0.8,
        )
        .unwrap();

    let llm: Arc<dyn Llm> = Canned::new(r#"[{"subj":"User","rel":"advisor","obj":"Dr Chen"}]"#);
    let r = run_pass(&cfg(), &llm, &shared, CancellationToken::new())
        .await
        .unwrap()
        .expect("a batch was available");
    assert_eq!(r.facts_asserted, 1);
    assert_eq!(r.facts_rejected, 0);

    // Now sweep every episode away and confirm the fact is still there.
    let purged = shared.memory.purge_observations_before(i64::MAX).unwrap();
    let _ = purged;
    let edges = shared
        .graph
        .query(KgQuery::Neighbors {
            entity: "User".into(),
            rel: Some("advisor".into()),
            depth: 1,
        })
        .unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].dst, "Dr Chen");
}

#[tokio::test]
async fn an_observation_derived_fact_is_marked_as_coming_from_the_screen() {
    // Provenance is the only thing that later distinguishes a fact the user
    // stated from one a web page put on screen.
    let shared = Arc::new(Shared::for_test());
    shared
        .memory
        .insert(
            EpisodeKind::Observation,
            "On screen (mail): a message from Dr Chen about the draft",
            0.25,
        )
        .unwrap();

    let llm: Arc<dyn Llm> = Canned::new(r#"[{"subj":"draft","rel":"from","obj":"Dr Chen"}]"#);
    run_pass(&cfg(), &llm, &shared, CancellationToken::new())
        .await
        .unwrap()
        .unwrap();

    let edges = shared
        .graph
        .query(KgQuery::Neighbors {
            entity: "draft".into(),
            rel: Some("from".into()),
            depth: 1,
        })
        .unwrap();
    assert_eq!(edges.len(), 1);
    let prov = shared
        .graph
        .edge_provenance("draft", "from", "Dr Chen")
        .expect("the edge records where it came from");
    assert!(prov.contains("/screen"), "provenance was {prov:?}");
}

#[tokio::test]
async fn a_relation_outside_the_vocabulary_is_rejected_not_invented() {
    let shared = Arc::new(Shared::for_test());
    shared
        .memory
        .insert(EpisodeKind::Conversation, "Dr Chen supervises me", 0.8)
        .unwrap();

    let llm: Arc<dyn Llm> = Canned::new(r#"[{"subj":"User","rel":"supervisor","obj":"Dr Chen"}]"#);
    let r = run_pass(&cfg(), &llm, &shared, CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.facts_asserted, 0);
    assert_eq!(r.facts_rejected, 1);
}

#[tokio::test]
async fn the_request_is_grammar_constrained_to_the_vocabulary() {
    let shared = Arc::new(Shared::for_test());
    shared
        .memory
        .insert(EpisodeKind::Conversation, "anything", 0.5)
        .unwrap();

    let canned = Canned::new("[]");
    let llm: Arc<dyn Llm> = canned.clone();
    run_pass(&cfg(), &llm, &shared, CancellationToken::new())
        .await
        .unwrap();

    let req = canned
        .seen
        .lock()
        .unwrap()
        .clone()
        .expect("a request went out");
    let grammar = req
        .grammar
        .expect("consolidation must constrain its output");
    assert!(grammar.contains("rel ::="));
    assert!(grammar.contains(r#""\"advisor\"""#));
    // No tools: this pass reads and writes rows, it never acts.
    assert_eq!(req.temperature, 0.0, "extraction must not be creative");
}

#[tokio::test]
async fn a_barren_batch_is_still_marked_so_it_is_not_reread_forever() {
    let shared = Arc::new(Shared::for_test());
    for i in 0..3 {
        shared
            .memory
            .insert(
                EpisodeKind::Observation,
                &format!("On screen: tab {i}"),
                0.2,
            )
            .unwrap();
    }

    let llm: Arc<dyn Llm> = Canned::new("[]");
    let r = run_pass(&cfg(), &llm, &shared, CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.episodes_read, 3);
    assert_eq!(r.facts_asserted, 0);
    assert_eq!(
        shared.memory.unconsolidated_count().unwrap(),
        0,
        "yielding no facts is an answer, not a reason to retry forever"
    );

    // And a second pass has nothing to do.
    assert!(run_pass(&cfg(), &llm, &shared, CancellationToken::new())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn a_failed_model_call_defers_the_batch_rather_than_consuming_it() {
    // A sidecar that is down must not silently burn through the backlog.
    let shared = Arc::new(Shared::for_test());
    shared
        .memory
        .insert(EpisodeKind::Conversation, "my advisor is Dr Chen", 0.8)
        .unwrap();

    let llm: Arc<dyn Llm> = Arc::new(Broken);
    assert!(run_pass(&cfg(), &llm, &shared, CancellationToken::new())
        .await
        .is_err());
    assert_eq!(
        shared.memory.unconsolidated_count().unwrap(),
        1,
        "the batch must still be pending after a failure"
    );
}

#[tokio::test]
async fn observations_can_be_excluded_from_the_source() {
    // For anyone who would rather the graph never learn from a screen.
    let shared = Arc::new(Shared::for_test());
    shared
        .memory
        .insert(EpisodeKind::Observation, "On screen: something", 0.2)
        .unwrap();

    let mut c = cfg();
    c.from_observations = false;
    let llm: Arc<dyn Llm> = Canned::new(r#"[{"subj":"a","rel":"owns","obj":"b"}]"#);
    assert!(
        run_pass(&c, &llm, &shared, CancellationToken::new())
            .await
            .unwrap()
            .is_none(),
        "with observations excluded there was nothing left to read"
    );
    // And the observation stays pending rather than being marked read.
    assert_eq!(shared.memory.unconsolidated_count().unwrap(), 1);
}

#[tokio::test]
async fn entries_reach_the_model_labelled_by_kind() {
    let shared = Arc::new(Shared::for_test());
    shared
        .memory
        .insert(EpisodeKind::Conversation, "my advisor is Dr Chen", 0.8)
        .unwrap();
    shared
        .memory
        .insert(EpisodeKind::Observation, "On screen: a paper", 0.2)
        .unwrap();

    let canned = Canned::new("[]");
    let llm: Arc<dyn Llm> = canned.clone();
    run_pass(&cfg(), &llm, &shared, CancellationToken::new())
        .await
        .unwrap();

    let req = canned.seen.lock().unwrap().clone().unwrap();
    let prompt = &req.messages[0].content;
    assert!(prompt.contains("[said]"), "{prompt}");
    assert!(prompt.contains("[seen on screen]"), "{prompt}");
    // The system prompt must tell the model those entries are data.
    assert!(req.system.contains("DATA"));
}

#[test]
fn the_grammar_is_well_formed_for_the_shipped_vocabulary() {
    let vocab = oracle_core::memory::KnowledgeGraph::default_vocab();
    let g = consolidate::fact_grammar(&vocab);
    // One production per required rule; a missing one makes llama.cpp reject
    // the whole grammar at request time, which surfaces as every pass failing.
    for rule in ["root ::=", "fact ::=", "rel ::=", "string ::=", "ws ::="] {
        assert!(g.contains(rule), "missing {rule} in:\n{g}");
    }
}
