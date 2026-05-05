//! `MicroUsd` — the only money type in the system.
//!
//! All cost / pricing / budget values flow through this newtype. Stored
//! as `i64` micro-USD (USD × 10^6), which gives sub-cent precision while
//! covering ~9.2 trillion USD of dynamic range — far beyond any
//! realistic spend window. Exact integer arithmetic, exact equality,
//! exact persistence (`INTEGER` SQL columns).
//!
//! Float `f64` is reserved for two boundaries only:
//!
//! * **Config TOML** — operators write `user_daily_usd = 5.0`. The
//!   [`usd_decimal_option`] serde-with module bridges that to
//!   `Option<MicroUsd>` on load and back to a USD float on save.
//! * **Display** — [`MicroUsd::as_usd_decimal`] for human-facing
//!   formatting. Never round-tripped back into a `MicroUsd` after
//!   arithmetic.
//!
//! Token-cost math uses [`MicroUsd::cost_for_tokens`], which performs
//! integer multiply-then-divide with the rounding pushed to the final
//! step (≤ 1 micro-USD truncation per call).

use std::fmt;
use std::iter::Sum;
use std::ops::{Add, AddAssign, Sub, SubAssign};

use serde::{Deserialize, Serialize};

/// Money expressed in micro-USD. `MicroUsd(1_000_000)` == \$1.00.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct MicroUsd(i64);

impl MicroUsd {
    /// One micro-USD per 10^6 USD.
    pub const PER_USD: i64 = 1_000_000;
    pub const ZERO: Self = Self(0);

    pub const fn from_micros(micros: i64) -> Self {
        Self(micros)
    }

    pub const fn into_micros(self) -> i64 {
        self.0
    }

    /// Boundary helper for human-facing TOML / CLI input. Rounds to the
    /// nearest micro-USD; values outside `i64` saturate. Must not appear
    /// in normal arithmetic — use [`MicroUsd::cost_for_tokens`] and
    /// `+` / `-` instead.
    pub fn from_usd_decimal(usd: f64) -> Self {
        if !usd.is_finite() {
            return Self::ZERO;
        }
        let scaled = (usd * Self::PER_USD as f64).round();
        if scaled >= i64::MAX as f64 {
            Self(i64::MAX)
        } else if scaled <= i64::MIN as f64 {
            Self(i64::MIN)
        } else {
            Self(scaled as i64)
        }
    }

    /// Display-only USD view. Lossy across the f64 boundary by design;
    /// never feed the result back into a `MicroUsd`.
    pub fn as_usd_decimal(self) -> f64 {
        self.0 as f64 / Self::PER_USD as f64
    }

    /// Compute spend for a token count given a "per 1M tokens" price.
    ///
    /// Integer math via i128 to avoid overflow on realistic token
    /// volumes (`tokens.max * price.max` fits in i128). Truncates the
    /// final division — at worst one micro-USD lost per call, well
    /// below the precision a billing system reports.
    pub fn cost_for_tokens(per_million: Self, tokens: u64) -> Self {
        let raw = (tokens as i128) * (per_million.0 as i128) / 1_000_000;
        Self(raw.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
    }
}

impl fmt::Display for MicroUsd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "${:.6}", self.as_usd_decimal())
    }
}

impl Add for MicroUsd {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }
}

impl AddAssign for MicroUsd {
    fn add_assign(&mut self, rhs: Self) {
        self.0 = self.0.saturating_add(rhs.0);
    }
}

impl Sub for MicroUsd {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }
}

impl SubAssign for MicroUsd {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 = self.0.saturating_sub(rhs.0);
    }
}

impl Sum for MicroUsd {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::ZERO, Add::add)
    }
}

impl<'a> Sum<&'a MicroUsd> for MicroUsd {
    fn sum<I: Iterator<Item = &'a MicroUsd>>(iter: I) -> Self {
        iter.copied().fold(Self::ZERO, Add::add)
    }
}

