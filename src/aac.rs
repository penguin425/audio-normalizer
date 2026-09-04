//! Streaming AAC, ALAC, and Vorbis encoding through an optional FFmpeg runtime.

use std::io::Write;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::OnceLock;

use crate::wav::{MAX_DECODE_SAMPLE_RATE_HZ, MIN_DECODE_SAMPLE_RATE_HZ};

pub struct AacStreamWriter {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    channels: usize,
    interleaved: Vec<u8>,
    codec: FfmpegCodec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfmpegCodec {
    Aac,
    Alac,
    Vorbis,
}

impl FfmpegCodec {
    fn name(self) -> &'static str {
        match self {
            Self::Aac => "AAC",
            Self::Alac => "ALAC",
            Self::Vorbis => "Vorbis",
        }
    }

    const fn encoder(self) -> &'static str {
        match self {
            Self::Aac => "aac",
            Self::Alac => "alac",
            Self::Vorbis => "libvorbis",
        }
    }

    const fn muxer(self) -> &'static str {
        match self {
            Self::Aac | Self::Alac => "ipod",
            Self::Vorbis => "ogg",
        }
    }
}

static AAC_PREFLIGHT: OnceLock<Result<(), String>> = OnceLock::new();
static ALAC_PREFLIGHT: OnceLock<Result<(), String>> = OnceLock::new();
static VORBIS_PREFLIGHT: OnceLock<Result<(), String>> = OnceLock::new();

/// Verify the exact encoder and muxer required by one FFmpeg-backed format.
/// Results are cached per process after the first successful or failed probe.
pub fn preflight_ffmpeg(codec: FfmpegCodec) -> Result<(), String> {
    let slot = match codec {
        FfmpegCodec::Aac => &AAC_PREFLIGHT,
        FfmpegCodec::Alac => &ALAC_PREFLIGHT,
        FfmpegCodec::Vorbis => &VORBIS_PREFLIGHT,
    };
    slot.get_or_init(|| run_ffmpeg_preflight(codec)).clone()
}

fn run_ffmpeg_preflight(codec: FfmpegCodec) -> Result<(), String> {
    let encoders = ffmpeg_capability_output("-encoders", codec)?;
    if !listed_ffmpeg_component(&encoders, codec.encoder(), CapabilityKind::Encoder) {
        return Err(format!(
            "FFmpeg {} output requires the exact `{}` encoder, but this runtime does not provide it",
            codec.name(),
            codec.encoder()
        ));
    }
    let muxers = ffmpeg_capability_output("-muxers", codec)?;
    if !listed_ffmpeg_component(&muxers, codec.muxer(), CapabilityKind::Muxer) {
        return Err(format!(
            "FFmpeg {} output requires the exact `{}` muxer, but this runtime does not provide it",
            codec.name(),
            codec.muxer()
        ));
    }
    Ok(())
}

fn ffmpeg_capability_output(argument: &str, codec: FfmpegCodec) -> Result<String, String> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", argument])
        .output()
        .map_err(|error| {
            format!(
                "inspect FFmpeg {} capabilities: {error}; install `ffmpeg` or choose another format",
                codec.name()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "inspect FFmpeg {} capabilities failed with {}: {}",
            codec.name(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| "FFmpeg capability output is not valid UTF-8".to_string())
}

#[derive(Clone, Copy)]
enum CapabilityKind {
    Encoder,
    Muxer,
}

fn listed_ffmpeg_component(output: &str, required: &str, kind: CapabilityKind) -> bool {
    output.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let Some(flags) = fields.next() else {
            return false;
        };
        let valid_flags = match kind {
            CapabilityKind::Encoder => flags.len() == 6 && flags.starts_with('A'),
            CapabilityKind::Muxer => flags.len() <= 2 && flags.contains('E'),
        };
        valid_flags
            && fields
                .next()
                .is_some_and(|names| names.split(',').any(|name| name == required))
    })
}

impl AacStreamWriter {
    pub fn create(
        path: &Path,
        sample_rate: u32,
        channels: u16,
        bitrate_kbps: i32,
    ) -> Result<Self, String> {
        Self::create_codec(path, sample_rate, channels, bitrate_kbps, FfmpegCodec::Aac)
    }

