//! What a project may spend, and what happens when it has spent it.
//!
//! Money and token ceilings share a window; either can stop the board.
//! Tokens cover subscription plans whose per-run price is zero.

use baybo_model::MicroUsd;
use baybo_store::project::IssueEventBody;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Meter {
    pub(crate) spent: i64,
    pub(crate) limit: i64,
}

impl Meter {
    fn of(limit: Option<i64>, spent: i64) -> Option<Self> {
        limit.map(|limit| Self { spent, limit })
    }

    fn is_exhausted(self) -> bool {
        self.spent >= self.limit
    }

    /// Compare proportional usage without rounding; `i128` prevents ordinary overflows.
    fn tighter_than(self, other: Self) -> bool {
        (self.spent as i128) * (other.limit as i128) > (other.spent as i128) * (self.limit as i128)
    }
}

/// Prefer an exhausted meter, then the tighter fraction; ties use tokens.
fn speaks_money(money: Meter, tokens: Meter) -> bool {
    match (money.is_exhausted(), tokens.is_exhausted()) {
        (true, false) => true,
        (false, true) => false,
        _ => money.tighter_than(tokens),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Figures {
    Money {
        spent_micros: i64,
        limit_micros: i64,
    },
    Tokens {
        spent_tokens: i64,
        limit_tokens: i64,
    },
}

impl Figures {
    fn money(meter: Meter) -> Self {
        Figures::Money {
            spent_micros: meter.spent,
            limit_micros: meter.limit,
        }
    }

    fn tokens(meter: Meter) -> Self {
        Figures::Tokens {
            spent_tokens: meter.spent,
            limit_tokens: meter.limit,
        }
    }

    pub(crate) fn exhausted(self) -> IssueEventBody {
        match self {
            Figures::Money {
                spent_micros,
                limit_micros,
            } => IssueEventBody::BudgetExhausted {
                spent_micros,
                limit_micros,
            },
            Figures::Tokens {
                spent_tokens,
                limit_tokens,
            } => IssueEventBody::TokenBudgetExhausted {
                spent_tokens,
                limit_tokens,
            },
        }
    }

    pub(crate) fn restored(self) -> IssueEventBody {
        match self {
            Figures::Money {
                spent_micros,
                limit_micros,
            } => IssueEventBody::BudgetRestored {
                spent_micros,
                limit_micros,
            },
            Figures::Tokens {
                spent_tokens,
                limit_tokens,
            } => IssueEventBody::TokenBudgetRestored {
                spent_tokens,
                limit_tokens,
            },
        }
    }
}

/// Whether a board can start more work right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Headroom {
    /// No ceilings, so no spend query is needed.
    Unlimited,
    Limited {
        money: Option<Meter>,
        tokens: Option<Meter>,
    },
}

impl Headroom {
    pub(crate) fn is_exhausted(self) -> bool {
        match self {
            Headroom::Unlimited => false,
            Headroom::Limited { money, tokens } => {
                money.is_some_and(Meter::is_exhausted) || tokens.is_some_and(Meter::is_exhausted)
            }
        }
    }

    /// Whether the **money** ceiling alone is spent.
    ///
    /// Separate from [`is_exhausted`](Self::is_exhausted), which answers
    /// whether the board may *start* more work and so counts both meters.
    /// Only money stops a run already in flight: a token ceiling measures
    /// subscription plans, where the turn is paid for whether or not it is
    /// allowed to finish, so throwing it away buys nothing.
    pub(crate) fn money_exhausted(self) -> bool {
        self.money().is_some_and(Meter::is_exhausted)
    }

    fn money(self) -> Option<Meter> {
        match self {
            Headroom::Unlimited => None,
            Headroom::Limited { money, .. } => money,
        }
    }

    /// Figures for the tightest configured meter.
    pub(crate) fn figures(self) -> Option<Figures> {
        let Headroom::Limited { money, tokens } = self else {
            return None;
        };
        match (money, tokens) {
            (Some(money), Some(tokens)) if speaks_money(money, tokens) => {
                Some(Figures::money(money))
            }
            (_, Some(tokens)) => Some(Figures::tokens(tokens)),
            (Some(money), None) => Some(Figures::money(money)),
            (None, None) => None,
        }
    }
}

pub(crate) fn headroom(
    money_limit: Option<MicroUsd>,
    token_limit: Option<i64>,
    spent: baybo_store::project::Spend,
) -> Headroom {
    let money = Meter::of(
        money_limit.map(MicroUsd::into_micros),
        spent.cost.into_micros(),
    );
    let tokens = Meter::of(token_limit, spent.tokens());
    if money.is_none() && tokens.is_none() {
        return Headroom::Unlimited;
    }
    Headroom::Limited { money, tokens }
}

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
    use baybo_store::project::Spend;

    fn spend(micros: i64, input: i64, output: i64) -> Spend {
        Spend {
            input_tokens: input,
            output_tokens: output,
            cost: MicroUsd::from_micros(micros),
        }
    }

    fn usd(micros: i64) -> Option<MicroUsd> {
        Some(MicroUsd::from_micros(micros))
    }

    #[test]
    fn no_limit_never_gates_and_reports_no_figures() {
        let h = headroom(None, None, spend(999_999_999, 999_999, 999_999));
        assert_eq!(h, Headroom::Unlimited);
        assert!(!h.is_exhausted());
        assert_eq!(h.figures(), None);
    }

    #[test]
    fn the_boundary_is_closed_against_starting_more_work() {
        assert!(headroom(usd(100), None, spend(100, 0, 0)).is_exhausted());
        assert!(headroom(usd(100), None, spend(101, 0, 0)).is_exhausted());
        assert!(!headroom(usd(100), None, spend(99, 0, 0)).is_exhausted());
    }

    #[test]
    fn the_token_boundary_is_closed_the_same_way() {
        assert!(headroom(None, Some(100), spend(0, 60, 40)).is_exhausted());
        assert!(headroom(None, Some(100), spend(0, 60, 41)).is_exhausted());
        assert!(!headroom(None, Some(100), spend(0, 60, 39)).is_exhausted());
    }

    #[test]
    fn a_zero_budget_stops_everything() {
        assert!(headroom(usd(0), None, spend(0, 0, 0)).is_exhausted());
        assert!(headroom(None, Some(0), spend(0, 0, 0)).is_exhausted());
    }

    #[test]
    fn a_money_ceiling_measures_nothing_on_a_plan_that_prices_at_zero() {
        let subscription_day = spend(0, 4_000_000, 1_000_000);
        assert!(!headroom(usd(1), None, subscription_day).is_exhausted());
        assert!(headroom(None, Some(1_000_000), subscription_day).is_exhausted());
    }

    #[test]
    fn either_ceiling_alone_stops_the_board() {
        let day = spend(500, 300, 200);
        assert!(headroom(usd(100), Some(999_999), day).is_exhausted());
        assert!(headroom(usd(999_999), Some(100), day).is_exhausted());
        assert!(!headroom(usd(999_999), Some(999_999), day).is_exhausted());
    }

    #[test]
    fn only_the_money_meter_stops_work_already_in_flight() {
        // A subscription day: the token ceiling is blown many times over and
        // the money ceiling is untouched, because those tokens price at zero.
        let subscription_day = spend(0, 4_000_000, 1_000_000);
        let h = headroom(usd(1_000_000), Some(1_000), subscription_day);
        assert!(h.is_exhausted(), "the board may not start more work");
        assert!(
            !h.money_exhausted(),
            "but nothing in flight is stopped for a ceiling that costs nothing"
        );

        let priced_day = spend(2_000, 10, 10);
        let h = headroom(usd(1_000), Some(999_999), priced_day);
        assert!(
            h.money_exhausted(),
            "real money over the ceiling does stop it"
        );
    }

    #[test]
    fn a_board_with_no_money_ceiling_never_stops_a_run() {
        let h = headroom(None, Some(100), spend(999_999_999, 999, 999));
        assert!(h.is_exhausted());
        assert!(!h.money_exhausted());
        assert!(!headroom(None, None, spend(1, 1, 1)).money_exhausted());
    }

    #[test]
    fn figures_survive_for_the_timeline() {
        assert_eq!(
            headroom(usd(100), None, spend(120, 0, 0)).figures(),
            Some(Figures::Money {
                spent_micros: 120,
                limit_micros: 100
            })
        );
        assert_eq!(
            headroom(None, Some(100), spend(0, 70, 50)).figures(),
            Some(Figures::Tokens {
                spent_tokens: 120,
                limit_tokens: 100
            })
        );
    }

    #[test]
    fn the_ceiling_that_stopped_the_board_is_the_one_it_speaks_in() {
        assert_eq!(
            headroom(usd(1_000), Some(400), spend(120, 300, 200)).figures(),
            Some(Figures::Tokens {
                spent_tokens: 500,
                limit_tokens: 400
            })
        );
        assert_eq!(
            headroom(
                usd(1_000_000),
                Some(1_000_000_000),
                spend(1_200_000, 40_000, 10_000)
            )
            .figures(),
            Some(Figures::Money {
                spent_micros: 1_200_000,
                limit_micros: 1_000_000
            })
        );
        assert_eq!(
            headroom(usd(0), Some(1_000_000), spend(0, 0, 0)).figures(),
            Some(Figures::Money {
                spent_micros: 0,
                limit_micros: 0
            })
        );
    }

    #[test]
    fn a_held_run_and_its_release_are_reported_in_one_unit() {
        let held = headroom(
            usd(1_000_000),
            Some(1_000_000_000),
            spend(1_200_000, 40_000, 10_000),
        );
        assert!(held.is_exhausted());
        let released = headroom(
            usd(5_000_000),
            Some(1_000_000_000),
            spend(1_200_000, 40_000, 10_000),
        );
        assert!(!released.is_exhausted());
        assert!(
            matches!(held.figures(), Some(Figures::Money { .. }))
                && matches!(released.figures(), Some(Figures::Money { .. })),
            "held {:?} then released {:?}",
            held.figures(),
            released.figures()
        );
    }

    #[test]
    fn a_tie_goes_to_tokens() {
        assert_eq!(
            headroom(usd(100), Some(100), spend(50, 25, 25)).figures(),
            Some(Figures::Tokens {
                spent_tokens: 50,
                limit_tokens: 100
            })
        );
    }

    #[test]
    fn the_tighter_meter_is_chosen_without_dividing_or_overflowing() {
        let huge = headroom(
            usd(9_000_000_000_000),
            Some(8_000_000_000),
            spend(8_999_999_999_999, 4_000_000_000, 0),
        );
        assert!(!huge.is_exhausted());
        assert_eq!(
            huge.figures(),
            Some(Figures::Money {
                spent_micros: 8_999_999_999_999,
                limit_micros: 9_000_000_000_000
            }),
            "money is at ~100% and tokens at 50%"
        );
    }

    #[test]
    fn an_exhausted_board_always_has_something_to_report() {
        for h in [
            headroom(usd(0), None, spend(0, 0, 0)),
            headroom(None, Some(0), spend(0, 0, 0)),
            headroom(usd(0), Some(0), spend(0, 0, 0)),
        ] {
            assert!(h.is_exhausted());
            assert!(h.figures().is_some());
        }
    }

    #[test]
    fn tokens_are_the_prompt_plus_the_completion_and_never_the_cache_twice() {
        assert_eq!(spend(0, 700, 300).tokens(), 1_000);
    }

    #[test]
    fn the_window_is_the_utc_day() {
        let now = DateTime::parse_from_rfc3339("2026-08-05T23:59:59Z")
            .expect("rfc3339")
            .with_timezone(&Utc);
        assert_eq!(day_start(now).to_rfc3339(), "2026-08-05T00:00:00+00:00");
        assert_eq!(day_start(day_start(now)), day_start(now));
    }
}
