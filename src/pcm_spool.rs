//! Ephemeral planar-f32 storage for exact two-stage normalization.

use std::fs::File;
use std::io::{BufReader, BufWriter, ErrorKind, Read, Seek, SeekFrom, Write};
use std::mem::size_of_val;
use std::sync::atomic::{AtomicBool, Ordering};

// Decoder and resampler chunks are commonly only a few KiB. Keep their exact
// record boundaries while amortizing temporary-file syscalls over a bounded
// buffer that remains small relative to one second of multichannel PCM.
const IO_BUFFER_BYTES: usize = 1024 * 1024;
const ESTIMATED_OWNED_CHUNK_BYTES: usize = 64 * 1024;
// Retain the common five-minute stereo delivery in userspace while bounding
// additional process memory. Only one top-level spool may hold this budget;
// nested file-level jobs keep the established temporary-file path.
const MAX_IN_MEMORY_PCM_BYTES: usize = 128 * 1024 * 1024;
static IN_MEMORY_PCM_SPOOL_ACTIVE: AtomicBool = AtomicBool::new(false);

struct MemorySpoolLease {
    tracked: bool,
}

impl MemorySpoolLease {
    fn try_acquire() -> Option<Self> {
        IN_MEMORY_PCM_SPOOL_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self { tracked: true })
    }

    #[cfg(test)]
    fn untracked() -> Self {
        Self { tracked: false }
    }
}

impl Drop for MemorySpoolLease {
    fn drop(&mut self) {
        if self.tracked {
            IN_MEMORY_PCM_SPOOL_ACTIVE.store(false, Ordering::Release);
        }
    }
}

enum PcmSpoolStorage {
    Memory {
        samples: Vec<f32>,
        record_frames: Vec<usize>,
        retained_bytes: usize,
        limit: usize,
        lease: MemorySpoolLease,
    },
    ChunkedMemory {
        chunks: Vec<Vec<Vec<f32>>>,
        retained_bytes: usize,
        limit: usize,
        lease: MemorySpoolLease,
    },
    File(BufWriter<File>),
    Transitioning,
}

pub(crate) struct PcmSpool {
    storage: PcmSpoolStorage,
    channels: usize,
    frames: usize,
}

impl PcmSpool {
    pub(crate) fn new(channels: usize, expected_pcm_bytes: Option<usize>) -> Result<Self, String> {
        Self::new_inner(
            channels,
            expected_pcm_bytes,
            rayon::current_thread_index().is_none(),
            false,
        )
    }

    /// Create storage for a pipeline whose top-level eligibility was checked
    /// before entering a Rayon scope. The scope body itself reports a worker
    /// index even though the caller still owns the single-file operation.
    pub(crate) fn new_for_top_level_pipeline(
        channels: usize,
        expected_pcm_bytes: Option<usize>,
    ) -> Result<Self, String> {
        Self::new_inner(channels, expected_pcm_bytes, true, true)
    }

    fn new_inner(
        channels: usize,
        expected_pcm_bytes: Option<usize>,
        allow_memory: bool,
        prefer_chunked_memory: bool,
    ) -> Result<Self, String> {
        if channels == 0 {
            return Err("PCM spool requires at least one channel".into());
        }
        let memory_capacity = expected_pcm_bytes
            .and_then(|bytes| bytes.checked_add(IO_BUFFER_BYTES))
            .filter(|bytes| *bytes <= MAX_IN_MEMORY_PCM_BYTES);
        let memory = if allow_memory {
            memory_capacity.and_then(|capacity| {
                MemorySpoolLease::try_acquire().and_then(|lease| {
                    if prefer_chunked_memory {
                        chunked_memory_storage(capacity, lease)
                    } else {
                        preallocated_memory_storage(capacity, lease)
                    }
                })
            })
        } else {
            None
        };
        let storage = match memory {
            Some(storage) => storage,
            None => file_storage()?,
        };
        Ok(Self {
            storage,
            channels,
            frames: 0,
        })
    }