    pub fn create_codec(
        path: &Path,
        sample_rate: u32,
        channels: u16,
        bitrate_kbps: i32,
        codec: FfmpegCodec,
    ) -> Result<Self, String> {
        validate_ffmpeg_sample_rate(codec, sample_rate)?;
        let channel_layout = ffmpeg_channel_layout(codec, channels)?;
        validate_ffmpeg_bitrate(codec, sample_rate, channels, bitrate_kbps)?;
        preflight_ffmpeg(codec)?;
        let mut command = Command::new("ffmpeg");
        command.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "f32le",
            "-ar",
            &sample_rate.to_string(),
            "-ac",
            &channels.to_string(),
            "-channel_layout",
            channel_layout,
            "-i",
            "pipe:0",
            "-map",
            "0:a:0",
            "-map_metadata",
            "-1",
        ]);
        match codec {
            FfmpegCodec::Aac => {
                command.args([
                    "-c:a",
                    "aac",
                    "-profile:a",
                    "aac_low",
                    "-b:a",
                    &format!("{bitrate_kbps}k"),
                    "-movflags",
                    "+faststart+use_metadata_tags",
                    "-f",
                    "ipod",
                ]);
            }
            FfmpegCodec::Alac => {
                command.args([
                    "-c:a",
                    "alac",
                    "-compression_level",
                    "5",
                    "-movflags",
                    "+faststart+use_metadata_tags",
                    "-f",
                    "ipod",
                ]);
            }
            FfmpegCodec::Vorbis => {
                command.args([
                    "-c:a",
                    "libvorbis",
                    "-b:a",
                    &format!("{bitrate_kbps}k"),
                    "-f",
                    "ogg",
                ]);
            }
        }
        let mut child = command
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                format!(
                    "start FFmpeg {} encoder: {error}; install `ffmpeg` or choose another format",
                    codec.name()
                )
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("FFmpeg {} encoder did not provide stdin", codec.name()))?;
        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            channels: channels as usize,
            interleaved: Vec::new(),
            codec,
        })
    }

    pub fn write_chunk(&mut self, planar: &[Vec<f32>]) -> Result<(), String> {
        if planar.len() != self.channels {
            return Err(format!(
                "{} encoder channel count changed",
                self.codec.name()
            ));
        }
        let frames = planar.first().map_or(0, Vec::len);
        if planar.iter().any(|channel| channel.len() != frames) {
            return Err(format!(
                "{} encoder input has unequal channel lengths",
                self.codec.name()
            ));
        }
        self.interleaved.clear();
        self.interleaved
            .reserve(frames * self.channels * size_of::<f32>());
        for frame in 0..frames {
            for channel in planar {
                self.interleaved
                    .extend_from_slice(&channel[frame].to_le_bytes());
            }
        }
        self.stdin
            .as_mut()
            .ok_or_else(|| format!("{} encoder is already finished", self.codec.name()))?
            .write_all(&self.interleaved)
            .map_err(|error| format!("write PCM to FFmpeg {} encoder: {error}", self.codec.name()))
    }

    pub fn finish(mut self) -> Result<(), String> {
        self.stdin.take();
        let child = self
            .child
            .take()
            .ok_or_else(|| format!("{} encoder is already finished", self.codec.name()))?;
        let output = child
            .wait_with_output()
            .map_err(|error| format!("wait for FFmpeg {} encoder: {error}", self.codec.name()))?;
        if output.status.success() {
            Ok(())
        } else {
            let details = String::from_utf8_lossy(&output.stderr);
            Err(format!(
                "FFmpeg {} encoder failed with {}: {}",
                self.codec.name(),
                output.status,
                details.trim()
            ))
        }
    }
}

