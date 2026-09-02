//! Conservative ISO-BMFF `ludt` writer used by metadata repair.
//!
//! The writer buffers only `moov`, preserves every unselected box byte, and
//! adjusts unfragmented `stco`/`co64` entries when the rewritten `moov` changes
//! the byte position of media that follows it.  Fragmented media files and
//! boxes with other absolute-offset mechanisms fail closed; an fMP4
//! initialization segment without media may still be repaired from an
//! explicitly supplied decoded reference.

use crate::container_qc::ContainerAudit;
use crate::dsp::lufs::StreamingAnalyzer;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const METHOD_DEFINITION_PROGRAM_LOUDNESS: u8 = 1;
const MEASUREMENT_SYSTEM_BS_1770: u8 = 2;
const RELIABILITY_ACCURATE: u8 = 3;
const MAX_SIGNED_PEAK_CODE: u16 = 0x07ff;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DecodedLoudness {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub frames: u64,
    pub decoded_samples: u64,
    pub integrated_lufs: f64,
    pub sample_peak_dbfs: f64,
    pub true_peak_dbtp: f64,
}

#[derive(Debug, Clone)]
pub(crate) struct ReferenceMeasurement {
    pub loudness: DecodedLoudness,
    pub gating_blocks: Vec<f64>,
}

#[derive(Debug, Clone)]
pub(crate) struct EncodedLoudness {
    pub program_code: u8,
    pub sample_peak_code: u16,
    pub true_peak_code: u16,
    pub program_loudness_lkfs: f64,
    pub sample_peak_dbfs: f64,
    pub true_peak_dbtp: f64,
}

