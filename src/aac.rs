//! Streaming AAC-LC in an M4A/MP4 container through an optional FFmpeg runtime.

use std::io::Write;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};

pub struct AacStreamWriter {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    channels: usize,
    interleaved: Vec<u8>,
}

impl AacStreamWriter {
    pub fn create(
        path: &Path,
        sample_rate: u32,
        channels: u16,
        bitrate_kbps: i32,
    ) -> Result<Self, String> {
        if sample_rate == 0 {
            return Err("AAC encoder requires a positive sample rate".into());
        }
        let channel_layout = match channels {
            1 => "mono",
            2 => "stereo",
            6 => "5.1",
            8 => "7.1",
            _ => return Err("AAC/M4A output supports mono, stereo, 5.1, or 7.1".into()),
        };
        if !(8..=1_024).contains(&bitrate_kbps) {
            return Err("AAC bitrate must be between 8 and 1024 kbps".into());
        }
        let mut child = Command::new("ffmpeg")
            .args([
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
            ])
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                format!(
                    "start FFmpeg AAC encoder: {error}; install `ffmpeg` or choose another format"
                )
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "FFmpeg AAC encoder did not provide stdin".to_string())?;
        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            channels: channels as usize,
            interleaved: Vec::new(),
        })
    }

    pub fn write_chunk(&mut self, planar: &[Vec<f32>]) -> Result<(), String> {
        if planar.len() != self.channels {
            return Err("AAC encoder channel count changed".into());
        }
        let frames = planar.first().map_or(0, Vec::len);
        if planar.iter().any(|channel| channel.len() != frames) {
            return Err("AAC encoder input has unequal channel lengths".into());
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
            .ok_or_else(|| "AAC encoder is already finished".to_string())?
            .write_all(&self.interleaved)
            .map_err(|error| format!("write PCM to FFmpeg AAC encoder: {error}"))
    }

    pub fn finish(mut self) -> Result<(), String> {
        self.stdin.take();
        let child = self
            .child
            .take()
            .ok_or_else(|| "AAC encoder is already finished".to_string())?;
        let output = child
            .wait_with_output()
            .map_err(|error| format!("wait for FFmpeg AAC encoder: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            let details = String::from_utf8_lossy(&output.stderr);
            Err(format!(
                "FFmpeg AAC encoder failed with {}: {}",
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