    pub(crate) fn write_chunk(&mut self, planar: &[Vec<f32>]) -> Result<(), String> {
        if planar.len() != self.channels {
            return Err(format!(
                "PCM spool expected {} channels, got {}",
                self.channels,
                planar.len()
            ));
        }
        let frames = planar.first().map_or(0, Vec::len);
        if planar.iter().any(|channel| channel.len() != frames) {
            return Err("PCM spool received unequal channel lengths".into());
        }
        if frames == 0 {
            return Ok(());
        }
        if matches!(self.storage, PcmSpoolStorage::ChunkedMemory { .. }) {
            return self
                .write_owned_chunk(planar.to_vec())
                .map(|_| ())
                .map_err(|_| "retain owned PCM spool chunk".to_string());
        }
        let frames_u64 = u64::try_from(frames)
            .map_err(|_| "PCM spool chunk length does not fit its record header".to_string())?;
        // Long and unknown-duration inputs use the established file-backed
        // spool. Keep that hot path as small as the pre-memory-spool
        // implementation: capacity accounting is only useful while a record
        // can still be retained in memory.
        if let PcmSpoolStorage::File(file) = &mut self.storage {
            write_file_record(file, frames_u64, planar)?;
            self.frames = self
                .frames
                .checked_add(frames)
                .ok_or_else(|| "PCM spool duration overflow".to_string())?;
            return Ok(());
        }
        let payload_bytes = planar.iter().try_fold(0usize, |total, channel| {
            total
                .checked_add(size_of_val(channel.as_slice()))
                .ok_or_else(|| "PCM spool record size overflow".to_string())
        })?;
        let record_bytes = size_of_val(&frames_u64)
            .checked_add(payload_bytes)
            .ok_or_else(|| "PCM spool record size overflow".to_string())?;
        let sample_values = frames
            .checked_mul(self.channels)
            .ok_or_else(|| "PCM spool record size overflow".to_string())?;
        self.prepare_record_storage(record_bytes, sample_values)?;
        match &mut self.storage {
            PcmSpoolStorage::Memory {
                samples,
                record_frames,
                ..
            } => {
                record_frames.push(frames);
                for channel in planar {
                    samples.extend_from_slice(channel);
                }
            }
            PcmSpoolStorage::File(file) => {
                write_file_record(file, frames_u64, planar)?;
            }
            PcmSpoolStorage::ChunkedMemory { .. } => {
                unreachable!("chunked writes return before flat storage handling")
            }
            PcmSpoolStorage::Transitioning => {
                return Err("PCM spool storage transition was interrupted".into());
            }
        }
        self.frames = self
            .frames
            .checked_add(frames)
            .ok_or_else(|| "PCM spool duration overflow".to_string())?;
        Ok(())
    }

    /// Retain a top-level analysis chunk without copying its PCM payload. The
    /// returned empty channel set replaces the retained allocation in the
    /// bounded producer pipeline. Other storage modes preserve the established
    /// write-and-recycle behavior.
    pub(crate) fn write_owned_chunk(
        &mut self,
        mut planar: Vec<Vec<f32>>,
    ) -> Result<Vec<Vec<f32>>, Vec<Vec<f32>>> {
        if planar.len() != self.channels {
            return Err(planar);
        }
        let frames = planar.first().map_or(0, Vec::len);
        if frames == 0 || planar.iter().any(|channel| channel.len() != frames) {
            return Err(planar);
        }
        let Some(next_total_frames) = self.frames.checked_add(frames) else {
            return Err(planar);
        };

        if !matches!(self.storage, PcmSpoolStorage::ChunkedMemory { .. }) {
            return match self.write_chunk(&planar) {
                Ok(()) => {
                    for channel in &mut planar {
                        channel.clear();
                    }
                    Ok(planar)
                }
                Err(_) => Err(planar),
            };
        }

        let retained_capacity_bytes =
            planar
                .iter()
                .try_fold(std::mem::size_of::<u64>(), |total, channel| {
                    channel
                        .capacity()
                        .checked_mul(std::mem::size_of::<f32>())
                        .and_then(|bytes| total.checked_add(bytes))
                });
        let Some(retained_capacity_bytes) = retained_capacity_bytes else {
            return Err(planar);
        };
        let retain_in_memory = match &mut self.storage {
            PcmSpoolStorage::ChunkedMemory {
                chunks,
                retained_bytes,
                limit,
                ..
            } => {
                retained_bytes
                    .checked_add(retained_capacity_bytes)
                    .is_some_and(|next| next <= *limit)
                    && chunks.try_reserve(1).is_ok()
            }
            _ => unreachable!("chunked storage checked above"),
        };
        if !retain_in_memory {
            if self.spill_to_file().is_err() {
                return Err(planar);
            }
            let frames_u64 = match u64::try_from(frames) {
                Ok(value) => value,
                Err(_) => return Err(planar),
            };
            let result = match &mut self.storage {
                PcmSpoolStorage::File(file) => write_file_record(file, frames_u64, &planar),
                _ => unreachable!("chunked spill must create file storage"),
            };
            if result.is_err() {
                return Err(planar);
            }
            self.frames = next_total_frames;
            for channel in &mut planar {
                channel.clear();
            }
            return Ok(planar);
        }

        let mut recycled = Vec::new();
        if recycled.try_reserve_exact(self.channels).is_err() {
            return Err(planar);
        }
        recycled.resize_with(self.channels, Vec::new);
        match &mut self.storage {
            PcmSpoolStorage::ChunkedMemory {
                chunks,
                retained_bytes,
                ..
            } => {
                chunks.push(planar);
                *retained_bytes += retained_capacity_bytes;
            }
            _ => unreachable!("chunked storage checked above"),
        }
        self.frames = next_total_frames;
        Ok(recycled)
    }

