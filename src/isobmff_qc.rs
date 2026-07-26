//! Bounded-memory ISO Base Media File Format structural and audio-track QC.

use crate::container_qc::{check, finish_audit, AuditCheck, ContainerAudit};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const MAX_BOXES: usize = 200_000;
const MAX_CONTROL_BOX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TRACKS: usize = 4_096;

#[derive(Clone, Copy, Debug)]
struct BoxHeader {
    kind: [u8; 4],
    start: u64,
    body_start: u64,
    end: u64,
}

impl BoxHeader {
    fn body_size(self) -> u64 {
        self.end - self.body_start
    }

    fn name(self) -> String {
        String::from_utf8_lossy(&self.kind).into_owned()
    }
}

#[derive(Default)]
struct Track {
    id: Option<u32>,
    handler: Option<[u8; 4]>,
    timescale: Option<u32>,
    duration: Option<u64>,
    codecs: Vec<String>,
    channels: Option<u16>,
    sample_rate: Option<u32>,
    stts_samples: Option<u64>,
    stts_duration: Option<u64>,
    sample_count: Option<u64>,
    sample_bytes: Option<u64>,
    chunk_offsets: Vec<u64>,
    sample_to_chunk: Vec<(u32, u32, u32)>,
    chunk_samples: Option<u64>,
}

#[derive(Default)]
struct Fragment {
    sequence: Option<u32>,
    track_ids: Vec<u32>,
    decode_times: Vec<(u32, u64)>,
    sample_count: u64,
}

#[derive(Default)]
struct State {
    box_count: usize,
    top_level: Vec<String>,
    major_brand: Option<String>,
    compatible_brands: Vec<String>,
    moov_count: usize,
    mdat_ranges: Vec<(u64, u64)>,
    has_mvex: bool,
    tracks: Vec<Track>,
    fragments: Vec<Fragment>,
}

pub(crate) fn looks_like_isobmff(header: &[u8], file_size: u64) -> bool {
    if header.len() < 8 || file_size < 8 {
        return false;
    }
    let size = u32::from_be_bytes(header[..4].try_into().unwrap());
    let kind: [u8; 4] = header[4..8].try_into().unwrap();
    let known = matches!(
        &kind,
        b"ftyp"
            | b"styp"
            | b"moov"
            | b"moof"
            | b"mdat"
            | b"sidx"
            | b"free"
            | b"skip"
            | b"wide"
            | b"emsg"
    );
    known && (size == 0 || size == 1 || u64::from(size) >= 8)
}

