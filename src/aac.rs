//! Streaming AAC, ALAC, and Vorbis encoding through an optional FFmpeg runtime.

use std::io::Write;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};

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
        if sample_rate == 0 {
            return Err(format!(
                "{} encoder requires a positive sample rate",
                codec.name()
            ));
        }
        let channel_layout = match channels {
            1 => "mono",
            2 => "stereo",
            6 => "5.1",
            8 => "7.1",
            _ => {
                return Err(format!(
                    "{}/{} output supports mono, stereo, 5.1, or 7.1",
                    codec.name(),
                    match codec {
                        FfmpegCodec::Vorbis => "Ogg",
                        _ => "M4A",
                    }
                ))
            }
        };
        if codec != FfmpegCodec::Alac && !(8..=1_024).contains(&bitrate_kbps) {
            return Err(format!(
                "{} bitrate must be between 8 and 1024 kbps",
                codec.name()
            ));
        }
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

impl Drop for AacStreamWriter {
    fn drop(&mut self) {
        self.stdin.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
