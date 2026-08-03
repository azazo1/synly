mod client;
pub(crate) mod pipe;
pub(crate) mod protocol;
pub(crate) mod security;
mod server;
mod tracing;

use std::time::Duration;

pub use client::request_elevation;
pub use server::run_agent;
pub(in crate::input) use client::{elevation_requested, is_ready, start_client};
pub(super) use tracing::init as init_tracing;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DISPATCH_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_DELIVERY_TIMEOUT: Duration = Duration::from_secs(4);
const CLIENT_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);
const AGENT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(test)]
mod tests;
