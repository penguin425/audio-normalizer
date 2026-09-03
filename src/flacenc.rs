//! Bounded-memory, pure-Rust FLAC output.

use crate::dsp::convert;
use crate::wav::{MAX_DECODE_SAMPLE_RATE_HZ, MIN_DECODE_SAMPLE_RATE_HZ};
use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, Frame, Stream, StreamInfo};
use flacenc::config::Encoder as EncoderConfig;
use flacenc::error::{Verified, Verify};
use flacenc::source::{Context, Fill, FrameBuf};
use rayon::prelude::*;
use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::Path;

const BLOCK_SIZE: usize = 4096;
const MIN_PARALLEL_BLOCKS: usize = 4;
const MAX_PARALLEL_SAMPLE_VALUES: usize = 256 * 1024;
const MAX_PARALLEL_TASKS: usize = 8;

struct ParallelFrameSlot {
    frame_buf: FrameBuf,
    sink: ByteSink,
    error: Option<String>,
}

impl ParallelFrameSlot {
    fn new(channels: usize) -> Result<Self, String> {
        Ok(Self {
            frame_buf: FrameBuf::with_size(channels, BLOCK_SIZE)
                .map_err(|error| error.to_string())?,
            sink: ByteSink::new(),
            error: None,
        })
    }
}