pub(crate) fn audit(path: &Path, mut file: File, file_size: u64) -> Result<ContainerAudit, String> {
    let mut wrapper = Vec::new();
    let mut bitstream = Vec::new();
    let mut xcheck = Vec::new();
    let mut state = State::default();

    let top = match list_boxes(path, &mut file, 0, file_size, &mut state.box_count) {
        Ok(boxes) => {
            wrapper.push(check(
                "FORGE-ISOBMFF-BOX-SCAN",
                true,
                format!("{} bounded top-level box(es) cover the file", boxes.len()),
                Some(json!(boxes
                    .iter()
                    .map(|item| item.name())
                    .collect::<Vec<_>>())),
            ));
            boxes
        }
        Err(error) => {
            wrapper.push(check("FORGE-ISOBMFF-BOX-SCAN", false, error, None));
            return Ok(finish_audit(
                path,
                "isobmff",
                wrapper,
                bitstream,
                xcheck,
                json!({"file_size_bytes": file_size}),
            ));
        }
    };

    for header in &top {
        state.top_level.push(header.name());
        match &header.kind {
            b"ftyp" | b"styp" => {
                if let Err(error) =
                    parse_file_type(path, &mut file, *header, &mut state, &mut wrapper)
                {
                    wrapper.push(check("FORGE-ISOBMFF-FILE-TYPE", false, error, None));
                }
            }
            b"moov" => {
                state.moov_count += 1;
                match parse_moov(
                    path,
                    &mut file,
                    *header,
                    &mut state,
                    &mut bitstream,
                    &mut xcheck,
                ) {
                    Ok(()) => bitstream.push(check(
                        "FORGE-ISOBMFF-MOVIE-STRUCTURE",
                        true,
                        "MovieBox child structure is bounded and complete",
                        None,
                    )),
                    Err(error) => {
                        bitstream.push(check("FORGE-ISOBMFF-MOVIE-STRUCTURE", false, error, None))
                    }
                }
            }
            b"mdat" => state.mdat_ranges.push((header.body_start, header.end)),
            b"moof" => match parse_moof(path, &mut file, *header, &mut state, &mut bitstream) {
                Ok(fragment) => state.fragments.push(fragment),
                Err(error) => bitstream.push(check(
                    "FORGE-ISOBMFF-FRAGMENT-STRUCTURE",
                    false,
                    error,
                    None,
                )),
            },
            _ => {}
        }
    }

    wrapper.push(check(
        "FORGE-ISOBMFF-BOX-LIMIT",
        state.box_count <= MAX_BOXES,
        format!(
            "{} box(es) scanned within the safety limit",
            state.box_count
        ),
        Some(json!({"count": state.box_count, "limit": MAX_BOXES})),
    ));
    wrapper.push(check(
        "FORGE-ISOBMFF-MOOV-UNIQUE",
        state.moov_count <= 1,
        if state.moov_count <= 1 {
            "at most one MovieBox is present"
        } else {
            "multiple MovieBox values are not allowed"
        },
        Some(json!(state.moov_count)),
    ));
    wrapper.push(check(
        "FORGE-ISOBMFF-FILE-TYPE-REQUIRED",
        state.moov_count == 0 || state.major_brand.is_some(),
        if state.moov_count == 0 || state.major_brand.is_some() {
            "files containing MovieBox declare a file type brand"
        } else {
            "files containing MovieBox require FileTypeBox"
        },
        state.major_brand.clone().map(Value::from),
    ));

    let has_moof = !state.fragments.is_empty();
    let init_segment = state.moov_count == 1 && state.has_mvex && !has_moof;
    let media_segment = state.moov_count == 0 && has_moof;
    let complete_file = state.moov_count == 1 && !init_segment;
    wrapper.push(check(
        "FORGE-ISOBMFF-STRUCTURE",
        init_segment || media_segment || complete_file,
        if init_segment {
            "fragmented initialization segment structure is valid"
        } else if media_segment {
            "standalone fragmented media segment structure is valid"
        } else if complete_file {
            "complete ISO-BMFF file structure is valid"
        } else {
            "file is neither a complete movie, initialization segment, nor media segment"
        },
        Some(json!({
            "initialization_segment": init_segment,
            "media_segment": media_segment,
            "fragmented": state.has_mvex || has_moof
        })),
    ));
    let requires_media = complete_file || media_segment || has_moof;
    wrapper.push(check(
        "FORGE-ISOBMFF-MDAT-REQUIRED",
        !requires_media || !state.mdat_ranges.is_empty(),
        if !requires_media || !state.mdat_ranges.is_empty() {
            "media data presence matches the file role"
        } else {
            "complete files and media segments require MediaDataBox"
        },
        Some(json!(state.mdat_ranges.len())),
    ));

    if state.moov_count == 1 {
        let audio_tracks: Vec<_> = state
            .tracks
            .iter()
            .filter(|track| track.handler == Some(*b"soun"))
            .collect();
        bitstream.push(check(
            "FORGE-ISOBMFF-AUDIO-TRACK",
            !audio_tracks.is_empty(),
            if audio_tracks.is_empty() {
                "MovieBox does not declare an audio track".into()
            } else {
                format!("{} audio track(s) declared", audio_tracks.len())
            },
            Some(json!(audio_tracks.len())),
        ));
        for (index, track) in audio_tracks.iter().enumerate() {
            bitstream.push(check(
                "FORGE-ISOBMFF-AUDIO-DESCRIPTION",
                !track.codecs.is_empty()
                    && track.timescale.is_some_and(|value| value > 0)
                    && track.channels.is_none_or(|value| value > 0)
                    && track.sample_rate.is_none_or(|value| value > 0),
                format!("audio track {} has a usable sample description", index + 1),
                Some(track_json(track)),
            ));
            if let (Some(stts_samples), Some(sample_count)) =
                (track.stts_samples, track.sample_count)
            {
                xcheck.push(check(
                    "FORGE-ISOBMFF-SAMPLE-COUNT-XCHECK",
                    stts_samples == sample_count,
                    "time-to-sample count matches sample-size count",
                    Some(json!({"track_id": track.id, "stts": stts_samples, "stsz": sample_count})),
                ));
            }
            if let (Some(chunk_samples), Some(sample_count)) =
                (track.chunk_samples, track.sample_count)
            {
                xcheck.push(check(
                    "FORGE-ISOBMFF-CHUNK-SAMPLE-COUNT-XCHECK",
                    chunk_samples == sample_count,
                    "sample-to-chunk expansion matches sample-size count",
                    Some(
                        json!({"track_id": track.id, "stsc": chunk_samples, "stsz": sample_count}),
                    ),
                ));
            }
            if let (Some(stts_duration), Some(duration)) = (track.stts_duration, track.duration) {
                xcheck.push(check(
                    "FORGE-ISOBMFF-DURATION-XCHECK",
                    stts_duration == duration,
                    "media duration matches the time-to-sample table",
                    Some(json!({"track_id": track.id, "mdhd": duration, "stts": stts_duration})),
                ));
            }
            if !track.chunk_offsets.is_empty() && !state.mdat_ranges.is_empty() {
                let invalid: Vec<_> = track
                    .chunk_offsets
                    .iter()
                    .copied()
                    .filter(|offset| {
                        !state
                            .mdat_ranges
                            .iter()
                            .any(|(start, end)| offset >= start && offset < end)
                    })
                    .collect();
                xcheck.push(check(
                    "FORGE-ISOBMFF-CHUNK-OFFSET-XCHECK",
                    invalid.is_empty(),
                    if invalid.is_empty() {
                        "every audio chunk offset points inside MediaDataBox"
                    } else {
                        "one or more audio chunk offsets point outside MediaDataBox"
                    },
                    Some(json!({"track_id": track.id, "invalid_offsets": invalid})),
                ));
            }
            if let Some(sample_bytes) = track.sample_bytes {
                let media_bytes: u64 = state
                    .mdat_ranges
                    .iter()
                    .map(|(start, end)| end - start)
                    .sum();
                xcheck.push(check(
                    "FORGE-ISOBMFF-SAMPLE-BYTES-XCHECK",
                    sample_bytes <= media_bytes,
                    "declared audio sample bytes fit in MediaDataBox payloads",
                    Some(json!({"track_id": track.id, "samples": sample_bytes, "media": media_bytes})),
                ));
            }
        }
    }

    if has_moof {
        let sequences: Vec<u32> = state
            .fragments
            .iter()
            .filter_map(|fragment| fragment.sequence)
            .collect();
        let sequence_ok = sequences.len() == state.fragments.len()
            && sequences.windows(2).all(|pair| pair[1] == pair[0] + 1);
        bitstream.push(check(
            "FORGE-ISOBMFF-FRAGMENT-SEQUENCE",
            sequence_ok,
            if sequence_ok {
                "MovieFragment sequence numbers are contiguous"
            } else {
                "MovieFragment sequence numbers are missing or non-contiguous"
            },
            Some(json!(sequences)),
        ));
        let mut last_decode_time: HashMap<u32, u64> = HashMap::new();
        let mut monotonic = true;
        for fragment in &state.fragments {
            for &(track_id, time) in &fragment.decode_times {
                if last_decode_time
                    .insert(track_id, time)
                    .is_some_and(|previous| time < previous)
                {
                    monotonic = false;
                }
            }
        }
        bitstream.push(check(
            "FORGE-ISOBMFF-FRAGMENT-TIMELINE",
            monotonic,
            if monotonic {
                "base decode times are monotonic per track"
            } else {
                "base decode time moves backwards"
            },
            Some(json!(state
                .fragments
                .iter()
                .flat_map(|fragment| fragment.decode_times.iter())
                .map(|(track, time)| json!({"track_id": track, "time": time}))
                .collect::<Vec<_>>())),
        ));
    }

    let properties = json!({
        "file_size_bytes": file_size,
        "top_level_boxes": state.top_level,
        "box_count": state.box_count,
        "major_brand": state.major_brand,
        "compatible_brands": state.compatible_brands,
        "fragmented": state.has_mvex || has_moof,
        "movie_fragments": state.fragments.len(),
        "media_data_boxes": state.mdat_ranges.len(),
        "tracks": state.tracks.iter().map(track_json).collect::<Vec<_>>()
    });
    Ok(finish_audit(
        path, "isobmff", wrapper, bitstream, xcheck, properties,
    ))
}

