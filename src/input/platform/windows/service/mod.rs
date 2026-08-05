mod client;
mod install;
mod protocol;
mod server;
mod tracing;

pub use install::{ServiceStatus, install, status, uninstall};
pub use server::run_service;
pub use tracing::init_tracing;

pub use client::{
    is_installed as service_installed, mark_install_attempted, uninstall_via_uac,
};
pub(crate) use client::{
    install_attempted, install_via_uac, is_available, mark_path_repair_attempted,
    manual_uninstall_requested, path_repair_attempted, spawn_agent,
};