    pub(crate) fn frames(&self) -> usize {
        self.frames
    }

    pub(crate) fn finish_writing(&mut self) -> Result<(), String> {
        match &mut self.storage {
            PcmSpoolStorage::Memory { .. } | PcmSpoolStorage::ChunkedMemory { .. } => Ok(()),
            PcmSpoolStorage::File(file) => file
                .flush()
                .map_err(|error| format!("flush PCM spool: {error}")),
            PcmSpoolStorage::Transitioning => {
                Err("PCM spool storage transition was interrupted".into())
            }
        }
    }

    fn prepare_record_storage(
        &mut self,
        record_bytes: usize,
        sample_values: usize,
    ) -> Result<(), String> {
        let spill = match &mut self.storage {
            PcmSpoolStorage::Memory {
                samples,
                record_frames,
                retained_bytes,
                limit,
                ..
            } => {
                let retained = retained_bytes
                    .checked_add(record_bytes)
                    .ok_or_else(|| "PCM spool size overflow".to_string())?;
                let spill = retained > *limit
                    || samples.try_reserve(sample_values).is_err()
                    || record_frames.try_reserve(1).is_err();
                if !spill {
                    *retained_bytes = retained;
                }
                spill
            }
            PcmSpoolStorage::File(_) => false,
            PcmSpoolStorage::ChunkedMemory { .. } => {
                unreachable!("borrowed chunk writes are routed through owned storage")
            }
            PcmSpoolStorage::Transitioning => {
                return Err("PCM spool storage transition was interrupted".into());
            }
        };
        if spill {
            self.spill_to_file()?;
        }
        Ok(())
    }

    fn spill_to_file(&mut self) -> Result<(), String> {
        let storage = std::mem::replace(&mut self.storage, PcmSpoolStorage::Transitioning);
        match storage {
            PcmSpoolStorage::Memory {
                samples,
                record_frames,
                retained_bytes,
                limit,
                lease,
            } => {
                let file = match tempfile::tempfile() {
                    Ok(file) => file,
                    Err(error) => {
                        self.storage = PcmSpoolStorage::Memory {
                            samples,
                            record_frames,
                            retained_bytes,
                            limit,
                            lease,
                        };
                        return Err(format!("create PCM spool: {error}"));
                    }
                };
                let mut file = BufWriter::with_capacity(IO_BUFFER_BYTES, file);
                if let Err(error) =
                    write_memory_records(&mut file, &samples, &record_frames, self.channels)
                {
                    self.storage = PcmSpoolStorage::Memory {
                        samples,
                        record_frames,
                        retained_bytes,
                        limit,
                        lease,
                    };
                    return Err(error);
                }
                self.storage = PcmSpoolStorage::File(file);
                drop(lease);
                Ok(())
            }
            PcmSpoolStorage::ChunkedMemory {
                chunks,
                retained_bytes,
                limit,
                lease,
            } => {
                let file = match tempfile::tempfile() {
                    Ok(file) => file,
                    Err(error) => {
                        self.storage = PcmSpoolStorage::ChunkedMemory {
                            chunks,
                            retained_bytes,
                            limit,
                            lease,
                        };
                        return Err(format!("create PCM spool: {error}"));
                    }
                };
                let mut file = BufWriter::with_capacity(IO_BUFFER_BYTES, file);
                if let Err(error) = write_chunked_memory_records(&mut file, &chunks, self.channels)
                {
                    self.storage = PcmSpoolStorage::ChunkedMemory {
                        chunks,
                        retained_bytes,
                        limit,
                        lease,
                    };
                    return Err(error);
                }
                self.storage = PcmSpoolStorage::File(file);
                drop(lease);
                Ok(())
            }
            storage => {
                self.storage = storage;
                Ok(())
            }
        }
    }

    #[cfg(test)]
    fn in_memory_for_test(channels: usize, limit: usize) -> Self {
        assert_ne!(channels, 0);
        Self {
            storage: PcmSpoolStorage::Memory {
                samples: Vec::new(),
                record_frames: Vec::new(),
                retained_bytes: 0,
                limit,
                lease: MemorySpoolLease::untracked(),
            },
            channels,
            frames: 0,
        }
    }

    #[cfg(test)]
    fn chunked_in_memory_for_test(channels: usize, limit: usize) -> Self {
        assert_ne!(channels, 0);
        Self {
            storage: PcmSpoolStorage::ChunkedMemory {
                chunks: Vec::new(),
                retained_bytes: 0,
                limit,
                lease: MemorySpoolLease::untracked(),
            },
            channels,
            frames: 0,
        }
    }

    #[cfg(test)]
    fn is_in_memory(&self) -> bool {
        matches!(self.storage, PcmSpoolStorage::Memory { .. })
    }

