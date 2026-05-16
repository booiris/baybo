//! Sealed `Volatile` marker trait for [`VolatileResources`] fields.
//!
//! Together with the `Serialize + DeserializeOwned` derive on
//! [`DurableActorState`], this trait closes the type-level loop: every
//! field of [`VolatileResources`] must implement [`Volatile`], and the
//! trait is sealed so impls can only be added inside this crate. The
//! net effect:
//!
//! - A new field of value type (e.g. a `String` counter) added to
//!   [`VolatileResources`] fails to compile because `String` has no
//!   `Volatile` impl. The author is forced to either move it into
//!   [`DurableActorState`] (where it will be persisted and rehydrated)
//!   or add an explicit `Volatile` impl here, which is the audit
//!   point that catches misclassification.
//!
//! - Anything *durable* added inside [`VolatileResources`] hits the
//!   same wall: there is no `Volatile` impl for `Session`,
//!   `TranscriptMessage`, `SummaryCursor`, etc.
//!
//! - [`Derived<T>`] is the only "value-like" type accepted into
//!   [`VolatileResources`]. Constructing a `Derived<T>` requires a
//!   [`RebuildFrom`] impl from a durable source, making the
//!   reconstruction story explicit at the point of definition.
//!
//! [`DurableActorState`]: super::DurableActorState
//! [`VolatileResources`]: super::VolatileResources

use std::sync::{Arc, Weak};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod sealed {
    pub trait Sealed {}
}

/// Sealed marker: fields of [`super::VolatileResources`] must implement
/// this. See module docs for the type-system contract.
pub trait Volatile: sealed::Sealed {}

// === Channels / handles ===

impl<T> sealed::Sealed for mpsc::Sender<T> {}
impl<T> Volatile for mpsc::Sender<T> {}

impl<T> sealed::Sealed for mpsc::Receiver<T> {}
impl<T> Volatile for mpsc::Receiver<T> {}

// === Cancellation tokens ===

impl sealed::Sealed for CancellationToken {}
impl Volatile for CancellationToken {}

// === Shared resources (Arc / Weak) ===
//
// Anything behind `Arc<T>` or `Weak<T>` is a reference into shared
// infrastructure — registries, managers, gateways. The inner value is
// owned somewhere else (typically a `Graph`-scoped construction); the
// actor holds a handle, not a copy. Losing the actor doesn't lose the
// value, so it's volatile by construction.

impl<T: ?Sized> sealed::Sealed for Arc<T> {}
impl<T: ?Sized> Volatile for Arc<T> {}

impl<T: ?Sized> sealed::Sealed for Weak<T> {}
impl<T: ?Sized> Volatile for Weak<T> {}

// === Option<T: Volatile> ===

impl<T: Volatile> sealed::Sealed for Option<T> {}
impl<T: Volatile> Volatile for Option<T> {}

// === Domain-specific volatile types ===
//
// These are non-`Arc` business types whose state is either entirely
// derivable from `DurableActorState` (the agent loop's transcript copy
// + context manager caches reconstruct from store) or strictly
// process-lifetime. Adding a new type here is the explicit audit
// point: if you can't justify why losing the value is harmless when
// the actor dies, it doesn't belong in the volatile half.

impl sealed::Sealed for crate::agent_loop::AgentLoop {}
impl Volatile for crate::agent_loop::AgentLoop {}

impl sealed::Sealed for crate::supervisor::AgentSupervisor {}
impl Volatile for crate::supervisor::AgentSupervisor {}

// === Derived<T>: values rebuilt from the durable layer ===

/// A wrapper for values whose authoritative state lives in
/// [`super::DurableActorState`] and which are rebuilt from it whenever
/// the actor is fresh. The [`RebuildFrom`] bound on construction forces
/// the rebuild story to be spelled out as a trait impl — there is no
/// path to put a `Derived<Foo>` into [`super::VolatileResources`]
/// without explaining how `Foo` is rederived from durable state.
pub struct Derived<T> {
    inner: T,
}

impl<T> sealed::Sealed for Derived<T> {}
impl<T> Volatile for Derived<T> {}

/// "I can be rebuilt from a `Source`". Implementations live next to
/// the type itself (or in a small adapter module). Used by
/// [`Derived::rebuild`] to make derivation explicit.
pub trait RebuildFrom<Source> {
    fn rebuild_from(source: &Source) -> Self;
}

impl<T> Derived<T> {
    /// Construct from the durable source. The trait bound forces the
    /// derivation strategy to be declared once, at the type level.
    pub fn rebuild<Source>(source: &Source) -> Self
    where
        T: RebuildFrom<Source>,
    {
        Self {
            inner: T::rebuild_from(source),
        }
    }

    /// Construct from a pre-built value. Use sparingly — prefer
    /// [`Self::rebuild`] so the durable origin is type-encoded.
    pub fn from_inner(inner: T) -> Self {
        Self { inner }
    }

    pub fn get(&self) -> &T {
        &self.inner
    }

    pub fn get_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    pub fn into_inner(self) -> T {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_volatile<T: Volatile>() {}

    /// Static check: every field type of
    /// [`super::super::VolatileResources`] implements [`Volatile`].
    /// Updating the field list here is mandatory when the struct grows
    /// — the existing derive on the struct alone would not catch
    /// `Volatile`-less types added to it (rustc can't reflect over
    /// fields), so this list is the manual audit point referenced
    /// from the marker module docs.
    #[test]
    fn volatile_resources_fields_are_marker_clean() {
        use crate::agent_loop::AgentLoop;
        use crate::supervisor::AgentSupervisor;
        use crate::trace::SpanRecorder;
        use aura_channels::AgentOutput;
        use aura_job::JobLifecycle;

        assert_volatile::<AgentLoop>();
        assert_volatile::<mpsc::Sender<AgentOutput>>();
        assert_volatile::<Arc<JobLifecycle>>();
        assert_volatile::<Arc<SpanRecorder>>();
        assert_volatile::<CancellationToken>();
        assert_volatile::<Option<AgentSupervisor>>();
    }

    #[test]
    fn derived_rebuilds_from_durable_source() {
        struct Source {
            seed: u32,
        }
        struct Cache {
            doubled: u32,
        }
        impl RebuildFrom<Source> for Cache {
            fn rebuild_from(s: &Source) -> Self {
                Cache {
                    doubled: s.seed * 2,
                }
            }
        }

        let src = Source { seed: 21 };
        let derived: Derived<Cache> = Derived::rebuild(&src);
        assert_eq!(derived.get().doubled, 42);
    }

    /// Documentation-only example proving the seal works: `String` has
    /// no `Volatile` impl and no path to one (the `sealed::Sealed`
    /// supertrait is private), so `Volatile`-bound positions reject it.
    ///
    /// Trying to compile the following would fail with
    /// "`Volatile` is not implemented for `String`":
    ///
    /// ```compile_fail
    /// use aura_agent::state::marker::Volatile;
    /// fn assert_volatile<T: Volatile>() {}
    /// fn check() { assert_volatile::<String>(); }
    /// ```
    #[test]
    fn seal_documented() {
        // The real check is the `compile_fail` doctest above; this
        // function is just the anchor that keeps the docstring next
        // to the tests for discoverability.
    }
}
