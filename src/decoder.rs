//! Universal audio decoder.
//!
//! WAV files use Forge's own hand-written, parallelized demuxer/decoder (the
//! fast path). Every other container/codec Forge supports — MP3, FLAC, AAC/ALAC
//! in MP4/M4A, Vorbis in OGG — is decoded by `symphonia`, a pure-Rust audio
//! decoding framework. This keeps the binary dependency-free at the system level
//! (no libsndfile, no ffmpeg) while still reading the formats users actually
//! have. All paths produce the same planar-f32 [`AudioBuffer`] the DSP engine
//! consumes.

use crate::wav::{default_channel_roles, AudioBuffer, ChannelRole, PcmKind, WavReader};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub channel_roles: Vec<ChannelRole>,
    pub source_kind: PcmKind,
}

/// Decode any supported audio file into a planar-f32 [`AudioBuffer`].
pub fn decode(path: &Path) -> Result<AudioBuffer, String> {
    decode_limited(path, u64::MAX)
}

/// Decode supported audio while bounding frames multiplied by channels.
///
/// WAVE inputs are rejected from their headers before the fast path allocates
/// its planar buffer. Compressed inputs are checked after every decoded packet.
pub fn decode_limited(path: &Path, max_decoded_samples: u64) -> Result<AudioBuffer, String> {
    if max_decoded_samples == 0 {
        return Err("decoded sample limit must be greater than zero".into());
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();

    // Fast path: Forge's own WAV demuxer (parallel, SIMD-friendly).
    if matches!(ext.as_str(), "wav" | "wave" | "bwf" | "bw64" | "rf64") {
        let info = WavReader::probe(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let bytes_per_frame = (info.kind.bytes_per_sample() as u64)
            .checked_mul(u64::from(info.channels))
            .ok_or_else(|| format!("{}: WAVE frame size overflow", path.display()))?;
        if bytes_per_frame == 0 {
            return Err(format!("{}: WAVE frame size is zero", path.display()));
        }
        enforce_decoded_sample_limit(
            path,
            info.data_size / bytes_per_frame,
            u64::from(info.channels),
            max_decoded_samples,
        )?;
        return WavReader::open(path).map_err(|e| format!("{}: {e}", path.display()));
    }
    if matches!(ext.as_str(), "dsf" | "dff") {
        let dsd = crate::dsd::probe(path)?;
        enforce_decoded_sample_limit(
            path,
            dsd.output_frames,
            u64::from(dsd.channels),
            max_decoded_samples,
        )?;
        let mut data = vec![Vec::new(); dsd.channels as usize];
        let info = crate::dsd::decode_stream(path, |stream_info, planar| {
            if planar.len() != data.len() {
                return Err("DSD decoded channel count changed".into());
            }
            for (destination, source) in data.iter_mut().zip(planar) {
                destination.append(source);
            }
            if stream_info.channels != dsd.channels {
                return Err("DSD stream metadata changed".into());
            }
            Ok(())
        })?;
        let frames = data.first().map_or(0, Vec::len);
        if frames as u64 != dsd.output_frames {
            return Err(format!(
                "{}: decoded DSD frame count {frames} does not match {}",
                path.display(),
                dsd.output_frames
            ));
        }
        return Ok(AudioBuffer {
            sample_rate: info.sample_rate,
            channels: info.channels,
            frames,
            data,
            channel_roles: info.channel_roles,
            source_kind: info.source_kind,
        });
    }
    if ext == "opus" {
        #[cfg(feature = "opus-encoding")]
        {
            let mut data: Vec<Vec<f32>> = Vec::new();
            let info = crate::opus::decode_stream(path, |info, planar| {
                let existing_frames = data.first().map_or(0, Vec::len) as u64;
                let packet_frames = planar.first().map_or(0, |samples| samples.len()) as u64;
                enforce_decoded_sample_limit(
                    path,
                    existing_frames.saturating_add(packet_frames),
                    u64::from(info.channels),
                    max_decoded_samples,
                )?;
                if data.is_empty() {
                    data = vec![Vec::new(); info.channels as usize];
                }
                for (destination, source) in data.iter_mut().zip(planar) {
                    destination.extend_from_slice(source);
                }
                Ok(())
            })?;
            let frames = data.first().map_or(0, Vec::len);
            return Ok(AudioBuffer {
                sample_rate: info.sample_rate,
                channels: info.channels,
                frames,
                data,
                channel_roles: info.channel_roles,
                source_kind: info.source_kind,
            });
        }
        #[cfg(not(feature = "opus-encoding"))]
        {
            return Err(
                "Ogg Opus support is unavailable; rebuild with `--features opus-encoding`".into(),
            );
        }
    }

    // Everything else via symphonia.
    decode_symphonia(path, &ext, max_decoded_samples)
}

fn decode_symphonia(
    path: &Path,
    ext: &str,
    max_decoded_samples: u64,
) -> Result<AudioBuffer, String> {
    use symphonia::core::errors::Error;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
    use symphonia::core::meta::MetadataOptions;
    use symphonia::default::{get_codecs, get_probe};

    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());

    let mut hint = Hint::new();
    if !ext.is_empty() {
        hint.with_extension(ext);
    }

    let mut format = get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("{}: probe failed: {e}", path.display()))?;

    let mut track = select_symphonia_audio_track(path, format.as_ref())?;
    require_symphonia_sample_rate(path, &track.codec_params)?;
    let decoder_options = symphonia_decoder_options();
    let mut decoder = get_codecs()
        .make_audio_decoder(&track.codec_params, &decoder_options)
        .map_err(|e| format!("{}: unsupported codec: {e}", path.display()))?;

    let mut planar: Vec<Vec<f32>> = Vec::new();
    let mut output_format: Option<SymphoniaOutputFormat> = None;
    let mut packet_planar = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(Error::ResetRequired) => {
                // Chained Ogg replaces the complete track list and uses a new
                // serial as its track ID. Re-select before accepting packets
                // from the new physical stream.
                let next_track = select_symphonia_audio_track(path, format.as_ref())?;
                require_symphonia_sample_rate(path, &next_track.codec_params)?;
                if let Some(output) = output_format.as_ref() {
                    validate_symphonia_track_compatibility(
                        path,
                        output,
                        &next_track.codec_params,
                        PcmKind::F32,
                    )?;
                }
                let next_decoder = get_codecs()
                    .make_audio_decoder(&next_track.codec_params, &decoder_options)
                    .map_err(|e| format!("{}: reinit decoder: {e}", path.display()))?;
                track = next_track;
                decoder = next_decoder;
                continue;
            }
            Err(e) => return Err(format!("{}: read packet: {e}", path.display())),
        };
        if packet.track_id != track.id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // Skip a single corrupt frame rather than aborting the whole file.
            Err(Error::DecodeError(_)) => continue,
            Err(e) => return Err(format!("{}: decode: {e}", path.display())),
        };

        let spec = decoded.spec();
        let ch = spec.channels().count();
        if ch == 0 {
            continue;
        }
        if let Some(output) = output_format.as_ref() {
            validate_symphonia_decoded_compatibility(path, output, spec, PcmKind::F32)?;
        } else {
            let output =
                establish_symphonia_output_format(path, &track.codec_params, spec, PcmKind::F32)?;
            planar = (0..ch).map(|_| Vec::new()).collect();
            output_format = Some(output);
        }
        let frames = decoded.frames();
        if frames == 0 {
            continue;
        }
        let total_frames = planar
            .first()
            .map_or(0, Vec::len)
            .checked_add(frames)
            .ok_or_else(|| format!("{}: decoded frame count overflow", path.display()))?;
        enforce_decoded_sample_limit(
            path,
            total_frames as u64,
            u64::from(output_format.as_ref().unwrap().channels),
            max_decoded_samples,
        )?;
        decoded.copy_to_vecs_planar::<f32>(&mut packet_planar);
        for (destination, source) in planar.iter_mut().zip(&packet_planar) {
            destination.extend_from_slice(source);
        }
    }

    let output_format = output_format
        .filter(|_| planar.first().is_some_and(|channel| !channel.is_empty()))
        .ok_or_else(|| format!("{}: no audio decoded", path.display()))?;
    if planar.len() != usize::from(output_format.channels) {
        return Err(format!("{}: no audio decoded", path.display()));
    }

    let frames = planar[0].len();
    Ok(AudioBuffer {
        sample_rate: output_format.sample_rate,
        channels: output_format.channels,
        frames,
        data: planar,
        channel_roles: output_format.channel_roles,
        source_kind: output_format.source_kind,
    })
}

