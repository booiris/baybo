// Aggregator: see Cargo.toml `autotests = false` rationale.

#[path = "admin_has_no_channels.rs"]
mod admin_has_no_channels;
#[path = "auth.rs"]
mod auth;
#[path = "channel_ws.rs"]
mod channel_ws;
#[path = "chat_api.rs"]
mod chat_api;
#[path = "jobs_pagination.rs"]
mod jobs_pagination;
#[path = "llm_endpoint.rs"]
mod llm_endpoint;
#[path = "logs_endpoint.rs"]
mod logs_endpoint;
#[path = "openapi_spec_sync.rs"]
mod openapi_spec_sync;
