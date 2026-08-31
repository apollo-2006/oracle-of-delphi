//! Two model tiers, and the rule for which one a caller gets.
//!
//! Oracle used to have exactly one model, and it was a 14B. That made every
//! design decision downstream a compromise: continuous background work was
//! impossible (you cannot hold 11 GB all day to summarize a window), so there
//! was no continuous background work, so the model had nothing to do between
//! turns, so unloading it was the only sane policy — and an assistant whose
//! model is usually unloaded is an assistant that does nothing when you are not
//! looking at it.
//!
//! Splitting the tier breaks that cycle:
//!
//! * **Big** (`[llm]`) — the 14B planner. Loaded on demand for a real turn or
//!   the away briefing, unloaded when idle, exactly as before. Nothing about its
//!   lifecycle changes.
//! * **Small** (`[llm.small]`) — a 2B-class model, resident. Cheap enough to
//!   leave running (~2.5 GB at Q4), which is what makes it able to do work that
//!   arrives continuously: reading the screen, folding episodes into the graph.
//!
//! The tiers are two `llama-server` processes on two ports, not two modes of one
//! server. That is deliberate — llama.cpp holds one model per server, and the
//! whole point is for one of them to be able to die while the other lives.
//!
//! ## Choosing a tier is a policy decision, not a fallback
//!
//! [`LlmTiers::small`] returns `None` when the small tier is off. Callers must
//! decide what that means for them, and the answer is usually "do nothing":
//! silently promoting ambient screen summarization to the 14B would reload 11 GB
//! of VRAM every few seconds, which is worse than the feature being absent. Only
//! callers that are genuinely tier-agnostic should fall back, and they should
//! say so at the call site.

use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::idle::LlmLifecycle;
use crate::llm::{LlamaServer, Llm, MockLlm};
use crate::supervisor::ChildHandle;

/// Which model a caller wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The 14B planner: turns, the away briefing. On-demand, idle-unloaded.
    Big,
    /// The resident small/vision model: ambient capture, consolidation.
    Small,
}

impl Tier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::Big => "big",
            Tier::Small => "small",
        }
    }
}

/// One tier's handle: the client, plus the lifecycle that keeps its server up.
struct TierHandle {
    llm: Arc<dyn Llm>,
    life: Arc<LlmLifecycle>,
}

/// Both tiers, and their lifecycles.
pub struct LlmTiers {
    big: TierHandle,
    /// None when `[llm.small] enabled = false` — the common case on a first run.
    small: Option<TierHandle>,
}

impl LlmTiers {
    /// Build from config and the supervisor's child handles.
    ///
    /// `big_child`/`small_child` are `None` when that tier is not supervised
    /// (mock backend, or the user starts the server themselves); the lifecycle
    /// then degrades to "assume it is up", which is the pre-existing behaviour.
    pub fn new(
        cfg: &Config,
        big_child: Option<ChildHandle>,
        small_child: Option<ChildHandle>,
    ) -> Self {
        let big = TierHandle {
            llm: build_client(&cfg.llm.backend, &cfg.llm.model, Tier::Big),
            life: Arc::new(LlmLifecycle::new(
                big_child,
                &cfg.llm.backend,
                Duration::from_secs(cfg.supervise.llm_ready_timeout_secs),
            )),
        };

        let small = if cfg.llm.small.enabled {
            Some(TierHandle {
                llm: build_client(&cfg.llm.small.backend, &cfg.llm.small.model, Tier::Small),
                life: Arc::new(LlmLifecycle::new(
                    small_child,
                    &cfg.llm.small.backend,
                    Duration::from_secs(cfg.supervise.small_llm_ready_timeout_secs),
                )),
            })
        } else {
            None
        };

        LlmTiers { big, small }
    }

    /// The planner. Always present.
    pub fn big(&self) -> Arc<dyn Llm> {
        self.big.llm.clone()
    }

    /// The resident small model, or None when the tier is disabled.
    ///
    /// Callers that would rather do nothing than wake the 14B — which is most
    /// background callers — should treat `None` as "feature off".
    pub fn small(&self) -> Option<Arc<dyn Llm>> {
        self.small.as_ref().map(|t| t.llm.clone())
    }

