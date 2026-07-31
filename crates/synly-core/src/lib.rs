#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

#[cfg(feature = "uniffi")]
pub mod ffi;

pub mod capabilities;
pub mod client;
pub mod crypto;
pub mod device;
pub mod discovery;
pub mod identity;
pub mod input;
pub mod protocol;
pub mod settings;
pub mod workspace;
