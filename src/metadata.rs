//! Cross-container audio metadata preservation.

use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::tag::{ItemKey, Tag, TagExt};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::normalize::Analysis;
use crate::wav::WaveChunk;

/// Copy the source's primary metadata tag to the destination container.
///
/// Lofty's generic tag representation retains common text fields and artwork.
/// Remapping discards only fields that the destination tag format cannot
/// represent.
pub fn copy_metadata(input: &Path, output: &Path) -> Result<(), String> {
    let source = lofty::read_from_path(input)
        .map_err(|error| format!("read metadata {}: {error}", input.display()))?;
    let Some(mut tag) = source.primary_tag().or_else(|| source.first_tag()).cloned() else {
        return Ok(());
    };

    let destination = match lofty::read_from_path(output) {
        Ok(destination) => destination,
        Err(_) if is_wave_family(output)? => return Ok(()),
        Err(error) => {
            return Err(format!(
                "read output metadata {}: {error}",
                output.display()
            ))
        }
    };
    tag.re_map(destination.primary_tag_type());
    tag.save_to_path(output, WriteOptions::default())
        .map_err(|error| format!("write metadata {}: {error}", output.display()))
}

/// Read the Broadcast Wave `bext` chunk without interpreting vendor fields.
pub fn read_bext(path: &Path) -> Result<Option<Vec<u8>>, String> {
    read_wave_chunk(path, *b"bext")
}

pub fn read_wave_chunk(path: &Path, wanted: [u8; 4]) -> Result<Option<Vec<u8>>, String> {
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let Some((offset, size)) = scan_wave_chunks(&mut file, |id, offset, size| {
        Ok((id == wanted).then_some((offset, size)))
    })?
    else {
        return Ok(None);
    };
    let size = usize::try_from(size).map_err(|_| "bext chunk is too large".to_string())?;
    let mut body = vec![0; size];
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek bext: {error}"))?;
    file.read_exact(&mut body)
        .map_err(|error| format!("read bext: {error}"))?;
    Ok(Some(body))
}

/// Preserve production metadata required by common BWF/ADM workflows.
pub fn prepare_broadcast_chunks(input: &Path) -> Result<Vec<WaveChunk>, String> {
    let mut chunks = vec![WaveChunk {
        id: *b"bext",
        body: prepare_bext(input)?,
    }];
    for id in [*b"axml", *b"bxml", *b"sxml", *b"chna", *b"iXML"] {
        if let Some(body) = read_wave_chunk(input, id)? {
            chunks.push(WaveChunk { id, body });
        }
    }
    Ok(chunks)
}

/// Prepare a BWF v2 `bext` body, preserving source production metadata.
pub fn prepare_bext(input: &Path) -> Result<Vec<u8>, String> {
    let mut bext = read_bext(input)?.unwrap_or_else(blank_bext);
    bext.resize(bext.len().max(602), 0);
    let version = u16::from_le_bytes([bext[346], bext[347]]).max(2);
    bext[346..348].copy_from_slice(&version.to_le_bytes());
    Ok(bext)
}

pub fn blank_bext() -> Vec<u8> {
    let mut bext = vec![0; 602];
    bext[346..348].copy_from_slice(&2u16.to_le_bytes());
    bext
}

/// Update the five EBU R 128 fields in an existing BWF v2 `bext` chunk.
pub fn update_bwf_loudness(path: &Path, analysis: &Analysis) -> Result<(), String> {
    let mut file = File::options()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    let bext = scan_wave_chunks(&mut file, |id, offset, size| {
        if id == *b"bext" {
            Ok(Some((offset, size)))
        } else {
            Ok(None)
        }
    })?
    .ok_or_else(|| format!("{}: missing BWF bext chunk", path.display()))?;
    if bext.1 < 422 {
        return Err(format!("{}: BWF bext chunk is too short", path.display()));
    }
    file.seek(SeekFrom::Start(bext.0 + 346))
        .map_err(|error| format!("seek BWF version: {error}"))?;
    file.write_all(&2u16.to_le_bytes())
        .map_err(|error| format!("write BWF version: {error}"))?;
    file.seek(SeekFrom::Start(bext.0 + 412))
        .map_err(|error| format!("seek BWF loudness metadata: {error}"))?;
    for value in [
        analysis.lufs,
        analysis.loudness_range_lu,
        analysis.true_peak_db(),
        analysis.max_momentary_lufs,
        analysis.max_short_term_lufs,
    ] {
        file.write_all(&bwf_value(value).to_le_bytes())
            .map_err(|error| format!("write BWF loudness metadata: {error}"))?;
    }
    file.flush()
        .map_err(|error| format!("flush BWF loudness metadata: {error}"))
}

