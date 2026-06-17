//! FrameVault — encode arbitrary files into YouTube-survivable video using a
//! video block channel bound together by Reed-Solomon ECC.
//!
//! Pure Rust port of the original Python implementation.

pub mod constants;
pub mod rs;
pub mod frame;
pub mod metadata;
pub mod qr;
pub mod media;
pub mod stream;
pub mod encode;
pub mod decode;
