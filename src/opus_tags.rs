//! Dependency-free RFC 7845 loudness metadata for existing Ogg Opus files.
//!
//! This module intentionally does not depend on libopus. Metadata-only
//! normalization must remain available when Opus encoding is not compiled in.

use ogg::{PacketReader, PacketWriteEndInfo, PacketWriter};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

pub type R128Tags = (Option<i16>, Option<i16>);
const MAX_OPUS_CHAINS: usize = 1_024;

/// Build an `OpusTags` packet containing RFC 7845 loudness comments.
#[cfg(any(feature = "opus-encoding", test))]
pub(crate) fn build_opus_tags(track_lufs: f64, album_lufs: Option<f64>) -> Vec<u8> {
    let vendor = b"Forge audio normalizer";
    let mut comments = Vec::new();
    if let Some(track_gain) = r128_gain(track_lufs) {
        comments.push(format!("R128_TRACK_GAIN={track_gain}"));
    }
    if let Some(album_gain) = album_lufs.and_then(r128_gain) {
        comments.push(format!("R128_ALBUM_GAIN={album_gain}"));
    }
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for comment in comments {
        tags.extend_from_slice(&(comment.len() as u32).to_le_bytes());
        tags.extend_from_slice(comment.as_bytes());
    }
    tags
}

/// Read the first logical stream's RFC 7845 track and album gains.
pub fn read_r128_tags(path: &Path) -> Result<R128Tags, String> {
    read_all_r128_tags(path)?
        .into_iter()
        .next()
        .ok_or_else(|| format!("{}: missing OpusTags", path.display()))
}

/// Rewrite every sequential Opus logical stream and verify the persisted tags.
pub fn rewrite_r128_tags(
    path: &Path,
    track_lufs: f64,
    album_lufs: Option<f64>,
) -> Result<(), String> {
    let file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut reader = PacketReader::new(BufReader::new(file));
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::Builder::new()
        .prefix(".forge-opus-tags-")
        .suffix(".opus")
        .tempfile_in(parent)
        .map_err(|error| format!("create temporary OpusTags file: {error}"))?;
    let mut rewritten = 0_usize;
    {
        let mut writer = PacketWriter::new(temporary.as_file_mut());
        let mut expected_tags_serial = None;
        while let Some(mut packet) = reader
            .read_packet()
            .map_err(|error| format!("{}: read Ogg packet: {error}", path.display()))?
        {
            if let Some(serial) = expected_tags_serial.take() {
                if packet.stream_serial() != serial || !packet.data.starts_with(b"OpusTags") {
                    return Err(format!(
                        "{}: missing OpusTags after OpusHead",
                        path.display()
                    ));
                }
                packet.data = replace_r128_comments(&packet.data, track_lufs, album_lufs)?;
                rewritten += 1;
                if rewritten > MAX_OPUS_CHAINS {
                    return Err(format!(
                        "{}: more than {MAX_OPUS_CHAINS} chained Opus streams",
                        path.display()
                    ));
                }
            } else if packet.first_in_stream() && packet.data.starts_with(b"OpusHead") {
                expected_tags_serial = Some(packet.stream_serial());
            }
            let serial = packet.stream_serial();
            let granule = packet.absgp_page();
            let end = if packet.last_in_stream() {
                PacketWriteEndInfo::EndStream
            } else if packet.last_in_page() {
                PacketWriteEndInfo::EndPage
            } else {
                PacketWriteEndInfo::NormalPacket
            };
            writer
                .write_packet(packet.data, serial, end, granule)
                .map_err(|error| format!("rewrite OpusTags: {error}"))?;
        }
        if expected_tags_serial.is_some() {
            return Err(format!("{}: missing OpusTags", path.display()));
        }
    }
    if rewritten == 0 {
        return Err(format!("{}: missing OpusTags", path.display()));
    }
    temporary.persist(path).map_err(|error| {
        format!(
            "replace {} after OpusTags update: {}",
            path.display(),
            error.error
        )
    })?;

    let expected = (r128_gain(track_lufs), album_lufs.and_then(r128_gain));
    let round_trip = read_all_r128_tags(path)?;
    if round_trip.len() != rewritten || round_trip.iter().any(|tags| *tags != expected) {
        return Err(format!(
            "{}: RFC 7845 loudness metadata changed during write/read round trip",
            path.display()
        ));
    }
    Ok(())
}

