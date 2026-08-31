//! Sources of proactive nudges.
//!
//! Each trigger is ordinary Rust that reads something and returns phrased
//! [`Nudge`]s. Deliberately no LLM: see the module docs on `proactive`.
//!
//! The pure decision half of every trigger is split out from the I/O half, so
//! the logic that decides *whether* something is worth saying is unit-testable
//! without a Google account.

use std::collections::HashSet;

use super::{Nudge, NudgeKind};
use crate::connectors::google_api::{CalEvent, MailSummary};

/// How far ahead of an event to speak up.
pub const DEFAULT_LEAD_MINUTES: i64 = 10;

/// Parse an RFC-3339 timestamp into unix seconds.
fn parse_rfc3339(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
}

/// Turn upcoming calendar events into nudges.
///
/// Pure: takes the events and the current time, so the windowing rule is
/// testable without a live calendar.
pub fn calendar_nudges(events: &[CalEvent], now: i64, lead_minutes: i64) -> Vec<Nudge> {
    let lead = lead_minutes * 60;
    let mut out = Vec::new();
    for ev in events {
        let Some(start) = parse_rfc3339(&ev.start) else {
            continue;
        };
        let secs_away = start - now;
        // Only the window ahead: an event already under way is not news, and
        // one further out than the lead time is not yet worth interrupting for.
        if secs_away <= 0 || secs_away > lead {
            continue;
        }
        let mins = (secs_away + 59) / 60; // round up: 90s reads as "2 minutes"
        let summary = ev.summary.trim();
        let what = if summary.is_empty() {
            "an event".to_string()
        } else {
            summary.to_string()
        };
        out.push(Nudge {
            kind: NudgeKind::Calendar,
            // Keyed on the event id, never the time, so the same meeting is
            // recognised as already-announced on the next poll.
            key: format!("cal:{}", ev.id),
            text: if mins <= 1 {
                format!("{what} starts in about a minute.")
            } else {
                format!("{what} starts in {mins} minutes.")
            },
        });
    }
    out
}

/// Turn unread mail into at most one nudge.
///
/// One nudge for the batch rather than one per message: five separate
/// interruptions for five emails is exactly the behaviour that makes people
/// turn an assistant off.
pub fn mail_nudges(mail: &[MailSummary]) -> Vec<Nudge> {
    let unread: Vec<&MailSummary> = mail.iter().filter(|m| m.unread).collect();
    let Some(first) = unread.first() else {
        return Vec::new();
    };

    let from = first.from.trim();
    let subject = first.subject.trim();
    let text = match unread.len() {
        1 => format!("New mail from {from}: {subject}."),
        n => format!("{n} new emails. The first is from {from}: {subject}."),
    };

    // Keyed on the newest message id, so the nudge repeats only when something
    // genuinely newer arrives -- not every poll while the same mail sits unread.
    vec![Nudge {
        kind: NudgeKind::Mail,
        key: format!("mail:{}", first.id),
        text,
    }]
}

/// Watches for named processes disappearing.
///
/// This is the trigger a cloud assistant structurally cannot have: "your build
/// finished" requires something living on the machine, watching the process
/// table. Calendar and mail nudges are the kind of thing a phone already does
/// better; this is not.
///
/// Stateful by necessity -- "finished" is a transition, not a condition -- so it
/// holds what it saw last poll. Matching is case-insensitive and by substring,
/// because the same build shows up as `cargo`, `cargo.exe` and
/// `cargo-nextest.exe` on different platforms.
pub struct ProcessWatcher {
    watching: Vec<String>,
    present: HashSet<String>,
    /// First poll only establishes a baseline. Without this, every watched
    /// process that was *already* finished before Oracle started would be
    /// announced as having just finished, at launch.
    primed: bool,
}

