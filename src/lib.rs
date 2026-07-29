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
mod aac_qc;
mod ac3_qc;
pub mod adm;
mod atomic;
pub mod audio_compare;
mod bwf_xml_qc;
#[cfg(feature = "clap-plugin")]
pub mod clap_plugin;
pub mod cli;
pub mod codec_qc;
pub mod compare;
pub mod container_qc;
pub mod dash_observe;
mod dash_patch;
pub mod dash_qc;
pub mod decoder;
pub mod dialogue_provider;
pub mod dsp;
mod flac_qc;
pub mod flacenc;
pub mod hls_qc;
mod iamf_qc;
mod id3_qc;
pub mod imf_qc;
mod isobmff_qc;
#[cfg(feature = "lv2-plugin")]
mod lv2;
mod matroska_qc;
pub mod metadata;
mod mp3_qc;
#[cfg(feature = "mp3-encoding")]
pub mod mp3enc;
mod mpegts_qc;
mod mxf_qc;
pub mod nmos_qc;
pub mod normalize;
mod ogg_qc;
#[cfg(feature = "opus-encoding")]
pub mod opus;
mod pcm_container_qc;
pub mod presentation_qc;
pub mod preset;
pub mod provenance;
pub mod qc;
pub mod realtime;
pub mod report;
pub mod rtp_qc;
pub mod sadm;
pub mod wav;
