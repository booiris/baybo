//! HTTP route modules for the gateway's v1 admin API.
//!
//! Each sub-module owns a contiguous slice of the REST surface and
//! registers its axum `Router` fragment via [`routes`]. Handlers receive
//! the shared [`crate::server::ApiState`] and translate manager results
//! into serializable DTOs defined in [`dto`].

pub mod channels;
pub mod config;
pub mod cron;
pub mod dto;
pub mod health;
pub mod jobs;
pub mod llm;
pub mod memory;
pub mod messages;
pub mod sessions;
pub mod skills;
pub mod status;
pub mod tools;
pub mod traces;

use axum::Router;

use crate::server::ApiState;

/// Compose all authenticated v1 routers. The caller wraps this in the
/// auth middleware and mounts it under `/v1`.
pub fn v1_router() -> Router<ApiState> {
    Router::new()
        .merge(status::routes())
        .merge(config::routes())
        .merge(sessions::routes())
        .merge(messages::routes())
        .merge(jobs::routes())
        .merge(cron::routes())
        .merge(memory::routes())
        .merge(traces::routes())
        .merge(skills::routes())
        .merge(tools::routes())
        .merge(channels::routes())
        .merge(llm::routes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test: axum 0.8 panics at `Router::route` if any path
    /// uses the legacy `:param` syntax instead of `{param}`. The panic
    /// only surfaces the first time a route is added, so if every module
    /// compiles into `v1_router` the whole surface is syntactically
    /// sound. Without this, a route typo only blows up at `start` time
    /// with no type-level signal.
    #[test]
    fn v1_router_assembles_without_panic() {
        let _ = v1_router();
    }

    #[test]
    fn health_router_assembles_without_panic() {
        let _: Router = health::routes();
    }
}
