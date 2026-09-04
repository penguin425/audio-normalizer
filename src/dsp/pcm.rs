//! Transactional preflight for typed PCM chunks.

pub(crate) struct PlanarChunkMessages<'a> {
    pub(crate) channel_count: &'a str,
    pub(crate) channel_length: &'a str,
    pub(crate) frame_overflow: &'a str,
}

/// Validate shape and samples without mutating any caller-owned DSP state.
pub(crate) fn validate_planar_chunk<T, C>(
    planar: &[C],
    expected_channels: usize,
    consumed_frames: usize,
    invalid_sample: impl Fn(&T) -> bool,
    invalid_label: &str,
    messages: PlanarChunkMessages<'_>,
) -> Result<usize, String>
where
    C: AsRef<[T]>,
{
    if planar.len() != expected_channels {
        return Err(messages.channel_count.into());
    }
    let chunk_frames = planar.first().map_or(0, |channel| channel.as_ref().len());
    if planar
        .iter()
        .any(|channel| channel.as_ref().len() != chunk_frames)
    {
        return Err(messages.channel_length.into());
    }
    consumed_frames
        .checked_add(chunk_frames)
        .ok_or_else(|| messages.frame_overflow.to_string())?;
    let first_invalid = planar
        .iter()
        .enumerate()
        .filter_map(|(channel, samples)| {
            samples
                .as_ref()
                .iter()
                .position(&invalid_sample)
                .map(|frame| (frame, channel))
        })
        .min();
    if let Some((frame, channel)) = first_invalid {
        return Err(format!(
            "{invalid_label} at frame {}, channel {channel}",
            consumed_frames + frame
        ));
    }
    Ok(chunk_frames)
}

/// Validate a complete interleaved chunk before a live processor advances.
pub(crate) fn validate_interleaved_chunk<T>(
    samples: &[T],
    channels: usize,
    consumed_frames: usize,
    invalid_sample: impl Fn(&T) -> bool,
    invalid_label: &str,
    alignment_error: &str,
) -> Result<usize, String> {
    if channels == 0 || !samples.len().is_multiple_of(channels) {
        return Err(alignment_error.into());
    }
    let chunk_frames = samples.len() / channels;
    consumed_frames
        .checked_add(chunk_frames)
        .ok_or_else(|| "stream frame count overflow".to_string())?;
    if let Some(index) = samples.iter().position(invalid_sample) {
        return Err(format!(
            "{invalid_label} at frame {}, channel {}",
            consumed_frames + index / channels,
            index % channels
        ));
    }
    Ok(chunk_frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planar_diagnostic_is_frame_then_channel_ordered() {
        let planar = [vec![0.0, f32::INFINITY], vec![f32::NAN, 0.0]];
        let error = validate_planar_chunk(
            &planar,
            2,
            9,
            |sample| !sample.is_finite(),
            "non-finite sample",
            PlanarChunkMessages {
                channel_count: "channels",
                channel_length: "length",
                frame_overflow: "overflow",
            },
        )
        .unwrap_err();
        assert_eq!(error, "non-finite sample at frame 9, channel 1");
    }
}
