//! Observability (architecture §0.4, §7): the latency-budget instrumentation.
//!
//! Every stage of a voice turn (endpoint → ASR → prompt → prefill → first
//! token → TTS first chunk → output) records into an HDR histogram keyed by
//! stage. `oracle-core doctor` reads these back, compares p50/p95/p99 against the
//! design budget, and names the stage that's blowing it — so a regression is a
//! one-line diagnosis, not a profiling expedition.

use hdrhistogram::Histogram;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// The pipeline stages we budget, in order (architecture §0.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    Endpoint,
    AsrFinal,
    PromptAssembly,
    LlmPrefill,
    FirstTokenClause,
    TtsFirstChunk,
    OutputDevice,
}

impl Stage {
    pub fn name(&self) -> &'static str {
        match self {
            Stage::Endpoint => "endpoint_detection",
            Stage::AsrFinal => "asr_final_flush",
            Stage::PromptAssembly => "prompt_assembly",
            Stage::LlmPrefill => "llm_prefill",
            Stage::FirstTokenClause => "first_token_clause",
            Stage::TtsFirstChunk => "tts_first_chunk",
            Stage::OutputDevice => "output_device",
        }
    }

    /// The design budget ceiling for this stage, in milliseconds (the upper end
    /// of the range from the architecture doc's latency table).
    pub fn budget_ms(&self) -> u64 {
        match self {
            Stage::Endpoint => 200,
            Stage::AsrFinal => 40,
            Stage::PromptAssembly => 15,
            Stage::LlmPrefill => 120,
            Stage::FirstTokenClause => 120,
            Stage::TtsFirstChunk => 90,
            Stage::OutputDevice => 30,
        }
    }

    pub fn all() -> [Stage; 7] {
        [
            Stage::Endpoint,
            Stage::AsrFinal,
            Stage::PromptAssembly,
            Stage::LlmPrefill,
            Stage::FirstTokenClause,
            Stage::TtsFirstChunk,
            Stage::OutputDevice,
        ]
    }
}

/// The total end-to-end budget target (p95), from §0.4.
pub const TOTAL_BUDGET_P95_MS: u64 = 575;

/// Thread-safe collection of per-stage latency histograms.
pub struct LatencyRecorder {
    hists: Mutex<BTreeMap<&'static str, Histogram<u64>>>,
}

impl Default for LatencyRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyRecorder {
    pub fn new() -> Self {
        LatencyRecorder {
            hists: Mutex::new(BTreeMap::new()),
        }
    }

    /// Record a stage duration in milliseconds.
    pub fn record(&self, stage: Stage, ms: u64) {
        let mut g = self.hists.lock().unwrap();
        let h = g.entry(stage.name()).or_insert_with(|| {
            // 1ms..60s range, 3 significant figures.
            Histogram::<u64>::new_with_bounds(1, 60_000, 3).unwrap()
        });
        let _ = h.record(ms.max(1));
    }

    /// Produce a per-stage report with p50/p95/p99 and a pass/fail vs budget.
    pub fn report(&self) -> DoctorReport {
        let g = self.hists.lock().unwrap();
        let mut stages = Vec::new();
        let mut sum_p95 = 0u64;
        for stage in Stage::all() {
            if let Some(h) = g.get(stage.name()) {
                let p50 = h.value_at_quantile(0.50);
                let p95 = h.value_at_quantile(0.95);
                let p99 = h.value_at_quantile(0.99);
                sum_p95 += p95;
                stages.push(StageStat {
                    name: stage.name(),
                    p50,
                    p95,
                    p99,
                    budget_ms: stage.budget_ms(),
                    over_budget: p95 > stage.budget_ms(),
                    samples: h.len(),
                });
            }
        }
        DoctorReport {
            stages,
            total_p95_ms: sum_p95,
            total_budget_ms: TOTAL_BUDGET_P95_MS,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StageStat {
    pub name: &'static str,
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub budget_ms: u64,
    pub over_budget: bool,
    pub samples: u64,
}

#[derive(Debug, Clone)]
pub struct DoctorReport {
    pub stages: Vec<StageStat>,
    pub total_p95_ms: u64,
    pub total_budget_ms: u64,
}

impl DoctorReport {
    /// The worst offender: the over-budget stage that most exceeds its ceiling.
    pub fn worst_offender(&self) -> Option<&StageStat> {
        self.stages
            .iter()
            .filter(|s| s.over_budget)
            .max_by_key(|s| s.p95.saturating_sub(s.budget_ms))
    }

    pub fn within_budget(&self) -> bool {
        self.total_p95_ms <= self.total_budget_ms && self.worst_offender().is_none()
    }

    /// Human-readable, for `oracle-core doctor`.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("Oracle of Delphi — latency report (voice turn, p95)\n");
        out.push_str("─────────────────────────────────────────────\n");
        out.push_str(&format!(
            "{:<22} {:>6} {:>6} {:>6} {:>7}  {}\n",
            "stage", "p50", "p95", "p99", "budget", "status"
        ));
        for s in &self.stages {
            out.push_str(&format!(
                "{:<22} {:>5}m {:>5}m {:>5}m {:>6}m  {}\n",
                s.name,
                s.p50,
                s.p95,
                s.p99,
                s.budget_ms,
                if s.over_budget { "OVER" } else { "ok" },
            ));
        }
        out.push_str("─────────────────────────────────────────────\n");
        out.push_str(&format!(
            "total p95: {}ms / {}ms budget — {}\n",
            self.total_p95_ms,
            self.total_budget_ms,
            if self.within_budget() {
                "WITHIN BUDGET"
            } else {
                "OVER BUDGET"
            }
        ));
        if let Some(w) = self.worst_offender() {
            out.push_str(&format!(
                "worst offender: {} (p95 {}ms, {}ms over ceiling)\n",
                w.name,
                w.p95,
                w.p95 - w.budget_ms
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_pipeline_is_within_budget() {
        let r = LatencyRecorder::new();
        // Record realistic in-budget samples for each stage.
        for _ in 0..100 {
            r.record(Stage::Endpoint, 160);
            r.record(Stage::AsrFinal, 30);
            r.record(Stage::PromptAssembly, 10);
            r.record(Stage::LlmPrefill, 90);
            r.record(Stage::FirstTokenClause, 100);
            r.record(Stage::TtsFirstChunk, 70);
            r.record(Stage::OutputDevice, 20);
        }
        let report = r.report();
        assert!(report.within_budget(), "{}", report.render());
        assert!(report.worst_offender().is_none());
    }

    #[test]
    fn doctor_names_the_worst_offender() {
        let r = LatencyRecorder::new();
        for _ in 0..100 {
            r.record(Stage::Endpoint, 160);
            r.record(Stage::LlmPrefill, 400); // blown: budget 120
            r.record(Stage::TtsFirstChunk, 200); // blown: budget 90, but less over
        }
        let report = r.report();
        assert!(!report.within_budget());
        let worst = report.worst_offender().unwrap();
        assert_eq!(worst.name, "llm_prefill"); // 280 over vs tts 110 over
    }

    #[test]
    fn report_renders_without_panicking() {
        let r = LatencyRecorder::new();
        r.record(Stage::Endpoint, 150);
        let s = r.report().render();
        assert!(s.contains("endpoint_detection"));
        assert!(s.contains("budget"));
    }
}
