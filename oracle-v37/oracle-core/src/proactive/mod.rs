//! Proactive nudges: the part of Pythia that speaks without being spoken to.
//!
//! # Why the LLM is not in this loop
//!
//! Everything else in the assistant runs with the user present: they asked for
//! something, they hear the answer, and an irreversible act stops for their
//! sanction. A proactive turn breaks all three assumptions — it happens with
//! nobody watching, possibly with nobody in the room.
//!
//! So the planner is deliberately absent here. A [`Trigger`] is ordinary Rust
//! that reads a source (calendar, mail) and returns fully-phrased [`Nudge`]s.
//! Nothing in this module can dispatch a tool, and the engine's only output is
//! speech. That is a hard architectural boundary, not a default to be relaxed:
//! it means the worst case for a bug here is Pythia saying something silly at
//! the wrong moment, never taking an unattended action.
//!
//! Phrasing the nudge through the LLM would sound better, and could be done
//! safely with an empty tool registry — see the note in the README. It is not
//! done yet.
//!
//! # Why the policy layer is bigger than the triggers
//!
//! An assistant that interrupts badly is worse than one that never speaks. The
//! triggers are simple; the judgment about *whether this is a moment to talk*
//! is where the work is, and it is all in [`NudgePolicy`]:
//!
//! * quiet hours, so nothing wakes you at 03:00
//! * a per-nudge cooldown, so one 3pm meeting is mentioned once, not every poll
//! * an hourly ceiling, so a misbehaving trigger cannot turn into a stream
//! * suppression while a real turn is in flight, so it never talks over you

use std::collections::HashMap;

pub mod routines;
pub mod triggers;

/// What kind of thing prompted the nudge. Carried through so the HUD can style
/// it and so the hourly ceiling can be reasoned about per source later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NudgeKind {
    Calendar,
    Mail,
    /// Something on this machine changed -- the class of trigger a cloud
    /// assistant structurally cannot have.
    Process,
}

impl NudgeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            NudgeKind::Calendar => "calendar",
            NudgeKind::Mail => "mail",
            NudgeKind::Process => "process",
        }
    }
}

/// One thing worth saying, already phrased.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nudge {
    pub kind: NudgeKind,
    /// Stable identity for this nudge across polls.
    ///
    /// The engine polls on a timer, so the *same* upcoming meeting is
    /// rediscovered every cycle. This key is what lets the cooldown recognise
    /// it as already-said, so it must derive from the underlying thing (the
    /// event id) and never from the current time.
    pub key: String,
    /// Exactly what Pythia will say.
    pub text: String,
}

/// Why a nudge was not spoken. Returned rather than logged so the caller can
/// surface it in `doctor` output and tests can assert on the reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suppressed {
    QuietHours,
    Cooldown,
    HourlyLimit,
    TurnInFlight,
}

#[derive(Debug, Clone)]
pub struct PolicyConfig {
    /// Local hour (0-23) at which quiet hours begin.
    pub quiet_from_hour: u32,
    /// Local hour (0-23) at which quiet hours end.
    pub quiet_until_hour: u32,
    /// Don't repeat the same nudge key within this many seconds.
    pub repeat_after_secs: i64,
    /// Ceiling on nudges actually spoken in any rolling hour.
    pub max_per_hour: usize,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        PolicyConfig {
            quiet_from_hour: 22,
            quiet_until_hour: 8,
            // Six hours: long enough that a meeting is mentioned once, short
            // enough that a genuinely recurring thing can resurface next day.
            repeat_after_secs: 6 * 3600,
            max_per_hour: 4,
        }
    }
}

/// The gate between "a trigger fired" and "Pythia speaks".
pub struct NudgePolicy {
    cfg: PolicyConfig,
    /// key -> unix time it was last spoken.
    last_spoken: HashMap<String, i64>,
    /// Unix times of nudges spoken recently, for the rolling hourly ceiling.
    recent: Vec<i64>,
}

impl NudgePolicy {
    pub fn new(cfg: PolicyConfig) -> Self {
        NudgePolicy {
            cfg,
            last_spoken: HashMap::new(),
            recent: Vec::new(),
        }
    }

    /// True when `hour` falls inside the configured quiet window.
    ///
    /// The window normally wraps midnight (22 → 08), so this is not a simple
    /// range test: it is "at or after the start OR before the end" when
    /// wrapping, and a plain range when it does not.
    pub fn in_quiet_hours(&self, hour: u32) -> bool {
        let (from, until) = (self.cfg.quiet_from_hour, self.cfg.quiet_until_hour);
        if from == until {
            return false; // zero-length window: quiet hours disabled
        }
        if from < until {
            hour >= from && hour < until
        } else {
            hour >= from || hour < until
        }
    }

