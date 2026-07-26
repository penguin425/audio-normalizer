//! Forge: a SIMD-accelerated EBU R128 / ITU-R BS.1770-5 loudness normalizer.
//!
//! This library crate exposes the WAV I/O, DSP, and normalization engine so it
//! can be embedded in other tools or exercised by integration tests. The
//! `forge` binary in `src/main.rs` is a thin CLI wrapper over this engine.

// clippy misreads `*emphasis*` inside doc comments as markdown list bullets and
// then flags the surrounding lines as lazy continuations. It's a false positive
// on emphasis markers, so allow it crate-wide.
#![allow(clippy::doc_lazy_continuation)]

#[cfg(feature = "ffmpeg-encoding")]
pub mod aac;
pub mod adm;
mod atomic;
#[cfg(feature = "clap-plugin")]
pub mod clap_plugin;
pub mod cli;
pub mod codec_qc;
pub mod compare;
pub mod container_qc;
pub mod decoder;
pub mod dialogue_provider;
pub mod dsp;
pub mod flacenc;
#[cfg(feature = "lv2-plugin")]
mod lv2;
pub mod metadata;
#[cfg(feature = "mp3-encoding")]
pub mod mp3enc;
pub mod normalize;
#[cfg(feature = "opus-encoding")]
pub mod opus;
pub mod presentation_qc;
pub mod preset;
pub mod qc;
pub mod realtime;
pub mod report;
pub mod sadm;
pub mod wav;