pub struct FlacStreamWriter {
    file: BufWriter<File>,
    config: Verified<EncoderConfig>,
    info: StreamInfo,
    frame_buf: FrameBuf,
    parallel_slots: Vec<ParallelFrameSlot>,
    context: Context,
    sink: ByteSink,
    pending: Vec<i32>,
    channels: usize,
    bits: usize,
    frame_number: usize,
    rngs: Vec<u64>,
    dither: bool,
    parallel_tasks: usize,
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
        if !(MIN_DECODE_SAMPLE_RATE_HZ..=MAX_DECODE_SAMPLE_RATE_HZ).contains(&sample_rate) {
            return Err(format!(
                "FLAC sample rate {sample_rate} Hz is outside Forge's supported {MIN_DECODE_SAMPLE_RATE_HZ}..={MAX_DECODE_SAMPLE_RATE_HZ} Hz range"
            ));
        }
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
            parallel_slots: Vec::new(),
            context,
            sink: ByteSink::new(),
            pending: Vec::new(),
            channels: channels as usize,
            bits: bits as usize,
            frame_number: 0,
            rngs: convert::dither_rngs(channels as usize),
            dither,
            parallel_tasks: if rayon::current_thread_index().is_some() {
                1
            } else {
                rayon::current_num_threads().clamp(1, MAX_PARALLEL_TASKS)
            },
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
        convert::append_quantized_interleaved_i32(
            planar,
            self.bits,
            self.dither,
            &mut self.rngs,
            &mut self.pending,
        );
        observe(&self.pending[pending_start..], self.bits)?;
        self.drain_blocks(false)
    }

    pub fn finish(mut self) -> Result<(), String> {
        let result = self.finish_inner();
        self.finalized = true;
        result
    }

    fn drain_blocks(&mut self, force: bool) -> Result<(), String> {
        let block_len = BLOCK_SIZE * self.channels;
        let worker_count = self.parallel_tasks;
        let max_parallel_blocks = (MAX_PARALLEL_SAMPLE_VALUES / block_len).max(MIN_PARALLEL_BLOCKS);
        if worker_count > 1 && !force && self.pending.len() / block_len < max_parallel_blocks {
            return Ok(());
        }

        let pending = std::mem::take(&mut self.pending);
        let mut used = 0;
        let parallel_threshold = if force {
            MIN_PARALLEL_BLOCKS
        } else {
            max_parallel_blocks
        };
        while worker_count > 1
            && pending.len().saturating_sub(used) / block_len >= parallel_threshold
        {
            let available_blocks = (pending.len() - used) / block_len;
            let batch_blocks = available_blocks.min(max_parallel_blocks);
            let batch_len = batch_blocks * block_len;
            self.encode_blocks_parallel(&pending[used..used + batch_len])?;
            used += batch_len;
        }
        if force || worker_count == 1 {
            while used + block_len <= pending.len() {
                self.encode_block(&pending[used..used + block_len])?;
                used += block_len;
            }
        }
        self.pending.extend_from_slice(&pending[used..]);
        Ok(())
    }

    fn encode_blocks_parallel(&mut self, samples: &[i32]) -> Result<(), String> {
        let block_len = BLOCK_SIZE * self.channels;
        debug_assert_eq!(samples.len() % block_len, 0);
        let block_count = samples.len() / block_len;
        while self.parallel_slots.len() < block_count {
            self.parallel_slots
                .push(ParallelFrameSlot::new(self.channels)?);
        }
        let first_frame_number = self.frame_number;
        let minimum_task_length = block_count.div_ceil(self.parallel_tasks);
        {
            let config = &self.config;
            let info = &self.info;
            let slots = &mut self.parallel_slots[..block_count];
            let context = &mut self.context;
            let (_, context_result) = rayon::join(
                || {
                    slots
                        .par_iter_mut()
                        .zip(samples.par_chunks_exact(block_len))
                        .with_min_len(minimum_task_length)
                        .enumerate()
                        .for_each(|(offset, (slot, block))| {
                            slot.sink.clear();
                            slot.error = None;
                            if let Err(error) = slot.frame_buf.fill_interleaved(block) {
                                slot.error = Some(error.to_string());
                                return;
                            }
                            match flacenc::encode_fixed_size_frame(
                                config,
                                &slot.frame_buf,
                                first_frame_number + offset,
                                info,
                            ) {
                                Ok(frame) => {
                                    if let Err(error) = frame.write(&mut slot.sink) {
                                        slot.error = Some(error.to_string());
                                    }
                                }
                                Err(error) => slot.error = Some(error.to_string()),
                            }
                        });
                },
                || {
                    // Preserve Context's established block boundaries and
                    // source order while overlapping its serial MD5 pass with
                    // independent frame encoding.
                    samples.chunks_exact(block_len).try_for_each(|block| {
                        context
                            .fill_interleaved(block)
                            .map_err(|error| error.to_string())
                    })
                },
            );
            context_result?;
        }

        for index in 0..block_count {
            let encoded = {
                let slot = &mut self.parallel_slots[index];
                if let Some(error) = slot.error.take() {
                    return Err(error);
                }
                slot.sink.as_slice()
            };
            let minimum = self.info.min_frame_size().min(encoded.len());
            let maximum = self.info.max_frame_size().max(encoded.len());
            self.info
                .set_frame_sizes(minimum, maximum)
                .map_err(|error| error.to_string())?;
            self.file
                .write_all(encoded)
                .map_err(|error| error.to_string())?;
            self.frame_number += 1;
        }
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
        self.write_frame(frame)
    }

    fn write_frame(&mut self, frame: Frame) -> Result<(), String> {
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
        self.drain_blocks(true)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_sample_rates_preserve_existing_destination() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("existing.flac");
        for sample_rate in [7_999, 384_001] {
            std::fs::write(&path, b"existing destination").unwrap();
            let error = FlacStreamWriter::create(&path, sample_rate, 2, 16, false)
                .err()
                .expect("unsupported FLAC rate must fail before opening the destination");
            assert!(
                error.contains(&format!("sample rate {sample_rate} Hz")),
                "{error}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"existing destination");
        }
    }

    fn encoded_with_threads(threads: usize, chunks: &[usize], bits: u16, dither: bool) -> Vec<u8> {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("output.flac");
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
            .install(|| {
                let mut writer = FlacStreamWriter::create(&path, 48_000, 2, bits, dither).unwrap();
                writer.parallel_tasks = threads.clamp(1, MAX_PARALLEL_TASKS);
                let mut frame_offset = 0_usize;
                for &frames in chunks {
                    let left = (frame_offset..frame_offset + frames)
                        .map(|frame| ((frame as f32 * 0.013_579).sin() * 0.91).clamp(-1.0, 1.0))
                        .collect::<Vec<_>>();
                    let right = (frame_offset..frame_offset + frames)
                        .map(|frame| ((frame as f32 * 0.009_731).cos() * 0.83).clamp(-1.0, 1.0))
                        .collect::<Vec<_>>();
                    writer.write_chunk(&[left, right]).unwrap();
                    frame_offset += frames;
                }
                writer.finish().unwrap();
            });
        std::fs::read(path).unwrap()
    }

    #[test]
    fn parallel_frames_match_serial_bit_for_bit() {
        let chunks = [BLOCK_SIZE * 17 + 137];
        let serial = encoded_with_threads(1, &chunks, 24, false);
        let parallel = encoded_with_threads(4, &chunks, 24, false);
        assert_eq!(parallel, serial);
    }

    #[test]
    fn parallel_frames_preserve_chunk_boundaries_tail_and_dither() {
        let chunks = [977, BLOCK_SIZE * 9 + 13, BLOCK_SIZE * 8 + 211];
        let serial = encoded_with_threads(1, &chunks, 16, true);
        let parallel = encoded_with_threads(4, &chunks, 16, true);
        assert_eq!(parallel, serial);
    }

    #[test]
    fn small_chunks_coalesce_to_the_bounded_parallel_batch() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("coalesced.flac");
        rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap()
            .install(|| {
                let mut writer = FlacStreamWriter::create(&path, 48_000, 2, 16, false).unwrap();
                writer.parallel_tasks = 4;
                let samples = vec![vec![0.125; BLOCK_SIZE], vec![-0.125; BLOCK_SIZE]];
                let batch_blocks = MAX_PARALLEL_SAMPLE_VALUES / (BLOCK_SIZE * 2);
                for block in 1..batch_blocks {
                    writer.write_chunk(&samples).unwrap();
                    assert_eq!(writer.pending.len(), block * BLOCK_SIZE * 2);
                }
                writer.write_chunk(&samples).unwrap();
                assert!(writer.pending.is_empty());
                writer.finish().unwrap();
            });
        assert!(std::fs::metadata(path).unwrap().len() > 42);
    }

    #[test]
    fn writer_created_inside_existing_rayon_work_avoids_nested_parallelism() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested.flac");
        rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap()
            .install(|| {
                let writer = FlacStreamWriter::create(&path, 48_000, 2, 16, false).unwrap();
                assert_eq!(writer.parallel_tasks, 1);
                writer.finish().unwrap();
            });
    }
}
