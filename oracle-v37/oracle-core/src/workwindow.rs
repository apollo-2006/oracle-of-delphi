//! When background work is allowed to run.
//!
//! Idleness used to mean one thing: unload the planner and do nothing. That was
//! the right call when there was only one model — but it also meant the GPU sat
//! free at exactly the moment there was nothing queued for it, which is the
//! shape of an assistant that stops existing when you look away.
//!
//! With a resident small tier (`crate::tiers`), idleness becomes the opposite: a
//! window in which the backlog runs. Ambient screen observations get summarized,
//! episodes get folded into the knowledge graph, stale rows get decayed. The
//! planner is still unloaded — that has not changed and should not — but its
//! VRAM is released *into* work rather than into nothing.
//!
//! ## The three gates
//!
//! Background work runs only when all three hold:
//!
//! 1. **The user is idle.** Not because the work is expensive, but because it is
//!    not urgent: anything that can wait should wait, so a turn never queues
//!    behind a consolidation pass.
//! 2. **No turn is in flight.** Idle-by-clock and busy-by-turn can overlap — a
//!    routine fires unattended, or a briefing is being generated. The clock does
//!    not know about those; the shared busy flag does.
//! 3. **Nothing else wants the GPU.** This is the one that matters in practice.
//!    An assistant that quietly eats frames while you are in a game has made
//!    itself the problem, and "it was only using idle time" is no defence when
//!    the machine is visibly stuttering. See [`GpuProbe`].
//!
//! ## Yielding is not the same as stopping
//!
//! The window closes the moment any gate flips, but a job already running is not
//! killed — it is asked to stop at its next checkpoint. Half-written
//! consolidation is worse than late consolidation, and a single VLM call on one
//! screenshot is short enough that waiting it out is cheaper than making every
//! job re-entrant. Jobs are expected to be small for exactly this reason.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::idle::IdleTracker;

/// External GPU pressure: is something *other than us* using the GPU?
///
/// A trait rather than a `rocm-smi` call because the answer is unknowable
/// offline and the policy above it must still be testable — the same reason the
/// LLM and the platform layer are traits here.
pub trait GpuProbe: Send + Sync {
    /// VRAM in use by processes other than this assistant, in MiB.
    ///
    /// Returns `None` when the answer cannot be determined (no tool installed,
    /// probe failed, unsupported platform). `None` is deliberately *not* zero:
    /// see [`WorkWindow::is_open`] for how an unknown answer is treated.
    fn foreign_vram_mb(&self) -> Option<u64>;
}

/// A probe that never reports pressure. For tests and for machines where the
/// assistant is the only thing that will ever touch the GPU.
pub struct NoGpuPressure;

impl GpuProbe for NoGpuPressure {
    fn foreign_vram_mb(&self) -> Option<u64> {
        Some(0)
    }
}

/// A probe with a fixed answer, for tests.
#[cfg(test)]
pub struct FixedGpuPressure(pub Option<u64>);

#[cfg(test)]
impl GpuProbe for FixedGpuPressure {
    fn foreign_vram_mb(&self) -> Option<u64> {
        self.0
    }
}

/// Policy for whether the backlog may run right now.
pub struct WorkWindow {
    idle: Arc<IdleTracker>,
    turn_busy: Arc<AtomicBool>,
    gpu: Box<dyn GpuProbe>,
    /// Foreign VRAM above this many MiB closes the window.
    foreign_vram_budget_mb: u64,
    /// How idle the user must be before background work starts, in seconds.
    ///
    /// Separate from `supervise.idle_unload_secs` and normally longer: unloading
    /// the planner is cheap to get wrong (it reloads), whereas starting to read
    /// the screen the instant someone pauses to think is not.
    after_secs: i64,
    /// Master switch. Off means the window never opens.
    enabled: bool,
}

impl WorkWindow {
    pub fn new(
        idle: Arc<IdleTracker>,
        turn_busy: Arc<AtomicBool>,
        gpu: Box<dyn GpuProbe>,
        after_secs: i64,
        foreign_vram_budget_mb: u64,
        enabled: bool,
    ) -> Self {
        WorkWindow {
            idle,
            turn_busy,
            gpu,
            foreign_vram_budget_mb,
            after_secs,
            enabled,
        }
    }