fn parse_file_type(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    state: &mut State,
    checks: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let body = read_control(path, file, header)?;
    let valid = body.len() >= 8 && (body.len() - 8) % 4 == 0;
    checks.push(check(
        "FORGE-ISOBMFF-FILE-TYPE",
        valid,
        if valid {
            "file type brand fields are complete"
        } else {
            "FileTypeBox must contain major/minor brand and aligned compatible brands"
        },
        Some(json!(body.len())),
    ));
    if valid {
        state.major_brand = Some(fourcc(&body[..4]));
        state.compatible_brands = body[8..].chunks_exact(4).map(fourcc).collect();
    }
    Ok(())
}

fn parse_moov(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    state: &mut State,
    bitstream: &mut Vec<AuditCheck>,
    xcheck: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let children = list_boxes(
        path,
        file,
        header.body_start,
        header.end,
        &mut state.box_count,
    )?;
    state.has_mvex |= children.iter().any(|item| item.kind == *b"mvex");
    for child in children {
        if child.kind == *b"trak" {
            if state.tracks.len() == MAX_TRACKS {
                bitstream.push(check(
                    "FORGE-ISOBMFF-TRACK-LIMIT",
                    false,
                    "track count exceeds the bounded safety limit",
                    Some(json!(MAX_TRACKS)),
                ));
                break;
            }
            state.tracks.push(parse_trak(
                path,
                file,
                child,
                &mut state.box_count,
                bitstream,
                xcheck,
            )?);
        }
    }
    Ok(())
}

fn parse_trak(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    box_count: &mut usize,
    bitstream: &mut Vec<AuditCheck>,
    xcheck: &mut Vec<AuditCheck>,
) -> Result<Track, String> {
    let children = list_boxes(path, file, header.body_start, header.end, box_count)?;
    let mut track = Track::default();
    for child in children {
        match &child.kind {
            b"tkhd" => track.id = parse_tkhd(path, file, child, bitstream)?,
            b"mdia" => parse_mdia(path, file, child, box_count, &mut track, bitstream, xcheck)?,
            _ => {}
        }
    }
    Ok(track)
}

fn parse_tkhd(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    checks: &mut Vec<AuditCheck>,
) -> Result<Option<u32>, String> {
    let body = read_control(path, file, header)?;
    let version = body.first().copied();
    let offset = match version {
        Some(0) => 12,
        Some(1) => 20,
        _ => usize::MAX,
    };
    let id = body.get(offset..offset.saturating_add(4)).map(be_u32);
    checks.push(check(
        "FORGE-ISOBMFF-TRACK-HEADER",
        id.is_some_and(|value| value != 0),
        "track header has a non-zero track ID",
        id.map(Value::from),
    ));
    Ok(id)
}