struct SymphoniaAudioTrack {
    id: u32,
    codec_params: symphonia::core::codecs::audio::AudioCodecParameters,
}

struct SymphoniaOutputFormat {
    sample_rate: u32,
    channels: u16,
    decoded_layout: symphonia::core::audio::Channels,
    declared_layout: Option<symphonia::core::audio::Channels>,
    channel_roles: Vec<ChannelRole>,
    source_kind: PcmKind,
}

fn select_symphonia_audio_track(
    path: &Path,
    format: &dyn symphonia::core::formats::FormatReader,
) -> Result<SymphoniaAudioTrack, String> {
    use symphonia::core::formats::TrackType;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| format!("{}: no audio track", path.display()))?;
    let codec_params = track
        .codec_params
        .as_ref()
        .and_then(|params| params.audio())
        .ok_or_else(|| format!("{}: audio codec parameters are missing", path.display()))?
        .clone();
    Ok(SymphoniaAudioTrack {
        id: track.id,
        codec_params,
    })
}

fn symphonia_decoder_options() -> symphonia::core::codecs::audio::AudioDecoderOptions {
    // Normalization and measurement operate on the audible programme. Trim
    // codec encoder delay and end padding so frame counts remain sample-accurate.
    symphonia::core::codecs::audio::AudioDecoderOptions::default().gapless(true)
}

