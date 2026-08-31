//! Standing orders: things the user asked for once that should keep happening.
//!
//! "Every weekday at 8:30, tell me my first meeting." Until now there was no way
//! to express that — every capability had to be invoked in the moment, which is
//! the difference between a tool you remember to use and an assistant.
//!
//! # Why these DO run the planner, when nudges do not
//!
//! [`super`] keeps the LLM out of the trigger loop, because a heuristic firing
//! unattended should never be able to act. A routine is different in kind: it is
//! the user's own instruction, written by them, merely time-shifted. Running it
//! is executing a request they made, not acting on a guess.
//!
//! The capability gate still applies in full. An unattended turn that reaches an
//! irreversible action hits the confirmer with nobody there to answer, times
//! out, and is denied. So a routine can read your calendar at 8:30; it cannot
//! quietly send mail on your behalf.

use std::sync::Mutex;

use chrono::{Datelike, NaiveDateTime, Timelike, Weekday};
use rusqlite::{params, Connection};

/// When a routine should fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    /// Every day at a local wall-clock time.
    Daily { hour: u32, minute: u32 },
    /// Monday to Friday at a local wall-clock time.
    Weekdays { hour: u32, minute: u32 },
    /// A fixed interval, regardless of wall clock.
    EveryMinutes(u32),
}

impl Schedule {
    /// Parse the small schedule vocabulary. Deliberately tiny: cron is
    /// unreadable out loud, and these three cover what people actually ask for.
    ///
    /// Accepts: `daily 08:30`, `weekdays 8:30`, `every 45m`, `every 2h`.
    pub fn parse(s: &str) -> Option<Schedule> {
        let s = s.trim().to_lowercase();
        let mut parts = s.split_whitespace();
        let kind = parts.next()?;
        let rest = parts.next()?;

        match kind {
            "daily" | "every-day" | "everyday" => {
                let (h, m) = parse_hhmm(rest)?;
                Some(Schedule::Daily { hour: h, minute: m })
            }
            "weekdays" | "weekday" => {
                let (h, m) = parse_hhmm(rest)?;
                Some(Schedule::Weekdays { hour: h, minute: m })
            }
            "every" => {
                let (num, unit) = rest.split_at(rest.find(|c: char| !c.is_ascii_digit())?);
                let n: u32 = num.parse().ok()?;
                if n == 0 {
                    return None; // would fire continuously
                }
                match unit {
                    "m" | "min" | "mins" | "minutes" => Some(Schedule::EveryMinutes(n)),
                    "h" | "hr" | "hrs" | "hours" => {
                        Some(Schedule::EveryMinutes(n.checked_mul(60)?))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Render back into the same vocabulary `parse` accepts, so a round trip
    /// through the database is lossless.
    pub fn render(&self) -> String {
        match self {
            Schedule::Daily { hour, minute } => format!("daily {hour:02}:{minute:02}"),
            Schedule::Weekdays { hour, minute } => format!("weekdays {hour:02}:{minute:02}"),
            Schedule::EveryMinutes(n) => format!("every {n}m"),
        }
    }

    /// Whether this routine is due.
    ///
    /// `local` is the user's wall clock (schedules are stated in it), `now_unix`
    /// and `last_fired` are absolute. Both are passed in rather than read from a
    /// clock so the rule is testable across a whole week without sleeping.
    pub fn is_due(&self, local: NaiveDateTime, now_unix: i64, last_fired: Option<i64>) -> bool {
        match *self {
            Schedule::EveryMinutes(n) => match last_fired {
                // Never fired: start the interval from now rather than firing
                // immediately, or adding a routine would always trigger it once.
                None => false,
                Some(last) => now_unix - last >= i64::from(n) * 60,
            },
            Schedule::Daily { hour, minute } => {
                due_at_wall_clock(local, hour, minute, now_unix, last_fired)
            }
            Schedule::Weekdays { hour, minute } => {
                if matches!(local.weekday(), Weekday::Sat | Weekday::Sun) {
                    return false;
                }
                due_at_wall_clock(local, hour, minute, now_unix, last_fired)
            }
        }
    }
}

fn parse_hhmm(s: &str) -> Option<(u32, u32)> {
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some((h, m))
}

/// Due if we are at or past the time today and have not already fired today.
///
/// "Today" is compared in local days, not as a 24h window: a routine that fired
/// at 08:30 must not become eligible again at 08:31 the next morning merely
/// because 24 hours have not quite elapsed.
fn due_at_wall_clock(
    local: NaiveDateTime,
    hour: u32,
    minute: u32,
    now_unix: i64,
    last_fired: Option<i64>,
) -> bool {
    let past_time = local.hour() > hour || (local.hour() == hour && local.minute() >= minute);
    if !past_time {
        return false;
    }
    match last_fired {
        None => true,
        Some(last) => {
            // Seconds since local midnight tell us when today began in absolute
            // terms; anything fired before that was on an earlier day.
            let secs_today = i64::from(local.hour()) * 3600
                + i64::from(local.minute()) * 60
                + i64::from(local.second());
            let local_midnight_unix = now_unix - secs_today;
            last < local_midnight_unix
        }
    }
}

/// One standing order.
#[derive(Debug, Clone)]
pub struct Routine {
    pub id: i64,
    /// Short label the user can refer to when removing it.
    pub name: String,
    /// What Pythia should do, in the user's own words. Run as a turn.
    pub prompt: String,
    pub schedule: Schedule,
    pub enabled: bool,
    pub last_fired: Option<i64>,
}

/// Persistent routine storage, in the same SQLite file as memory.
pub struct RoutineStore {
    conn: Mutex<Connection>,
}

impl RoutineStore {
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS routines (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                prompt TEXT NOT NULL,
                schedule TEXT NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                last_fired INTEGER
             );",
        )?;
        Ok(RoutineStore {
            conn: Mutex::new(conn),
        })
    }

    /// Add or replace a routine by name, returning its id.
    ///
    /// Upsert rather than insert: "every morning tell me my meetings" said twice
    /// should update the standing order, not create a duplicate that fires twice.
    pub fn upsert(&self, name: &str, prompt: &str, schedule: Schedule) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO routines (name, prompt, schedule, enabled, last_fired)
             VALUES (?1, ?2, ?3, 1, NULL)
             ON CONFLICT(name) DO UPDATE SET prompt = ?2, schedule = ?3, enabled = 1",
            params![name, prompt, schedule.render()],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM routines WHERE name = ?1",
            params![name],
            |r| r.get(0),
        )?)
    }