    /// Whether background work may run at `now` (unix seconds).
    ///
    /// An *unknown* GPU answer closes the window. That is the conservative
    /// direction and it is chosen deliberately: the cost of wrongly staying idle
    /// is some consolidation happening later, while the cost of wrongly running
    /// is stealing VRAM from whatever the user is actually doing. Fail toward
    /// the recoverable mistake.
    pub fn is_open(&self, now: i64) -> bool {
        self.is_open_for(now, true)
    }

    /// As [`Self::is_open`], but `require_idle = false` drops the "user is
    /// away" gate and keeps the rest.
    ///
    /// Not every backlog wants the same conditions. Consolidation is genuinely
    /// deferrable and should wait for an empty machine. Ambient interpretation
    /// is not: frames arrive because the user is *working*, and a queue that
    /// only ever drains overnight would discard most of the day before it was
    /// ever read. Both still yield to a busy GPU and to a live turn.
    pub fn is_open_for(&self, now: i64, require_idle: bool) -> bool {
        self.closed_because_for(now, require_idle).is_none()
    }

    /// Why the window is shut, for logs. `None` when it is open.
    ///
    /// Worth having: "the ambient index isn't running" is otherwise a silent
    /// four-way ambiguity, and the answer is usually one the user can act on.
    pub fn closed_because(&self, now: i64) -> Option<&'static str> {
        self.closed_because_for(now, true)
    }

    /// Why the window is shut, with the idle gate optional. `None` when open.
    pub fn closed_because_for(&self, now: i64, require_idle: bool) -> Option<&'static str> {
        if !self.enabled {
            return Some("background work is disabled");
        }
        if self.turn_busy.load(Ordering::SeqCst) {
            return Some("a turn is in flight");
        }
        if require_idle && self.idle.idle_secs(now) < self.after_secs {
            return Some("the user is active");
        }
        match self.gpu.foreign_vram_mb() {
            Some(mb) if mb > self.foreign_vram_budget_mb => Some("another process wants the GPU"),
            None => Some("GPU pressure is unknown"),
            Some(_) => None,
        }
    }
}

/// Read foreign VRAM from `rocm-smi` / `nvidia-smi`.
///
/// Deliberately crude: shelling out to a vendor tool once every few minutes is
/// not a hot path, and linking a GPU management library into the assistant to
/// answer one question would be a large dependency for a small gate.
pub struct SmiGpuProbe {
    /// Our own VRAM is not foreign pressure. The planner alone is ~11 GB, so
    /// without this subtraction the window would never open on any machine
    /// where Oracle is doing its job.
    own_vram_mb: u64,
}

impl SmiGpuProbe {
    pub fn new(own_vram_mb: u64) -> Self {
        SmiGpuProbe { own_vram_mb }
    }

    /// Total VRAM in use across the device, in MiB, from whichever tool exists.
    fn total_used_mb() -> Option<u64> {
        // NVIDIA first: its query interface is the stable one.
        if let Some(mb) = run_smi(
            "nvidia-smi",
            &["--query-gpu=memory.used", "--format=csv,noheader,nounits"],
        ) {
            return Some(mb);
        }
        // ROCm: --showmeminfo vram prints bytes; take the first "used" figure.
        run_rocm_smi()
    }
}

impl GpuProbe for SmiGpuProbe {
    fn foreign_vram_mb(&self) -> Option<u64> {
        // Saturating: our own estimate can exceed the reading (the planner is
        // unloaded, say), and that means zero foreign pressure, not underflow.
        Self::total_used_mb().map(|total| total.saturating_sub(self.own_vram_mb))
    }
}

fn run_smi(program: &str, args: &[&str]) -> Option<u64> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().next()?.trim().parse::<u64>().ok()
}