fn bwf_value(value: f64) -> i16 {
    if value.is_finite() {
        (value * 100.0)
            .round()
            .clamp(i16::MIN as f64, (i16::MAX - 1) as f64) as i16
    } else {
        i16::MAX
    }
}

fn is_wave_family(path: &Path) -> Result<bool, String> {
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut magic = [0; 4];
    file.read_exact(&mut magic)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(matches!(&magic, b"RIFF" | b"RF64" | b"BW64"))
}

fn scan_wave_chunks<T>(
    file: &mut File,
    mut inspect: impl FnMut([u8; 4], u64, u64) -> Result<Option<T>, String>,
) -> Result<Option<T>, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek WAVE header: {error}"))?;
    let mut header = [0; 12];
    file.read_exact(&mut header)
        .map_err(|error| format!("read WAVE header: {error}"))?;
    if !matches!(&header[..4], b"RIFF" | b"RF64" | b"BW64") || &header[8..] != b"WAVE" {
        return Ok(None);
    }
    loop {
        let mut chunk = [0; 8];
        match file.read_exact(&mut chunk) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(format!("read WAVE chunk: {error}")),
        }
        let id: [u8; 4] = chunk[..4].try_into().unwrap();
        let size = u32::from_le_bytes(chunk[4..].try_into().unwrap()) as u64;
        let offset = file
            .stream_position()
            .map_err(|error| format!("locate WAVE chunk: {error}"))?;
        if let Some(result) = inspect(id, offset, size)? {
            return Ok(Some(result));
        }
        if id == *b"data" {
            return Ok(None);
        }
        file.seek(SeekFrom::Start(
            offset
                .checked_add(size)
                .and_then(|position| position.checked_add(size & 1))
                .ok_or_else(|| "WAVE chunk offset overflow".to_string())?,
        ))
        .map_err(|error| format!("skip WAVE chunk: {error}"))?;
    }
}

/// Write ReplayGain 2.0 fields while leaving encoded audio untouched.
///
/// ReplayGain 2.0 uses EBU R128 measurement with a -18 LUFS reference.
pub fn write_replaygain(
    path: &Path,
    track_lufs: f64,
    track_peak: f32,
    album: Option<(f64, f32)>,
) -> Result<(), String> {
    let tagged = lofty::read_from_path(path)
        .map_err(|error| format!("read metadata {}: {error}", path.display()))?;
    let mut tag = tagged
        .primary_tag()
        .or_else(|| tagged.first_tag())
        .cloned()
        .unwrap_or_else(|| Tag::new(tagged.primary_tag_type()));
    tag.re_map(tagged.primary_tag_type());
    tag.insert_text(
        ItemKey::ReplayGainTrackGain,
        format!("{:+.2} dB", -18.0 - track_lufs),
    );
    tag.insert_text(ItemKey::ReplayGainTrackPeak, format!("{:.8}", track_peak));
    if let Some((album_lufs, album_peak)) = album {
        tag.insert_text(
            ItemKey::ReplayGainAlbumGain,
            format!("{:+.2} dB", -18.0 - album_lufs),
        );
        tag.insert_text(ItemKey::ReplayGainAlbumPeak, format!("{:.8}", album_peak));
    }
    tag.save_to_path(path, WriteOptions::default())
        .map_err(|error| format!("write metadata {}: {error}", path.display()))
}