    pub(crate) fn replay(
        &mut self,
        mut consume: impl FnMut(&mut [Vec<f32>]) -> Result<(), String>,
    ) -> Result<(), String> {
        let channels = self.channels;
        let frames = self.frames;
        match &mut self.storage {
            PcmSpoolStorage::Memory {
                samples,
                record_frames,
                ..
            } => replay_memory_records(samples, record_frames, channels, frames, &mut consume),
            PcmSpoolStorage::ChunkedMemory { chunks, .. } => {
                validate_chunked_memory_geometry(chunks, channels, frames)?;
                for chunk in chunks {
                    consume(chunk)?;
                }
                Ok(())
            }
            PcmSpoolStorage::File(file) => {
                file.seek(SeekFrom::Start(0))
                    .map_err(|error| format!("rewind PCM spool: {error}"))?;
                let mut reader = BufReader::with_capacity(IO_BUFFER_BYTES, file.get_mut());
                replay_records(&mut reader, channels, frames, &mut consume)
            }
            PcmSpoolStorage::Transitioning => {
                Err("PCM spool storage transition was interrupted".into())
            }
        }
    }

    pub(crate) fn can_replay_borrowed(&self) -> bool {
        matches!(
            &self.storage,
            PcmSpoolStorage::Memory { .. } | PcmSpoolStorage::ChunkedMemory { .. }
        )
    }

    /// Hand immutable views of the retained channel planes directly to a
    /// consumer. File-backed spools retain the established reusable copy path.
    pub(crate) fn replay_borrowed(
        &self,
        mut consume: impl FnMut(&[&[f32]]) -> Result<(), String>,
    ) -> Result<(), String> {
        let channels = self.channels;
        let expected_frames = self.frames;
        match &self.storage {
            PcmSpoolStorage::Memory {
                samples,
                record_frames,
                ..
            } => {
                validate_memory_geometry(samples, record_frames, channels, expected_frames)?;
                let mut planar = Vec::with_capacity(channels);
                let mut remaining = samples.as_slice();
                for &frames in record_frames.iter() {
                    let record_values = frames
                        .checked_mul(channels)
                        .ok_or_else(|| "PCM spool record size overflow".to_string())?;
                    let (record, tail) = remaining.split_at(record_values);
                    planar.clear();
                    planar.extend(record.chunks_exact(frames));
                    debug_assert_eq!(planar.len(), channels);
                    consume(&planar)?;
                    remaining = tail;
                }
                debug_assert!(remaining.is_empty());
            }
            PcmSpoolStorage::ChunkedMemory { chunks, .. } => {
                validate_chunked_memory_geometry(chunks, channels, expected_frames)?;
                let mut planar = Vec::with_capacity(channels);
                for chunk in chunks {
                    planar.clear();
                    planar.extend(chunk.iter().map(Vec::as_slice));
                    consume(&planar)?;
                }
            }
            _ => return Err("PCM spool is not retained in memory".into()),
        }
        Ok(())
    }

    /// Replay by handing each channel allocation to the consumer and accepting
    /// a recycled set for the next record. This avoids a PCM copy when replay is
    /// connected to a bounded writer pipeline.
    pub(crate) fn replay_owned(
        &mut self,
        mut consume: impl FnMut(Vec<Vec<f32>>) -> Result<Vec<Vec<f32>>, String>,
    ) -> Result<(), String> {
        if let PcmSpoolStorage::ChunkedMemory { chunks, .. } = &mut self.storage {
            validate_chunked_memory_geometry(chunks, self.channels, self.frames)?;
            for chunk in std::mem::take(chunks) {
                let _ = consume(chunk)?;
            }
            return Ok(());
        }
        let mut handoff = Vec::new();
        let channels = self.channels;
        self.replay(|planar| {
            handoff.reserve(planar.len());
            for channel in planar.iter_mut() {
                handoff.push(std::mem::take(channel));
            }
            let mut recycled = consume(std::mem::take(&mut handoff))?;
            if recycled.len() != channels {
                return Err(format!(
                    "PCM spool consumer returned {} channels, expected {channels}",
                    recycled.len()
                ));
            }
            for (slot, channel) in planar.iter_mut().zip(recycled.drain(..)) {
                *slot = channel;
            }
            handoff = recycled;
            Ok(())
        })
    }
}

fn preallocated_memory_storage(
    capacity_bytes: usize,
    lease: MemorySpoolLease,
) -> Option<PcmSpoolStorage> {
    let mut samples = Vec::new();
    samples
        .try_reserve_exact(capacity_bytes.div_ceil(std::mem::size_of::<f32>()))
        .ok()?;
    Some(PcmSpoolStorage::Memory {
        samples,
        record_frames: Vec::new(),
        retained_bytes: 0,
        limit: MAX_IN_MEMORY_PCM_BYTES,
        lease,
    })
}

