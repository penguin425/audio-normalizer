//! Stable runtime evidence for normalization job fingerprints.
//!
//! A batch job's semantic identity must describe the implementation that can
//! affect its bytes, while excluding process-local routing details.  This
//! module therefore records the selected decoder/track, measurement revisions,
//! and ordered format selection plus de-duplicated writer evidence.  It
//! deliberately does not include paths to inputs or caches, cache hit state,
//! timestamps, or process identifiers.

use crate::analysis::AnalysisEngine;
use crate::bound_analysis::{BOUND_ANALYSIS_VERSION, MEASUREMENT_ALGORITHM_REVISION};
use crate::decoder::InputDescriptor;
use crate::normalize::OutputFormat;
use crate::stable_input::INPUT_CONTENT_BINDING_VERSION;
use serde_json::{json, Value};

/// Versioned schema identifier for [`normalization_semantic_context`].
pub const NORMALIZATION_SEMANTIC_CONTEXT_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/normalization-semantic-context-v1";

/// Version of the semantic context representation.
pub const NORMALIZATION_SEMANTIC_CONTEXT_VERSION: u32 = 1;

/// Revision used when hashing the semantic context into a job identity.
///
/// This is separate from the JSON schema version so the caller can revise the
/// envelope/hash policy without pretending that the evidence fields changed.
pub const NORMALIZATION_FINGERPRINT_REVISION: u32 = 1;

/// Alias documenting the same stable fingerprint revision for callers that
/// use the context terminology.
pub const NORMALIZATION_SEMANTIC_CONTEXT_REVISION: u32 = NORMALIZATION_FINGERPRINT_REVISION;

/// Maximum ordered output selections represented by one normalization context.
pub const MAX_NORMALIZATION_SEMANTIC_OUTPUTS: usize = 100_000;

/// Revision of the format-independent normalization pipeline.
pub const NORMALIZATION_PIPELINE_REVISION: &str = "forge-normalization-pipeline-v1";

/// Semantic revision of the input descriptor/decoder route binding.
pub const INPUT_DESCRIPTOR_SEMANTIC_REVISION: &str = "forge-input-descriptor-v2";

/// Reviewed native WAVE writer implementation identity.
pub const WAV_WRITER_IMPLEMENTATION_ID: &str = "forge::wav::WavStreamWriter";
/// Reviewed native WAVE writer pipeline revision.
pub const WAV_WRITER_PIPELINE_REVISION: &str = "forge-wav-writer-v1";

/// Reviewed native FLAC writer implementation identity.
pub const FLAC_WRITER_IMPLEMENTATION_ID: &str = "forge::flacenc::FlacStreamWriter";
/// Reviewed native FLAC writer pipeline revision.
pub const FLAC_WRITER_PIPELINE_REVISION: &str = "forge-flac-writer-v1";

/// Reviewed MP3 writer implementation identity.
pub const MP3_WRITER_IMPLEMENTATION_ID: &str = "forge::mp3enc::Mp3StreamWriter";

/// Reviewed Ogg Opus writer implementation identity.
pub const OPUS_WRITER_IMPLEMENTATION_ID: &str = "forge::opus::OpusStreamWriter";

/// Reviewed FFmpeg-backed writer implementation identity.
pub const FFMPEG_WRITER_IMPLEMENTATION_ID: &str = "forge::aac::AacStreamWriter";
/// Reviewed FFmpeg-backed writer pipeline revision.
pub const FFMPEG_WRITER_PIPELINE_REVISION: &str = "forge-ffmpeg-writer-v1";

