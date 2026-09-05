//! Shell logic that belongs to neither shell.
//!
//! A store moves here when it satisfies all three clauses of the rule in
//! `docs/modules/mobile/core.md`: pure data logic with no UI-framework or
//! observation coupling, an on-disk format that is pinned or gateway-fed, and a
//! suite that can become a Rust golden test. The point is not to shrink the
//! shells — it is that two shells implementing one rule implement it
//! differently on the second try, and the difference shows up as a row saying
//! something the gateway contradicts.
//!
//! What does NOT belong here is anything the platform owns: pickers, media,
//! WebView hosting, the observation wiring a shell's UI framework needs.

pub(crate) mod title;