fn chunked_memory_storage(
    capacity_bytes: usize,
    lease: MemorySpoolLease,
) -> Option<PcmSpoolStorage> {
    let mut chunks = Vec::new();
    chunks
        .try_reserve_exact(capacity_bytes.div_ceil(ESTIMATED_OWNED_CHUNK_BYTES))
        .ok()?;
    Some(PcmSpoolStorage::ChunkedMemory {
        chunks,
        retained_bytes: 0,
        limit: MAX_IN_MEMORY_PCM_BYTES,
        lease,
    })
}

fn file_storage() -> Result<PcmSpoolStorage, String> {
    let file = tempfile::tempfile().map_err(|error| format!("create PCM spool: {error}"))?;
    Ok(PcmSpoolStorage::File(BufWriter::with_capacity(
        IO_BUFFER_BYTES,
        file,
    )))
}

fn write_memory_records(
    writer: &mut impl Write,
    samples: &[f32],
    record_frames: &[usize],
    channels: usize,
) -> Result<(), String> {
    let mut offset = 0usize;
    for &frames in record_frames {
        if frames == 0 {
            return Err("PCM spool contains an empty record".into());
        }
        let record_values = frames
            .checked_mul(channels)
            .ok_or_else(|| "PCM spool record size overflow".to_string())?;
        let end = offset
            .checked_add(record_values)
            .filter(|end| *end <= samples.len())
            .ok_or_else(|| "PCM spool memory geometry is inconsistent".to_string())?;
        let frames_u64 = u64::try_from(frames)
            .map_err(|_| "PCM spool chunk length does not fit its record header".to_string())?;
        writer
            .write_all(&frames_u64.to_le_bytes())
            .map_err(|error| format!("spill PCM spool record: {error}"))?;
        for channel in samples[offset..end].chunks_exact(frames) {
            writer
                .write_all(samples_as_bytes(channel))
                .map_err(|error| format!("spill PCM spool samples: {error}"))?;
        }
        offset = end;
    }
    if offset != samples.len() {
        return Err("PCM spool memory geometry is inconsistent".into());
    }
    Ok(())
}

fn write_chunked_memory_records(
    writer: &mut impl Write,
    chunks: &[Vec<Vec<f32>>],
    channels: usize,
) -> Result<(), String> {
    for chunk in chunks {
        if chunk.len() != channels {
            return Err("PCM spool memory geometry is inconsistent".into());
        }
        let frames = chunk.first().map_or(0, Vec::len);
        if frames == 0 || chunk.iter().any(|channel| channel.len() != frames) {
            return Err("PCM spool memory geometry is inconsistent".into());
        }
        let frames_u64 = u64::try_from(frames)
            .map_err(|_| "PCM spool chunk length does not fit its record header".to_string())?;
        write_file_record(writer, frames_u64, chunk)?;
    }
    Ok(())
}

fn validate_memory_geometry(
    samples: &[f32],
    record_frames: &[usize],
    channels: usize,
    expected_frames: usize,
) -> Result<(), String> {
    let mut sample_values = 0usize;
    let mut replayed_frames = 0usize;
    for &frames in record_frames {
        if frames == 0 {
            return Err("PCM spool contains an empty record".into());
        }
        sample_values = sample_values
            .checked_add(
                frames
                    .checked_mul(channels)
                    .ok_or_else(|| "PCM spool record size overflow".to_string())?,
            )
            .ok_or_else(|| "PCM spool size overflow".to_string())?;
        replayed_frames = replayed_frames
            .checked_add(frames)
            .ok_or_else(|| "PCM spool replay duration overflow".to_string())?;
    }
    if sample_values != samples.len() {
        return Err("PCM spool memory geometry is inconsistent".into());
    }
    if replayed_frames != expected_frames {
        return Err(format!(
            "PCM spool replayed {replayed_frames} frames, expected {expected_frames}"
        ));
    }
    Ok(())
}

fn validate_chunked_memory_geometry(
    chunks: &[Vec<Vec<f32>>],
    channels: usize,
    expected_frames: usize,
) -> Result<(), String> {
    let mut replayed_frames = 0usize;
    for chunk in chunks {
        if chunk.len() != channels {
            return Err("PCM spool memory geometry is inconsistent".into());
        }
        let frames = chunk.first().map_or(0, Vec::len);
        if frames == 0 || chunk.iter().any(|channel| channel.len() != frames) {
            return Err("PCM spool memory geometry is inconsistent".into());
        }
        replayed_frames = replayed_frames
            .checked_add(frames)
            .ok_or_else(|| "PCM spool replay duration overflow".to_string())?;
    }
    if replayed_frames != expected_frames {
        return Err(format!(
            "PCM spool replayed {replayed_frames} frames, expected {expected_frames}"
        ));
    }
    Ok(())
}