/// `serde(with = …)` bridge that lets TOML/JSON keep writing USD floats
/// (`user_daily_usd = 5.0`) while the in-memory value is `Option<MicroUsd>`.
pub mod usd_decimal_option {
    use super::MicroUsd;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(v: &Option<MicroUsd>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match v {
            None => s.serialize_none(),
            Some(m) => s.serialize_f64(m.as_usd_decimal()),
        }
    }

    pub fn deserialize<'de, D>(d: D) -> Result<Option<MicroUsd>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw: Option<f64> = Option::deserialize(d)?;
        Ok(raw.map(MicroUsd::from_usd_decimal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_usd_decimal_round_trips_typical_prices() {
        // Anthropic Sonnet input ($3.00 / MTok) and Haiku input ($0.25 / MTok).
        assert_eq!(MicroUsd::from_usd_decimal(3.0).into_micros(), 3_000_000);
        assert_eq!(MicroUsd::from_usd_decimal(0.25).into_micros(), 250_000);
        assert_eq!(MicroUsd::from_usd_decimal(0.0).into_micros(), 0);
    }

    #[test]
    fn from_usd_decimal_handles_non_finite_safely() {
        assert_eq!(MicroUsd::from_usd_decimal(f64::NAN), MicroUsd::ZERO);
        assert_eq!(MicroUsd::from_usd_decimal(f64::INFINITY), MicroUsd::ZERO);
    }

    #[test]
    fn cost_for_tokens_does_integer_math() {
        // Sonnet: 1k input @ $3/MTok = $0.003 = 3000 micro-USD.
        let price = MicroUsd::from_usd_decimal(3.0);
        assert_eq!(MicroUsd::cost_for_tokens(price, 1_000).into_micros(), 3_000);

        // Sub-micro-USD per token (Haiku at $0.25/MTok = 0.25 micro-USD/token).
        // 4 tokens → 1 micro-USD. 3 tokens → 0 micro-USD (truncates).
        let haiku = MicroUsd::from_usd_decimal(0.25);
        assert_eq!(MicroUsd::cost_for_tokens(haiku, 4).into_micros(), 1);
        assert_eq!(MicroUsd::cost_for_tokens(haiku, 3).into_micros(), 0);
    }

    #[test]
    fn arithmetic_saturates_instead_of_panicking() {
        let max = MicroUsd::from_micros(i64::MAX);
        assert_eq!((max + MicroUsd::from_micros(1)).into_micros(), i64::MAX);
        let min = MicroUsd::from_micros(i64::MIN);
        assert_eq!((min - MicroUsd::from_micros(1)).into_micros(), i64::MIN);
    }

    #[test]
    fn sum_iterates_over_micro_usd() {
        let xs = [
            MicroUsd::from_micros(100),
            MicroUsd::from_micros(250),
            MicroUsd::from_micros(50),
        ];
        let total: MicroUsd = xs.iter().copied().sum();
        assert_eq!(total.into_micros(), 400);
        let total_ref: MicroUsd = xs.iter().sum();
        assert_eq!(total_ref.into_micros(), 400);
    }

    #[test]
    fn serde_round_trips_as_integer_micros() {
        let v = MicroUsd::from_micros(3_500_000);
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, "3500000");
        let back: MicroUsd = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn usd_decimal_option_bridges_human_facing_floats() {
        // Mirrors the path TOML configs go through (serde_json stands in
        // for the actual parser — both produce an `Option<f64>` from the
        // same scalar shape).
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct W {
            #[serde(with = "usd_decimal_option")]
            v: Option<MicroUsd>,
        }
        let parsed: W = serde_json::from_str(r#"{"v": 5.0}"#).unwrap();
        assert_eq!(parsed.v, Some(MicroUsd::from_usd_decimal(5.0)));
        let rendered = serde_json::to_string(&parsed).unwrap();
        assert_eq!(rendered, r#"{"v":5.0}"#);

        let none: W = serde_json::from_str(r#"{"v": null}"#).unwrap();
        assert_eq!(none.v, None);
    }
}
