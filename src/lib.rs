//! Forge: a SIMD-accelerated EBU R128 / ITU-R BS.1770-5 loudness normalizer.
//!
//! This library crate exposes the WAV I/O, DSP, and normalization engine so it
//! can be embedded in other tools or exercised by integration tests. The
//! `forge` binary in `src/main.rs` is a thin CLI wrapper over this engine.
//!
//! # API stability
//!
//! Starting with v0.94.0, every documented public item is covered by Forge's
//! source-compatibility policy, including items enabled by optional Cargo
//! features. See `API-STABILITY.md` in the repository for the complete
//! contract and its deliberately narrow exceptions.

// clippy misreads `*emphasis*` inside doc comments as markdown list bullets and
// then flags the surrounding lines as lazy continuations. It's a false positive
// on emphasis markers, so allow it crate-wide.
#![allow(clippy::doc_lazy_continuation)]

#[cfg(feature = "ffmpeg-encoding")]
pub mod aac;
mod aac_qc;
mod aaf_effect_qc;
mod aaf_meta_qc;
mod aaf_object_qc;
mod aaf_qc;
mod ac3_qc;
pub mod ac4_adapter;
pub mod adm;
pub mod aes31_qc;
mod alac_qc;
pub mod analysis;
pub mod analysis_cache;
pub mod anomaly_provider;
mod atomic;
pub mod audio_compare;
pub mod batch;
mod bwf_xml_qc;
pub mod c_api;
pub mod catalogue;
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
pub mod dsd;
pub mod dsp;
pub mod dts_adapter;
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
mod monkeys_audio_qc;
mod mp3_qc;
#[cfg(feature = "mp3-encoding")]
pub mod mp3enc;
pub mod mpegh_adapter;
mod mpegts_qc;
pub mod multi_delivery;
mod mxf_qc;
pub mod nmos_qc;
pub mod normalization_diff;
pub mod normalize;
mod ogg_qc;
#[cfg(feature = "onnx-provider")]
pub mod onnx_provider;
#[cfg(feature = "opus-encoding")]
pub mod opus;
pub mod opus_tags;
mod pcm_container_qc;
pub mod presentation_qc;
pub mod preset;
pub mod provenance;
pub mod qc;
pub mod realtime;
pub mod report;
pub mod report_tools;
pub mod rtp_qc;
pub mod sadm;
pub mod segment_normalize;
pub mod watch;
pub mod wav;
mod wavpack_qc;