fn replay_memory_records(
    samples: &[f32],
    record_frames: &[usize],
    channels: usize,
    expected_frames: usize,
    consume: &mut impl FnMut(&mut [Vec<f32>]) -> Result<(), String>,
) -> Result<(), String> {
    validate_memory_geometry(samples, record_frames, channels, expected_frames)?;
    let mut planar = (0..channels).map(|_| Vec::new()).collect::<Vec<_>>();
    let mut offset = 0usize;
    for &frames in record_frames {
        let record_values = frames * channels;
        let end = offset + record_values;
        for (destination, source) in planar
            .iter_mut()
            .zip(samples[offset..end].chunks_exact(frames))
        {
            destination.clear();
            destination.extend_from_slice(source);
        }
        consume(&mut planar)?;
        offset = end;
    }
    Ok(())
}

fn replay_records(
    reader: &mut impl Read,
    channels: usize,
    expected_frames: usize,
    consume: &mut impl FnMut(&mut [Vec<f32>]) -> Result<(), String>,
) -> Result<(), String> {
    let mut planar = (0..channels).map(|_| Vec::new()).collect::<Vec<_>>();
    let mut replayed_frames = 0usize;
    while let Some(frames) = read_record_frames(reader)? {
        if frames == 0 {
            return Err("PCM spool contains an empty record".into());
        }
        for channel in &mut planar {
            channel.resize(frames, 0.0);
            reader
                .read_exact(samples_as_bytes_mut(channel))
                .map_err(|error| format!("read PCM spool samples: {error}"))?;
        }
        replayed_frames = replayed_frames
            .checked_add(frames)
            .ok_or_else(|| "PCM spool replay duration overflow".to_string())?;
        consume(&mut planar)?;
    }
    if replayed_frames != expected_frames {
        return Err(format!(
            "PCM spool replayed {replayed_frames} frames, expected {expected_frames}"
        ));
    }
    Ok(())
}

#[inline]
fn write_file_record(
    file: &mut impl Write,
    frames: u64,
    planar: &[Vec<f32>],
) -> Result<(), String> {
    file.write_all(&frames.to_le_bytes())
        .map_err(|error| format!("write PCM spool record: {error}"))?;
    for channel in planar {
        file.write_all(samples_as_bytes(channel))
            .map_err(|error| format!("write PCM spool samples: {error}"))?;
    }
    Ok(())
}

fn read_record_frames(reader: &mut impl Read) -> Result<Option<usize>, String> {
    let mut bytes = [0u8; 8];
    let first = reader
        .read(&mut bytes)
        .map_err(|error| format!("read PCM spool record: {error}"))?;
    if first == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut bytes[first..]).map_err(|error| {
        if error.kind() == ErrorKind::UnexpectedEof {
            "PCM spool ended inside a record header".to_string()
        } else {
            format!("read PCM spool record: {error}")
        }
    })?;
    usize::try_from(u64::from_le_bytes(bytes))
        .map(Some)
        .map_err(|_| "PCM spool record length is too large for this platform".to_string())
}

#[inline]
fn samples_as_bytes(samples: &[f32]) -> &[u8] {
    // SAFETY: `u8` has alignment one and every initialized byte of an `f32`
    // may be observed. The returned slice cannot outlive `samples`.
    unsafe { std::slice::from_raw_parts(samples.as_ptr().cast(), size_of_val(samples)) }
}

