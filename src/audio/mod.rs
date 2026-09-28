mod capture;
mod codec;
mod config;
mod error;
mod fec;
#[cfg(all(test, any(target_os = "macos", target_os = "windows")))]
mod hardware_tests;
mod platform;
mod playback;
mod protocol;
mod receiver;
mod runtime;
#[cfg(feature = "sdl2-audio")]
mod sdl2;
mod sender;

pub use config::{AudioLayout, CodecConfig};
pub use runtime::{
    AudioChannelDirection, AudioTaskHandle, bind_and_spawn_receiver_with_config,
    spawn_sender_with_config,
};