    pub fn list(&self) -> anyhow::Result<Vec<Routine>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, prompt, schedule, enabled, last_fired FROM routines ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<i64>>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, name, prompt, sched, enabled, last_fired) = row?;
            // A schedule string that no longer parses (hand-edited, or written
            // by a newer version) is skipped rather than failing the whole list.
            let Some(schedule) = Schedule::parse(&sched) else {
                tracing::warn!(%name, %sched, "skipping routine with an unparseable schedule");
                continue;
            };
            out.push(Routine {
                id,
                name,
                prompt,
                schedule,
                enabled: enabled != 0,
                last_fired,
            });
        }
        Ok(out)
    }

    pub fn remove(&self, name: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM routines WHERE name = ?1", params![name])? > 0)
    }

    pub fn set_enabled(&self, name: &str, on: bool) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE routines SET enabled = ?2 WHERE name = ?1",
            params![name, i64::from(on)],
        )? > 0)
    }

    pub fn mark_fired(&self, id: i64, when: i64) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE routines SET last_fired = ?2 WHERE id = ?1",
            params![id, when],
        )?;
        Ok(())
    }

    /// Every enabled routine that is due now.
    pub fn due(&self, local: NaiveDateTime, now_unix: i64) -> anyhow::Result<Vec<Routine>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|r| r.enabled && r.schedule.is_due(local, now_unix, r.last_fired))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }
    fn unix(s: &str) -> i64 {
        dt(s).and_utc().timestamp()
    }

    #[test]
    fn schedule_vocabulary_round_trips() {
        for s in ["daily 08:30", "weekdays 17:00", "every 45m"] {
            let parsed = Schedule::parse(s).expect(s);
            assert_eq!(Schedule::parse(&parsed.render()), Some(parsed));
        }
        assert_eq!(
            Schedule::parse("every 2h"),
            Some(Schedule::EveryMinutes(120))
        );
        assert_eq!(
            Schedule::parse("DAILY 8:05"),
            Schedule::parse("daily 08:05")
        );
    }

    #[test]
    fn nonsense_schedules_are_rejected() {
        for s in [
            "",
            "daily",
            "daily 25:00",
            "daily 08:99",
            "every 0m",
            "yearly 08:00",
            "every 5x",
        ] {
            assert!(Schedule::parse(s).is_none(), "should reject {s:?}");
        }
    }

    #[test]
    fn a_daily_routine_fires_once_per_day_after_its_time() {
        let s = Schedule::Daily {
            hour: 8,
            minute: 30,
        };
        // Before the time: not due.
        assert!(!s.is_due(dt("2026-09-01 08:29:00"), unix("2026-09-01 08:29:00"), None));
        // At the time, never fired: due.
        assert!(s.is_due(dt("2026-09-01 08:30:00"), unix("2026-09-01 08:30:00"), None));

        // Fired this morning: not due again today, even much later.
        let fired = unix("2026-09-01 08:30:00");
        assert!(!s.is_due(
            dt("2026-09-01 08:31:00"),
            unix("2026-09-01 08:31:00"),
            Some(fired)
        ));
        assert!(!s.is_due(
            dt("2026-09-01 23:59:00"),
            unix("2026-09-01 23:59:00"),
            Some(fired)
        ));

        // Next morning: due again.
        assert!(s.is_due(
            dt("2026-09-02 08:30:00"),
            unix("2026-09-02 08:30:00"),
            Some(fired)
        ));
    }

    #[test]
    fn a_daily_routine_does_not_refire_on_a_rolling_24h_window() {
        // The bug this guards: comparing "more than 24h since last fire" makes a
        // routine that ran at 08:30 ineligible at 08:29 the next day and then
        // eligible at 08:31 -- drifting later every single day.
        let s = Schedule::Daily {
            hour: 8,
            minute: 30,
        };
        let fired = unix("2026-09-01 08:30:00");
        assert!(
            s.is_due(
                dt("2026-09-02 08:30:00"),
                unix("2026-09-02 08:30:00"),
                Some(fired)
            ),
            "must fire at exactly the same wall-clock time the next day"
        );
    }

    #[test]
    fn weekday_routines_skip_the_weekend() {
        let s = Schedule::Weekdays { hour: 9, minute: 0 };
        // 2026-09-05 is a Saturday, 09-06 Sunday, 09-07 Monday.
        assert!(!s.is_due(dt("2026-09-05 09:00:00"), unix("2026-09-05 09:00:00"), None));
        assert!(!s.is_due(dt("2026-09-06 09:00:00"), unix("2026-09-06 09:00:00"), None));
        assert!(s.is_due(dt("2026-09-07 09:00:00"), unix("2026-09-07 09:00:00"), None));
    }

    #[test]
    fn an_interval_routine_does_not_fire_the_moment_it_is_added() {
        // Otherwise "every 30 minutes, check X" runs immediately on creation,
        // which is never what someone means.
        let s = Schedule::EveryMinutes(30);
        assert!(!s.is_due(dt("2026-09-01 12:00:00"), unix("2026-09-01 12:00:00"), None));
    }

    #[test]
    fn an_interval_routine_fires_on_its_interval() {
        let s = Schedule::EveryMinutes(30);
        let last = unix("2026-09-01 12:00:00");
        assert!(!s.is_due(
            dt("2026-09-01 12:29:00"),
            unix("2026-09-01 12:29:00"),
            Some(last)
        ));
        assert!(s.is_due(
            dt("2026-09-01 12:30:00"),
            unix("2026-09-01 12:30:00"),
            Some(last)
        ));
    }

    fn store() -> RoutineStore {
        RoutineStore::open(":memory:").unwrap()
    }

    #[test]
    fn routines_persist_and_round_trip() {
        let s = store();
        s.upsert(
            "morning",
            "tell me my first meeting",
            Schedule::Daily {
                hour: 8,
                minute: 30,
            },
        )
        .unwrap();
        let all = s.list().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "morning");
        assert_eq!(all[0].prompt, "tell me my first meeting");
        assert_eq!(
            all[0].schedule,
            Schedule::Daily {
                hour: 8,
                minute: 30
            }
        );
        assert!(all[0].enabled);
        assert!(all[0].last_fired.is_none());
    }

    #[test]
    fn adding_the_same_name_twice_updates_rather_than_duplicates() {
        let s = store();
        s.upsert(
            "morning",
            "first version",
            Schedule::Daily { hour: 8, minute: 0 },
        )
        .unwrap();
        s.upsert(
            "morning",
            "second version",
            Schedule::Daily { hour: 9, minute: 0 },
        )
        .unwrap();
        let all = s.list().unwrap();
        assert_eq!(all.len(), 1, "must not fire twice every morning");
        assert_eq!(all[0].prompt, "second version");
        assert_eq!(all[0].schedule, Schedule::Daily { hour: 9, minute: 0 });
    }

    #[test]
    fn disabled_routines_are_never_due() {
        let s = store();
        s.upsert("m", "x", Schedule::Daily { hour: 8, minute: 0 })
            .unwrap();
        s.set_enabled("m", false).unwrap();
        let due = s
            .due(dt("2026-09-01 09:00:00"), unix("2026-09-01 09:00:00"))
            .unwrap();
        assert!(due.is_empty());
    }

    #[test]
    fn marking_fired_takes_it_out_of_the_due_set() {
        let s = store();
        let id = s
            .upsert("m", "x", Schedule::Daily { hour: 8, minute: 0 })
            .unwrap();
        let now = unix("2026-09-01 09:00:00");
        assert_eq!(s.due(dt("2026-09-01 09:00:00"), now).unwrap().len(), 1);
        s.mark_fired(id, now).unwrap();
        assert!(s
            .due(dt("2026-09-01 09:01:00"), now + 60)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn removing_a_routine_reports_whether_it_existed() {
        let s = store();
        s.upsert("m", "x", Schedule::EveryMinutes(10)).unwrap();
        assert!(s.remove("m").unwrap());
        assert!(!s.remove("m").unwrap(), "second removal finds nothing");
        assert!(s.list().unwrap().is_empty());
    }
}