/// Build the stable semantic context used by a normalization job identity.
///
/// `audio_track` is the requested zero-based audio-track index, or `None` for
/// the container's default audio track.  It is deliberately not an
/// `InputDescriptor`: a batch context is shared by many assets, and one
/// asset's probed codec or route must not become the semantic identity for all
/// of them.  The batch manifest binds each input's complete content hash; the
/// selected track and versioned decoder then derive the per-asset descriptor
/// deterministically from those bytes.  Callers that need a separate audit
/// record can use [`input_descriptor_semantic_evidence`]. `output_formats`
/// retains the exact order (including duplicates), while `writers` contains
/// one evidence object per distinct format in first-occurrence order.  This
/// keeps the context bounded even for the 100,000-asset batch limit.
///
/// FFmpeg, LAME, and libopus runtime probes are performed only when their
/// corresponding format is requested.  Callers that promise a side-effect-
/// free dry run should not invoke this function for those formats.
pub fn normalization_semantic_context(
    analysis_engine: AnalysisEngine,
    audio_track: Option<u32>,
    formats: &[OutputFormat],
) -> Result<Value, String> {
    if formats.len() > MAX_NORMALIZATION_SEMANTIC_OUTPUTS {
        return Err(format!(
            "normalization semantic context exceeds the {MAX_NORMALIZATION_SEMANTIC_OUTPUTS}-output limit"
        ));
    }
    let mut output_formats = Vec::with_capacity(formats.len());
    let mut writers = Vec::new();
    let mut seen_formats = Vec::new();
    for &format in formats {
        output_formats.push(format_id(format));
        if !seen_formats.contains(&format) {
            writers.push(writer_evidence(format)?);
            seen_formats.push(format);
        }
    }

    let engine_id = analysis_engine.id();
    let track_selection = if audio_track.is_some() {
        "index"
    } else {
        "default"
    };
    Ok(json!({
        "schema": NORMALIZATION_SEMANTIC_CONTEXT_SCHEMA,
        "revision": NORMALIZATION_SEMANTIC_CONTEXT_VERSION,
        "bound_analysis_version": BOUND_ANALYSIS_VERSION,
        "measurement_algorithm_revision": MEASUREMENT_ALGORITHM_REVISION,
        "input_content_binding_version": INPUT_CONTENT_BINDING_VERSION,
        "analysis_engine": {
            "id": engine_id,
        },
        "analysis_engine_id": engine_id,
        "decoder": {
            "semantic_revision": INPUT_DESCRIPTOR_SEMANTIC_REVISION,
            "input_descriptor_version": crate::decoder::INPUT_DESCRIPTOR_VERSION,
        },
        "input_descriptor_semantic_revision": INPUT_DESCRIPTOR_SEMANTIC_REVISION,
        "audio_track": audio_track,
        "audio_track_selection": {
            "kind": track_selection,
            "index": audio_track,
        },
        "normalization": {
            "pipeline_revision": NORMALIZATION_PIPELINE_REVISION,
        },
        "output_formats": output_formats,
        "writers": writers,
    }))
}

/// Export the stable subset of one already-probed input descriptor that can
/// affect decoding and measurement.
///
/// This helper is intentionally separate from [`normalization_semantic_context`]:
/// a batch containing many assets must not put one asset's codec or route in a
/// context shared by every asset.  Callers that retain per-asset audit data can
/// attach this value to that asset after probing it.
pub fn input_descriptor_semantic_evidence(audio_track: &InputDescriptor) -> Result<Value, String> {
    let info = audio_track.stream_info();
    let declared_channel_layout = serde_json::to_value(audio_track.declared_channel_layout())
        .map_err(|error| format!("encode declared channel-layout evidence: {error}"))?;
    let effective_channel_layout = serde_json::to_value(audio_track.channel_layout())
        .map_err(|error| format!("encode effective channel-layout evidence: {error}"))?;
    let channel_roles = serde_json::to_value(&info.channel_roles)
        .map_err(|error| format!("encode channel-role evidence: {error}"))?;
    Ok(json!({
        "semantic_revision": INPUT_DESCRIPTOR_SEMANTIC_REVISION,
        "input_descriptor_version": audio_track.version(),
        "decoder_route": audio_track.decoder_route_id(),
        "container": audio_track.container().id(),
        "codec": audio_track.codec().id(),
        "audio_track_index": audio_track.track_index(),
        "audio_track_id": audio_track.track_id(),
        "source_start_frame": audio_track.source_range().start(),
        "source_frames": audio_track.source_range().frames(),
        "sample_rate_hz": info.sample_rate,
        "channels": info.channels,
        "pcm_kind": pcm_kind_id(info.source_kind),
        "channel_roles": channel_roles,
        "declared_layout_provenance": audio_track.declared_layout_provenance(),
        "explicit_channel_roles": audio_track.uses_explicit_channel_roles(),
        "declared_channel_layout": declared_channel_layout,
        "effective_channel_layout": effective_channel_layout,
        "explicit_channel_layout": audio_track.uses_explicit_channel_layout(),
    }))
}

