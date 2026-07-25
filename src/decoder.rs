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

pub struct StreamInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub channel_roles: Vec<ChannelRole>,
    pub source_kind: PcmKind,
}

/// Decode any supported audio file into a planar-f32 [`AudioBuffer`].
pub fn decode(path: &Path) -> Result<AudioBuffer, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();

    // Fast path: Forge's own WAV demuxer (parallel, SIMD-friendly).
    if ext == "wav" || ext == "wave" {
        return WavReader::open(path).map_err(|e| format!("{}: {e}", path.display()));
    }
    if ext == "opus" {
        #[cfg(feature = "opus-encoding")]
        {
            let mut data: Vec<Vec<f32>> = Vec::new();
            let info = crate::opus::decode_stream(path, |info, planar| {
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
    decode_symphonia(path, &ext)
}

fn decode_symphonia(path: &Path, ext: &str) -> Result<AudioBuffer, String> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::errors::Error;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;
    use symphonia::default::{get_codecs, get_probe};

    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());

    let mut hint = Hint::new();
    if !ext.is_empty() {
        hint.with_extension(ext);
    }

    let probed = get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("{}: probe failed: {e}", path.display()))?;

    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| format!("{}: no audio track", path.display()))?
        .clone();

    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| format!("{}: unknown sample rate", path.display()))?;

    let mut decoder = get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("{}: unsupported codec: {e}", path.display()))?;

    let mut planar: Vec<Vec<f32>> = Vec::new();
    let mut channels: u16 = 0;
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            // Clean end of stream.
            Err(Error::IoError(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            // A mid-stream format change (e.g. MP3 sample-rate switch): rebuild
            // the decoder and continue.
            Err(Error::ResetRequired) => {
                decoder = get_codecs()
                    .make(&track.codec_params, &DecoderOptions::default())
                    .map_err(|e| format!("{}: reinit decoder: {e}", path.display()))?;
                continue;
            }
            Err(e) => return Err(format!("{}: read packet: {e}", path.display())),
        };

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // Skip a single corrupt frame rather than aborting the whole file.
            Err(Error::DecodeError(_)) => continue,
            Err(e) => return Err(format!("{}: decode: {e}", path.display())),
        };

        let spec = *decoded.spec();
        let ch = spec.channels.count();
        if ch == 0 {
            continue;
        }
        if channels == 0 {
            channels = ch as u16;
            planar = (0..ch).map(|_| Vec::new()).collect();
        }
        let frames = decoded.frames();
        let need = frames * ch;
        if sample_buf.as_ref().is_none_or(|b| b.len() < need) {
            // `Duration` is a u64 frame count; the buffer stores frames*ch samples.
            sample_buf = Some(SampleBuffer::<f32>::new(frames as u64, spec));
        }
        let sb = sample_buf.as_mut().unwrap();
        sb.copy_interleaved_ref(decoded);
        let inter = sb.samples();
        // De-interleave the freshly copied `frames*ch` samples into planar channels.
        for c in 0..ch {
            let plane = &mut planar[c];
            for f in 0..frames {
                plane.push(inter[f * ch + c]);
            }
        }
    }

    if channels == 0 || planar[0].is_empty() {
        return Err(format!("{}: no audio decoded", path.display()));
    }

    let frames = planar[0].len();
    let channel_roles = track
        .codec_params
        .channels
        .map(roles_from_symphonia)
        .filter(|roles| roles.len() == channels as usize)
        .unwrap_or_else(|| default_channel_roles(channels));
    Ok(AudioBuffer {
        sample_rate,
        channels,
        frames,
        data: planar,
        channel_roles,
        // Compressed inputs have no integer "source kind"; report float, which
        // is also the default WAV output kind for these files.
        source_kind: PcmKind::F32,
    })
}