fn require_symphonia_sample_rate(
    path: &Path,
    codec_params: &symphonia::core::codecs::audio::AudioCodecParameters,
) -> Result<u32, String> {
    codec_params
        .sample_rate
        .ok_or_else(|| format!("{}: unknown sample rate", path.display()))
}

fn establish_symphonia_output_format(
    path: &Path,
    codec_params: &symphonia::core::codecs::audio::AudioCodecParameters,
    decoded: &symphonia::core::audio::AudioSpec,
    source_kind: PcmKind,
) -> Result<SymphoniaOutputFormat, String> {
    let sample_rate = require_symphonia_sample_rate(path, codec_params)?;
    if decoded.rate() != sample_rate {
        return Err(format!(
            "{}: decoded sample rate {} does not match track sample rate {sample_rate}",
            path.display(),
            decoded.rate()
        ));
    }
    let channel_count = decoded.channels().count();
    let channels = u16::try_from(channel_count).map_err(|_| {
        format!(
            "{}: too many decoded channels: {channel_count}",
            path.display()
        )
    })?;
    if let Some(declared) = codec_params.channels.as_ref() {
        if declared.count() != channel_count {
            return Err(format!(
                "{}: decoded channel count {channel_count} does not match track channel count {}",
                path.display(),
                declared.count()
            ));
        }
    }
    let role_layout = codec_params.channels.as_ref().unwrap_or(decoded.channels());
    let mut channel_roles = roles_from_symphonia(role_layout);
    if channel_roles.len() != channel_count {
        channel_roles = default_channel_roles(channels);
    }
    Ok(SymphoniaOutputFormat {
        sample_rate,
        channels,
        decoded_layout: decoded.channels().clone(),
        declared_layout: codec_params.channels.clone(),
        channel_roles,
        source_kind,
    })
}