fn validate_ffmpeg_sample_rate(codec: FfmpegCodec, sample_rate: u32) -> Result<(), String> {
    if !(MIN_DECODE_SAMPLE_RATE_HZ..=MAX_DECODE_SAMPLE_RATE_HZ).contains(&sample_rate) {
        return Err(format!(
            "{} encoder sample rate {sample_rate} Hz is outside Forge's supported {MIN_DECODE_SAMPLE_RATE_HZ}..={MAX_DECODE_SAMPLE_RATE_HZ} Hz range",
            codec.name(),
        ));
    }
    // This is the intersection of FFmpeg's native AAC rates and Forge's
    // supported decode/measurement domain. If given a different raw-PCM rate,
    // FFmpeg silently inserts a resampler and persists a different rate.
    const AAC_SAMPLE_RATES: [u32; 12] = [
        8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 64_000, 88_200,
        96_000,
    ];
    if codec == FfmpegCodec::Aac && !AAC_SAMPLE_RATES.contains(&sample_rate) {
        return Err(format!(
            "AAC encoder cannot preserve unsupported sample rate {sample_rate} Hz"
        ));
    }
    Ok(())
}

fn ffmpeg_channel_layout(codec: FfmpegCodec, channels: u16) -> Result<&'static str, String> {
    // The bundled AAC decoder supports only mono/stereo. FFmpeg's ALAC output
    // does not persist authoritative multichannel layout metadata that Forge
    // can bind on re-decode (and some 7.1 runtimes also downmix). Refuse M4A
    // multichannel output before starting the subprocess. Ogg Vorbis has an
    // independent, verified channel mapping and remains supported below.
    if matches!(codec, FfmpegCodec::Aac | FfmpegCodec::Alac) && channels > 2 {
        return Err(format!(
            "{} multichannel output is disabled because Forge cannot verify its speaker layout",
            codec.name()
        ));
    }
    match channels {
        1 => Ok("mono"),
        2 => Ok("stereo"),
        6 => Ok("5.1"),
        8 => Ok("7.1"),
        _ => Err(format!(
            "{}/{} output supports mono, stereo, 5.1, or 7.1",
            codec.name(),
            match codec {
                FfmpegCodec::Vorbis => "Ogg",
                _ => "M4A",
            }
        )),
    }
}

fn validate_ffmpeg_bitrate(
    codec: FfmpegCodec,
    sample_rate: u32,
    channels: u16,
    bitrate_kbps: i32,
) -> Result<(), String> {
    let (minimum, maximum) = match codec {
        FfmpegCodec::Alac => return Ok(()),
        FfmpegCodec::Vorbis => {
            vorbis_bitrate_range(sample_rate, channels).ok_or_else(|| {
                format!(
                    "Vorbis {channels}-channel managed-bitrate encoding does not support {sample_rate} Hz"
                )
            })?
        }
        FfmpegCodec::Aac => aac_bitrate_range(sample_rate, channels),
    };
    if !(minimum..=maximum).contains(&bitrate_kbps) {
        return Err(format!(
            "{} {channels}-channel bitrate at {sample_rate} Hz must be between {minimum} and {maximum} kbps",
            codec.name()
        ));
    }
    Ok(())
}

/// Requested bitrate range that FFmpeg's native AAC encoder will retain.
///
/// FFmpeg 6.1.1 limits an AAC raw-data block to 6144 bits per channel and uses
/// 1024 PCM frames per block. During encoder initialization it silently clamps
/// `bit_rate` to `6144 * channels / 1024 * sample_rate`, or exactly six bits
/// per input sample. Reject settings above that threshold rather than letting
/// the subprocess persist a different bitrate. The 8 kbps lower bound is the
/// existing Forge API floor; the native encoder applies no corresponding
/// positive-bitrate clamp.
fn aac_bitrate_range(sample_rate: u32, channels: u16) -> (i32, i32) {
    let native_maximum_kbps =
        (u64::from(sample_rate) * 6 * u64::from(channels) / 1_000).min(1_024) as i32;
    (8, native_maximum_kbps)
}

