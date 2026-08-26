//! Ephemeral planar-f32 storage for exact two-stage normalization.

use std::fs::File;
use std::io::{BufReader, BufWriter, Cursor, ErrorKind, Read, Seek, SeekFrom, Write};
use std::mem::size_of_val;
use std::sync::atomic::{AtomicBool, Ordering};

// Decoder and resampler chunks are commonly only a few KiB. Keep their exact
// record boundaries while amortizing temporary-file syscalls over a bounded
// buffer that remains small relative to one second of multichannel PCM.
const IO_BUFFER_BYTES: usize = 1024 * 1024;
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
        bytes: Vec<u8>,
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
        if channels == 0 {
            return Err("PCM spool requires at least one channel".into());
        }
        let fits_memory_budget = expected_pcm_bytes
            .and_then(|bytes| bytes.checked_add(IO_BUFFER_BYTES))
            .is_some_and(|bytes| bytes <= MAX_IN_MEMORY_PCM_BYTES);
        let memory = if fits_memory_budget && rayon::current_thread_index().is_none() {
            MemorySpoolLease::try_acquire().map(|lease| PcmSpoolStorage::Memory {
                bytes: Vec::new(),
                limit: MAX_IN_MEMORY_PCM_BYTES,
                lease,
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
        let frames_u64 = u64::try_from(frames)
            .map_err(|_| "PCM spool chunk length does not fit its record header".to_string())?;
        let payload_bytes = planar.iter().try_fold(0usize, |total, channel| {
            total
                .checked_add(size_of_val(channel.as_slice()))
                .ok_or_else(|| "PCM spool record size overflow".to_string())
        })?;
        let record_bytes = size_of_val(&frames_u64)
            .checked_add(payload_bytes)
            .ok_or_else(|| "PCM spool record size overflow".to_string())?;
        self.prepare_record_storage(record_bytes)?;
        match &mut self.storage {
            PcmSpoolStorage::Memory { bytes, .. } => {
                bytes.extend_from_slice(&frames_u64.to_le_bytes());
                for channel in planar {
                    bytes.extend_from_slice(samples_as_bytes(channel));
                }
            }
            PcmSpoolStorage::File(file) => {
                file.write_all(&frames_u64.to_le_bytes())
                    .map_err(|error| format!("write PCM spool record: {error}"))?;
                for channel in planar {
                    file.write_all(samples_as_bytes(channel))
                        .map_err(|error| format!("write PCM spool samples: {error}"))?;
                }
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

    pub(crate) fn frames(&self) -> usize {
        self.frames
    }

    pub(crate) fn finish_writing(&mut self) -> Result<(), String> {
        match &mut self.storage {
            PcmSpoolStorage::Memory { .. } => Ok(()),
            PcmSpoolStorage::File(file) => file
                .flush()
                .map_err(|error| format!("flush PCM spool: {error}")),
            PcmSpoolStorage::Transitioning => {
                Err("PCM spool storage transition was interrupted".into())
            }
        }
    }

    fn prepare_record_storage(&mut self, record_bytes: usize) -> Result<(), String> {
        let spill = match &mut self.storage {
            PcmSpoolStorage::Memory { bytes, limit, .. } => {
                let retained = bytes
                    .len()
                    .checked_add(record_bytes)
                    .ok_or_else(|| "PCM spool size overflow".to_string())?;
                retained > *limit || bytes.try_reserve(record_bytes).is_err()
            }
            PcmSpoolStorage::File(_) => false,
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
        let (bytes, limit, lease) = match storage {
            PcmSpoolStorage::Memory {
                bytes,
                limit,
                lease,
            } => (bytes, limit, lease),
            storage => {
                self.storage = storage;
                return Ok(());
            }
        };
        let file = match tempfile::tempfile() {
            Ok(file) => file,
            Err(error) => {
                self.storage = PcmSpoolStorage::Memory {
                    bytes,
                    limit,
                    lease,
                };
                return Err(format!("create PCM spool: {error}"));
            }
        };
        let mut file = BufWriter::with_capacity(IO_BUFFER_BYTES, file);
        if let Err(error) = file.write_all(&bytes) {
            self.storage = PcmSpoolStorage::Memory {
                bytes,
                limit,
                lease,
            };
            return Err(format!("spill PCM spool: {error}"));
        }
        self.storage = PcmSpoolStorage::File(file);
        drop(lease);
        Ok(())
    }

    #[cfg(test)]
    fn in_memory_for_test(channels: usize, limit: usize) -> Self {
        assert_ne!(channels, 0);
        Self {
            storage: PcmSpoolStorage::Memory {
                bytes: Vec::new(),
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
            PcmSpoolStorage::Memory { bytes, .. } => {
                let mut reader = Cursor::new(bytes.as_slice());
                replay_records(&mut reader, channels, frames, &mut consume)
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

    /// Replay by handing each channel allocation to the consumer and accepting
    /// a recycled set for the next record. This avoids a PCM copy when replay is
    /// connected to a bounded writer pipeline.
    pub(crate) fn replay_owned(
        &mut self,
        mut consume: impl FnMut(Vec<Vec<f32>>) -> Result<Vec<Vec<f32>>, String>,
    ) -> Result<(), String> {
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

fn file_storage() -> Result<PcmSpoolStorage, String> {
    let file = tempfile::tempfile().map_err(|error| format!("create PCM spool: {error}"))?;
    Ok(PcmSpoolStorage::File(BufWriter::with_capacity(
        IO_BUFFER_BYTES,
        file,
    )))
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
