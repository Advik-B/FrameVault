//! FrameVault — encode arbitrary files into YouTube-survivable video using a
//! video block channel bound together by Reed-Solomon ECC.
//!
//! Pure Rust port of the original Python implementation. The codec core builds for
//! `wasm32-unknown-unknown` too: `media`, the CLI and the file-based entry points sit behind
//! the default `native` feature (FFmpeg via libav).

pub mod budget;
pub mod constants;
pub mod rs;
pub mod frame;
pub mod metadata;
pub mod qr;
#[cfg(feature = "native")]
pub mod media;
pub mod stream;
pub mod encode;
pub mod decode;
