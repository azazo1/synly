//! 承载连接的异步字节流与小包复用, 不依赖 TCP 地址.

pub mod bluetooth;
pub mod clipboard;
pub mod clipboard_sender;
pub mod clipboard_route;
pub mod binding;
pub mod bridge;
pub mod frames;
pub mod generation;
pub mod logical;
pub mod lan_candidate;
pub mod mux;
pub mod routing;
pub mod stream;