fn read_all_r128_tags(path: &Path) -> Result<Vec<R128Tags>, String> {
    let file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut reader = PacketReader::new(BufReader::new(file));
    let mut result = Vec::new();
    let mut expected_tags_serial = None;
    while let Some(packet) = reader
        .read_packet()
        .map_err(|error| format!("{}: read Ogg packet: {error}", path.display()))?
    {
        if let Some(serial) = expected_tags_serial.take() {
            if packet.stream_serial() != serial || !packet.data.starts_with(b"OpusTags") {
                return Err(format!(
                    "{}: missing OpusTags after OpusHead",
                    path.display()
                ));
            }
            result.push(parse_r128_comments(&packet.data)?);
            if result.len() > MAX_OPUS_CHAINS {
                return Err(format!(
                    "{}: more than {MAX_OPUS_CHAINS} chained Opus streams",
                    path.display()
                ));
            }
        } else if packet.first_in_stream() && packet.data.starts_with(b"OpusHead") {
            expected_tags_serial = Some(packet.stream_serial());
        }
    }
    if expected_tags_serial.is_some() {
        return Err(format!("{}: missing OpusTags", path.display()));
    }
    Ok(result)
}

fn r128_gain(lufs: f64) -> Option<i16> {
    if !lufs.is_finite() {
        return None;
    }
    Some(
        ((-23.0 - lufs) * 256.0)
            .round()
            .clamp(i16::MIN as f64, i16::MAX as f64) as i16,
    )
}

pub(crate) fn parse_r128_comments(data: &[u8]) -> Result<R128Tags, String> {
    if !data.starts_with(b"OpusTags") || data.len() < 16 {
        return Err("invalid OpusTags".into());
    }
    let mut offset = 8;
    let vendor_len = read_u32(data, &mut offset)? as usize;
    offset = offset
        .checked_add(vendor_len)
        .filter(|end| *end <= data.len())
        .ok_or_else(|| "truncated OpusTags vendor".to_string())?;
    let count = read_u32(data, &mut offset)?;
    let mut track = None;
    let mut album = None;
    for _ in 0..count {
        let length = read_u32(data, &mut offset)? as usize;
        let end = offset
            .checked_add(length)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| "truncated OpusTags comment".to_string())?;
        let comment = std::str::from_utf8(&data[offset..end])
            .map_err(|_| "non-UTF-8 OpusTags comment".to_string())?;
        let (key, value) = comment.split_once('=').unwrap_or((comment, ""));
        if key.eq_ignore_ascii_case("R128_TRACK_GAIN") {
            track = value.parse().ok();
        } else if key.eq_ignore_ascii_case("R128_ALBUM_GAIN") {
            album = value.parse().ok();
        }
        offset = end;
    }
    Ok((track, album))
}

fn replace_r128_comments(
    data: &[u8],
    track_lufs: f64,
    album_lufs: Option<f64>,
) -> Result<Vec<u8>, String> {
    if !data.starts_with(b"OpusTags") || data.len() < 16 {
        return Err("invalid OpusTags".into());
    }
    let mut offset = 8;
    let vendor_len = read_u32(data, &mut offset)? as usize;
    let vendor_end = offset
        .checked_add(vendor_len)
        .filter(|end| *end <= data.len())
        .ok_or_else(|| "truncated OpusTags vendor".to_string())?;
    let vendor = &data[offset..vendor_end];
    offset = vendor_end;
    let count = read_u32(data, &mut offset)?;
    let mut comments = Vec::new();
    for _ in 0..count {
        let length = read_u32(data, &mut offset)? as usize;
        let end = offset
            .checked_add(length)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| "truncated OpusTags comment".to_string())?;
        let comment = data[offset..end].to_vec();
        let key = comment.split(|byte| *byte == b'=').next().unwrap_or(&[]);
        if !key.eq_ignore_ascii_case(b"R128_TRACK_GAIN")
            && !key.eq_ignore_ascii_case(b"R128_ALBUM_GAIN")
        {
            comments.push(comment);
        }
        offset = end;
    }
    if let Some(track_gain) = r128_gain(track_lufs) {
        comments.push(format!("R128_TRACK_GAIN={track_gain}").into_bytes());
    }
    if let Some(album_gain) = album_lufs.and_then(r128_gain) {
        comments.push(format!("R128_ALBUM_GAIN={album_gain}").into_bytes());
    }
    let mut result = b"OpusTags".to_vec();
    result.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    result.extend_from_slice(vendor);
    result.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for comment in comments {
        result.extend_from_slice(&(comment.len() as u32).to_le_bytes());
        result.extend_from_slice(&comment);
    }
    Ok(result)
}