fn parse_mdia(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    box_count: &mut usize,
    track: &mut Track,
    bitstream: &mut Vec<AuditCheck>,
    xcheck: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let children = list_boxes(path, file, header.body_start, header.end, box_count)?;
    for child in &children {
        match &child.kind {
            b"mdhd" => parse_mdhd(path, file, *child, track, bitstream)?,
            b"hdlr" => track.handler = parse_hdlr(path, file, *child, bitstream)?,
            _ => {}
        }
    }
    for child in children {
        if child.kind == *b"minf" {
            parse_minf(path, file, child, box_count, track, bitstream, xcheck)?;
        }
    }
    Ok(())
}

fn parse_mdhd(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    track: &mut Track,
    checks: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let body = read_control(path, file, header)?;
    let version = body.first().copied();
    let (timescale_offset, duration_offset, duration_bytes) = match version {
        Some(0) => (12, 16, 4),
        Some(1) => (20, 24, 8),
        _ => (usize::MAX, usize::MAX, 0),
    };
    track.timescale = body
        .get(timescale_offset..timescale_offset.saturating_add(4))
        .map(be_u32);
    track.duration = match duration_bytes {
        4 => body
            .get(duration_offset..duration_offset + 4)
            .map(be_u32)
            .map(u64::from),
        8 => body.get(duration_offset..duration_offset + 8).map(be_u64),
        _ => None,
    };
    checks.push(check(
        "FORGE-ISOBMFF-MEDIA-HEADER",
        track.timescale.is_some_and(|value| value > 0) && track.duration.is_some(),
        "media header has a positive timescale and duration",
        Some(json!({"timescale": track.timescale, "duration": track.duration})),
    ));
    Ok(())
}

fn parse_hdlr(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    checks: &mut Vec<AuditCheck>,
) -> Result<Option<[u8; 4]>, String> {
    let body = read_control(path, file, header)?;
    let handler: Option<[u8; 4]> = body.get(8..12).map(|value| value.try_into().unwrap());
    checks.push(check(
        "FORGE-ISOBMFF-HANDLER",
        handler.is_some(),
        "handler box contains a handler type",
        handler.map(|value| Value::from(fourcc(&value))),
    ));
    Ok(handler)
}

fn parse_minf(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    box_count: &mut usize,
    track: &mut Track,
    bitstream: &mut Vec<AuditCheck>,
    xcheck: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    for child in list_boxes(path, file, header.body_start, header.end, box_count)? {
        if child.kind == *b"stbl" {
            parse_stbl(path, file, child, box_count, track, bitstream, xcheck)?;
        }
    }
    Ok(())
}

fn parse_stbl(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    box_count: &mut usize,
    track: &mut Track,
    bitstream: &mut Vec<AuditCheck>,
    _xcheck: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let mut seen = HashSet::new();
    for child in list_boxes(path, file, header.body_start, header.end, box_count)? {
        seen.insert(child.kind);
        match &child.kind {
            b"stsd" => parse_stsd(path, file, child, track, bitstream)?,
            b"stts" => parse_stts(path, file, child, track, bitstream)?,
            b"stsz" => parse_stsz(path, file, child, track, bitstream)?,
            b"stz2" => parse_stz2(path, file, child, track, bitstream)?,
            b"stsc" => parse_stsc(path, file, child, track, bitstream)?,
            b"stco" => track.chunk_offsets = parse_offsets(path, file, child, false, bitstream)?,
            b"co64" => track.chunk_offsets = parse_offsets(path, file, child, true, bitstream)?,
            _ => {}
        }
    }
    track.chunk_samples = expand_chunk_samples(&track.sample_to_chunk, track.chunk_offsets.len());
    if track.handler == Some(*b"soun") {
        for (present, name, observed) in [
            (seen.contains(b"stsd"), "SampleDescriptionBox", "stsd"),
            (seen.contains(b"stts"), "TimeToSampleBox", "stts"),
            (
                seen.contains(b"stsz") || seen.contains(b"stz2"),
                "SampleSizeBox or CompactSampleSizeBox",
                "stsz|stz2",
            ),
            (seen.contains(b"stsc"), "SampleToChunkBox", "stsc"),
            (
                seen.contains(b"stco") || seen.contains(b"co64"),
                "ChunkOffsetBox or ChunkLargeOffsetBox",
                "stco|co64",
            ),
        ] {
            bitstream.push(check(
                "FORGE-ISOBMFF-SAMPLE-TABLE",
                present,
                format!("{name} is present for the audio track"),
                Some(json!(observed)),
            ));
        }
    }
    Ok(())
}

fn parse_stsd(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    track: &mut Track,
    checks: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let body = read_control(path, file, header)?;
    if body.len() < 8 {
        checks.push(check(
            "FORGE-ISOBMFF-SAMPLE-DESCRIPTION",
            false,
            "SampleDescriptionBox is truncated",
            Some(json!(body.len())),
        ));
        return Ok(());
    }
    let count = be_u32(&body[4..8]) as usize;
    let mut offset = 8;
    let mut valid = true;
    for _ in 0..count {
        if body.len().saturating_sub(offset) < 8 {
            valid = false;
            break;
        }
        let size = be_u32(&body[offset..offset + 4]) as usize;
        if size < 8 || size > body.len() - offset {
            valid = false;
            break;
        }
        let codec = fourcc(&body[offset + 4..offset + 8]);
        track.codecs.push(codec);
        if track.handler == Some(*b"soun") && size >= 36 {
            track.channels = Some(u16::from_be_bytes(
                body[offset + 24..offset + 26].try_into().unwrap(),
            ));
            track.sample_rate = Some(be_u32(&body[offset + 32..offset + 36]) >> 16);
        }
        offset += size;
    }
    valid &= offset == body.len();
    checks.push(check(
        "FORGE-ISOBMFF-SAMPLE-DESCRIPTION",
        valid && count > 0,
        "sample description entries are bounded and complete",
        Some(json!({"declared": count, "codecs": track.codecs})),
    ));
    Ok(())
}