    pub fn small_enabled(&self) -> bool {
        self.small.is_some()
    }

    /// The lifecycle for one tier, for callers that manage load/unload directly
    /// (the idle work window, the turn path).
    pub fn lifecycle(&self, tier: Tier) -> Option<Arc<LlmLifecycle>> {
        match tier {
            Tier::Big => Some(self.big.life.clone()),
            Tier::Small => self.small.as_ref().map(|t| t.life.clone()),
        }
    }

    /// Ensure a tier's server is up and answering before a request goes out.
    ///
    /// True when the tier is absent: "not configured" is not "failed to load",
    /// and a caller that already checked [`Self::small`] should not have to
    /// handle a second flavour of absence.
    pub async fn ensure_ready(&self, tier: Tier) -> bool {
        match self.lifecycle(tier) {
            Some(life) => life.ensure_ready().await,
            None => true,
        }
    }

    /// Unload the big tier, leaving a resident small tier running.
    ///
    /// This is the whole shape of the new idle policy in one method: idleness
    /// releases the 11 GB and keeps the 2.5 GB, because only one of those is
    /// worth reloading and only one of them has work to do while you are away.
    pub fn unload_big(&self) -> bool {
        self.big.life.unload()
    }
}

/// One tier's client. Mock and real share a construction site so a tier can be
/// pointed at "mock" independently of the other — which is how the ambient
/// pipeline gets tested without a VLM download.
fn build_client(backend: &str, model: &str, tier: Tier) -> Arc<dyn Llm> {
    if backend == "mock" {
        eprintln!("[oracle] {} LLM backend: mock (offline)", tier.as_str());
        Arc::new(MockLlm::demo())
    } else {
        eprintln!(
            "[oracle] {} LLM backend: {} (model {})",
            tier.as_str(),
            backend,
            model
        );
        Arc::new(LlamaServer::new(backend.to_string(), model.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn cfg_with_small(enabled: bool) -> Config {
        let mut cfg = Config::default();
        cfg.llm.backend = "mock".into();
        cfg.llm.small.enabled = enabled;
        cfg.llm.small.backend = "mock".into();
        cfg
    }

    #[test]
    fn the_small_tier_is_absent_until_it_is_enabled() {
        let tiers = LlmTiers::new(&cfg_with_small(false), None, None);
        assert!(tiers.small().is_none());
        assert!(!tiers.small_enabled());
        // The planner is never optional.
        assert!(tiers.lifecycle(Tier::Big).is_some());
        assert!(tiers.lifecycle(Tier::Small).is_none());
    }

    #[test]
    fn enabling_the_small_tier_yields_a_second_client() {
        let tiers = LlmTiers::new(&cfg_with_small(true), None, None);
        assert!(tiers.small().is_some());
        assert!(tiers.small_enabled());
        assert!(tiers.lifecycle(Tier::Small).is_some());
    }

    #[tokio::test]
    async fn an_absent_tier_is_ready_rather_than_failed() {
        // "Not configured" must not read as "failed to load" at a call site
        // that already decided it can live without the tier.
        let tiers = LlmTiers::new(&cfg_with_small(false), None, None);
        assert!(tiers.ensure_ready(Tier::Small).await);
    }

    #[test]
    fn a_shared_backend_is_rejected_before_two_servers_fight_over_a_port() {
        let mut cfg = Config::default();
        cfg.llm.backend = "http://127.0.0.1:8080".into();
        cfg.llm.small.enabled = true;
        cfg.llm.small.backend = "http://127.0.0.1:8080/".into();
        let err = cfg.validate().expect_err("same endpoint must not validate");
        assert!(
            format!("{err}").contains("two servers on two ports"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_disabled_small_tier_does_not_have_to_be_configured() {
        let mut cfg = Config::default();
        cfg.llm.backend = "mock".into();
        cfg.llm.small.enabled = false;
        cfg.llm.small.backend = String::new();
        assert!(cfg.validate().is_ok());
    }
}
