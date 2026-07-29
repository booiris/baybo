// Aggregator: see Cargo.toml `autotests = false` rationale.

#[path = "admin_has_no_channels.rs"]
mod admin_has_no_channels;
#[path = "agents_api.rs"]
mod agents_api;
#[path = "auth.rs"]
mod auth;
#[path = "channel_ws.rs"]
mod channel_ws;
#[path = "chat_api.rs"]
mod chat_api;
#[path = "cron_api.rs"]
mod cron_api;
#[path = "device_channel_ws.rs"]
mod device_channel_ws;
#[path = "llm_endpoint.rs"]
mod llm_endpoint;
#[path = "logs_endpoint.rs"]
mod logs_endpoint;
#[path = "openapi_spec_sync.rs"]
mod openapi_spec_sync;
#[path = "turns_pagination.rs"]
mod turns_pagination;
