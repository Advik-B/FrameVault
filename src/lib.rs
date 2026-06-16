//! FrameVault — encode arbitrary files into YouTube-survivable video using an
//! independent video block channel and an audio 4-FSK channel, bound together
//! by Reed-Solomon ECC.
//!
//! Pure Rust port of the original Python implementation.

pub mod constants;
pub mod rs;
pub mod frame;
pub mod audio;
pub mod metadata;
pub mod qr;
pub mod media;
pub mod encode;
pub mod decode;
