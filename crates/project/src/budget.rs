//! What a project may spend, and what happens when it has spent it.
//!
//! Pure, like [`crate::runs::triggers_run`] and
//! [`crate::comments::comment_delivery`]: the decision is one function so
//! the gate, the timeline entry, and anything that later shows a burn bar
//! all read the same rule.

use baybo_model::MicroUsd;
use chrono::{DateTime, Utc};

/// Whether a board can start more work right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Headroom {
    /// No ceiling is set. The default, and the only state in which the
    /// gate costs nothing — no spend query runs.
    Unlimited,
    Available {
        spent: MicroUsd,
        limit: MicroUsd,
    },
    Exhausted {
        spent: MicroUsd,
        limit: MicroUsd,
    },
}

impl Headroom {
    pub fn is_exhausted(self) -> bool {
        matches!(self, Headroom::Exhausted { .. })
    }

    /// `(spent, limit)` in micro-USD, for the timeline entry. `None` when
    /// there is no ceiling to report against.
    pub fn figures(self) -> Option<(i64, i64)> {
        match self {
            Headroom::Unlimited => None,
            Headroom::Available { spent, limit } | Headroom::Exhausted { spent, limit } => {
                Some((spent.into_micros(), limit.into_micros()))
            }
        }
    }
}

/// Decide against a limit and a day's spend.
///
/// The comparison is `>=`: a board that has spent exactly its ceiling has
/// no room for a run whose cost is unknown in advance. Erring the other way
/// would let every board overspend by one run's worth, every day.
pub(crate) fn headroom(limit: Option<MicroUsd>, spent: MicroUsd) -> Headroom {
    let Some(limit) = limit else {
        return Headroom::Unlimited;
    };
    if spent.into_micros() >= limit.into_micros() {
        Headroom::Exhausted { spent, limit }
    } else {
        Headroom::Available { spent, limit }
    }
}

/// The start of the UTC day containing `now` — the window a daily budget
/// measures.
///
/// UTC rather than a local zone: the operator's timezone is not stored, a
/// server's is an accident of deployment, and a budget that rolls over at a
/// time nobody can predict is worse than one that rolls over at an
/// inconvenient one.
pub(crate) fn day_start(now: DateTime<Utc>) -> DateTime<Utc> {
    now.date_naive()
        .and_hms_opt(0, 0, 0)
        // `00:00:00` is valid for every date, so this cannot fire; falling
        // back to `now` would make the window empty rather than panicking.
        .map_or(now, |naive| naive.and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usd(micros: i64) -> MicroUsd {
        MicroUsd::from_micros(micros)
    }

    #[test]
    fn no_limit_never_gates_and_reports_no_figures() {
        let h = headroom(None, usd(999_999_999));
        assert_eq!(h, Headroom::Unlimited);
        assert!(!h.is_exhausted());
        assert_eq!(h.figures(), None);
    }

    #[test]
    fn the_boundary_is_closed_against_starting_more_work() {
        // Exactly at the ceiling is exhausted. A run's cost is unknown until
        // it has run, so "there is 0 left" cannot mean "start one more".
        assert!(headroom(Some(usd(100)), usd(100)).is_exhausted());
        assert!(headroom(Some(usd(100)), usd(101)).is_exhausted());
        assert!(!headroom(Some(usd(100)), usd(99)).is_exhausted());
    }

    #[test]
    fn a_zero_budget_stops_everything() {
        // Not a special case, and worth pinning: `0` is how an operator
        // pauses a board without archiving it.
        assert!(headroom(Some(MicroUsd::ZERO), MicroUsd::ZERO).is_exhausted());
    }

    #[test]
    fn figures_survive_for_the_timeline() {
        assert_eq!(
            headroom(Some(usd(100)), usd(120)).figures(),
            Some((120, 100))
        );
        assert_eq!(headroom(Some(usd(100)), usd(20)).figures(), Some((20, 100)));
    }

    #[test]
    fn the_window_is_the_utc_day() {
        let now = DateTime::parse_from_rfc3339("2026-08-05T23:59:59Z")
            .expect("rfc3339")
            .with_timezone(&Utc);
        assert_eq!(day_start(now).to_rfc3339(), "2026-08-05T00:00:00+00:00");
        // …and the first instant of a day is its own window start, so a run
        // at midnight is not measured against yesterday.
        assert_eq!(day_start(day_start(now)), day_start(now));
    }
}