fn validate_symphonia_track_compatibility(
    path: &Path,
    output: &SymphoniaOutputFormat,
    codec_params: &symphonia::core::codecs::audio::AudioCodecParameters,
    source_kind: PcmKind,
) -> Result<(), String> {
    let sample_rate = require_symphonia_sample_rate(path, codec_params)?;
    if sample_rate != output.sample_rate {
        return Err(format!(
            "{}: chained stream sample rate changed from {} to {sample_rate}",
            path.display(),
            output.sample_rate
        ));
    }
    if source_kind != output.source_kind {
        return Err(format!(
            "{}: chained stream source sample kind changed from {:?} to {:?}",
            path.display(),
            output.source_kind,
            source_kind
        ));
    }
    if let Some(layout) = codec_params.channels.as_ref() {
        if layout.count() != usize::from(output.channels) {
            return Err(format!(
                "{}: chained stream channel count changed from {} to {}",
                path.display(),
                output.channels,
                layout.count()
            ));
        }
        let expected_layout = output
            .declared_layout
            .as_ref()
            .unwrap_or(&output.decoded_layout);
        if layout != expected_layout {
            return Err(format!(
                "{}: chained stream channel layout changed from {expected_layout} to {layout}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn validate_symphonia_decoded_compatibility(
    path: &Path,
    output: &SymphoniaOutputFormat,
    decoded: &symphonia::core::audio::AudioSpec,
    source_kind: PcmKind,
) -> Result<(), String> {
    if decoded.rate() != output.sample_rate {
        return Err(format!(
            "{}: decoded sample rate changed from {} to {}",
            path.display(),
            output.sample_rate,
            decoded.rate()
        ));
    }
    if decoded.channels().count() != usize::from(output.channels) {
        return Err(format!(
            "{}: decoded channel count changed from {} to {}",
            path.display(),
            output.channels,
            decoded.channels().count()
        ));
    }
    if decoded.channels() != &output.decoded_layout {
        return Err(format!(
            "{}: decoded channel layout changed from {} to {}",
            path.display(),
            output.decoded_layout,
            decoded.channels()
        ));
    }
    if source_kind != output.source_kind {
        return Err(format!(
            "{}: decoded source sample kind changed from {:?} to {:?}",
            path.display(),
            output.source_kind,
            source_kind
        ));
    }
    Ok(())
}

fn enforce_decoded_sample_limit(
    path: &Path,
    frames: u64,
    channels: u64,
    max_decoded_samples: u64,
) -> Result<(), String> {
    let samples = frames
        .checked_mul(channels)
        .ok_or_else(|| format!("{}: decoded sample count overflow", path.display()))?;
    if samples > max_decoded_samples {
        return Err(format!(
            "{}: decoded sample count {samples} exceeds safety limit {max_decoded_samples}",
            path.display()
        ));
    }
    Ok(())
}

fn roles_from_symphonia(channels: &symphonia::core::audio::Channels) -> Vec<ChannelRole> {
    use symphonia::core::audio::{ChannelLabel, Channels};

    let advanced = channels.count() > 6;
    match channels {
        Channels::Positioned(positions) => positions
            .iter()
            .map(|position| role_from_symphonia_position(position, advanced))
            .collect(),
        Channels::Discrete(count) => default_channel_roles(*count),
        Channels::Ambisonic(order) => {
            let count = (1 + usize::from(*order)) * (1 + usize::from(*order));
            vec![ChannelRole::Main; count]
        }
        Channels::Custom(labels) => labels
            .iter()
            .map(|label| match label {
                ChannelLabel::Positioned(position) => {
                    role_from_symphonia_position(*position, advanced)
                }
                ChannelLabel::Discrete(_)
                | ChannelLabel::Ambisonic(_)
                | ChannelLabel::AmbisonicBFormat(_) => ChannelRole::Main,
                _ => ChannelRole::Main,
            })
            .collect(),
        Channels::None => Vec::new(),
        _ => vec![ChannelRole::Main; channels.count()],
    }
}

fn role_from_symphonia_position(
    position: symphonia::core::audio::Position,
    advanced: bool,
) -> ChannelRole {
    use symphonia::core::audio::Position;

    if position.intersects(Position::LFE1 | Position::LFE2) {
        ChannelRole::Lfe
    } else if !advanced
        && position.intersects(
            Position::REAR_LEFT
                | Position::REAR_RIGHT
                | Position::REAR_CENTER
                | Position::SIDE_LEFT
                | Position::SIDE_RIGHT,
        )
    {
        ChannelRole::Surround
    } else if position.intersects(Position::SIDE_LEFT) {
        ChannelRole::positioned(-90, 0)
    } else if position.intersects(Position::SIDE_RIGHT) {
        ChannelRole::positioned(90, 0)
    } else if position.intersects(Position::REAR_LEFT) {
        ChannelRole::positioned(-135, 0)
    } else if position.intersects(Position::REAR_RIGHT) {
        ChannelRole::positioned(135, 0)
    } else if position.intersects(Position::REAR_CENTER) {
        ChannelRole::positioned(180, 0)
    } else if position.intersects(Position::TOP_REAR_LEFT) {
        ChannelRole::positioned(-135, 45)
    } else if position.intersects(Position::TOP_REAR_RIGHT) {
        ChannelRole::positioned(135, 45)
    } else if position.intersects(Position::TOP_REAR_CENTER) {
        ChannelRole::positioned(180, 45)
    } else {
        ChannelRole::Main
    }
}

/// Decode an audio file in bounded chunks without retaining the complete
/// sample stream.
pub fn decode_stream<F>(path: &Path, mut consume: F) -> Result<StreamInfo, String>
where
    F: FnMut(&StreamInfo, &mut [Vec<f32>]) -> Result<(), String>,
{
    use symphonia::core::errors::Error;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
    use symphonia::core::meta::MetadataOptions;
    use symphonia::default::{get_codecs, get_probe};

    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if matches!(extension.as_str(), "wav" | "wave" | "bwf" | "bw64" | "rf64") {
        return decode_wav_stream(path, consume);
    }
    if matches!(extension.as_str(), "dsf" | "dff") {
        return crate::dsd::decode_stream(path, consume);
    }
    if extension == "opus" {
        #[cfg(feature = "opus-encoding")]
        {
            return crate::opus::decode_stream(path, consume);
        }
        #[cfg(not(feature = "opus-encoding"))]
        {
            return Err(
                "Ogg Opus support is unavailable; rebuild with `--features opus-encoding`".into(),
            );
        }
    }
    let file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let stream = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if !extension.is_empty() {
        hint.with_extension(&extension);
    }
    let mut format = get_probe()
        .probe(
            &hint,
            stream,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|error| format!("{}: probe failed: {error}", path.display()))?;
    let mut track = select_symphonia_audio_track(path, format.as_ref())?;
    require_symphonia_sample_rate(path, &track.codec_params)?;
    let decoder_options = symphonia_decoder_options();
    let mut decoder = get_codecs()
        .make_audio_decoder(&track.codec_params, &decoder_options)
        .map_err(|error| format!("{}: unsupported codec: {error}", path.display()))?;
    let mut output_format: Option<SymphoniaOutputFormat> = None;
    let mut info: Option<StreamInfo> = None;
    let mut planar = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(Error::ResetRequired) => {
                let next_track = select_symphonia_audio_track(path, format.as_ref())?;
                require_symphonia_sample_rate(path, &next_track.codec_params)?;
                let next_source_kind =
                    source_kind(&extension, next_track.codec_params.bits_per_sample);
                if let Some(output) = output_format.as_ref() {
                    validate_symphonia_track_compatibility(
                        path,
                        output,
                        &next_track.codec_params,
                        next_source_kind,
                    )?;
                }
                let next_decoder = get_codecs()
                    .make_audio_decoder(&next_track.codec_params, &decoder_options)
                    .map_err(|error| format!("{}: reinit decoder: {error}", path.display()))?;
                track = next_track;
                decoder = next_decoder;
                continue;
            }
            Err(error) => return Err(format!("{}: read packet: {error}", path.display())),
        };
        if packet.track_id != track.id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(Error::DecodeError(_)) => continue,
            Err(error) => return Err(format!("{}: decode: {error}", path.display())),
        };
        let spec = decoded.spec();
        let decoded_channels = spec.channels().count();
        if decoded_channels == 0 {
            continue;
        }
        let current_source_kind = source_kind(&extension, track.codec_params.bits_per_sample);
        if let Some(output) = output_format.as_ref() {
            validate_symphonia_decoded_compatibility(path, output, spec, current_source_kind)?;
        } else {
            let output = establish_symphonia_output_format(
                path,
                &track.codec_params,
                spec,
                current_source_kind,
            )?;
            info = Some(StreamInfo {
                sample_rate: output.sample_rate,
                channels: output.channels,
                channel_roles: output.channel_roles.clone(),
                source_kind: output.source_kind,
            });
            output_format = Some(output);
        }
        let frames = decoded.frames();
        if frames == 0 {
            continue;
        }
        decoded.copy_to_vecs_planar::<f32>(&mut planar);
        consume(info.as_ref().unwrap(), &mut planar)?;
    }

    info.ok_or_else(|| format!("{}: no audio decoded", path.display()))
}

fn decode_wav_stream<F>(path: &Path, mut consume: F) -> Result<StreamInfo, String>
where
    F: FnMut(&StreamInfo, &mut [Vec<f32>]) -> Result<(), String>,
{
    let wav = WavReader::probe(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let info = StreamInfo {
        sample_rate: wav.sample_rate,
        channels: wav.channels,
        channel_roles: wav.channel_roles,
        source_kind: wav.kind,
    };
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    file.seek(SeekFrom::Start(wav.data_offset))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let data_size = usize::try_from(wav.data_size).map_err(|_| {
        format!(
            "{}: audio data is too large for this platform",
            path.display()
        )
    })?;

    let frame_bytes = info.channels as usize * info.source_kind.bytes_per_sample();
    let chunk_bytes = (64 * 1024 / frame_bytes).max(1) * frame_bytes;
    let mut remaining = data_size;
    let mut bytes = vec![0; chunk_bytes];
    while remaining >= frame_bytes {
        let read_size = remaining.min(chunk_bytes);
        let aligned = read_size - read_size % frame_bytes;
        file.read_exact(&mut bytes[..aligned])
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let mut planar = crate::dsp::convert::decode_planar(
            &bytes[..aligned],
            info.source_kind,
            info.channels as usize,
        );
        consume(&info, &mut planar)?;
        remaining -= aligned;
    }
    Ok(info)
}

fn source_kind(extension: &str, bits: Option<u32>) -> PcmKind {
    if matches!(extension, "wav" | "wave" | "bwf" | "bw64" | "rf64") {
        match bits {
            Some(8) => PcmKind::U8,
            Some(16) => PcmKind::S16,
            Some(24) => PcmKind::S24,
            Some(32) => PcmKind::S32,
            Some(64) => PcmKind::F64,
            _ => PcmKind::F32,
        }
    } else {
        PcmKind::F32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia::core::audio::layouts::{CHANNEL_LAYOUT_MONO, CHANNEL_LAYOUT_STEREO};
    use symphonia::core::codecs::audio::AudioCodecParameters;

    fn output_format() -> SymphoniaOutputFormat {
        SymphoniaOutputFormat {
            sample_rate: 48_000,
            channels: 2,
            decoded_layout: CHANNEL_LAYOUT_STEREO.clone(),
            declared_layout: Some(CHANNEL_LAYOUT_STEREO.clone()),
            channel_roles: default_channel_roles(2),
            source_kind: PcmKind::F32,
        }
    }

    fn codec_params(
        sample_rate: u32,
        layout: symphonia::core::audio::Channels,
    ) -> AudioCodecParameters {
        let mut params = AudioCodecParameters::new();
        params.with_sample_rate(sample_rate).with_channels(layout);
        params
    }

    #[test]
    fn reset_compatibility_rejects_rate_and_layout_changes() {
        let path = Path::new("fixture.ogg");
        let output = output_format();

        let rate_error = validate_symphonia_track_compatibility(
            path,
            &output,
            &codec_params(44_100, CHANNEL_LAYOUT_STEREO.clone()),
            PcmKind::F32,
        )
        .unwrap_err();
        assert!(rate_error.contains("sample rate changed from 48000 to 44100"));

        let layout_error = validate_symphonia_track_compatibility(
            path,
            &output,
            &codec_params(48_000, CHANNEL_LAYOUT_MONO.clone()),
            PcmKind::F32,
        )
        .unwrap_err();
        assert!(layout_error.contains("channel count changed from 2 to 1"));
    }

    #[cfg(feature = "ffmpeg-encoding")]
    fn write_exact_tone(path: &Path, sample_rate: u32, frames: usize) {
        let samples: Vec<f32> = (0..frames)
            .map(|frame| {
                0.1 * (std::f64::consts::TAU * 997.0 * frame as f64 / sample_rate as f64).sin()
                    as f32
            })
            .collect();
        let buffer = AudioBuffer {
            sample_rate,
            channels: 2,
            frames,
            data: vec![samples.clone(), samples],
            channel_roles: default_channel_roles(2),
            source_kind: PcmKind::F32,
        };
        crate::wav::WavWriter::write(path, &buffer, PcmKind::S24, false).unwrap();
    }

    #[cfg(feature = "ffmpeg-encoding")]
    fn encode_vorbis(input: &Path, output: &Path, serial_offset: u32) {
        let status = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-y", "-i"])
            .arg(input)
            .args(["-map_metadata", "-1", "-c:a", "libvorbis", "-q:a", "4"])
            .arg("-serial_offset")
            .arg(serial_offset.to_string())
            .arg(output)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(feature = "ffmpeg-encoding")]
    fn ogg_serial(path: &Path) -> u32 {
        let bytes = std::fs::read(path).unwrap();
        assert!(bytes.len() >= 18 && &bytes[..4] == b"OggS");
        u32::from_le_bytes(bytes[14..18].try_into().unwrap())
    }

    #[cfg(feature = "ffmpeg-encoding")]
    #[test]
    fn chained_vorbis_reselects_track_and_preserves_gapless_duration() {
        let directory = tempfile::tempdir().unwrap();
        let first_wav = directory.path().join("first.wav");
        let second_wav = directory.path().join("second.wav");
        let first_ogg = directory.path().join("first.ogg");
        let second_ogg = directory.path().join("second.ogg");
        let chained = directory.path().join("chained.ogg");
        write_exact_tone(&first_wav, 48_000, 4_800);
        write_exact_tone(&second_wav, 48_000, 7_200);
        encode_vorbis(&first_wav, &first_ogg, 100);
        encode_vorbis(&second_wav, &second_ogg, 200);
        assert_ne!(ogg_serial(&first_ogg), ogg_serial(&second_ogg));

        let first = decode(&first_ogg).unwrap();
        let second = decode(&second_ogg).unwrap();
        // Explicit gapless decoding removes codec priming/padding and keeps
        // the audible programme length sample-accurate.
        assert_eq!(first.frames, 4_800);
        assert_eq!(second.frames, 7_200);
        let audit = crate::container_qc::audit(&first_ogg).unwrap();
        assert!(audit.passed, "{audit:#?}");
        assert_eq!(
            audit.properties["decoded"]["frames"].as_u64(),
            Some(first.frames as u64)
        );

        let mut bytes = std::fs::read(&first_ogg).unwrap();
        bytes.extend_from_slice(&std::fs::read(&second_ogg).unwrap());
        std::fs::write(&chained, bytes).unwrap();

        let decoded = decode(&chained).unwrap();
        assert_eq!(decoded.frames, first.frames + second.frames);
        for channel in 0..decoded.channels as usize {
            let mut expected = first.data[channel].clone();
            expected.extend_from_slice(&second.data[channel]);
            assert_eq!(decoded.data[channel], expected);
        }

        let mut streamed = vec![Vec::new(); decoded.channels as usize];
        let info = decode_stream(&chained, |_, planar| {
            for (destination, source) in streamed.iter_mut().zip(planar) {
                destination.extend_from_slice(source);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(info.sample_rate, decoded.sample_rate);
        assert_eq!(info.channels, decoded.channels);
        assert_eq!(streamed, decoded.data);
    }

    #[cfg(feature = "ffmpeg-encoding")]
    #[test]
    fn chained_vorbis_rejects_sample_rate_change() {
        let directory = tempfile::tempdir().unwrap();
        let first_wav = directory.path().join("first.wav");
        let second_wav = directory.path().join("second.wav");
        let first_ogg = directory.path().join("first.ogg");
        let second_ogg = directory.path().join("second.ogg");
        let chained = directory.path().join("rate-change.ogg");
        write_exact_tone(&first_wav, 48_000, 4_800);
        write_exact_tone(&second_wav, 44_100, 4_410);
        encode_vorbis(&first_wav, &first_ogg, 300);
        encode_vorbis(&second_wav, &second_ogg, 400);

        let mut bytes = std::fs::read(&first_ogg).unwrap();
        bytes.extend_from_slice(&std::fs::read(&second_ogg).unwrap());
        std::fs::write(&chained, bytes).unwrap();

        let error = decode(&chained).unwrap_err();
        assert!(error.contains("sample rate changed from 48000 to 44100"));
        let error = decode_stream(&chained, |_, _| Ok(())).unwrap_err();
        assert!(error.contains("sample rate changed from 48000 to 44100"));
    }
}
