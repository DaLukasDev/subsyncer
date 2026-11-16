//! subsyncer: the CLI/daemon side — FFmpeg, voice activity detection, cache,
//! HTTP API and library scanning around `subsyncer-core`.

pub mod cache;
pub mod config;
pub mod engine;
pub mod logging;
pub mod media;
pub mod scan;
pub mod server;
pub mod vad;
