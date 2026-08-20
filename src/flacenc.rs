//! Bounded-memory, pure-Rust FLAC output.

use crate::dsp::convert;
use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, Stream, StreamInfo};
use flacenc::config::Encoder as EncoderConfig;
use flacenc::error::{Verified, Verify};
use flacenc::source::{Context, Fill, FrameBuf};
use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::Path;

const BLOCK_SIZE: usize = 4096;

pub struct FlacStreamWriter {
    file: BufWriter<File>,
    config: Verified<EncoderConfig>,
    info: StreamInfo,
    frame_buf: FrameBuf,
    context: Context,
    sink: ByteSink,
    pending: Vec<i32>,
    channels: usize,
    bits: usize,
    frame_number: usize,
    rngs: Vec<u64>,
    dither: bool,
    finalized: bool,
}

impl FlacStreamWriter {
    pub fn create(
        path: &Path,
        sample_rate: u32,
        channels: u16,
        bits: u16,
        dither: bool,
    ) -> Result<Self, String> {
        if !(1..=8).contains(&channels) {
            return Err(format!(
                "FLAC output supports 1..=8 channels, got {channels}"
            ));
        }
        if !matches!(bits, 16 | 24) {
            return Err(format!("FLAC output supports 16 or 24 bits, got {bits}"));
        }
        let config = EncoderConfig::default()
            .into_verified()
            .map_err(|(_, error)| error.to_string())?;
        let mut info = StreamInfo::new(sample_rate as usize, channels as usize, bits as usize)
            .map_err(|error| error.to_string())?;
        info.set_block_sizes(BLOCK_SIZE, BLOCK_SIZE)
            .map_err(|error| error.to_string())?;
        let frame_buf = FrameBuf::with_size(channels as usize, BLOCK_SIZE)
            .map_err(|error| error.to_string())?;
        let context = Context::new(bits as usize, channels as usize);
        let mut writer = Self {
            file: BufWriter::new(
                File::create(path)
                    .map_err(|error| format!("create {}: {error}", path.display()))?,
            ),
            config,
            info,
            frame_buf,
            context,
            sink: ByteSink::new(),
            pending: Vec::new(),
            channels: channels as usize,
            bits: bits as usize,
            frame_number: 0,
            rngs: convert::dither_rngs(channels as usize),
            dither,
            finalized: false,
        };
        let header = writer.header()?;
        writer.file.write_all(&header).map_err(|e| e.to_string())?;
        Ok(writer)
    }

    pub fn write_chunk(&mut self, planar: &[Vec<f32>]) -> Result<(), String> {
        self.write_chunk_observed(planar, |_, _| Ok(()))
    }

    /// Quantize one chunk, expose the exact interleaved integer samples to a
    /// caller, then encode them. This lets normalization verification share
    /// the encoder pass instead of reopening a completed lossless file.
    pub(crate) fn write_chunk_observed(
        &mut self,
        planar: &[Vec<f32>],
        observe: impl FnOnce(&[i32], usize) -> Result<(), String>,
    ) -> Result<(), String> {
        if planar.len() != self.channels {
            return Err("FLAC chunk channel count changed".into());
        }
        let frames = planar.first().map_or(0, Vec::len);
        if planar.iter().any(|channel| channel.len() != frames) {
            return Err("FLAC chunk has unequal channel lengths".into());
        }
        let pending_start = self.pending.len();
        self.pending.reserve(frames * self.channels);
        let scale = (1_u32 << (self.bits - 1)) as f32;
        let min = -(1_i32 << (self.bits - 1));
        let max = (1_i32 << (self.bits - 1)) - 1;
        for frame in 0..frames {
            for (channel, samples) in planar.iter().enumerate() {
                let noise = if self.dither {
                    convert::tpdf(&mut self.rngs[channel]) as f32
                } else {
                    0.0
                };
                self.pending.push(
                    (samples[frame] * scale + noise)
                        .round()
                        .clamp(min as f32, max as f32) as i32,
                );
            }
        }
        observe(&self.pending[pending_start..], self.bits)?;
        self.drain_blocks()
    }

    pub fn finish(mut self) -> Result<(), String> {
        let result = self.finish_inner();
        self.finalized = true;
        result
    }

    fn drain_blocks(&mut self) -> Result<(), String> {
        let block_len = BLOCK_SIZE * self.channels;
        let pending = std::mem::take(&mut self.pending);
        let mut used = 0;
        while used + block_len <= pending.len() {
            self.encode_block(&pending[used..used + block_len])?;
            used += block_len;
        }
        self.pending.extend_from_slice(&pending[used..]);
        Ok(())
    }

    fn encode_block(&mut self, samples: &[i32]) -> Result<(), String> {
        self.frame_buf
            .fill_interleaved(samples)
            .map_err(|error| error.to_string())?;
        self.context
            .fill_interleaved(samples)
            .map_err(|error| error.to_string())?;
        let frame = flacenc::encode_fixed_size_frame(
            &self.config,
            &self.frame_buf,
            self.frame_number,
            &self.info,
        )
        .map_err(|error| error.to_string())?;
        self.info.update_frame_info(&frame);
        self.sink.clear();
        frame
            .write(&mut self.sink)
            .map_err(|error| error.to_string())?;
        self.file
            .write_all(self.sink.as_slice())
            .map_err(|error| error.to_string())?;
        self.frame_number += 1;
        Ok(())
    }

    fn finish_inner(&mut self) -> Result<(), String> {
        if !self.pending.is_empty() {
            let tail = std::mem::take(&mut self.pending);
            self.encode_block(&tail)?;
        }
        self.info
            .set_block_sizes(BLOCK_SIZE, BLOCK_SIZE)
            .map_err(|error| error.to_string())?;
        self.info.set_md5_digest(&self.context.md5_digest());
        self.info.set_total_samples(self.context.total_samples());
        let header = self.header()?;
        self.file.rewind().map_err(|error| error.to_string())?;
        self.file
            .write_all(&header)
            .and_then(|_| self.file.flush())
            .map_err(|error| error.to_string())
    }

    fn header(&self) -> Result<Vec<u8>, String> {
        let mut info = self.info.clone();
        if self.frame_number == 0 {
            info.set_frame_sizes(0, 0)
                .map_err(|error| error.to_string())?;
        }
        let stream = Stream::with_stream_info(info);
        let mut sink = ByteSink::new();
        stream.write(&mut sink).map_err(|error| error.to_string())?;
        Ok(sink.into_inner())
    }
}

impl Drop for FlacStreamWriter {
    fn drop(&mut self) {
        if !self.finalized {
            let _ = self.finish_inner();
        }
    }
}
