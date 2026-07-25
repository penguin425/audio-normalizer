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
use std::path::Path;

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
