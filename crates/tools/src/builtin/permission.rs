//! The configured `permission` mode, as the tools see it.
//!
//! The mirror of `baybo_config::PermissionPolicy` on this side of the crate
//! boundary — `baybo-tools` does not depend on `baybo-config`, so the boot
//! path maps one onto the other.
//!
//! It lives here rather than in [`super::bash`] because it is no longer
//! Bash's alone: `Free` waives the approval gate, and the tools that consult
//! that are `Bash`, `Write` and `Edit`. What stays Bash's is the *sandbox*
//! half of the same setting — which route a command takes, and the
//! `bench-bash` build that overrides it — and that half stays in `bash`.

use std::sync::atomic::{AtomicU8, Ordering};

/// How much a tool must ask before acting.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PermissionMode {
    #[default]
    Auto = 0,
    Manual = 1,
    Free = 2,
}

impl PermissionMode {
    /// Whether the approval gate is waived outright.
    ///
    /// The one home for the question, because the tools that ask it declare
    /// their resources independently and a second spelling would be a tool
    /// that kept prompting after the operator turned prompting off. It is
    /// deliberately *not* [`super::bash::permission_skips_os_sandbox`]: that
    /// one answers which execution route a command takes and folds in the
    /// `bench-bash` build, which has no bearing on whether a file write
    /// needs a human.
    ///
    /// `Free` is the whole of it. `Auto` and `Manual` differ in how much
    /// Bash asks, never in whether the gate exists.
    pub fn waives_approval(self) -> bool {
        self == Self::Free
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Manual,
            2 => Self::Free,
            _ => Self::Auto,
        }
    }

    /// The discriminant. Also the index of this mode's pre-rendered `Bash`
    /// description, which is why it is not private — the array is sized to
    /// this enum, so a variant added without extending it fails to compile
    /// rather than panicking on the first call in the new mode.
    pub(crate) fn encode(self) -> u8 {
        self as u8
    }
}

/// Shared, hot-swappable permission mode. A config reload calls [`Self::set`]
/// and every tool holding the `Arc` sees it on its next call — for `Bash`,
/// both the execution path and the live-rendered tool description.
/// Lock-free: the mode is a single byte read on the hot path.
pub struct LivePermissionMode(AtomicU8);

impl LivePermissionMode {
    pub fn new(permission: PermissionMode) -> Self {
        Self(AtomicU8::new(permission.encode()))
    }

    pub fn get(&self) -> PermissionMode {
        PermissionMode::from_u8(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, permission: PermissionMode) {
        self.0.store(permission.encode(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_free_waives_the_gate() {
        assert!(PermissionMode::Free.waives_approval());
        assert!(!PermissionMode::Auto.waives_approval());
        assert!(!PermissionMode::Manual.waives_approval());
    }

    #[test]
    fn every_mode_survives_the_atomic_round_trip() {
        for mode in [
            PermissionMode::Auto,
            PermissionMode::Manual,
            PermissionMode::Free,
        ] {
            let live = LivePermissionMode::new(PermissionMode::default());
            live.set(mode);
            assert_eq!(live.get(), mode, "{mode:?}");
        }
    }
}