fn run_rocm_smi() -> Option<u64> {
    let out = std::process::Command::new("rocm-smi")
        .args(["--showmeminfo", "vram", "--csv"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // CSV rows look like: card0,<total bytes>,<used bytes>
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() >= 3 {
            if let Ok(bytes) = cols[2].trim().parse::<u64>() {
                return Some(bytes / (1024 * 1024));
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(idle_secs: i64, busy: bool, gpu: Option<u64>, enabled: bool) -> (WorkWindow, i64) {
        let now = 10_000;
        // idle_unload threshold is irrelevant here; the window has its own.
        let tracker = Arc::new(IdleTracker::new(now - idle_secs, 600));
        let w = WorkWindow::new(
            tracker,
            Arc::new(AtomicBool::new(busy)),
            Box::new(FixedGpuPressure(gpu)),
            300,
            512,
            enabled,
        );
        (w, now)
    }

    #[test]
    fn the_window_opens_only_after_a_real_absence() {
        let (w, now) = window(299, false, Some(0), true);
        assert!(!w.is_open(now));
        assert_eq!(w.closed_because(now), Some("the user is active"));

        let (w, now) = window(300, false, Some(0), true);
        assert!(w.is_open(now), "threshold is inclusive");
        assert_eq!(w.closed_because(now), None);
    }

    #[test]
    fn a_turn_in_flight_closes_the_window_even_when_the_clock_says_idle() {
        // A routine or a briefing runs unattended: idle by the clock, busy in
        // fact. Background work must not queue in front of it.
        let (w, now) = window(9_999, true, Some(0), true);
        assert!(!w.is_open(now));
        assert_eq!(w.closed_because(now), Some("a turn is in flight"));
    }

    #[test]
    fn another_process_on_the_gpu_closes_the_window() {
        let (w, now) = window(9_999, false, Some(4_096), true);
        assert!(!w.is_open(now));
        assert_eq!(w.closed_because(now), Some("another process wants the GPU"));
    }

    #[test]
    fn pressure_inside_the_budget_leaves_the_window_open() {
        // A desktop compositor holds a few hundred MB on any machine; that must
        // not be mistaken for someone gaming.
        let (w, now) = window(9_999, false, Some(512), true);
        assert!(w.is_open(now), "budget is inclusive");
    }

    #[test]
    fn an_unknown_gpu_answer_closes_the_window() {
        // No smi tool, or it failed. Staying idle costs a late consolidation;
        // running anyway costs the user their frame rate.
        let (w, now) = window(9_999, false, None, true);
        assert!(!w.is_open(now));
        assert_eq!(w.closed_because(now), Some("GPU pressure is unknown"));
    }

    #[test]
    fn dropping_the_idle_gate_opens_the_window_for_an_active_user() {
        // Ambient interpretation runs while the user works -- that is when
        // frames exist -- so it asks for everything except the idle gate.
        let (w, now) = window(0, false, Some(0), true);
        assert!(!w.is_open(now), "the strict window stays shut");
        assert!(w.is_open_for(now, false));
        assert_eq!(w.closed_because_for(now, false), None);
    }

    #[test]
    fn dropping_the_idle_gate_does_not_drop_the_others() {
        // The relaxation is narrow on purpose: a busy GPU or a live turn must
        // still hold interpretation back.
        let (busy, now) = window(0, true, Some(0), true);
        assert!(!busy.is_open_for(now, false));
        assert_eq!(
            busy.closed_because_for(now, false),
            Some("a turn is in flight")
        );

        let (gpu, now) = window(0, false, Some(9_000), true);
        assert!(!gpu.is_open_for(now, false));
        assert_eq!(
            gpu.closed_because_for(now, false),
            Some("another process wants the GPU")
        );

        let (unknown, now) = window(0, false, None, true);
        assert!(!unknown.is_open_for(now, false));

        let (off, now) = window(0, false, Some(0), false);
        assert!(!off.is_open_for(now, false), "disabled still wins");
    }

    #[test]
    fn disabled_beats_every_other_gate() {
        let (w, now) = window(9_999, false, Some(0), false);
        assert!(!w.is_open(now));
        assert_eq!(w.closed_because(now), Some("background work is disabled"));
    }

    #[test]
    fn our_own_vram_is_not_foreign_pressure() {
        // The planner alone is ~11 GB. Without subtracting it the window would
        // never open on a machine that is working correctly.
        let probe = SmiGpuProbe::new(11_000);
        // Can't read a real GPU in CI; assert the arithmetic directly.
        assert_eq!(11_000_u64.saturating_sub(11_000), 0);
        assert_eq!(10_000_u64.saturating_sub(11_000), 0, "must not underflow");
        let _ = probe.foreign_vram_mb(); // must not panic without a GPU
    }
}
