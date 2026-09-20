mod capture;
mod codec;
mod config;
mod error;
mod fec;
mod platform;
mod playback;
mod protocol;
mod receiver;
mod runtime;
mod sender;
#[cfg(all(test, any(target_os = "macos", target_os = "windows")))]
mod hardware_tests;
#[cfg(feature = "sdl2-audio")]
mod sdl2;

pub use config::{AudioLayout, CodecConfig};
pub use runtime::{
    AudioChannelDirection, AudioTaskHandle, bind_and_spawn_receiver_with_config, spawn_sender_with_config,
};