fn read_u32(data: &[u8], offset: &mut usize) -> Result<u32, String> {
    let end = offset
        .checked_add(4)
        .filter(|end| *end <= data.len())
        .ok_or_else(|| "truncated OpusTags".to_string())?;
    let value = u32::from_le_bytes(data[*offset..end].try_into().unwrap());
    *offset = end;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufWriter;

    fn write_stream(
        writer: &mut PacketWriter<'_, BufWriter<File>>,
        serial: u32,
        track_lufs: f64,
        album_lufs: Option<f64>,
    ) {
        let mut head = b"OpusHead".to_vec();
        head.extend_from_slice(&[1, 2]);
        head.extend_from_slice(&312_u16.to_le_bytes());
        head.extend_from_slice(&48_000_u32.to_le_bytes());
        head.extend_from_slice(&0_i16.to_le_bytes());
        head.push(0);
        writer
            .write_packet(head, serial, PacketWriteEndInfo::EndPage, 0)
            .unwrap();
        writer
            .write_packet(
                build_opus_tags(track_lufs, album_lufs),
                serial,
                PacketWriteEndInfo::EndPage,
                0,
            )
            .unwrap();
        writer
            .write_packet(
                vec![0xF8, 0xFF, 0xFE],
                serial,
                PacketWriteEndInfo::EndStream,
                960,
            )
            .unwrap();
    }

    #[test]
    fn rewrites_and_verifies_every_chained_stream_without_libopus() {
        let temporary = tempfile::NamedTempFile::new().unwrap();
        {
            let file = temporary.reopen().unwrap();
            let mut writer = PacketWriter::new(BufWriter::new(file));
            write_stream(&mut writer, 11, -18.0, None);
            write_stream(&mut writer, 22, -19.0, Some(-20.0));
        }

        rewrite_r128_tags(temporary.path(), -16.0, Some(-18.0)).unwrap();
        assert_eq!(
            read_r128_tags(temporary.path()).unwrap(),
            (Some(-1792), Some(-1280))
        );
        assert_eq!(
            read_all_r128_tags(temporary.path()).unwrap(),
            vec![(Some(-1792), Some(-1280)); 2]
        );
    }

    #[test]
    fn rejects_ogg_streams_without_opus_tags() {
        let temporary = tempfile::NamedTempFile::new().unwrap();
        {
            let file = temporary.reopen().unwrap();
            let mut writer = PacketWriter::new(BufWriter::new(file));
            writer
                .write_packet(
                    b"OpusHead-invalid".to_vec(),
                    33,
                    PacketWriteEndInfo::EndPage,
                    0,
                )
                .unwrap();
            writer
                .write_packet(b"not-tags".to_vec(), 33, PacketWriteEndInfo::EndStream, 0)
                .unwrap();
        }
        let error = rewrite_r128_tags(temporary.path(), -18.0, None).unwrap_err();
        assert!(error.contains("missing OpusTags"));
    }

    #[test]
    fn omits_undefined_track_and_album_loudness() {
        let tags = build_opus_tags(f64::NEG_INFINITY, Some(f64::NAN));
        assert_eq!(parse_r128_comments(&tags).unwrap(), (None, None));

        let temporary = tempfile::NamedTempFile::new().unwrap();
        {
            let file = temporary.reopen().unwrap();
            let mut writer = PacketWriter::new(BufWriter::new(file));
            write_stream(&mut writer, 44, -18.0, Some(-20.0));
        }
        rewrite_r128_tags(temporary.path(), f64::NEG_INFINITY, Some(f64::INFINITY)).unwrap();
        assert_eq!(read_r128_tags(temporary.path()).unwrap(), (None, None));
    }
}