fn pcm_kind_id(kind: crate::wav::PcmKind) -> &'static str {
    match kind {
        crate::wav::PcmKind::U8 => "pcm-u8",
        crate::wav::PcmKind::S16 => "pcm-s16le",
        crate::wav::PcmKind::S24 => "pcm-s24le",
        crate::wav::PcmKind::S32 => "pcm-s32le",
        crate::wav::PcmKind::F32 => "pcm-f32le",
        crate::wav::PcmKind::F64 => "pcm-f64le",
    }
}

const fn format_id(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Wav => "wav",
        OutputFormat::Flac => "flac",
        OutputFormat::Mp3 => "mp3",
        OutputFormat::Opus => "opus",
        OutputFormat::M4a => "m4a",
        OutputFormat::Alac => "alac",
        OutputFormat::Vorbis => "vorbis",
    }
}

fn writer_evidence(format: OutputFormat) -> Result<Value, String> {
    match format {
        OutputFormat::Wav => Ok(native_writer_evidence(
            "wav",
            WAV_WRITER_IMPLEMENTATION_ID,
            WAV_WRITER_PIPELINE_REVISION,
        )),
        OutputFormat::Flac => Ok(native_writer_evidence(
            "flac",
            FLAC_WRITER_IMPLEMENTATION_ID,
            FLAC_WRITER_PIPELINE_REVISION,
        )),
        OutputFormat::Mp3 => mp3_writer_evidence(),
        OutputFormat::Opus => opus_writer_evidence(),
        OutputFormat::M4a => m4a_writer_evidence(),
        OutputFormat::Alac => alac_writer_evidence(),
        OutputFormat::Vorbis => vorbis_writer_evidence(),
    }
}

fn native_writer_evidence(
    format: &'static str,
    implementation_id: &'static str,
    pipeline_revision: &'static str,
) -> Value {
    json!({
        "format": format,
        "implementation_id": implementation_id,
        "pipeline_revision": pipeline_revision,
        "capability_success": true,
        "runtime": {
            "kind": "native",
            "capability_success": true,
        },
    })
}

#[cfg(feature = "mp3-encoding")]
fn mp3_writer_evidence() -> Result<Value, String> {
    let lame_version = crate::mp3enc::lame_runtime_version()?;
    Ok(json!({
        "format": "mp3",
        "implementation_id": MP3_WRITER_IMPLEMENTATION_ID,
        "pipeline_revision": crate::mp3enc::MP3_WRITER_PIPELINE_REVISION,
        "capability_success": true,
        "runtime": {
            "kind": "lame",
            "lame_get_version": lame_version.clone(),
            "lame_version": lame_version,
            "capability_success": true,
        },
    }))
}

#[cfg(not(feature = "mp3-encoding"))]
fn mp3_writer_evidence() -> Result<Value, String> {
    Err(
        "MP3 semantic runtime evidence is unavailable: build with the `mp3-encoding` feature"
            .into(),
    )
}

#[cfg(feature = "opus-encoding")]
fn opus_writer_evidence() -> Result<Value, String> {
    let opus_version = crate::opus::opus_runtime_version()?;
    Ok(json!({
        "format": "opus",
        "implementation_id": OPUS_WRITER_IMPLEMENTATION_ID,
        "pipeline_revision": crate::opus::OPUS_WRITER_PIPELINE_REVISION,
        "capability_success": true,
        "runtime": {
            "kind": "libopus",
            "opus_version": opus_version,
            "container": "ogg",
            "capability_success": true,
        },
    }))
}

#[cfg(not(feature = "opus-encoding"))]
fn opus_writer_evidence() -> Result<Value, String> {
    Err(
        "Opus semantic runtime evidence is unavailable: build with the `opus-encoding` feature"
            .into(),
    )
}

#[cfg(feature = "ffmpeg-encoding")]
fn ffmpeg_writer_evidence(
    format: &'static str,
    codec: crate::aac::FfmpegCodec,
) -> Result<Value, String> {
    let evidence = crate::aac::ffmpeg_runtime_evidence(codec)?;
    // The preflight still returns (and retains) the pinned executable identity
    // used by the writer.  Its path is process-local routing information,
    // though, so keep it out of the serialized semantic evidence.  In
    // particular, do not stringify the path here: a valid non-UTF-8 executable
    // path must not make semantic-context construction fail.
    Ok(ffmpeg_semantic_writer_evidence(format, &evidence))
}