fn parse_stts(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    track: &mut Track,
    checks: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let body = read_control(path, file, header)?;
    let parsed = parse_counted_entries(&body, 8);
    let mut samples = 0_u64;
    let mut duration = 0_u64;
    if let Some(ref entries) = parsed {
        for entry in entries {
            let count = u64::from(be_u32(&entry[..4]));
            let delta = u64::from(be_u32(&entry[4..8]));
            samples = samples.saturating_add(count);
            duration = duration.saturating_add(count.saturating_mul(delta));
        }
        track.stts_samples = Some(samples);
        track.stts_duration = Some(duration);
    }
    checks.push(check(
        "FORGE-ISOBMFF-TIME-TO-SAMPLE",
        parsed.is_some(),
        "time-to-sample entries are bounded and complete",
        Some(json!({"samples": track.stts_samples, "duration": track.stts_duration})),
    ));
    Ok(())
}

fn parse_stsz(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    track: &mut Track,
    checks: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let body = read_control(path, file, header)?;
    let valid_header = body.len() >= 12;
    let mut valid = valid_header;
    if valid_header {
        let uniform = u64::from(be_u32(&body[4..8]));
        let count = u64::from(be_u32(&body[8..12]));
        track.sample_count = Some(count);
        if uniform == 0 {
            let required = 12_u64.saturating_add(count.saturating_mul(4));
            valid = required == body.len() as u64;
            if valid {
                track.sample_bytes = Some(
                    body[12..]
                        .chunks_exact(4)
                        .map(|item| u64::from(be_u32(item)))
                        .sum(),
                );
            }
        } else {
            valid = body.len() == 12;
            track.sample_bytes = Some(uniform.saturating_mul(count));
        }
    }
    checks.push(check(
        "FORGE-ISOBMFF-SAMPLE-SIZE",
        valid,
        "sample-size entries are bounded and complete",
        Some(json!({"samples": track.sample_count, "bytes": track.sample_bytes})),
    ));
    Ok(())
}

fn parse_stz2(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    track: &mut Track,
    checks: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let body = read_control(path, file, header)?;
    let mut valid = body.len() >= 12;
    if valid {
        let field_size = body[7];
        let count = be_u32(&body[8..12]) as usize;
        let payload = &body[12..];
        let required = match field_size {
            4 => count.div_ceil(2),
            8 => count,
            16 => count.saturating_mul(2),
            _ => {
                valid = false;
                0
            }
        };
        valid &= payload.len() == required;
        if valid {
            let bytes = match field_size {
                4 => payload
                    .iter()
                    .enumerate()
                    .map(|(index, value)| {
                        let base = u64::from(value >> 4) + u64::from(value & 0x0f);
                        if index * 2 + 1 < count {
                            base
                        } else {
                            u64::from(value >> 4)
                        }
                    })
                    .sum(),
                8 => payload.iter().map(|value| u64::from(*value)).sum(),
                16 => payload
                    .chunks_exact(2)
                    .map(|value| u64::from(u16::from_be_bytes(value.try_into().unwrap())))
                    .sum(),
                _ => 0,
            };
            track.sample_count = Some(count as u64);
            track.sample_bytes = Some(bytes);
        }
    }
    checks.push(check(
        "FORGE-ISOBMFF-COMPACT-SAMPLE-SIZE",
        valid,
        "compact sample-size entries are bounded and complete",
        Some(json!({"samples": track.sample_count, "bytes": track.sample_bytes})),
    ));
    Ok(())
}

fn parse_stsc(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    track: &mut Track,
    checks: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let body = read_control(path, file, header)?;
    let parsed = parse_counted_entries(&body, 12);
    let mut entries = Vec::new();
    let mut valid = parsed.is_some();
    if let Some(values) = parsed {
        for value in values {
            let entry = (
                be_u32(&value[..4]),
                be_u32(&value[4..8]),
                be_u32(&value[8..12]),
            );
            valid &= entry.0 > 0
                && entry.1 > 0
                && entry.2 > 0
                && entries
                    .last()
                    .is_none_or(|previous: &(u32, u32, u32)| entry.0 > previous.0);
            entries.push(entry);
        }
    }
    if valid {
        track.sample_to_chunk = entries;
    }
    checks.push(check(
        "FORGE-ISOBMFF-SAMPLE-TO-CHUNK",
        valid,
        "sample-to-chunk entries are ordered, positive, and complete",
        Some(json!(track.sample_to_chunk)),
    ));
    Ok(())
}

