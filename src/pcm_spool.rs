//! Ephemeral planar-f32 storage for exact two-stage normalization.

use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::mem::size_of_val;

pub(crate) struct PcmSpool {
    file: File,
    channels: usize,
    frames: usize,
}

impl PcmSpool {
    pub(crate) fn new(channels: usize) -> Result<Self, String> {
        if channels == 0 {
            return Err("PCM spool requires at least one channel".into());
        }
        Ok(Self {
            file: tempfile::tempfile().map_err(|error| format!("create PCM spool: {error}"))?,
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
        self.file
            .write_all(&frames_u64.to_le_bytes())
            .map_err(|error| format!("write PCM spool record: {error}"))?;
        for channel in planar {
            self.file
                .write_all(samples_as_bytes(channel))
                .map_err(|error| format!("write PCM spool samples: {error}"))?;
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

    pub(crate) fn replay(
        &mut self,
        mut consume: impl FnMut(&mut [Vec<f32>]) -> Result<(), String>,
    ) -> Result<(), String> {
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|error| format!("rewind PCM spool: {error}"))?;
        let mut planar = (0..self.channels).map(|_| Vec::new()).collect::<Vec<_>>();
        let mut replayed_frames = 0usize;
        while let Some(frames) = read_record_frames(&mut self.file)? {
            if frames == 0 {
                return Err("PCM spool contains an empty record".into());
            }
            for channel in &mut planar {
                channel.resize(frames, 0.0);
                self.file
                    .read_exact(samples_as_bytes_mut(channel))
                    .map_err(|error| format!("read PCM spool samples: {error}"))?;
            }
            replayed_frames = replayed_frames
                .checked_add(frames)
                .ok_or_else(|| "PCM spool replay duration overflow".to_string())?;
            consume(&mut planar)?;
        }
        if replayed_frames != self.frames {
            return Err(format!(
                "PCM spool replayed {replayed_frames} frames, expected {}",
                self.frames
            ));
        }
        Ok(())
    }
}

fn read_record_frames(file: &mut File) -> Result<Option<usize>, String> {
    let mut bytes = [0u8; 8];
    let first = file
        .read(&mut bytes)
        .map_err(|error| format!("read PCM spool record: {error}"))?;
    if first == 0 {
        return Ok(None);
    }
    file.read_exact(&mut bytes[first..]).map_err(|error| {
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
        let mut spool = PcmSpool::new(2).unwrap();
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
    fn rejects_changed_channel_geometry() {
        let mut spool = PcmSpool::new(2).unwrap();
        assert!(spool.write_chunk(&[vec![0.0]]).is_err());
        assert!(spool.write_chunk(&[vec![0.0, 1.0], vec![0.0]]).is_err());
    }
}