#[cfg(feature = "ffmpeg-encoding")]
fn ffmpeg_semantic_writer_evidence(
    format: &'static str,
    evidence: &crate::aac::FfmpegRuntimeEvidence,
) -> Value {
    json!({
        "format": format,
        "implementation_id": FFMPEG_WRITER_IMPLEMENTATION_ID,
        "pipeline_revision": FFMPEG_WRITER_PIPELINE_REVISION,
        "capability_success": evidence.capability_success,
        "runtime": {
            "kind": "ffmpeg",
            "executable_byte_len": evidence.executable_byte_len,
            "executable_sha256": evidence.executable_sha256.clone(),
            "byte_len": evidence.executable_byte_len,
            "sha256": evidence.executable_sha256.clone(),
            "encoder": evidence.encoder,
            "muxer": evidence.muxer,
            "capability_success": evidence.capability_success,
        },
    })
}

#[cfg(not(feature = "ffmpeg-encoding"))]
fn unavailable_ffmpeg_writer_evidence(format: &'static str) -> Result<Value, String> {
    Err(format!(
        "{format} semantic runtime evidence is unavailable: build with the `ffmpeg-encoding` feature"
    ))
}

#[cfg(feature = "ffmpeg-encoding")]
fn m4a_writer_evidence() -> Result<Value, String> {
    ffmpeg_writer_evidence("m4a", crate::aac::FfmpegCodec::Aac)
}

#[cfg(not(feature = "ffmpeg-encoding"))]
fn m4a_writer_evidence() -> Result<Value, String> {
    unavailable_ffmpeg_writer_evidence("m4a")
}

#[cfg(feature = "ffmpeg-encoding")]
fn alac_writer_evidence() -> Result<Value, String> {
    ffmpeg_writer_evidence("alac", crate::aac::FfmpegCodec::Alac)
}

#[cfg(not(feature = "ffmpeg-encoding"))]
fn alac_writer_evidence() -> Result<Value, String> {
    unavailable_ffmpeg_writer_evidence("alac")
}

#[cfg(feature = "ffmpeg-encoding")]
fn vorbis_writer_evidence() -> Result<Value, String> {
    ffmpeg_writer_evidence("vorbis", crate::aac::FfmpegCodec::Vorbis)
}

