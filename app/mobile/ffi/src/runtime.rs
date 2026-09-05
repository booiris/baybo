//! The core's tokio runtime. UniFFI polls exported async fns on its own FFI
//! machinery with no ambient tokio context, but the transport uses
//! `tokio::spawn` / `tokio::time` / `tokio::net` throughout — so every exported
//! async body is spawned onto this owned multi-thread runtime and awaited via
//! its (runtime-independent) `JoinHandle`. Detached tasks the transport spawns
//! (the chat pump, the pairing pump) inherit the runtime through that spawn and
//! keep running after the exported call returns.

use std::sync::OnceLock;

use crate::api::BayboError;

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// The process-wide runtime, built on first use. Two workers: the transport is
/// a handful of IO/timer-bound tasks (per-leg supervisor loops, the chat pump,
/// dial children, ack timers, one-shot blob/pairing legs).
pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            // Runtime construction fails only on OS-level resource exhaustion —
            // nothing the app can recover from or degrade around.
            .unwrap_or_else(|e| panic!("build tokio runtime: {e}"))
    })
}

/// Run `fut` on the core runtime and fold its `String` error into [`BayboError`]
/// at the FFI boundary.
pub(crate) async fn run<T: Send + 'static>(
    fut: impl Future<Output = Result<T, String>> + Send + 'static,
) -> Result<T, BayboError> {
    match runtime().spawn(fut).await {
        Ok(result) => result.map_err(BayboError::from_msg),
        Err(e) => Err(BayboError::Other {
            message: format!("core task failed: {e}"),
        }),
    }
}
