//! Admin surface (TCP + bearer token). Hosts config/status/jobs/cron/
//! memory/traces/skills/tools/llm and a read-only channel list. No chat
//! content flows through these endpoints.

pub mod channels;
pub mod config;
pub mod cron;
pub mod jobs;
pub mod llm;
pub mod memory;
pub mod skills;
pub mod status;
pub mod tools;
pub mod traces;

use axum::Router;

use crate::server::AdminState;

/// Compose the admin v1 router. The caller wraps this in the admin auth
/// middleware and mounts it under `/v1`.
pub fn v1_router() -> Router<AdminState> {
    Router::new()
        .merge(status::routes())
        .merge(config::routes())
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

    #[test]
    fn admin_v1_router_assembles_without_panic() {
        let _ = v1_router();
    }
}