#[inline]
fn samples_as_bytes_mut(samples: &mut [f32]) -> &mut [u8] {
    // SAFETY: every 32-bit pattern is a valid `f32` representation, the vector
    // already owns initialized storage, and the byte slice has the same unique
    // mutable lifetime as `samples`.
    unsafe { std::slice::from_raw_parts_mut(samples.as_mut_ptr().cast(), size_of_val(samples)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_memory_spool_reserves_once_before_capture() {
        let capacity = IO_BUFFER_BYTES + 4096;
        let storage = preallocated_memory_storage(capacity, MemorySpoolLease::untracked())
            .expect("small exact reservation should succeed");
        match storage {
            PcmSpoolStorage::Memory {
                samples,
                record_frames,
                ..
            } => {
                assert_eq!(samples.len(), 0);
                assert!(samples.capacity() * std::mem::size_of::<f32>() >= capacity);
                assert!(record_frames.is_empty());
            }
            _ => panic!("preallocation should produce memory storage"),
        }
    }

    #[test]
    fn chunked_memory_spool_retains_and_replays_owned_allocations() {
        let mut left = Vec::with_capacity(16);
        left.extend([0.25_f32, -0.5, 0.75]);
        let mut right = Vec::with_capacity(16);
        right.extend([-0.125_f32, 0.375, -0.625]);
        let left_pointer = left.as_ptr();
        let right_pointer = right.as_ptr();
        let mut spool = PcmSpool::chunked_in_memory_for_test(2, 4096);

        let recycled = spool.write_owned_chunk(vec![left, right]).unwrap();
        assert_eq!(recycled.len(), 2);
        assert!(recycled.iter().all(Vec::is_empty));
        assert!(recycled.iter().all(|channel| channel.capacity() == 0));
        assert_eq!(spool.frames(), 3);

        let mut borrowed_calls = 0;
        spool
            .replay_borrowed(|planar| {
                borrowed_calls += 1;
                assert_eq!(planar[0].as_ptr(), left_pointer);
                assert_eq!(planar[1].as_ptr(), right_pointer);
                assert_eq!(planar[0], [0.25, -0.5, 0.75]);
                assert_eq!(planar[1], [-0.125, 0.375, -0.625]);
                Ok(())
            })
            .unwrap();
        assert_eq!(borrowed_calls, 1);

        let mut owned_calls = 0;
        let mut recycled = Some(recycled);
        spool
            .replay_owned(|chunk| {
                owned_calls += 1;
                assert_eq!(chunk[0].as_ptr(), left_pointer);
                assert_eq!(chunk[1].as_ptr(), right_pointer);
                Ok(recycled.take().unwrap())
            })
            .unwrap();
        assert_eq!(owned_calls, 1);
    }

    #[test]
    fn chunked_memory_spool_spills_owned_records_to_the_exact_file_format() {
        let first = vec![vec![0.25_f32, -0.5, 0.75], vec![-0.25, 0.5, -0.75]];
        let second = vec![vec![1.0_f32], vec![-1.0]];
        let first_capacity_bytes = size_of::<u64>()
            + first
                .iter()
                .map(|channel| channel.capacity() * size_of::<f32>())
                .sum::<usize>();
        let expected = [first.clone(), second.clone()]
            .into_iter()
            .map(|planar| {
                planar
                    .iter()
                    .map(|channel| channel.iter().map(|sample| sample.to_bits()).collect())
                    .collect::<Vec<Vec<u32>>>()
            })
            .collect::<Vec<_>>();
        let mut spool = PcmSpool::chunked_in_memory_for_test(2, first_capacity_bytes);

        let first_recycled = spool.write_owned_chunk(first).unwrap();
        assert!(matches!(
            spool.storage,
            PcmSpoolStorage::ChunkedMemory { .. }
        ));
        assert!(first_recycled.iter().all(|channel| channel.capacity() == 0));
        let second_recycled = spool.write_owned_chunk(second).unwrap();
        assert!(matches!(spool.storage, PcmSpoolStorage::File(_)));
        assert!(second_recycled.iter().all(Vec::is_empty));
        spool.finish_writing().unwrap();

        for _ in 0..2 {
            let mut replayed = Vec::new();
            spool
                .replay(|planar| {
                    replayed.push(
                        planar
                            .iter()
                            .map(|channel| channel.iter().map(|sample| sample.to_bits()).collect())
                            .collect::<Vec<Vec<u32>>>(),
                    );
                    Ok(())
                })
                .unwrap();
            assert_eq!(replayed, expected);
        }
    }

    #[test]
    fn replay_preserves_chunks_and_float_bits_across_rewinds() {
        let chunks = [
            vec![
                vec![0.0, -0.0, f32::from_bits(0x7fc0_1234)],
                vec![1.0, -1.0, 0.25],
            ],
            vec![vec![0.5, -0.5], vec![0.125, -0.125]],
        ];
        let mut spool = PcmSpool::new(2, Some(40)).unwrap();
        for chunk in &chunks {
            spool.write_chunk(chunk).unwrap();
        }
        assert_eq!(spool.frames(), 5);

        for _ in 0..2 {
            let mut replayed = Vec::new();
            spool
                .replay(|chunk| {
                    replayed.push(
                        chunk
                            .iter()
                            .map(|channel| channel.iter().map(|sample| sample.to_bits()).collect())
                            .collect::<Vec<Vec<u32>>>(),
                    );
                    for channel in chunk {
                        channel.fill(42.0);
                    }
                    Ok(())
                })
                .unwrap();
            let expected = chunks
                .iter()
                .map(|chunk| {
                    chunk
                        .iter()
                        .map(|channel| channel.iter().map(|sample| sample.to_bits()).collect())
                        .collect::<Vec<Vec<u32>>>()
                })
                .collect::<Vec<_>>();
            assert_eq!(replayed, expected);
        }
    }

    #[test]
    fn owned_replay_refills_recycled_channel_allocations() {
        let chunks = [
            vec![vec![0.25, -0.5, 0.75], vec![-0.25, 0.5, -0.75]],
            vec![vec![1.0, -1.0], vec![0.125, -0.125]],
        ];
        let mut spool = PcmSpool::new(2, Some(40)).unwrap();
        for chunk in &chunks {
            spool.write_chunk(chunk).unwrap();
        }

        let replacement = vec![Vec::with_capacity(8), Vec::with_capacity(8)];
        let replacement_pointers = [replacement[0].as_ptr(), replacement[1].as_ptr()];
        let mut replacement = Some(replacement);
        let mut observed = Vec::new();
        spool
            .replay_owned(|chunk| {
                observed.push(
                    chunk
                        .iter()
                        .map(|channel| channel.iter().map(|sample| sample.to_bits()).collect())
                        .collect::<Vec<Vec<u32>>>(),
                );
                if observed.len() == 1 {
                    Ok(replacement.take().unwrap())
                } else {
                    assert_eq!(chunk[0].as_ptr(), replacement_pointers[0]);
                    assert_eq!(chunk[1].as_ptr(), replacement_pointers[1]);
                    Ok(chunk)
                }
            })
            .unwrap();
        let expected = chunks
            .iter()
            .map(|chunk| {
                chunk
                    .iter()
                    .map(|channel| channel.iter().map(|sample| sample.to_bits()).collect())
                    .collect::<Vec<Vec<u32>>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(observed, expected);

        let error = spool.replay_owned(|_| Ok(vec![Vec::new()])).unwrap_err();
        assert!(error.contains("returned 1 channels, expected 2"));
    }

    #[test]
    fn borrowed_replay_exposes_exact_records_without_consuming_them() {
        let chunks = [
            vec![vec![0.25, -0.5, 0.75], vec![-0.25, 0.5, -0.75]],
            vec![vec![1.0, -1.0], vec![0.125, -0.125]],
        ];
        let mut spool = PcmSpool::in_memory_for_test(2, 1024);
        for chunk in &chunks {
            spool.write_chunk(chunk).unwrap();
        }
        let expected = chunks
            .iter()
            .map(|chunk| {
                chunk
                    .iter()
                    .map(|channel| channel.iter().map(|sample| sample.to_bits()).collect())
                    .collect::<Vec<Vec<u32>>>()
            })
            .collect::<Vec<_>>();
        assert!(spool.can_replay_borrowed());
        for _ in 0..2 {
            let mut observed = Vec::new();
            spool
                .replay_borrowed(|planar| {
                    observed.push(
                        planar
                            .iter()
                            .map(|channel| channel.iter().map(|sample| sample.to_bits()).collect())
                            .collect::<Vec<Vec<u32>>>(),
                    );
                    Ok(())
                })
                .unwrap();
            assert_eq!(observed, expected);
        }
        spool.replay(|_| Ok(())).unwrap();
    }

    #[test]
    fn replay_preserves_records_larger_than_the_io_buffer() {
        let frames = IO_BUFFER_BYTES / size_of::<f32>() + 137;
        let left = (0..frames)
            .map(|frame| f32::from_bits(frame as u32 ^ 0x3f00_0000))
            .collect::<Vec<_>>();
        let right = (0..frames)
            .map(|frame| f32::from_bits(frame as u32 ^ 0xbf00_0000))
            .collect::<Vec<_>>();
        let expected = [left, right];
        let mut spool = PcmSpool::new(2, Some(frames * 2 * size_of::<f32>())).unwrap();
        spool.write_chunk(&expected).unwrap();

        spool
            .replay(|actual| {
                for (actual, expected) in actual.iter().zip(&expected) {
                    assert_eq!(
                        actual
                            .iter()
                            .map(|sample| sample.to_bits())
                            .collect::<Vec<_>>(),
                        expected
                            .iter()
                            .map(|sample| sample.to_bits())
                            .collect::<Vec<_>>()
                    );
                }
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn bounded_memory_spool_spills_to_the_exact_file_format() {
        let first = vec![vec![0.25, -0.5, 0.75], vec![-0.25, 0.5, -0.75]];
        let second = vec![vec![1.0], vec![-1.0]];
        let mut spool = PcmSpool::in_memory_for_test(2, 40);
        spool.write_chunk(&first).unwrap();
        assert!(spool.is_in_memory());
        spool.write_chunk(&second).unwrap();
        assert!(!spool.is_in_memory());
        spool.finish_writing().unwrap();

        for _ in 0..2 {
            let mut replayed = Vec::new();
            spool
                .replay(|planar| {
                    replayed.push(
                        planar
                            .iter()
                            .map(|channel| channel.iter().map(|sample| sample.to_bits()).collect())
                            .collect::<Vec<Vec<u32>>>(),
                    );
                    Ok(())
                })
                .unwrap();
            let expected = [&first, &second]
                .into_iter()
                .map(|planar| {
                    planar
                        .iter()
                        .map(|channel| channel.iter().map(|sample| sample.to_bits()).collect())
                        .collect::<Vec<Vec<u32>>>()
                })
                .collect::<Vec<_>>();
            assert_eq!(replayed, expected);
        }
    }

    #[test]
    fn rejects_changed_channel_geometry() {
        let mut spool = PcmSpool::new(2, None).unwrap();
        assert!(spool.write_chunk(&[vec![0.0]]).is_err());
        assert!(spool.write_chunk(&[vec![0.0, 1.0], vec![0.0]]).is_err());
    }
}
