mod client;
mod install;
mod protocol;
mod server;
mod tracing;

pub use install::{ServiceStatus, install, restart, status, uninstall};
pub use server::run_service;
pub use tracing::init_tracing;

pub(crate) use client::{
    install_attempted, install_via_uac, is_available, manual_uninstall_requested,
    mark_path_repair_attempted, path_repair_attempted, spawn_agent,
};
pub use client::{
    is_installed as service_installed, is_running as service_running, mark_install_attempted,
    restart_via_uac, uninstall_via_uac,
};