/// Conservative total-bitrate ranges accepted by libvorbis managed mode.
///
/// FFmpeg uses `vorbis_encode_setup_managed` when `-b:a` is supplied. In
/// libvorbis 1.3.7 that API divides the requested total bitrate by the channel
/// count and selects one of the `setup_{8,11,16,22,32,44,44p51}.h` rate maps.
/// The catch-all setup above 50 kHz has no managed-bitrate entries; the one
/// exception is the six-channel 44p51 setup, which extends through 70 kHz.
/// Round lower bounds up to the next 8 kbps step to stay safely inside the
/// published maps, and retain Forge's existing 1024 kbps public ceiling.
fn vorbis_bitrate_range(sample_rate: u32, channels: u16) -> Option<(i32, i32)> {
    let (minimum_per_channel_bps, maximum_per_channel_bps) = match (sample_rate, channels) {
        (8_000..=8_999, 2) => (6_000_i64, 32_000_i64),
        (8_000..=8_999, 1 | 6 | 8) => (8_000, 42_000),
        (9_000..=14_999, 2) => (8_000, 44_000),
        (9_000..=14_999, 1 | 6 | 8) => (12_000, 50_000),
        (15_000..=18_999, 2) => (12_000, 86_000),
        (15_000..=18_999, 1 | 6 | 8) => (16_000, 100_000),
        (19_000..=25_999, 2) => (15_000, 86_000),
        (19_000..=25_999, 1 | 6 | 8) => (16_000, 90_000),
        (26_000..=39_999, 2) => (18_000, 190_000),
        (26_000..=39_999, 1 | 6 | 8) => (30_000, 190_000),
        (40_000..=50_000, 2) => (22_500, 250_001),
        (40_000..=70_000, 6) => (14_000, 240_001),
        (40_000..=50_000, 1 | 8) => (32_000, 240_001),
        _ => return None,
    };
    let channels = i64::from(channels);
    let exact_minimum_kbps = (minimum_per_channel_bps * channels + 999).checked_div(1_000)?;
    let minimum_kbps = (exact_minimum_kbps + 7).checked_div(8)?.checked_mul(8)?;
    let maximum_kbps = (maximum_per_channel_bps * channels)
        .checked_div(1_000)?
        .min(1_024);
    Some((
        i32::try_from(minimum_kbps).ok()?,
        i32::try_from(maximum_kbps).ok()?,
    ))
}