#[derive(Debug, Clone)]
pub(crate) struct TargetTrack {
    pub track_id: u32,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u16>,
    pub codecs: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct RewriteResult {
    pub bytes_written: u64,
    pub changed: bool,
    pub replaced_existing: bool,
    pub moov_size_delta: i64,
    pub adjusted_chunk_offsets: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RewriteLimits {
    pub max_input_bytes: u64,
    pub max_moov_bytes: u64,
    pub max_boxes: u32,
}

#[derive(Clone, Copy, Debug)]
struct FileBox {
    kind: [u8; 4],
    start: u64,
    body_start: u64,
    end: u64,
}

impl FileBox {
    fn size(self) -> u64 {
        self.end - self.start
    }

    fn body_size(self) -> u64 {
        self.end - self.body_start
    }
}

#[derive(Clone, Copy, Debug)]
struct SliceBox {
    kind: [u8; 4],
    start: usize,
    body_start: usize,
    end: usize,
}

impl SliceBox {
    fn raw(self, bytes: &[u8]) -> &[u8] {
        &bytes[self.start..self.end]
    }

    fn body(self, bytes: &[u8]) -> &[u8] {
        &bytes[self.body_start..self.end]
    }
}

pub(crate) fn select_target(audit: &ContainerAudit) -> Result<TargetTrack, String> {
    if audit.format != "isobmff" {
        return Err("ISO-BMFF loudness repair requires an MP4/M4A/fMP4 source".into());
    }
    let tracks = audit.properties["tracks"]
        .as_array()
        .ok_or("ISO-BMFF audit has no track inventory")?;
    let audio = tracks
        .iter()
        .filter(|track| track["handler"].as_str() == Some("soun"))
        .collect::<Vec<_>>();
    if audio.len() != 1 {
        return Err(format!(
            "ISO-BMFF loudness repair requires exactly one audio track; found {}",
            audio.len()
        ));
    }
    let track = audio[0];
    let track_id = track["track_id"]
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value != 0)
        .ok_or("ISO-BMFF audio track has no valid numeric track ID")?;
    let codecs = track["codecs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().map(ToOwned::to_owned))
        .collect::<Vec<_>>();
    let conflicting = codecs.iter().find(|codec| {
        matches!(
            codec.as_str(),
            "apac" | "iamf" | "mhm1" | "mhm2" | "mha1" | "mha2" | "ac-4"
        )
    });
    if let Some(codec) = conflicting {
        return Err(format!(
            "ISO-BMFF loudness repair does not write ludt for {codec}; presentation-aware or in-stream loudness metadata is required"
        ));
    }
    if !track["xhe_aac_usac_config"].is_null() {
        return Err(
            "ISO-BMFF loudness repair refuses xHE-AAC because ludt would take precedence over its required in-stream loudness metadata"
                .into(),
        );
    }
    Ok(TargetTrack {
        track_id,
        sample_rate_hz: track["sample_rate_hz"]
            .as_u64()
            .and_then(|value| u32::try_from(value).ok()),
        channels: track["channels"]
            .as_u64()
            .and_then(|value| u16::try_from(value).ok()),
        codecs,
    })
}

pub(crate) fn analyze_reference(
    path: &Path,
    max_decoded_samples: u64,
) -> Result<ReferenceMeasurement, String> {
    if max_decoded_samples == 0 {
        return Err("ISO-BMFF loudness max_decoded_samples must be positive".into());
    }
    let mut analyzer = None;
    let mut decoded_samples = 0_u64;
    let info = crate::decoder::decode_stream(path, |info, planar| {
        let frames = planar.first().map_or(0_usize, Vec::len);
        if planar.iter().any(|channel| channel.len() != frames) {
            return Err("decoded reference channel lengths differ".into());
        }
        let chunk_samples = u64::try_from(frames)
            .ok()
            .and_then(|frames| frames.checked_mul(u64::from(info.channels)))
            .ok_or("decoded reference sample count overflow")?;
        decoded_samples = decoded_samples
            .checked_add(chunk_samples)
            .ok_or("decoded reference sample count overflow")?;
        if decoded_samples > max_decoded_samples {
            return Err(format!(
                "decoded reference exceeds max_decoded_samples ({max_decoded_samples})"
            ));
        }
        if analyzer.is_none() {
            if info.channels > 6
                && info
                    .channel_roles
                    .iter()
                    .all(|role| matches!(role, crate::wav::ChannelRole::Main))
            {
                return Err(format!(
                    "{}: ambiguous {}-channel layout cannot be written as BS.1770 loudness metadata",
                    path.display(),
                    info.channels
                ));
            }
            analyzer = Some(StreamingAnalyzer::new(
                info.sample_rate,
                info.channel_roles.clone(),
            ));
        }
        analyzer
            .as_mut()
            .expect("decoder callback initializes loudness analyzer")
            .process(planar)
    })?;
    let measured = analyzer
        .ok_or_else(|| format!("{}: decoded reference contains no audio", path.display()))?
        .finish();
    let frames = u64::try_from(measured.frames)
        .map_err(|_| "decoded reference frame count exceeds u64".to_string())?;
    let result = DecodedLoudness {
        sample_rate_hz: info.sample_rate,
        channels: info.channels,
        frames,
        decoded_samples,
        integrated_lufs: measured.ebu.integrated_lufs,
        sample_peak_dbfs: linear_db(measured.sample_peak),
        true_peak_dbtp: linear_db(measured.true_peak),
    };
    if measured
        .ebu
        .gating_blocks
        .iter()
        .any(|value| !value.is_finite() || *value < 0.0)
    {
        return Err(format!(
            "{}: decoded reference contains an invalid BS.1770 gating block",
            path.display()
        ));
    }
    for (name, value) in [
        ("integrated loudness", result.integrated_lufs),
        ("sample peak", result.sample_peak_dbfs),
        ("true peak", result.true_peak_dbtp),
    ] {
        if value.is_nan() || value == f64::INFINITY {
            return Err(format!(
                "{}: decoded reference {name} is invalid",
                path.display()
            ));
        }
    }
    Ok(ReferenceMeasurement {
        loudness: result,
        gating_blocks: measured.ebu.gating_blocks,
    })
}

pub(crate) fn validate_encodable_measurement(
    path: &Path,
    measured: &DecodedLoudness,
) -> Result<(), String> {
    for (name, value) in [
        ("integrated loudness", measured.integrated_lufs),
        ("sample peak", measured.sample_peak_dbfs),
        ("true peak", measured.true_peak_dbtp),
    ] {
        if !value.is_finite() {
            return Err(format!(
                "{}: decoded reference {name} is not finite",
                path.display()
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_reference_geometry(
    target: &TargetTrack,
    reference: &DecodedLoudness,
) -> Result<(), String> {
    if let Some(channels) = target.channels {
        if channels != reference.channels {
            return Err(format!(
                "decoded reference has {} channels but ISO-BMFF track {} declares {channels}",
                reference.channels, target.track_id
            ));
        }
    }
    if let Some(sample_rate) = target.sample_rate_hz {
        if sample_rate != reference.sample_rate_hz {
            return Err(format!(
                "decoded reference uses {} Hz but ISO-BMFF track {} declares {sample_rate} Hz",
                reference.sample_rate_hz, target.track_id
            ));
        }
    }
    Ok(())
}

pub(crate) fn encode_measurement(measured: &DecodedLoudness) -> Result<EncodedLoudness, String> {
    encode_values(
        measured.integrated_lufs,
        measured.sample_peak_dbfs,
        measured.true_peak_dbtp,
    )
}

pub(crate) fn encode_values(
    integrated_lufs: f64,
    sample_peak_dbfs: f64,
    true_peak_dbtp: f64,
) -> Result<EncodedLoudness, String> {
    let program_code_f = ((integrated_lufs + 57.75) * 4.0).round();
    if !(0.0..=255.0).contains(&program_code_f) {
        return Err(format!(
            "integrated loudness {:.3} LUFS is outside the ISO/MPEG methodValue range -57.75..=6.0 LKFS",
            integrated_lufs
        ));
    }
    let program_code = program_code_f as u8;
    let sample_peak_code = encode_peak("sample peak", sample_peak_dbfs)?;
    let true_peak_code = encode_peak("true peak", true_peak_dbtp)?;
    Ok(EncodedLoudness {
        program_code,
        sample_peak_code,
        true_peak_code,
        program_loudness_lkfs: -57.75 + f64::from(program_code) * 0.25,
        sample_peak_dbfs: decode_peak(sample_peak_code),
        true_peak_dbtp: decode_peak(true_peak_code),
    })
}

pub(crate) fn verify_round_trip(
    audit: &ContainerAudit,
    track_id: u32,
    expected_track: &EncodedLoudness,
    expected_album: Option<&EncodedLoudness>,
) -> bool {
    let Some(tracks) = audit.properties["tracks"].as_array() else {
        return false;
    };
    let matching = tracks
        .iter()
        .filter(|track| track["track_id"].as_u64() == Some(u64::from(track_id)))
        .collect::<Vec<_>>();
    if matching.len() != 1 || matching[0]["loudness_box_count"].as_u64() != Some(1) {
        return false;
    }
    let Some(entries) = matching[0]["loudness"].as_array() else {
        return false;
    };
    verify_scope(entries, "track", expected_track)
        && expected_album.is_none_or(|expected| verify_scope(entries, "album", expected))
}

fn verify_scope(entries: &[serde_json::Value], scope: &str, expected: &EncodedLoudness) -> bool {
    let matching = entries
        .iter()
        .filter(|entry| entry["scope"].as_str() == Some(scope))
        .collect::<Vec<_>>();
    if matching.len() != 1 {
        return false;
    }
    let entry = matching[0];
    if entry["version"].as_u64() != Some(0)
        || !entry["eq_set_id"].is_null()
        || entry["downmix_id"].as_u64() != Some(0)
        || entry["drc_set_id"].as_u64() != Some(0)
        || entry["sample_peak_code"].as_i64() != Some(i64::from(expected.sample_peak_code))
        || entry["true_peak_code"].as_i64() != Some(i64::from(expected.true_peak_code))
        || entry["true_peak_measurement_system"].as_u64() != Some(2)
        || entry["true_peak_reliability"].as_u64() != Some(3)
    {
        return false;
    }
    let Some(measurements) = entry["measurements"].as_array() else {
        return false;
    };
    measurements.len() == 1
        && measurements[0]["method_definition"].as_u64() == Some(1)
        && measurements[0]["method_value"].as_u64() == Some(u64::from(expected.program_code))
        && measurements[0]["measurement_system"].as_u64() == Some(2)
        && measurements[0]["reliability"].as_u64() == Some(3)
        && measurements[0]["value_lkfs"]
            .as_f64()
            .is_some_and(|value| (value - expected.program_loudness_lkfs).abs() < 1e-12)
}

fn encode_peak(name: &str, value: f64) -> Result<u16, String> {
    let code = ((20.0 - value) * 32.0).round();
    if !(1.0..=f64::from(MAX_SIGNED_PEAK_CODE)).contains(&code) {
        return Err(format!(
            "{name} {value:.3} dB is outside the conservative ISO-BMFF signed 12-bit range -43.969..=19.969 dB"
        ));
    }
    Ok(code as u16)
}

fn decode_peak(code: u16) -> f64 {
    20.0 - f64::from(code) / 32.0
}

fn linear_db(value: f32) -> f64 {
    if value > 0.0 {
        20.0 * f64::from(value).log10()
    } else {
        f64::NEG_INFINITY
    }
}

pub(crate) fn rewrite(
    source: &Path,
    output: &mut dyn Write,
    target_track_id: u32,
    track_loudness: &EncodedLoudness,
    album_loudness: Option<&EncodedLoudness>,
    limits: RewriteLimits,
) -> Result<RewriteResult, String> {
    let RewriteLimits {
        max_input_bytes,
        max_moov_bytes,
        max_boxes,
    } = limits;
    let (mut input, file_size, top) = scan_file(source, max_input_bytes, max_boxes)?;
    let moov = unique_box(&top, *b"moov", "movie")?;
    if moov.size() > max_moov_bytes {
        return Err(format!(
            "ISO-BMFF moov is {} bytes, above max_metadata_chunk_bytes {max_moov_bytes}",
            moov.size()
        ));
    }
    let moov_size = usize::try_from(moov.size())
        .map_err(|_| "ISO-BMFF moov does not fit memory".to_string())?;
    let mut original_moov = vec![0_u8; moov_size];
    input
        .seek(SeekFrom::Start(moov.start))
        .and_then(|_| input.read_exact(&mut original_moov))
        .map_err(|error| format!("read {} moov: {error}", source.display()))?;

    let mut box_count = u32::try_from(top.len()).unwrap_or(u32::MAX);
    let (mut rewritten_moov, replaced_existing) = replace_loudness_in_moov(
        &original_moov,
        target_track_id,
        track_loudness,
        album_loudness,
        &mut box_count,
        max_boxes,
    )?;
    let delta = i64::try_from(rewritten_moov.len())
        .ok()
        .and_then(|new| i64::try_from(original_moov.len()).ok().map(|old| new - old))
        .ok_or("ISO-BMFF moov size delta does not fit i64")?;

    let has_fragment_media = top.iter().any(|item| item.kind == *b"moof");
    let unsupported_top_level_offsets = top
        .iter()
        .find(|item| matches!(&item.kind, b"sidx" | b"ssix" | b"mfra"));
    if delta != 0 && has_fragment_media {
        return Err(
            "ISO-BMFF loudness repair supports fMP4 initialization segments, not files that also contain moof media fragments"
                .into(),
        );
    }
    if delta != 0 {
        if let Some(item) = unsupported_top_level_offsets {
            return Err(format!(
                "ISO-BMFF loudness repair would move a {} box whose absolute/relative offsets are not rewritten",
                fourcc(item.kind)
            ));
        }
        if contains_offset_box(&rewritten_moov, &[*b"saio", *b"iloc", *b"tfra"], max_boxes)? {
            return Err(
                "ISO-BMFF loudness repair found saio/iloc/tfra offsets and refuses to move media bytes"
                    .into(),
            );
        }
    }
    let adjusted_chunk_offsets = if delta == 0 {
        0
    } else {
        patch_chunk_offsets(&mut rewritten_moov, moov.end, delta, max_boxes)?
    };

    copy_file_range(&mut input, output, 0, moov.start)?;
    output
        .write_all(&rewritten_moov)
        .map_err(|error| format!("write rewritten ISO-BMFF moov: {error}"))?;
    copy_file_range(&mut input, output, moov.end, file_size - moov.end)?;
    let bytes_written = file_size
        .checked_add_signed(delta)
        .ok_or("rewritten ISO-BMFF size overflow")?;
    Ok(RewriteResult {
        bytes_written,
        changed: rewritten_moov != original_moov,
        replaced_existing,
        moov_size_delta: delta,
        adjusted_chunk_offsets,
    })
}

pub(crate) fn mdat_sha256(
    path: &Path,
    max_input_bytes: u64,
    max_boxes: u32,
) -> Result<Option<String>, String> {
    let (mut file, _, boxes) = scan_file(path, max_input_bytes, max_boxes)?;
    let mdats = boxes
        .iter()
        .filter(|item| item.kind == *b"mdat")
        .copied()
        .collect::<Vec<_>>();
    if mdats.is_empty() {
        return Ok(None);
    }
    let mut hasher = Sha256::new();
    hasher.update(b"forge-isobmff-mdat-v1");
    hasher.update((mdats.len() as u64).to_be_bytes());
    let mut buffer = [0_u8; 128 * 1024];
    for item in mdats {
        hasher.update(item.body_size().to_be_bytes());
        file.seek(SeekFrom::Start(item.body_start))
            .map_err(|error| format!("seek {} mdat: {error}", path.display()))?;
        let mut remaining = item.body_size();
        while remaining > 0 {
            let want = usize::try_from(remaining.min(buffer.len() as u64)).unwrap();
            file.read_exact(&mut buffer[..want])
                .map_err(|error| format!("read {} mdat: {error}", path.display()))?;
            hasher.update(&buffer[..want]);
            remaining -= want as u64;
        }
    }
    Ok(Some(hex_digest(hasher.finalize())))
}

fn scan_file(
    path: &Path,
    max_input_bytes: u64,
    max_boxes: u32,
) -> Result<(File, u64, Vec<FileBox>), String> {
    if max_boxes == 0 {
        return Err("ISO-BMFF max_chunks must be positive".into());
    }
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let file_size = file
        .metadata()
        .map_err(|error| format!("stat {}: {error}", path.display()))?
        .len();
    if file_size > max_input_bytes {
        return Err(format!(
            "{} is {file_size} bytes, above max_input_bytes {max_input_bytes}",
            path.display()
        ));
    }
    let mut output = Vec::new();
    let mut offset = 0_u64;
    while offset < file_size {
        if output.len() >= max_boxes as usize {
            return Err(format!("ISO-BMFF exceeds max_chunks ({max_boxes})"));
        }
        if file_size - offset < 8 {
            return Err(format!("truncated ISO-BMFF box header at byte {offset}"));
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| format!("seek {}: {error}", path.display()))?;
        let mut base = [0_u8; 8];
        file.read_exact(&mut base)
            .map_err(|error| format!("read {} box header: {error}", path.display()))?;
        let size32 = u32::from_be_bytes(base[..4].try_into().unwrap());
        let kind = base[4..8].try_into().unwrap();
        let (size, header_size) = match size32 {
            0 => (file_size - offset, 8_u64),
            1 => {
                if file_size - offset < 16 {
                    return Err(format!("truncated extended ISO-BMFF box at byte {offset}"));
                }
                let mut extended = [0_u8; 8];
                file.read_exact(&mut extended)
                    .map_err(|error| format!("read {} extended box: {error}", path.display()))?;
                (u64::from_be_bytes(extended), 16)
            }
            value => (u64::from(value), 8),
        };
        if size < header_size {
            return Err(format!(
                "{} box at byte {offset} is smaller than its header",
                fourcc(kind)
            ));
        }
        let end = offset
            .checked_add(size)
            .ok_or("ISO-BMFF top-level box size overflow")?;
        if end > file_size {
            return Err(format!(
                "{} box at byte {offset} exceeds the file",
                fourcc(kind)
            ));
        }
        output.push(FileBox {
            kind,
            start: offset,
            body_start: offset + header_size,
            end,
        });
        offset = end;
    }
    Ok((file, file_size, output))
}

fn unique_box(boxes: &[FileBox], kind: [u8; 4], label: &str) -> Result<FileBox, String> {
    let matching = boxes
        .iter()
        .filter(|item| item.kind == kind)
        .copied()
        .collect::<Vec<_>>();
    if matching.len() != 1 {
        return Err(format!(
            "ISO-BMFF loudness repair requires exactly one {label} box; found {}",
            matching.len()
        ));
    }
    Ok(matching[0])
}

fn replace_loudness_in_moov(
    moov: &[u8],
    target_track_id: u32,
    track_loudness: &EncodedLoudness,
    album_loudness: Option<&EncodedLoudness>,
    box_count: &mut u32,
    max_boxes: u32,
) -> Result<(Vec<u8>, bool), String> {
    let root = parse_one(moov)?;
    if root.kind != *b"moov" || root.start != 0 || root.end != moov.len() {
        return Err("buffered ISO-BMFF movie box is malformed".into());
    }
    let children = list_slice_boxes(moov, root.body_start, root.end, box_count, max_boxes)?;
    let mut target_index = None;
    for (index, child) in children.iter().enumerate() {
        if child.kind != *b"trak" {
            continue;
        }
        if track_identity(moov, *child, box_count, max_boxes)? == Some((target_track_id, *b"soun"))
            && target_index.replace(index).is_some()
        {
            return Err(format!(
                "ISO-BMFF contains duplicate audio track ID {target_track_id}"
            ));
        }
    }
    let target_index = target_index.ok_or_else(|| {
        format!("ISO-BMFF movie does not contain audio track ID {target_track_id}")
    })?;
    let (replacement, replaced_existing) = replace_loudness_in_track(
        moov,
        children[target_index],
        track_loudness,
        album_loudness,
        box_count,
        max_boxes,
    )?;
    let mut body = Vec::new();
    for (index, child) in children.iter().enumerate() {
        if index == target_index {
            body.extend_from_slice(&replacement);
        } else {
            body.extend_from_slice(child.raw(moov));
        }
    }
    Ok((make_box(*b"moov", &body)?, replaced_existing))
}

fn track_identity(
    bytes: &[u8],
    track: SliceBox,
    box_count: &mut u32,
    max_boxes: u32,
) -> Result<Option<(u32, [u8; 4])>, String> {
    let children = list_slice_boxes(bytes, track.body_start, track.end, box_count, max_boxes)?;
    let tkhd = children
        .iter()
        .filter(|child| child.kind == *b"tkhd")
        .collect::<Vec<_>>();
    let mdia = children
        .iter()
        .filter(|child| child.kind == *b"mdia")
        .collect::<Vec<_>>();
    if tkhd.len() != 1 || mdia.len() != 1 {
        return Ok(None);
    }
    let body = tkhd[0].body(bytes);
    let id_offset = match body.first().copied() {
        Some(0) => 12,
        Some(1) => 20,
        _ => return Ok(None),
    };
    let track_id = body
        .get(id_offset..id_offset + 4)
        .map(|value| u32::from_be_bytes(value.try_into().unwrap()));
    let media_children =
        list_slice_boxes(bytes, mdia[0].body_start, mdia[0].end, box_count, max_boxes)?;
    let handlers = media_children
        .iter()
        .filter(|child| child.kind == *b"hdlr")
        .collect::<Vec<_>>();
    let handler = (handlers.len() == 1)
        .then(|| handlers[0].body(bytes).get(8..12))
        .flatten()
        .map(|value| value.try_into().unwrap());
    Ok(track_id.zip(handler))
}

fn replace_loudness_in_track(
    bytes: &[u8],
    track: SliceBox,
    track_loudness: &EncodedLoudness,
    album_loudness: Option<&EncodedLoudness>,
    box_count: &mut u32,
    max_boxes: u32,
) -> Result<(Vec<u8>, bool), String> {
    let children = list_slice_boxes(bytes, track.body_start, track.end, box_count, max_boxes)?;
    let udta_indices = children
        .iter()
        .enumerate()
        .filter_map(|(index, child)| (child.kind == *b"udta").then_some(index))
        .collect::<Vec<_>>();
    if udta_indices.len() > 1 {
        return Err("ISO-BMFF audio track contains multiple udta boxes".into());
    }
    let new_tlou = make_loudness_base(*b"tlou", track_loudness)?;
    let new_alou = album_loudness
        .map(|loudness| make_loudness_base(*b"alou", loudness))
        .transpose()?;
    let mut replaced_existing = false;
    let mut body = Vec::new();
    for (index, child) in children.iter().enumerate() {
        if udta_indices.first() == Some(&index) {
            let (replacement, replaced) = replace_ludt_in_udta(
                bytes,
                *child,
                &new_tlou,
                new_alou.as_deref(),
                box_count,
                max_boxes,
            )?;
            body.extend_from_slice(&replacement);
            replaced_existing = replaced;
        } else {
            body.extend_from_slice(child.raw(bytes));
        }
    }
    if udta_indices.is_empty() {
        let mut loudness = new_tlou;
        if let Some(new_alou) = new_alou {
            loudness.extend_from_slice(&new_alou);
        }
        body.extend_from_slice(&make_box(*b"udta", &make_box(*b"ludt", &loudness)?)?);
    }
    Ok((make_box(*b"trak", &body)?, replaced_existing))
}

fn replace_ludt_in_udta(
    bytes: &[u8],
    udta: SliceBox,
    new_tlou: &[u8],
    new_alou: Option<&[u8]>,
    box_count: &mut u32,
    max_boxes: u32,
) -> Result<(Vec<u8>, bool), String> {
    let children = list_slice_boxes(bytes, udta.body_start, udta.end, box_count, max_boxes)?;
    let ludt_count = children
        .iter()
        .filter(|child| child.kind == *b"ludt")
        .count();
    if ludt_count > 1 {
        return Err("ISO-BMFF audio track contains multiple ludt boxes".into());
    }
    let mut body = Vec::new();
    for child in children {
        if child.kind == *b"ludt" {
            body.extend_from_slice(&replace_loudness_in_ludt(
                bytes, child, new_tlou, new_alou, box_count, max_boxes,
            )?);
        } else {
            body.extend_from_slice(child.raw(bytes));
        }
    }
    if ludt_count == 0 {
        let mut loudness = new_tlou.to_vec();
        if let Some(new_alou) = new_alou {
            loudness.extend_from_slice(new_alou);
        }
        body.extend_from_slice(&make_box(*b"ludt", &loudness)?);
    }
    Ok((make_box(*b"udta", &body)?, ludt_count == 1))
}

fn replace_loudness_in_ludt(
    bytes: &[u8],
    ludt: SliceBox,
    new_tlou: &[u8],
    new_alou: Option<&[u8]>,
    box_count: &mut u32,
    max_boxes: u32,
) -> Result<Vec<u8>, String> {
    let children = list_slice_boxes(bytes, ludt.body_start, ludt.end, box_count, max_boxes)?;
    let tlou_count = children
        .iter()
        .filter(|child| child.kind == *b"tlou")
        .count();
    if tlou_count > 1 {
        return Err("ISO-BMFF loudness box contains multiple tlou boxes".into());
    }
    let alou_count = children
        .iter()
        .filter(|child| child.kind == *b"alou")
        .count();
    if alou_count > 1 {
        return Err("ISO-BMFF loudness box contains multiple alou boxes".into());
    }
    let mut body = Vec::new();
    for child in children {
        if child.kind == *b"tlou" {
            body.extend_from_slice(new_tlou);
            if let Some(new_alou) = new_alou {
                // The reference serializer writes every track entry before
                // every album entry. Reinsert alou here even when the source
                // had the two known children in the opposite order.
                body.extend_from_slice(new_alou);
            }
        } else if child.kind == *b"alou" && new_alou.is_some() {
            // Replaced beside tlou above.
        } else {
            // When no album measurement was supplied, existing album
            // loudness remains byte-for-byte. Unknown children are preserved
            // too; post-write QC rejects structurally invalid children.
            body.extend_from_slice(child.raw(bytes));
        }
    }
    if tlou_count == 0 {
        body.extend_from_slice(new_tlou);
        if let Some(new_alou) = new_alou {
            body.extend_from_slice(new_alou);
        }
    }
    make_box(*b"ludt", &body)
}

fn make_loudness_base(kind: [u8; 4], loudness: &EncodedLoudness) -> Result<Vec<u8>, String> {
    if !matches!(&kind, b"tlou" | b"alou") {
        return Err("ISO-BMFF loudness base must be tlou or alou".into());
    }
    let ids = 0_u16;
    let peaks = (u32::from(loudness.sample_peak_code) << 12) | u32::from(loudness.true_peak_code);
    let mut body = vec![0, 0, 0, 0]; // FullBox version 0, flags 0.
    body.extend_from_slice(&ids.to_be_bytes());
    body.extend_from_slice(&[
        ((peaks >> 16) & 0xff) as u8,
        ((peaks >> 8) & 0xff) as u8,
        (peaks & 0xff) as u8,
    ]);
    body.push((MEASUREMENT_SYSTEM_BS_1770 << 4) | RELIABILITY_ACCURATE);
    body.push(1);
    body.extend_from_slice(&[
        METHOD_DEFINITION_PROGRAM_LOUDNESS,
        loudness.program_code,
        (MEASUREMENT_SYSTEM_BS_1770 << 4) | RELIABILITY_ACCURATE,
    ]);
    make_box(kind, &body)
}

fn make_box(kind: [u8; 4], body: &[u8]) -> Result<Vec<u8>, String> {
    let size = body
        .len()
        .checked_add(8)
        .ok_or("ISO-BMFF box size overflow")?;
    let size = u32::try_from(size).map_err(|_| "ISO-BMFF box exceeds 32-bit size".to_string())?;
    let mut output = Vec::with_capacity(size as usize);
    output.extend_from_slice(&size.to_be_bytes());
    output.extend_from_slice(&kind);
    output.extend_from_slice(body);
    Ok(output)
}

fn parse_one(bytes: &[u8]) -> Result<SliceBox, String> {
    let mut count = 0;
    let boxes = list_slice_boxes(bytes, 0, bytes.len(), &mut count, u32::MAX)?;
    if boxes.len() != 1 {
        return Err("expected exactly one ISO-BMFF box".into());
    }
    Ok(boxes[0])
}

fn list_slice_boxes(
    bytes: &[u8],
    start: usize,
    end: usize,
    box_count: &mut u32,
    max_boxes: u32,
) -> Result<Vec<SliceBox>, String> {
    let mut output = Vec::new();
    let mut offset = start;
    while offset < end {
        if *box_count >= max_boxes {
            return Err(format!("ISO-BMFF exceeds max_chunks ({max_boxes})"));
        }
        let base = bytes
            .get(offset..offset + 8)
            .ok_or_else(|| format!("truncated ISO-BMFF box header at moov byte {offset}"))?;
        let size32 = u32::from_be_bytes(base[..4].try_into().unwrap());
        let kind = base[4..8].try_into().unwrap();
        let (size, header_size) = match size32 {
            0 => (end - offset, 8),
            1 => {
                let value = bytes
                    .get(offset + 8..offset + 16)
                    .ok_or_else(|| format!("truncated extended box at moov byte {offset}"))?;
                let size = u64::from_be_bytes(value.try_into().unwrap());
                let size = usize::try_from(size)
                    .map_err(|_| "ISO-BMFF extended box does not fit memory".to_string())?;
                (size, 16)
            }
            value => (value as usize, 8),
        };
        if size < header_size {
            return Err(format!(
                "{} box at moov byte {offset} is smaller than its header",
                fourcc(kind)
            ));
        }
        let box_end = offset
            .checked_add(size)
            .ok_or("ISO-BMFF box size overflow")?;
        if box_end > end {
            return Err(format!(
                "{} box at moov byte {offset} exceeds its parent",
                fourcc(kind)
            ));
        }
        output.push(SliceBox {
            kind,
            start: offset,
            body_start: offset + header_size,
            end: box_end,
        });
        *box_count += 1;
        offset = box_end;
    }
    Ok(output)
}

fn contains_offset_box(bytes: &[u8], wanted: &[[u8; 4]], max_boxes: u32) -> Result<bool, String> {
    let root = parse_one(bytes)?;
    let mut count = 1;
    contains_offset_box_in(bytes, root, wanted, &mut count, max_boxes)
}

fn contains_offset_box_in(
    bytes: &[u8],
    parent: SliceBox,
    wanted: &[[u8; 4]],
    count: &mut u32,
    max_boxes: u32,
) -> Result<bool, String> {
    let (start, traverse) = container_child_start(bytes, parent);
    if !traverse {
        return Ok(false);
    }
    let children = list_slice_boxes(bytes, start, parent.end, count, max_boxes)?;
    for child in children {
        if wanted.contains(&child.kind)
            || contains_offset_box_in(bytes, child, wanted, count, max_boxes)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn container_child_start(bytes: &[u8], item: SliceBox) -> (usize, bool) {
    let simple = matches!(
        &item.kind,
        b"moov"
            | b"trak"
            | b"mdia"
            | b"minf"
            | b"stbl"
            | b"edts"
            | b"dinf"
            | b"udta"
            | b"mvex"
            | b"mfra"
            | b"tref"
            | b"ipro"
            | b"sinf"
            | b"schi"
            | b"meco"
    );
    if simple {
        (item.body_start, true)
    } else if item.kind == *b"meta" && item.end.saturating_sub(item.body_start) >= 4 {
        (item.body_start + 4, true)
    } else {
        let _ = bytes;
        (item.body_start, false)
    }
}

fn patch_chunk_offsets(
    moov: &mut [u8],
    old_moov_end: u64,
    delta: i64,
    max_boxes: u32,
) -> Result<u64, String> {
    let root = parse_one(moov)?;
    let mut count = 1_u32;
    patch_offsets_in(moov, root, old_moov_end, delta, &mut count, max_boxes)
}

fn patch_offsets_in(
    bytes: &mut [u8],
    parent: SliceBox,
    threshold: u64,
    delta: i64,
    count: &mut u32,
    max_boxes: u32,
) -> Result<u64, String> {
    let (start, traverse) = container_child_start(bytes, parent);
    if !traverse {
        return Ok(0);
    }
    let children = list_slice_boxes(bytes, start, parent.end, count, max_boxes)?;
    let mut adjusted = 0_u64;
    for child in children {
        match &child.kind {
            b"stco" => adjusted += patch_offset_table(bytes, child, false, threshold, delta)?,
            b"co64" => adjusted += patch_offset_table(bytes, child, true, threshold, delta)?,
            _ => {
                adjusted += patch_offsets_in(bytes, child, threshold, delta, count, max_boxes)?;
            }
        }
    }
    Ok(adjusted)
}

fn patch_offset_table(
    bytes: &mut [u8],
    item: SliceBox,
    wide: bool,
    threshold: u64,
    delta: i64,
) -> Result<u64, String> {
    let body = bytes
        .get(item.body_start..item.end)
        .ok_or("ISO-BMFF chunk-offset body is out of range")?;
    if body.len() < 8 || body[0] != 0 || body[1..4] != [0, 0, 0] {
        return Err(format!(
            "{} chunk-offset FullBox is malformed",
            fourcc(item.kind)
        ));
    }
    let count = u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize;
    let width = if wide { 8 } else { 4 };
    let expected = 8_usize
        .checked_add(
            count
                .checked_mul(width)
                .ok_or("chunk-offset table size overflow")?,
        )
        .ok_or("chunk-offset table size overflow")?;
    if expected != body.len() {
        return Err(format!(
            "{} chunk-offset table length is invalid",
            fourcc(item.kind)
        ));
    }
    let mut adjusted = 0_u64;
    for index in 0..count {
        let offset = item.body_start + 8 + index * width;
        let old = if wide {
            u64::from_be_bytes(bytes[offset..offset + 8].try_into().unwrap())
        } else {
            u64::from(u32::from_be_bytes(
                bytes[offset..offset + 4].try_into().unwrap(),
            ))
        };
        if old < threshold {
            continue;
        }
        let new = old
            .checked_add_signed(delta)
            .ok_or_else(|| format!("{} chunk offset adjustment overflows", fourcc(item.kind)))?;
        if wide {
            bytes[offset..offset + 8].copy_from_slice(&new.to_be_bytes());
        } else {
            let new = u32::try_from(new).map_err(|_| {
                "stco adjustment exceeds 32 bits; co64 promotion is intentionally not implicit"
                    .to_string()
            })?;
            bytes[offset..offset + 4].copy_from_slice(&new.to_be_bytes());
        }
        adjusted += 1;
    }
    Ok(adjusted)
}

fn copy_file_range(
    input: &mut File,
    output: &mut dyn Write,
    start: u64,
    mut bytes: u64,
) -> Result<(), String> {
    input
        .seek(SeekFrom::Start(start))
        .map_err(|error| format!("seek ISO-BMFF source to {start}: {error}"))?;
    let mut buffer = [0_u8; 128 * 1024];
    while bytes > 0 {
        let want = usize::try_from(bytes.min(buffer.len() as u64)).unwrap();
        input
            .read_exact(&mut buffer[..want])
            .map_err(|error| format!("read ISO-BMFF source bytes: {error}"))?;
        output
            .write_all(&buffer[..want])
            .map_err(|error| format!("write ISO-BMFF output bytes: {error}"))?;
        bytes -= want as u64;
    }
    Ok(())
}

fn fourcc(kind: [u8; 4]) -> String {
    String::from_utf8_lossy(&kind).into_owned()
}

fn hex_digest(value: impl AsRef<[u8]>) -> String {
    value
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn full_box(version: u8, payload: &[u8]) -> Vec<u8> {
        let mut body = vec![version, 0, 0, 0];
        body.extend_from_slice(payload);
        body
    }

    fn track(track_id: u32, chunk_offset: u32) -> Vec<u8> {
        track_with_extra(track_id, chunk_offset, Vec::new())
    }

    fn track_with_extra(track_id: u32, chunk_offset: u32, extra: Vec<u8>) -> Vec<u8> {
        let mut tkhd = vec![0_u8; 24];
        tkhd[12..16].copy_from_slice(&track_id.to_be_bytes());
        let tkhd = make_box(*b"tkhd", &tkhd).unwrap();
        let hdlr = make_box(
            *b"hdlr",
            &full_box(0, &[vec![0; 4], b"soun".to_vec(), vec![0; 12]].concat()),
        )
        .unwrap();
        let stco = make_box(
            *b"stco",
            &full_box(
                0,
                &[1_u32.to_be_bytes(), chunk_offset.to_be_bytes()].concat(),
            ),
        )
        .unwrap();
        let stbl = make_box(*b"stbl", &stco).unwrap();
        let minf = make_box(*b"minf", &stbl).unwrap();
        let mdia = make_box(*b"mdia", &[hdlr, minf].concat()).unwrap();
        make_box(*b"trak", &[tkhd, mdia, extra].concat()).unwrap()
    }

    fn measurement() -> EncodedLoudness {
        encode_measurement(&DecodedLoudness {
            sample_rate_hz: 48_000,
            channels: 2,
            frames: 48_000,
            decoded_samples: 96_000,
            integrated_lufs: -23.04,
            sample_peak_dbfs: -2.01,
            true_peak_dbtp: -1.02,
        })
        .unwrap()
    }

    fn limits() -> RewriteLimits {
        RewriteLimits {
            max_input_bytes: 1024 * 1024,
            max_moov_bytes: 1024 * 1024,
            max_boxes: 100,
        }
    }

    #[test]
    fn quantizes_normative_loudness_and_peak_steps() {
        let encoded = measurement();
        assert_eq!(encoded.program_code, 139);
        assert_eq!(encoded.program_loudness_lkfs, -23.0);
        assert!((encoded.true_peak_dbtp - -1.03125).abs() < 1e-12);
    }

    #[test]
    fn inserts_ludt_and_moves_unfragmented_chunk_offsets() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.m4a");
        let ftyp = make_box(*b"ftyp", b"M4A \0\0\0\0M4A ").unwrap();
        let provisional_moov = make_box(*b"moov", &track(1, 0)).unwrap();
        let old_mdat_body = ftyp.len() + provisional_moov.len() + 8;
        let moov = make_box(*b"moov", &track(1, old_mdat_body as u32)).unwrap();
        let mdat = make_box(*b"mdat", &[1, 2, 3, 4]).unwrap();
        std::fs::write(&path, [ftyp, moov.clone(), mdat].concat()).unwrap();

        let mut output = Vec::new();
        let result = rewrite(&path, &mut output, 1, &measurement(), None, limits()).unwrap();
        assert!(result.changed);
        assert!(!result.replaced_existing);
        assert_eq!(result.adjusted_chunk_offsets, 1);
        assert!(output.windows(4).any(|value| value == b"ludt"));
        let expected = u32::try_from(old_mdat_body as i64 + result.moov_size_delta).unwrap();
        let stco_at = output
            .windows(4)
            .position(|value| value == b"stco")
            .unwrap();
        let offset = u32::from_be_bytes(output[stco_at + 12..stco_at + 16].try_into().unwrap());
        assert_eq!(offset, expected);
        let mut cursor = Cursor::new(output);
        cursor.set_position(0);
    }

    #[test]
    fn refuses_fragment_media_when_moov_size_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fragmented.mp4");
        let moov = make_box(*b"moov", &track(1, 0)).unwrap();
        let moof = make_box(*b"moof", &[]).unwrap();
        std::fs::write(&path, [moov, moof].concat()).unwrap();
        let error = rewrite(&path, &mut Vec::new(), 1, &measurement(), None, limits()).unwrap_err();
        assert!(error.contains("moof"));
    }

    #[test]
    fn replaces_tlou_without_discarding_album_loudness() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("album.m4a");
        let album = make_box(*b"alou", &[0, 0, 0, 0, 9, 8, 7]).unwrap();
        let old_tlou = make_loudness_base(
            *b"tlou",
            &encode_measurement(&DecodedLoudness {
                sample_rate_hz: 48_000,
                channels: 2,
                frames: 48_000,
                decoded_samples: 96_000,
                integrated_lufs: -16.0,
                sample_peak_dbfs: -2.0,
                true_peak_dbtp: -1.0,
            })
            .unwrap(),
        )
        .unwrap();
        let udta = make_box(
            *b"udta",
            &make_box(*b"ludt", &[old_tlou, album.clone()].concat()).unwrap(),
        )
        .unwrap();
        let moov = make_box(*b"moov", &track_with_extra(1, 0, udta)).unwrap();
        std::fs::write(&path, moov).unwrap();

        let mut output = Vec::new();
        let result = rewrite(&path, &mut output, 1, &measurement(), None, limits()).unwrap();
        assert!(result.replaced_existing);
        assert!(output
            .windows(album.len())
            .any(|window| window == album.as_slice()));
    }

    #[test]
    fn writes_track_then_album_loudness_and_replaces_old_album_value() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("album-replace.m4a");
        let old_album = make_box(*b"alou", &[0, 0, 0, 0, 9, 8, 7]).unwrap();
        let old_track = make_loudness_base(*b"tlou", &measurement()).unwrap();
        let udta = make_box(
            *b"udta",
            &make_box(*b"ludt", &[old_album.clone(), old_track].concat()).unwrap(),
        )
        .unwrap();
        let moov = make_box(*b"moov", &track_with_extra(1, 0, udta)).unwrap();
        std::fs::write(&path, moov).unwrap();
        let album = encode_measurement(&DecodedLoudness {
            sample_rate_hz: 48_000,
            channels: 2,
            frames: 96_000,
            decoded_samples: 192_000,
            integrated_lufs: -19.11,
            sample_peak_dbfs: -3.02,
            true_peak_dbtp: -2.01,
        })
        .unwrap();

        let mut output = Vec::new();
        rewrite(
            &path,
            &mut output,
            1,
            &measurement(),
            Some(&album),
            limits(),
        )
        .unwrap();

        let tlou = output
            .windows(4)
            .position(|value| value == b"tlou")
            .unwrap();
        let alou = output
            .windows(4)
            .position(|value| value == b"alou")
            .unwrap();
        assert!(tlou < alou);
        assert_eq!(
            output.windows(4).filter(|value| *value == b"alou").count(),
            1
        );
        assert!(!output
            .windows(old_album.len())
            .any(|window| window == old_album.as_slice()));
    }

    #[test]
    fn refuses_nonzero_chunk_offset_fullbox_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invalid-stco.mp4");
        let mut moov = make_box(*b"moov", &track(1, 0)).unwrap();
        let stco = moov.windows(4).position(|value| value == b"stco").unwrap();
        moov[stco + 4] = 1;
        std::fs::write(&path, moov).unwrap();

        let error = rewrite(&path, &mut Vec::new(), 1, &measurement(), None, limits()).unwrap_err();
        assert!(error.contains("stco chunk-offset FullBox is malformed"));
    }
}