#[cfg(not(feature = "ffmpeg-encoding"))]
fn vorbis_writer_evidence() -> Result<Value, String> {
    unavailable_ffmpeg_writer_evidence("vorbis")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::{InputDescriptor, InputDescriptorOptions};
    use crate::stable_input::{StableInput, StableInputOptions};
    use crate::wav::{PcmKind, WavStreamWriter};
    use std::path::Path;

    fn descriptor() -> (tempfile::TempDir, InputDescriptor) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input.wav");
        let mut writer = WavStreamWriter::create(&path, 48_000, 1, 1, PcmKind::S16, false).unwrap();
        writer.write_chunk(&[vec![0.0]]).unwrap();
        writer.finish().unwrap();
        let options = StableInputOptions::new(1024 * 1024).unwrap();
        let stable = StableInput::from_path(&path, &options).unwrap();
        let descriptor = InputDescriptor::probe(stable, InputDescriptorOptions::default()).unwrap();
        (directory, descriptor)
    }

    #[test]
    fn native_context_is_deterministic_and_excludes_volatile_values() {
        let (_directory, _descriptor) = descriptor();
        let first = normalization_semantic_context(
            AnalysisEngine::Fast,
            None,
            &[OutputFormat::Wav, OutputFormat::Flac],
        )
        .unwrap();
        let second = normalization_semantic_context(
            AnalysisEngine::Fast,
            None,
            &[OutputFormat::Wav, OutputFormat::Flac],
        )
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(first["bound_analysis_version"], BOUND_ANALYSIS_VERSION);
        assert_eq!(
            first["measurement_algorithm_revision"],
            MEASUREMENT_ALGORITHM_REVISION
        );
        assert_eq!(
            first["input_content_binding_version"],
            INPUT_CONTENT_BINDING_VERSION
        );
        let encoded = serde_json::to_string(&first).unwrap();
        for volatile in ["cache", "timestamp", "pid", "process_id", "hit"] {
            assert!(
                !encoded.contains(volatile),
                "volatile key leaked: {volatile}"
            );
        }
    }

    #[test]
    fn ordered_output_formats_keep_duplicate_entries() {
        let (_directory, _descriptor) = descriptor();
        let default_context =
            normalization_semantic_context(AnalysisEngine::Reference, None, &[]).unwrap();
        let selected_context =
            normalization_semantic_context(AnalysisEngine::Reference, Some(2), &[]).unwrap();
        assert_ne!(
            default_context["audio_track"],
            selected_context["audio_track"]
        );

        let context = normalization_semantic_context(
            AnalysisEngine::Reference,
            Some(2),
            &[OutputFormat::Flac, OutputFormat::Wav, OutputFormat::Flac],
        )
        .unwrap();
        let formats = context["output_formats"].as_array().unwrap();
        assert_eq!(formats.len(), 3);
        assert_eq!(formats[0], "flac");
        assert_eq!(formats[1], "wav");
        assert_eq!(formats[2], "flac");
        let writers = context["writers"].as_array().unwrap();
        assert_eq!(writers.len(), 2);
        assert_eq!(writers[0]["format"], "flac");
        assert_eq!(writers[1]["format"], "wav");
        assert_eq!(
            context["analysis_engine_id"],
            AnalysisEngine::Reference.id()
        );
        assert_eq!(context["audio_track"], 2);
        assert_eq!(context["audio_track_selection"]["kind"], "index");
    }

    #[test]
    fn descriptor_track_identity_is_bound() {
        let (_directory, descriptor) = descriptor();
        let context = normalization_semantic_context(AnalysisEngine::Fast, None, &[]).unwrap();
        assert_eq!(context["audio_track"], Value::Null);
        assert_eq!(context["audio_track_selection"]["kind"], "default");
        let descriptor_context = input_descriptor_semantic_evidence(&descriptor).unwrap();
        assert_eq!(
            descriptor_context["audio_track_index"],
            descriptor.track_index()
        );
        assert_eq!(descriptor_context["audio_track_id"], descriptor.track_id());
        assert_eq!(
            descriptor_context["input_descriptor_version"],
            descriptor.version()
        );
        assert_eq!(
            descriptor_context["decoder_route"],
            descriptor.decoder_route_id()
        );
    }

    #[test]
    fn unsupported_optional_writer_reports_feature_requirement() {
        let (_directory, _descriptor) = descriptor();
        #[cfg(not(feature = "mp3-encoding"))]
        assert!(
            normalization_semantic_context(AnalysisEngine::Fast, None, &[OutputFormat::Mp3])
                .unwrap_err()
                .contains("mp3-encoding")
        );
        #[cfg(not(feature = "opus-encoding"))]
        assert!(
            normalization_semantic_context(AnalysisEngine::Fast, None, &[OutputFormat::Opus])
                .unwrap_err()
                .contains("opus-encoding")
        );
        #[cfg(not(feature = "ffmpeg-encoding"))]
        assert!(
            normalization_semantic_context(AnalysisEngine::Fast, None, &[OutputFormat::M4a])
                .unwrap_err()
                .contains("ffmpeg-encoding")
        );
    }

    #[test]
    fn descriptor_fixture_has_no_path_or_source_binding() {
        let (_directory, _descriptor) = descriptor();
        let context = normalization_semantic_context(AnalysisEngine::Fast, None, &[]).unwrap();
        let encoded = serde_json::to_string(&context).unwrap();
        assert!(!encoded.contains("input.wav"));
        assert!(!encoded.contains("source_path"));
        assert!(!encoded.contains("sha256"));
        assert!(!Path::new("input.wav").exists());
    }

    #[cfg(feature = "mp3-encoding")]
    #[test]
    fn mp3_evidence_contains_the_linked_lame_runtime_version() {
        let (_directory, _descriptor) = descriptor();
        let context =
            normalization_semantic_context(AnalysisEngine::Fast, None, &[OutputFormat::Mp3])
                .unwrap();
        let version = crate::mp3enc::lame_runtime_version().unwrap();
        assert_eq!(context["writers"][0]["runtime"]["lame_version"], version);
        assert_eq!(
            context["writers"][0]["pipeline_revision"],
            crate::mp3enc::MP3_WRITER_PIPELINE_REVISION
        );
    }

    #[cfg(feature = "opus-encoding")]
    #[test]
    fn opus_evidence_contains_the_linked_libopus_runtime_version() {
        let (_directory, _descriptor) = descriptor();
        let context =
            normalization_semantic_context(AnalysisEngine::Fast, None, &[OutputFormat::Opus])
                .unwrap();
        let version = crate::opus::opus_runtime_version().unwrap();
        assert_eq!(context["writers"][0]["runtime"]["opus_version"], version);
        assert_eq!(
            context["writers"][0]["pipeline_revision"],
            crate::opus::OPUS_WRITER_PIPELINE_REVISION
        );
    }

    #[cfg(feature = "ffmpeg-encoding")]
    #[test]
    fn ffmpeg_evidence_reuses_the_cached_preflight_identity() {
        let (_directory, _descriptor) = descriptor();
        let first =
            normalization_semantic_context(AnalysisEngine::Fast, None, &[OutputFormat::M4a])
                .unwrap();
        let second =
            normalization_semantic_context(AnalysisEngine::Fast, None, &[OutputFormat::M4a])
                .unwrap();
        let first_runtime = &first["writers"][0]["runtime"];
        let second_runtime = &second["writers"][0]["runtime"];
        assert_eq!(first_runtime, second_runtime);
        assert_eq!(first_runtime["encoder"], "aac");
        assert_eq!(first_runtime["muxer"], "ipod");
        assert!(first_runtime["capability_success"].as_bool().unwrap());
        assert!(first_runtime.get("canonical_executable_path").is_none());
        assert!(first_runtime.get("executable_path").is_none());
        assert!(!serde_json::to_string(first_runtime)
            .unwrap()
            .contains("executable_path"));
    }

    #[cfg(all(feature = "ffmpeg-encoding", unix))]
    #[test]
    fn ffmpeg_semantic_evidence_ignores_path_and_accepts_non_utf8_path() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::path::PathBuf;

        let first = crate::aac::FfmpegRuntimeEvidence {
            executable_path: PathBuf::from("/opt/forge/bin/ffmpeg"),
            executable_byte_len: 123,
            executable_sha256: "a".repeat(64),
            encoder: "aac",
            muxer: "ipod",
            capability_success: true,
        };
        let second = crate::aac::FfmpegRuntimeEvidence {
            executable_path: PathBuf::from(OsString::from_vec(b"/private/\xff/ffmpeg".to_vec())),
            ..first.clone()
        };

        let first_value = ffmpeg_semantic_writer_evidence("m4a", &first);
        let second_value = ffmpeg_semantic_writer_evidence("m4a", &second);
        assert_eq!(first_value, second_value);
        let encoded = serde_json::to_string(&second_value).unwrap();
        assert!(!encoded.contains("canonical_executable_path"));
        assert!(!encoded.contains("executable_path"));
    }

    #[test]
    fn repeated_formats_keep_context_within_the_batch_limit() {
        // Keep this at the production batch ceiling so the context's own
        // 4 MiB validation bound is exercised at the supported maximum.
        let formats = vec![OutputFormat::Wav; MAX_NORMALIZATION_SEMANTIC_OUTPUTS];
        let context = normalization_semantic_context(AnalysisEngine::Fast, None, &formats).unwrap();
        let encoded = serde_json::to_vec(&context).unwrap();
        assert_eq!(
            context["output_formats"].as_array().unwrap().len(),
            formats.len()
        );
        assert_eq!(context["writers"].as_array().unwrap().len(), 1);
        assert!(encoded.len() <= crate::batch::MAX_SEMANTIC_CONTEXT_BYTES);
    }

    #[test]
    fn rejects_more_than_the_supported_output_count_before_allocation() {
        let formats = vec![OutputFormat::Wav; MAX_NORMALIZATION_SEMANTIC_OUTPUTS + 1];
        let error =
            normalization_semantic_context(AnalysisEngine::Fast, None, &formats).unwrap_err();
        assert!(error.contains("100000-output limit"), "{error}");
    }
}