    /// Decide whether this nudge may be spoken now, recording it if so.
    ///
    /// `local_hour` is passed in rather than read from the clock so the
    /// decision is a pure function of its inputs and can be tested across the
    /// whole day without touching the system time.
    pub fn admit(
        &mut self,
        nudge: &Nudge,
        now: i64,
        local_hour: u32,
        turn_in_flight: bool,
    ) -> Result<(), Suppressed> {
        // Never talk over a real conversation.
        if turn_in_flight {
            return Err(Suppressed::TurnInFlight);
        }
        if self.in_quiet_hours(local_hour) {
            return Err(Suppressed::QuietHours);
        }
        if let Some(&last) = self.last_spoken.get(&nudge.key) {
            if now - last < self.cfg.repeat_after_secs {
                return Err(Suppressed::Cooldown);
            }
        }

        // Drop anything older than an hour, then test the ceiling.
        self.recent.retain(|t| now - *t < 3600);
        if self.recent.len() >= self.cfg.max_per_hour {
            return Err(Suppressed::HourlyLimit);
        }

        self.last_spoken.insert(nudge.key.clone(), now);
        self.recent.push(now);
        Ok(())
    }

    /// How many nudges have been spoken in the last hour, as of `now`.
    pub fn spoken_last_hour(&self, now: i64) -> usize {
        self.recent.iter().filter(|t| now - **t < 3600).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nudge(key: &str) -> Nudge {
        Nudge {
            kind: NudgeKind::Calendar,
            key: key.to_string(),
            text: "something".into(),
        }
    }

    fn policy() -> NudgePolicy {
        NudgePolicy::new(PolicyConfig::default())
    }

    #[test]
    fn quiet_window_wraps_midnight() {
        let p = policy(); // 22 -> 08
        assert!(p.in_quiet_hours(23));
        assert!(p.in_quiet_hours(3));
        assert!(p.in_quiet_hours(22), "start is inclusive");
        assert!(!p.in_quiet_hours(8), "end is exclusive");
        assert!(!p.in_quiet_hours(14));
    }

    #[test]
    fn quiet_window_without_wrap_is_a_plain_range() {
        let p = NudgePolicy::new(PolicyConfig {
            quiet_from_hour: 9,
            quiet_until_hour: 17,
            ..PolicyConfig::default()
        });
        assert!(p.in_quiet_hours(12));
        assert!(!p.in_quiet_hours(8));
        assert!(!p.in_quiet_hours(20));
    }

    #[test]
    fn equal_bounds_disable_quiet_hours() {
        let p = NudgePolicy::new(PolicyConfig {
            quiet_from_hour: 0,
            quiet_until_hour: 0,
            ..PolicyConfig::default()
        });
        for h in 0..24 {
            assert!(!p.in_quiet_hours(h), "hour {h} should not be quiet");
        }
    }

    #[test]
    fn nothing_is_spoken_during_quiet_hours() {
        let mut p = policy();
        assert_eq!(
            p.admit(&nudge("a"), 0, 3, false),
            Err(Suppressed::QuietHours)
        );
    }

    #[test]
    fn a_turn_in_flight_beats_every_other_consideration() {
        let mut p = policy();
        assert_eq!(
            p.admit(&nudge("a"), 0, 14, true),
            Err(Suppressed::TurnInFlight)
        );
    }

    #[test]
    fn the_same_nudge_is_not_repeated_within_its_cooldown() {
        // The engine polls on a timer, so it rediscovers the same meeting every
        // cycle; without this it would announce it once a minute.
        let mut p = policy();
        assert!(p.admit(&nudge("evt-1"), 1_000, 14, false).is_ok());
        assert_eq!(
            p.admit(&nudge("evt-1"), 1_060, 14, false),
            Err(Suppressed::Cooldown)
        );
        // A different thing is unaffected.
        assert!(p.admit(&nudge("evt-2"), 1_060, 14, false).is_ok());
    }

    #[test]
    fn cooldown_expires() {
        let mut p = policy();
        assert!(p.admit(&nudge("evt-1"), 0, 14, false).is_ok());
        assert!(p.admit(&nudge("evt-1"), 6 * 3600 + 1, 14, false).is_ok());
    }

    #[test]
    fn a_runaway_trigger_is_capped_per_hour() {
        let mut p = policy(); // max 4
        for i in 0..4 {
            assert!(
                p.admit(&nudge(&format!("k{i}")), 1_000, 14, false).is_ok(),
                "nudge {i} should be admitted"
            );
        }
        assert_eq!(
            p.admit(&nudge("k4"), 1_000, 14, false),
            Err(Suppressed::HourlyLimit)
        );
    }

    #[test]
    fn the_hourly_ceiling_is_a_rolling_window() {
        let mut p = policy();
        for i in 0..4 {
            assert!(p.admit(&nudge(&format!("k{i}")), 1_000, 14, false).is_ok());
        }
        // An hour and a second later the window has emptied.
        let later = 1_000 + 3601;
        assert_eq!(p.spoken_last_hour(later), 0);
        assert!(p.admit(&nudge("k9"), later, 14, false).is_ok());
    }

    #[test]
    fn a_suppressed_nudge_does_not_consume_the_hourly_budget() {
        // Otherwise a trigger firing during quiet hours would silently exhaust
        // the morning's allowance.
        let mut p = policy();
        for i in 0..10 {
            let _ = p.admit(&nudge(&format!("q{i}")), 1_000, 3, false);
        }
        assert_eq!(p.spoken_last_hour(1_000), 0);
        assert!(p.admit(&nudge("morning"), 1_000, 14, false).is_ok());
    }
}