fn parse_offsets(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    wide: bool,
    checks: &mut Vec<AuditCheck>,
) -> Result<Vec<u64>, String> {
    let body = read_control(path, file, header)?;
    let width = if wide { 8 } else { 4 };
    let parsed = parse_counted_entries(&body, width);
    let offsets: Vec<u64> = parsed
        .as_ref()
        .map(|entries| {
            entries
                .iter()
                .map(|entry| {
                    if wide {
                        be_u64(entry)
                    } else {
                        u64::from(be_u32(entry))
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    checks.push(check(
        "FORGE-ISOBMFF-CHUNK-OFFSETS",
        parsed.is_some(),
        "chunk-offset entries are bounded and complete",
        Some(json!(offsets.len())),
    ));
    Ok(offsets)
}

fn expand_chunk_samples(entries: &[(u32, u32, u32)], chunk_count: usize) -> Option<u64> {
    if chunk_count == 0 {
        return if entries.is_empty() { Some(0) } else { None };
    }
    if entries.first().map(|entry| entry.0) != Some(1) {
        return None;
    }
    let mut total = 0_u64;
    for (index, entry) in entries.iter().enumerate() {
        let start = entry.0 as usize;
        let end = entries
            .get(index + 1)
            .map(|next| next.0 as usize)
            .unwrap_or(chunk_count + 1);
        if start > chunk_count || end <= start || end > chunk_count + 1 {
            return None;
        }
        total = total.saturating_add(((end - start) as u64).saturating_mul(u64::from(entry.1)));
    }
    Some(total)
}

fn parse_moof(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    state: &mut State,
    checks: &mut Vec<AuditCheck>,
) -> Result<Fragment, String> {
    let children = list_boxes(
        path,
        file,
        header.body_start,
        header.end,
        &mut state.box_count,
    )?;
    let mut fragment = Fragment::default();
    for child in children {
        match &child.kind {
            b"mfhd" => {
                let body = read_control(path, file, child)?;
                fragment.sequence = body.get(4..8).map(be_u32);
            }
            b"traf" => parse_traf(path, file, child, state, &mut fragment, checks)?,
            _ => {}
        }
    }
    checks.push(check(
        "FORGE-ISOBMFF-FRAGMENT-HEADER",
        fragment.sequence.is_some() && !fragment.track_ids.is_empty(),
        "movie fragment has a sequence number and track fragment",
        Some(json!({"sequence": fragment.sequence, "tracks": fragment.track_ids})),
    ));
    Ok(fragment)
}

fn parse_traf(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    state: &mut State,
    fragment: &mut Fragment,
    checks: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let children = list_boxes(
        path,
        file,
        header.body_start,
        header.end,
        &mut state.box_count,
    )?;
    let mut track_id = None;
    let mut decode_time = None;
    for child in children {
        match &child.kind {
            b"tfhd" => {
                let body = read_control(path, file, child)?;
                track_id = body.get(4..8).map(be_u32);
            }
            b"tfdt" => {
                let body = read_control(path, file, child)?;
                decode_time = match body.first() {
                    Some(0) => body.get(4..8).map(be_u32).map(u64::from),
                    Some(1) => body.get(4..12).map(be_u64),
                    _ => None,
                };
            }
            b"trun" => {
                let body = read_control(path, file, child)?;
                if let Some(count) = body.get(4..8).map(be_u32) {
                    fragment.sample_count = fragment.sample_count.saturating_add(u64::from(count));
                }
            }
            _ => {}
        }
    }
    if let Some(id) = track_id {
        fragment.track_ids.push(id);
        if let Some(time) = decode_time {
            fragment.decode_times.push((id, time));
        }
    }
    checks.push(check(
        "FORGE-ISOBMFF-TRACK-FRAGMENT",
        track_id.is_some() && decode_time.is_some(),
        "track fragment identifies its track and base decode time",
        Some(json!({"track_id": track_id, "decode_time": decode_time})),
    ));
    Ok(())
}

fn list_boxes(
    path: &Path,
    file: &mut File,
    start: u64,
    end: u64,
    box_count: &mut usize,
) -> Result<Vec<BoxHeader>, String> {
    let mut boxes = Vec::new();
    let mut offset = start;
    while offset < end {
        if *box_count == MAX_BOXES {
            return Err(format!(
                "{}: ISO-BMFF box count exceeds safety limit {MAX_BOXES}",
                path.display()
            ));
        }
        if end - offset < 8 {
            return Err(format!(
                "{}: truncated ISO-BMFF box header at byte {offset}",
                path.display()
            ));
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| format!("seek {} to {offset}: {error}", path.display()))?;
        let mut base = [0_u8; 8];
        file.read_exact(&mut base)
            .map_err(|error| format!("read {} box at {offset}: {error}", path.display()))?;
        let size32 = be_u32(&base[..4]);
        let kind: [u8; 4] = base[4..8].try_into().unwrap();
        let (size, header_size) = match size32 {
            0 => (end - offset, 8),
            1 => {
                if end - offset < 16 {
                    return Err(format!(
                        "{}: truncated extended box header at byte {offset}",
                        path.display()
                    ));
                }
                let mut extended = [0_u8; 8];
                file.read_exact(&mut extended).map_err(|error| {
                    format!("read {} extended box at {offset}: {error}", path.display())
                })?;
                (be_u64(&extended), 16)
            }
            value => (u64::from(value), 8),
        };
        if size < header_size {
            return Err(format!(
                "{}: {} box at byte {offset} is smaller than its header",
                path.display(),
                fourcc(&kind)
            ));
        }
        let box_end = offset.checked_add(size).ok_or_else(|| {
            format!(
                "{}: {} box size overflows at byte {offset}",
                path.display(),
                fourcc(&kind)
            )
        })?;
        if box_end > end {
            return Err(format!(
                "{}: {} box ending at byte {box_end} exceeds parent bound {end}",
                path.display(),
                fourcc(&kind)
            ));
        }
        boxes.push(BoxHeader {
            kind,
            start: offset,
            body_start: offset + header_size,
            end: box_end,
        });
        *box_count += 1;
        offset = box_end;
        if size32 == 0 && offset != end {
            return Err(format!(
                "{}: zero-sized box does not end its parent",
                path.display()
            ));
        }
    }
    Ok(boxes)
}

fn read_control(path: &Path, file: &mut File, header: BoxHeader) -> Result<Vec<u8>, String> {
    if header.body_size() > MAX_CONTROL_BOX_BYTES {
        return Err(format!(
            "{}: {} control box at byte {} exceeds {} byte safety limit",
            path.display(),
            header.name(),
            header.start,
            MAX_CONTROL_BOX_BYTES
        ));
    }
    let size = usize::try_from(header.body_size()).expect("bounded control box fits usize");
    let mut body = vec![0_u8; size];
    file.seek(SeekFrom::Start(header.body_start))
        .map_err(|error| format!("seek {}: {error}", path.display()))?;
    file.read_exact(&mut body)
        .map_err(|error| format!("read {} {} box: {error}", path.display(), header.name()))?;
    Ok(body)
}

fn parse_counted_entries(body: &[u8], width: usize) -> Option<Vec<&[u8]>> {
    if body.len() < 8 || width == 0 {
        return None;
    }
    let count = be_u32(&body[4..8]) as usize;
    let required = 8_usize.checked_add(count.checked_mul(width)?)?;
    if required != body.len() {
        return None;
    }
    Some(body[8..].chunks_exact(width).collect())
}

fn track_json(track: &Track) -> Value {
    json!({
        "track_id": track.id,
        "handler": track.handler.map(|value| fourcc(&value)),
        "timescale": track.timescale,
        "duration": track.duration,
        "codecs": track.codecs,
        "channels": track.channels,
        "sample_rate_hz": track.sample_rate,
        "sample_count": track.sample_count,
        "sample_bytes": track.sample_bytes,
        "chunk_count": track.chunk_offsets.len(),
        "chunk_samples": track.chunk_samples
    })
}

fn fourcc(value: &[u8]) -> String {
    String::from_utf8_lossy(value).into_owned()
}

fn be_u32(value: &[u8]) -> u32 {
    u32::from_be_bytes(value.try_into().unwrap())
}

fn be_u64(value: &[u8]) -> u64 {
    u64::from_be_bytes(value.try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn boxed(kind: &[u8; 4], body: Vec<u8>) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend_from_slice(&(u32::try_from(body.len() + 8).unwrap()).to_be_bytes());
        output.extend_from_slice(kind);
        output.extend_from_slice(&body);
        output
    }

    fn full_box(version: u8, payload: Vec<u8>) -> Vec<u8> {
        let mut body = vec![version, 0, 0, 0];
        body.extend(payload);
        body
    }

    fn minimal_audio_mp4(chunk_offset: u32) -> Vec<u8> {
        let ftyp = boxed(
            b"ftyp",
            [b"M4A ".as_slice(), &[0, 0, 0, 0], b"isom"].concat(),
        );
        let tkhd = boxed(
            b"tkhd",
            full_box(
                0,
                [vec![0; 8], 1_u32.to_be_bytes().to_vec(), vec![0; 68]].concat(),
            ),
        );
        let mdhd = boxed(
            b"mdhd",
            full_box(
                0,
                [
                    vec![0; 8],
                    48_000_u32.to_be_bytes().to_vec(),
                    1_024_u32.to_be_bytes().to_vec(),
                    vec![0; 4],
                ]
                .concat(),
            ),
        );
        let hdlr = boxed(
            b"hdlr",
            full_box(0, [vec![0; 4], b"soun".to_vec(), vec![0; 12]].concat()),
        );
        let mut sample_entry = vec![0_u8; 28];
        sample_entry[6..8].copy_from_slice(&1_u16.to_be_bytes());
        sample_entry[16..18].copy_from_slice(&2_u16.to_be_bytes());
        sample_entry[18..20].copy_from_slice(&16_u16.to_be_bytes());
        sample_entry[24..28].copy_from_slice(&(48_000_u32 << 16).to_be_bytes());
        let stsd = boxed(
            b"stsd",
            full_box(
                0,
                [1_u32.to_be_bytes().to_vec(), boxed(b"mp4a", sample_entry)].concat(),
            ),
        );
        let stts = boxed(
            b"stts",
            full_box(
                0,
                [
                    1_u32.to_be_bytes(),
                    1_u32.to_be_bytes(),
                    1_024_u32.to_be_bytes(),
                ]
                .concat(),
            ),
        );
        let stsz = boxed(
            b"stsz",
            full_box(0, [4_u32.to_be_bytes(), 1_u32.to_be_bytes()].concat()),
        );
        let stsc = boxed(
            b"stsc",
            full_box(
                0,
                [
                    1_u32.to_be_bytes(),
                    1_u32.to_be_bytes(),
                    1_u32.to_be_bytes(),
                    1_u32.to_be_bytes(),
                ]
                .concat(),
            ),
        );
        let stco = boxed(
            b"stco",
            full_box(
                0,
                [1_u32.to_be_bytes(), chunk_offset.to_be_bytes()].concat(),
            ),
        );
        let stbl = boxed(b"stbl", [stsd, stts, stsz, stsc, stco].concat());
        let minf = boxed(b"minf", stbl);
        let mdia = boxed(b"mdia", [mdhd, hdlr, minf].concat());
        let trak = boxed(b"trak", [tkhd, mdia].concat());
        let moov = boxed(b"moov", trak);
        let mdat = boxed(b"mdat", vec![1, 2, 3, 4]);
        [ftyp, moov, mdat].concat()
    }

    fn media_fragment(sequence: u32, decode_time: u64) -> Vec<u8> {
        let mfhd = boxed(b"mfhd", full_box(0, sequence.to_be_bytes().to_vec()));
        let tfhd = boxed(b"tfhd", full_box(0, 1_u32.to_be_bytes().to_vec()));
        let tfdt = boxed(b"tfdt", full_box(1, decode_time.to_be_bytes().to_vec()));
        let trun = boxed(b"trun", full_box(0, 1_u32.to_be_bytes().to_vec()));
        boxed(
            b"moof",
            [mfhd, boxed(b"traf", [tfhd, tfdt, trun].concat())].concat(),
        )
    }

    fn fragmented_media(sequences: &[u32]) -> Vec<u8> {
        let styp = boxed(
            b"styp",
            [b"msdh".as_slice(), &[0, 0, 0, 0], b"msdh", b"msix"].concat(),
        );
        let mut output = styp;
        for (index, sequence) in sequences.iter().enumerate() {
            output.extend(media_fragment(*sequence, index as u64 * 1_024));
            output.extend(boxed(b"mdat", vec![1, 2, 3, 4]));
        }
        output
    }

    #[test]
    fn audits_complete_audio_mp4_without_reading_media_payload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("audio.m4a");
        let preliminary = minimal_audio_mp4(0);
        let mdat_start = preliminary
            .windows(4)
            .position(|window| window == b"mdat")
            .unwrap() as u32
            + 4;
        File::create(&path)
            .unwrap()
            .write_all(&minimal_audio_mp4(mdat_start))
            .unwrap();
        let result = crate::container_qc::audit(&path).unwrap();
        assert!(result.passed, "{result:#?}");
        assert_eq!(result.format, "isobmff");
        assert_eq!(result.properties["tracks"][0]["codecs"][0], "mp4a");
    }

    #[test]
    fn rejects_box_that_exceeds_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("broken.mp4");
        let mut file = File::create(&path).unwrap();
        file.write_all(&100_u32.to_be_bytes()).unwrap();
        file.write_all(b"ftyp").unwrap();
        file.write_all(b"isom").unwrap();
        drop(file);
        let result = crate::container_qc::audit(&path).unwrap();
        assert!(!result.passed);
        assert!(result.layers[0]
            .checks
            .iter()
            .any(|item| { item.rule_id == "FORGE-ISOBMFF-BOX-SCAN" && !item.passed }));
    }

    #[test]
    fn validates_fragment_sequence_and_decode_timeline() {
        let directory = tempfile::tempdir().unwrap();
        let valid_path = directory.path().join("valid.m4s");
        std::fs::write(&valid_path, fragmented_media(&[7, 8])).unwrap();
        let valid = crate::container_qc::audit(&valid_path).unwrap();
        assert!(valid.passed, "{valid:#?}");
        assert_eq!(valid.properties["movie_fragments"], 2);

        let invalid_path = directory.path().join("invalid.m4s");
        std::fs::write(&invalid_path, fragmented_media(&[7, 9])).unwrap();
        let invalid = crate::container_qc::audit(&invalid_path).unwrap();
        assert!(!invalid.passed);
        assert!(invalid.layers[1]
            .checks
            .iter()
            .any(|item| { item.rule_id == "FORGE-ISOBMFF-FRAGMENT-SEQUENCE" && !item.passed }));
    }

    #[test]
    fn nested_box_corruption_is_reported_as_qc_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested.mp4");
        let ftyp = boxed(
            b"ftyp",
            [b"isom".as_slice(), &[0, 0, 0, 0], b"isom"].concat(),
        );
        let mut corrupt_child = Vec::new();
        corrupt_child.extend_from_slice(&100_u32.to_be_bytes());
        corrupt_child.extend_from_slice(b"trak");
        std::fs::write(&path, [ftyp, boxed(b"moov", corrupt_child)].concat()).unwrap();
        let result = crate::container_qc::audit(&path).unwrap();
        assert!(!result.passed);
        assert!(result.layers[1]
            .checks
            .iter()
            .any(|item| { item.rule_id == "FORGE-ISOBMFF-MOVIE-STRUCTURE" && !item.passed }));
    }
}