impl ProcessWatcher {
    pub fn new(watching: Vec<String>) -> Self {
        ProcessWatcher {
            watching: watching
                .into_iter()
                .map(|w| w.trim().to_lowercase())
                .filter(|w| !w.is_empty())
                .collect(),
            present: HashSet::new(),
            primed: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.watching.is_empty()
    }

    /// Feed the current process names; get a nudge for each watched one that
    /// was running last time and is not now.
    pub fn poll(&mut self, running: &[String]) -> Vec<Nudge> {
        let lower: Vec<String> = running.iter().map(|p| p.to_lowercase()).collect();

        let mut now_present = HashSet::new();
        for w in &self.watching {
            if lower.iter().any(|p| p.contains(w.as_str())) {
                now_present.insert(w.clone());
            }
        }

        let mut out = Vec::new();
        if self.primed {
            for gone in self.present.difference(&now_present) {
                out.push(Nudge {
                    kind: NudgeKind::Process,
                    // Keyed on the process, so a flapping watcher is caught by
                    // the policy cooldown rather than repeating.
                    key: format!("proc:{gone}"),
                    text: format!("{gone} just finished."),
                });
            }
        }
        self.present = now_present;
        self.primed = true;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str, summary: &str, start: &str) -> CalEvent {
        CalEvent {
            id: id.into(),
            summary: summary.into(),
            start: start.into(),
            end: start.into(),
        }
    }

    fn at(unix: i64) -> String {
        chrono::DateTime::from_timestamp(unix, 0)
            .unwrap()
            .to_rfc3339()
    }

    #[test]
    fn an_event_inside_the_lead_window_is_announced() {
        let now = 1_700_000_000;
        let events = [ev("a", "Standup", &at(now + 300))]; // 5 min away
        let n = calendar_nudges(&events, now, DEFAULT_LEAD_MINUTES);
        assert_eq!(n.len(), 1);
        assert!(n[0].text.contains("Standup"), "got: {}", n[0].text);
        assert!(n[0].text.contains("5 minutes"), "got: {}", n[0].text);
        assert_eq!(n[0].key, "cal:a");
    }

    #[test]
    fn events_outside_the_window_are_left_alone() {
        let now = 1_700_000_000;
        let events = [
            ev("far", "Next week", &at(now + 86_400)),
            ev("past", "Already started", &at(now - 60)),
            ev("now", "Starting exactly now", &at(now)),
        ];
        assert!(calendar_nudges(&events, now, DEFAULT_LEAD_MINUTES).is_empty());
    }

    #[test]
    fn the_key_is_the_event_not_the_time() {
        // The engine re-polls; the key must be stable so the cooldown can
        // recognise the same meeting a minute later.
        let now = 1_700_000_000;
        let events = [ev("a", "Standup", &at(now + 300))];
        let first = calendar_nudges(&events, now, DEFAULT_LEAD_MINUTES);
        let later = calendar_nudges(&events, now + 60, DEFAULT_LEAD_MINUTES);
        assert_eq!(first[0].key, later[0].key);
        assert_ne!(first[0].text, later[0].text, "the wording does count down");
    }

    #[test]
    fn under_a_minute_reads_naturally() {
        let now = 1_700_000_000;
        let events = [ev("a", "Standup", &at(now + 30))];
        let n = calendar_nudges(&events, now, DEFAULT_LEAD_MINUTES);
        assert!(n[0].text.contains("about a minute"), "got: {}", n[0].text);
    }

    #[test]
    fn a_nameless_event_still_reads_as_a_sentence() {
        let now = 1_700_000_000;
        let events = [ev("a", "   ", &at(now + 300))];
        let n = calendar_nudges(&events, now, DEFAULT_LEAD_MINUTES);
        assert!(n[0].text.starts_with("an event"), "got: {}", n[0].text);
    }

    #[test]
    fn unparseable_timestamps_are_skipped_not_panicked() {
        let now = 1_700_000_000;
        let events = [ev("a", "Broken", "not-a-timestamp")];
        assert!(calendar_nudges(&events, now, DEFAULT_LEAD_MINUTES).is_empty());
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_first_poll_only_establishes_a_baseline() {
        // Otherwise every watched process that finished before Oracle started
        // gets announced the moment it launches.
        let mut w = ProcessWatcher::new(names(&["cargo"]));
        assert!(w.poll(&names(&["chrome.exe"])).is_empty());
    }

    #[test]
    fn a_watched_process_disappearing_is_announced_once() {
        let mut w = ProcessWatcher::new(names(&["cargo"]));
        w.poll(&names(&["cargo.exe", "chrome.exe"])); // baseline: running
        let n = w.poll(&names(&["chrome.exe"])); // gone
        assert_eq!(n.len(), 1);
        assert!(n[0].text.contains("cargo"), "got: {}", n[0].text);
        assert_eq!(n[0].key, "proc:cargo");
        // Still gone on the next poll: not news any more.
        assert!(w.poll(&names(&["chrome.exe"])).is_empty());
    }

    #[test]
    fn a_process_that_keeps_running_says_nothing() {
        let mut w = ProcessWatcher::new(names(&["cargo"]));
        w.poll(&names(&["cargo.exe"]));
        assert!(w.poll(&names(&["cargo.exe"])).is_empty());
    }

    #[test]
    fn matching_is_case_insensitive_and_by_substring() {
        // The same build is cargo, cargo.exe, or MSBuild.exe depending on host.
        let mut w = ProcessWatcher::new(names(&["MSBuild"]));
        w.poll(&names(&["msbuild.exe"]));
        assert_eq!(w.poll(&names(&[])).len(), 1);
    }

    #[test]
    fn a_process_can_finish_more_than_once() {
        // Second build of the day should still be reported.
        let mut w = ProcessWatcher::new(names(&["cargo"]));
        w.poll(&names(&["cargo"]));
        assert_eq!(w.poll(&names(&[])).len(), 1);
        w.poll(&names(&["cargo"]));
        assert_eq!(w.poll(&names(&[])).len(), 1);
    }

    #[test]
    fn watching_nothing_is_inert() {
        let mut w = ProcessWatcher::new(names(&["", "  "]));
        assert!(w.is_empty());
        w.poll(&names(&["cargo"]));
        assert!(w.poll(&names(&[])).is_empty());
    }

    fn mail(id: &str, from: &str, subject: &str, unread: bool) -> MailSummary {
        MailSummary {
            id: id.into(),
            thread_id: id.into(),
            from: from.into(),
            subject: subject.into(),
            snippet: String::new(),
            unread,
        }
    }

    #[test]
    fn one_unread_email_is_announced_directly() {
        let m = [mail("1", "Priya", "dinner friday", true)];
        let n = mail_nudges(&m);
        assert_eq!(n.len(), 1);
        assert!(n[0].text.contains("Priya"));
        assert!(n[0].text.contains("dinner friday"));
    }

    #[test]
    fn several_unread_emails_become_one_nudge_not_several() {
        let m = [
            mail("1", "Priya", "dinner friday", true),
            mail("2", "Advisor", "thesis draft", true),
            mail("3", "Bank", "statement", true),
        ];
        let n = mail_nudges(&m);
        assert_eq!(n.len(), 1, "must not interrupt once per message");
        assert!(n[0].text.starts_with("3 new emails"), "got: {}", n[0].text);
        assert!(n[0].text.contains("Priya"), "should name the first");
    }

    #[test]
    fn read_mail_says_nothing() {
        let m = [mail("1", "Priya", "dinner friday", false)];
        assert!(mail_nudges(&m).is_empty());
        assert!(mail_nudges(&[]).is_empty());
    }

    #[test]
    fn the_mail_key_tracks_the_newest_message() {
        // Same unread mail on the next poll -> same key -> cooldown suppresses.
        let m = [mail("1", "Priya", "dinner friday", true)];
        assert_eq!(mail_nudges(&m)[0].key, "mail:1");
        let newer = [
            mail("2", "Advisor", "thesis draft", true),
            mail("1", "Priya", "dinner friday", true),
        ];
        assert_eq!(mail_nudges(&newer)[0].key, "mail:2");
    }
}