impl Drop for AacStreamWriter {
    fn drop(&mut self) {
        self.stdin.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_parser_requires_exact_audio_encoder_and_muxer_names() {
        let encoders = "\
Encoders:\n\
 V..... aac_video not audio\n\
 A....D aac AAC encoder\n\
 A..... libvorbis Vorbis encoder\n";
        assert!(listed_ffmpeg_component(
            encoders,
            "aac",
            CapabilityKind::Encoder
        ));
        assert!(listed_ffmpeg_component(
            encoders,
            "libvorbis",
            CapabilityKind::Encoder
        ));
        assert!(!listed_ffmpeg_component(
            encoders,
            "vorbis",
            CapabilityKind::Encoder
        ));
        assert!(!listed_ffmpeg_component(
            encoders,
            "aac_video",
            CapabilityKind::Encoder
        ));

        let muxers = "\
File formats:\n\
 D  ipod            demux-only decoy\n\
  E mov,mp4,m4a     QuickTime family\n\
  E ipod            iPod H.264 MP4\n\
 DE ogg             Ogg\n";
        assert!(listed_ffmpeg_component(
            muxers,
            "ipod",
            CapabilityKind::Muxer
        ));
        assert!(listed_ffmpeg_component(
            muxers,
            "m4a",
            CapabilityKind::Muxer
        ));
        assert!(listed_ffmpeg_component(
            muxers,
            "ogg",
            CapabilityKind::Muxer
        ));
        assert!(!listed_ffmpeg_component(
            muxers,
            "ipo",
            CapabilityKind::Muxer
        ));
    }

    #[test]
    fn unsafe_m4a_layouts_are_rejected_before_creating_output() {
        let directory = tempfile::tempdir().unwrap();
        for (codec, channels, diagnostic) in [
            (FfmpegCodec::Aac, 6, "cannot verify"),
            (FfmpegCodec::Aac, 8, "cannot verify"),
            (FfmpegCodec::Alac, 6, "cannot verify"),
            (FfmpegCodec::Alac, 8, "cannot verify"),
        ] {
            let path = directory
                .path()
                .join(format!("unsafe-{codec:?}-{channels}.m4a"));
            let error = AacStreamWriter::create_codec(&path, 48_000, channels, 256, codec)
                .err()
                .expect("unsafe M4A layout must fail before FFmpeg starts");
            assert!(error.contains(diagnostic), "{error}");
            assert!(!path.exists());
        }

        for sample_rate in [7_350, 384_000] {
            let path = directory
                .path()
                .join(format!("unsupported-aac-rate-{sample_rate}.m4a"));
            std::fs::write(&path, b"existing destination").unwrap();
            let error = AacStreamWriter::create_codec(&path, sample_rate, 2, 192, FfmpegCodec::Aac)
                .err()
                .expect("unsupported AAC rate must fail before FFmpeg starts");
            assert!(
                error.contains(&format!("sample rate {sample_rate} Hz")),
                "{error}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"existing destination");
        }

        for sample_rate in [7_999, 384_001] {
            let path = directory
                .path()
                .join(format!("unsupported-alac-rate-{sample_rate}.m4a"));
            std::fs::write(&path, b"existing destination").unwrap();
            let error =
                AacStreamWriter::create_codec(&path, sample_rate, 2, 192, FfmpegCodec::Alac)
                    .err()
                    .expect("unsupported ALAC rate must fail before FFmpeg starts");
            assert!(
                error.contains(&format!("sample rate {sample_rate} Hz")),
                "{error}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"existing destination");
        }

        for (sample_rate, channels, bitrate, maximum) in [(8_000, 1, 49, 48), (48_000, 2, 577, 576)]
        {
            let path = directory.path().join(format!(
                "clamped-aac-{sample_rate}-{channels}-{bitrate}.m4a"
            ));
            std::fs::write(&path, b"existing destination").unwrap();
            let error = AacStreamWriter::create_codec(
                &path,
                sample_rate,
                channels,
                bitrate,
                FfmpegCodec::Aac,
            )
            .err()
            .expect("internally clamped AAC bitrate must fail before FFmpeg starts");
            assert!(
                error.contains(&format!("between 8 and {maximum} kbps")),
                "{error}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"existing destination");
        }

        for (sample_rate, channels, bitrate) in [
            (48_000, 1, 31),
            (48_000, 2, 512),
            (48_000, 6, 87),
            (48_000, 8, 192),
            (96_000, 2, 192),
        ] {
            let path = directory.path().join(format!(
                "unsafe-vorbis-{sample_rate}-{channels}-{bitrate}.ogg"
            ));
            std::fs::write(&path, b"existing destination").unwrap();
            let error = AacStreamWriter::create_codec(
                &path,
                sample_rate,
                channels,
                bitrate,
                FfmpegCodec::Vorbis,
            )
            .err()
            .expect("unsafe Vorbis setup must fail before FFmpeg starts");
            assert!(
                error.contains("bitrate") || error.contains("does not support"),
                "{error}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"existing destination");
        }
    }

    #[test]
    fn channel_layout_validation_preserves_existing_formats() {
        for sample_rate in [
            8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 64_000, 88_200,
            96_000,
        ] {
            validate_ffmpeg_sample_rate(FfmpegCodec::Aac, sample_rate).unwrap();
        }
        assert!(validate_ffmpeg_sample_rate(FfmpegCodec::Aac, 0).is_err());
        assert!(validate_ffmpeg_sample_rate(FfmpegCodec::Aac, 7_350).is_err());
        assert!(validate_ffmpeg_sample_rate(FfmpegCodec::Aac, 384_000).is_err());
        assert!(validate_ffmpeg_sample_rate(FfmpegCodec::Alac, 7_999).is_err());
        validate_ffmpeg_sample_rate(FfmpegCodec::Alac, 384_000).unwrap();
        assert!(validate_ffmpeg_sample_rate(FfmpegCodec::Alac, 384_001).is_err());
        validate_ffmpeg_sample_rate(FfmpegCodec::Vorbis, 384_000).unwrap();

        for codec in [FfmpegCodec::Aac, FfmpegCodec::Alac, FfmpegCodec::Vorbis] {
            assert_eq!(ffmpeg_channel_layout(codec, 1).unwrap(), "mono");
            assert_eq!(ffmpeg_channel_layout(codec, 2).unwrap(), "stereo");
        }
        assert_eq!(
            ffmpeg_channel_layout(FfmpegCodec::Vorbis, 6).unwrap(),
            "5.1"
        );
        assert_eq!(
            ffmpeg_channel_layout(FfmpegCodec::Vorbis, 8).unwrap(),
            "7.1"
        );
        assert!(ffmpeg_channel_layout(FfmpegCodec::Aac, 6).is_err());
        assert!(ffmpeg_channel_layout(FfmpegCodec::Aac, 8).is_err());
        assert!(ffmpeg_channel_layout(FfmpegCodec::Alac, 6).is_err());
        assert!(ffmpeg_channel_layout(FfmpegCodec::Alac, 8).is_err());
    }

    #[test]
    fn vorbis_managed_bitrate_ranges_are_sample_rate_and_channel_aware() {
        let expected = [
            (8_000, [(8, 42), (16, 64), (48, 252), (64, 336)]),
            (11_025, [(16, 50), (16, 88), (72, 300), (96, 400)]),
            (16_000, [(16, 100), (24, 172), (96, 600), (128, 800)]),
            (22_050, [(16, 90), (32, 172), (96, 540), (128, 720)]),
            (32_000, [(32, 190), (40, 380), (184, 1_024), (240, 1_024)]),
            (48_000, [(32, 240), (48, 500), (88, 1_024), (256, 1_024)]),
        ];
        for (sample_rate, ranges) in expected {
            for (channels, expected_range) in [1, 2, 6, 8].into_iter().zip(ranges) {
                assert_eq!(
                    vorbis_bitrate_range(sample_rate, channels),
                    Some(expected_range),
                    "sample_rate={sample_rate}, channels={channels}"
                );
                let (minimum, maximum) = expected_range;
                assert!(validate_ffmpeg_bitrate(
                    FfmpegCodec::Vorbis,
                    sample_rate,
                    channels,
                    minimum - 1
                )
                .is_err());
                validate_ffmpeg_bitrate(FfmpegCodec::Vorbis, sample_rate, channels, minimum)
                    .unwrap();
                validate_ffmpeg_bitrate(FfmpegCodec::Vorbis, sample_rate, channels, maximum)
                    .unwrap();
                assert!(validate_ffmpeg_bitrate(
                    FfmpegCodec::Vorbis,
                    sample_rate,
                    channels,
                    maximum + 1
                )
                .is_err());
            }
        }

        assert_eq!(vorbis_bitrate_range(70_000, 6), Some((88, 1_024)));
        assert_eq!(vorbis_bitrate_range(50_001, 1), None);
        assert_eq!(vorbis_bitrate_range(50_001, 2), None);
        assert_eq!(vorbis_bitrate_range(50_001, 8), None);
        assert_eq!(vorbis_bitrate_range(70_001, 6), None);
        assert_eq!(vorbis_bitrate_range(7_999, 2), None);

        validate_ffmpeg_bitrate(FfmpegCodec::Aac, 48_000, 2, 8).unwrap();
        validate_ffmpeg_bitrate(FfmpegCodec::Alac, 48_000, 2, 0).unwrap();
    }

    #[test]
    fn aac_bitrate_limits_reject_only_settings_the_native_encoder_would_clamp() {
        let expected = [
            (8_000, 48, 96),
            (11_025, 66, 132),
            (12_000, 72, 144),
            (16_000, 96, 192),
            (22_050, 132, 264),
            (24_000, 144, 288),
            (32_000, 192, 384),
            (44_100, 264, 529),
            (48_000, 288, 576),
            (64_000, 384, 768),
            (88_200, 529, 1_024),
            (96_000, 576, 1_024),
        ];
        for (sample_rate, mono_maximum, stereo_maximum) in expected {
            for (channels, maximum) in [(1, mono_maximum), (2, stereo_maximum)] {
                assert_eq!(aac_bitrate_range(sample_rate, channels), (8, maximum));
                assert!(
                    validate_ffmpeg_bitrate(FfmpegCodec::Aac, sample_rate, channels, 7).is_err()
                );
                validate_ffmpeg_bitrate(FfmpegCodec::Aac, sample_rate, channels, 8).unwrap();
                validate_ffmpeg_bitrate(FfmpegCodec::Aac, sample_rate, channels, maximum).unwrap();
                assert!(validate_ffmpeg_bitrate(
                    FfmpegCodec::Aac,
                    sample_rate,
                    channels,
                    maximum + 1
                )
                .is_err());
            }
        }
    }
}
