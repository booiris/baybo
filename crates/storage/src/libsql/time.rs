//! Unix-microsecond <-> `DateTime<Utc>` conversion helpers.
//!
//! Every timestamp column in the libsql schema stores an `INTEGER`
//! count of microseconds since the Unix epoch. Going through these
//! helpers keeps the unit explicit at every site.
//!
//! µs is a deliberate compromise:
//! - chrono's `timestamp_micros()` is infallible (i64 µs covers
//!   ±292,000 years, well outside any plausible `DateTime`)
//! - finer than ms, so sub-millisecond span ordering survives
//!   round-trip — useful for fast local tool calls
//! - coarser than chrono's native ns, so we don't carry a
//!   pseudo-precision through the DB that nobody consumes
//!
//! API surfaces (gateway HTTP / OpenAPI / web UI) are not expected
//! to expose µs directly — they serialize `DateTime<Utc>` as RFC3339
//! and convert to whatever unit the consumer expects.

use chrono::{DateTime, Utc};

/// Encode a `DateTime<Utc>` for storage as `INTEGER` µs.
pub(crate) fn to_us(dt: DateTime<Utc>) -> i64 {
    dt.timestamp_micros()
}

/// Decode an `INTEGER` µs column back to `DateTime<Utc>`. Falls back
/// to `Utc::now()` only if a row carries an out-of-range value
/// (impossible in practice — `to_us` accepts every chrono `DateTime`).
pub(crate) fn from_us(us: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_micros(us).unwrap_or_else(Utc::now)
}

/// `Utc::now()` as Unix µs.
pub(crate) fn now_us() -> i64 {
    to_us(Utc::now())
}