fn roles_from_symphonia(channels: symphonia::core::audio::Channels) -> Vec<ChannelRole> {
    use symphonia::core::audio::Channels;
    channels
        .iter()
        .map(|channel| {
            if channel.intersects(Channels::LFE1 | Channels::LFE2) {
                ChannelRole::Lfe
            } else if channel.intersects(
                Channels::REAR_LEFT
                    | Channels::REAR_RIGHT
                    | Channels::REAR_CENTRE
                    | Channels::SIDE_LEFT
                    | Channels::SIDE_RIGHT
                    | Channels::REAR_LEFT_CENTRE
                    | Channels::REAR_RIGHT_CENTRE
                    | Channels::TOP_REAR_LEFT
                    | Channels::TOP_REAR_CENTRE
                    | Channels::TOP_REAR_RIGHT,
            ) {
                ChannelRole::Surround
            } else {
                ChannelRole::Main
            }
        })
        .collect()
}

/// Decode an audio file in bounded chunks without retaining the complete
/// sample stream.
pub fn decode_stream<F>(path: &Path, mut consume: F) -> Result<StreamInfo, String>
where
    F: FnMut(&StreamInfo, &mut [Vec<f32>]) -> Result<(), String>,
{
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::errors::Error;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;
    use symphonia::default::{get_codecs, get_probe};

    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if matches!(extension.as_str(), "wav" | "wave") {
        return decode_wav_stream(path, consume);
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
    let probed = get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|error| format!("{}: probe failed: {error}", path.display()))?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| format!("{}: no audio track", path.display()))?
        .clone();
    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| format!("{}: unknown sample rate", path.display()))?;
    let signaled_channels = track
        .codec_params
        .channels
        .ok_or_else(|| format!("{}: unknown channel layout", path.display()))?;
    let channels = signaled_channels.count() as u16;
    let (channel_roles, source_kind) = if matches!(extension.as_str(), "wav" | "wave") {
        let wav = WavReader::probe(path).map_err(|error| format!("{}: {error}", path.display()))?;
        if wav.sample_rate != sample_rate || wav.channels != channels {
            return Err(format!(
                "{}: inconsistent WAV stream format",
                path.display()
            ));
        }
        (wav.channel_roles, wav.kind)
    } else {
        (
            roles_from_symphonia(signaled_channels),
            source_kind(&extension, track.codec_params.bits_per_sample),
        )
    };
    let info = StreamInfo {
        sample_rate,
        channels,
        channel_roles,
        source_kind,
    };
    let mut decoder = get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|error| format!("{}: unsupported codec: {error}", path.display()))?;
    let mut sample_buffer: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(Error::IoError(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                break
            }
            Err(Error::ResetRequired) => {
                decoder = get_codecs()
                    .make(&track.codec_params, &DecoderOptions::default())
                    .map_err(|error| format!("{}: reinit decoder: {error}", path.display()))?;
                continue;
            }
            Err(error) => return Err(format!("{}: read packet: {error}", path.display())),
        };
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(Error::DecodeError(_)) => continue,
            Err(error) => return Err(format!("{}: decode: {error}", path.display())),
        };
        let spec = *decoded.spec();
        let decoded_channels = spec.channels.count();
        if decoded_channels != channels as usize || spec.rate != sample_rate {
            return Err(format!("{}: mid-stream format change", path.display()));
        }
        let frames = decoded.frames();
        if sample_buffer
            .as_ref()
            .is_none_or(|buffer| buffer.capacity() < frames * decoded_channels)
        {
            sample_buffer = Some(SampleBuffer::<f32>::new(frames as u64, spec));
        }
        let buffer = sample_buffer.as_mut().unwrap();
        buffer.copy_interleaved_ref(decoded);
        let samples = buffer.samples();
        let mut planar: Vec<Vec<f32>> = (0..decoded_channels)
            .map(|_| Vec::with_capacity(frames))
            .collect();
        for frame in samples[..frames * decoded_channels].chunks_exact(decoded_channels) {
            for (channel, sample) in planar.iter_mut().zip(frame) {
                channel.push(*sample);
            }
        }
        consume(&info, &mut planar)?;
    }

    Ok(info)
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
    file.seek(SeekFrom::Start(12))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let data_size = loop {
        let mut header = [0u8; 8];
        file.read_exact(&mut header)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        if &header[..4] == b"data" {
            break size;
        }
        file.seek(SeekFrom::Current((size + (size & 1)) as i64))
            .map_err(|error| format!("{}: {error}", path.display()))?;
    };

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
    if matches!(extension, "wav" | "wave") {
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
