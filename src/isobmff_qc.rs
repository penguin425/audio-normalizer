//! Bounded-memory ISO Base Media File Format structural and audio-track QC.

use crate::container_qc::{check, finish_audit, AuditCheck, ContainerAudit};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const MAX_BOXES: usize = 200_000;
const MAX_CONTROL_BOX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TRACKS: usize = 4_096;
const MAX_TABLE_ENTRIES: usize = 10_000_000;
const MAX_TIMED_ID3_EVENTS: usize = 4_096;
const MAX_TIMED_ID3_STORED_BYTES: usize = 64 * 1024 * 1024;

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
    header_duration: Option<u64>,
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
    ludt_count: usize,
    loudness: Vec<LoudnessEntry>,
    drc_boxes: Vec<String>,
    aac_config: Option<crate::aac_qc::AscInfo>,
    edit_media_time: Option<i64>,
    edit_segment_duration: Option<u64>,
    sample_group_types: Vec<String>,
    roll_distances: Vec<i16>,
    sample_group_samples: Option<u64>,
}

#[derive(Clone)]
struct LoudnessEntry {
    scope: &'static str,
    version: u8,
    eq_set_id: Option<u8>,
    downmix_id: u8,
    drc_set_id: u8,
    sample_peak_code: i16,
    true_peak_code: i16,
    true_peak_measurement_system: u8,
    true_peak_reliability: u8,
    measurements: Vec<LoudnessMeasurement>,
}

#[derive(Clone)]
struct LoudnessMeasurement {
    method_definition: u8,
    method_value: u8,
    measurement_system: u8,
    reliability: u8,
}

#[derive(Default)]
struct Fragment {
    sequence: Option<u32>,
    track_ids: Vec<u32>,
    decode_times: Vec<(u32, u64)>,
    sample_count: u64,
    movie_relative: Option<bool>,
}

#[derive(Serialize)]
struct TimedId3Event {
    version: u8,
    timescale: u32,
    presentation_time: u64,
    event_duration: u32,
    id: u32,
    scheme_id_uri: String,
    value: String,
    tag: crate::id3_qc::Id3Tag,
}

#[derive(Default)]
struct State {
    box_count: usize,
    top_level: Vec<String>,
    major_brand: Option<String>,
    compatible_brands: Vec<String>,
    moov_count: usize,
    movie_duration: Option<u64>,
    movie_timescale: Option<u32>,
    mvex_after_tracks: bool,
    mdat_ranges: Vec<(u64, u64)>,
    has_mvex: bool,
    tracks: Vec<Track>,
    fragments: Vec<Fragment>,
    timed_id3: Vec<TimedId3Event>,
    timed_id3_bytes: usize,
    timed_id3_limit_hit: bool,
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
            b"emsg" => match parse_event_message(path, &mut file, *header) {
                Ok(Some(event)) => {
                    let size = event.tag.size_bytes;
                    if !state.timed_id3_limit_hit
                        && state.timed_id3.len() < MAX_TIMED_ID3_EVENTS
                        && state.timed_id3_bytes.saturating_add(size) <= MAX_TIMED_ID3_STORED_BYTES
                    {
                        bitstream.push(check(
                            "FORGE-ISOBMFF-TIMED-ID3",
                            true,
                            "CMAF EventMessageBox carries a complete ID3v2.4 tag",
                            Some(json!({
                                "timescale": event.timescale,
                                "presentation_time": event.presentation_time,
                                "id": event.id,
                                "frames": event.tag.frame_count,
                                "relative_volume_adjustments":
                                    event.tag.relative_volume_adjustments.len()
                            })),
                        ));
                        state.timed_id3_bytes += size;
                        state.timed_id3.push(event);
                    } else if !state.timed_id3_limit_hit {
                        state.timed_id3_limit_hit = true;
                        bitstream.push(check(
                            "FORGE-ISOBMFF-TIMED-ID3-LIMIT",
                            false,
                            "CMAF timed-ID3 evidence exceeds the event-count or stored-byte safety limit",
                            Some(json!({
                                "event_limit": MAX_TIMED_ID3_EVENTS,
                                "stored_byte_limit": MAX_TIMED_ID3_STORED_BYTES
                            })),
                        ));
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    bitstream.push(check("FORGE-ISOBMFF-EVENT-MESSAGE", false, error, None))
                }
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
            if let (Some(group_samples), Some(sample_count)) =
                (track.sample_group_samples, track.sample_count)
            {
                xcheck.push(check(
                    "FORGE-ISOBMFF-SAMPLE-GROUP-COUNT-XCHECK",
                    group_samples == sample_count,
                    "sample-to-group runs cover the declared sample count",
                    Some(json!({
                        "track_id": track.id,
                        "sample_groups": group_samples,
                        "sample_count": sample_count
                    })),
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
            if let (Some(asc), Some(access_units), Some(stts_duration)) = (
                track.aac_config.as_ref(),
                track.stts_samples,
                track.stts_duration,
            ) {
                let coded_samples = access_units.saturating_mul(u64::from(asc.frame_samples));
                let timing_valid = track.timescale == Some(asc.output_sample_rate_hz)
                    && stts_duration <= coded_samples
                    && coded_samples - stts_duration < u64::from(asc.frame_samples);
                xcheck.push(check(
                    "FORGE-ISOBMFF-AAC-SAMPLE-TIMING",
                    timing_valid,
                    "AAC access-unit count, ASC frame length, media timescale, and stts duration agree",
                    Some(json!({
                        "track_id": track.id,
                        "access_units": access_units,
                        "frame_samples": asc.frame_samples,
                        "coded_samples": coded_samples,
                        "stts_duration": stts_duration,
                        "transport_end_trim_samples": coded_samples - stts_duration,
                        "media_timescale": track.timescale,
                        "output_sample_rate_hz": asc.output_sample_rate_hz
                    })),
                ));
                if let Some(media_time) = track.edit_media_time {
                    let presentation_samples = track
                        .edit_segment_duration
                        .zip(state.movie_timescale)
                        .and_then(|(duration, timescale)| {
                            duration
                                .checked_mul(u64::from(asc.output_sample_rate_hz))
                                .map(|scaled| {
                                    (scaled + u64::from(timescale) / 2) / u64::from(timescale)
                                })
                        });
                    let delay = u64::try_from(media_time).ok();
                    let end_padding =
                        delay
                            .zip(presentation_samples)
                            .and_then(|(delay, presentation)| {
                                coded_samples.checked_sub(delay)?.checked_sub(presentation)
                            });
                    xcheck.push(check(
                        "FORGE-ISOBMFF-AAC-GAPLESS",
                        end_padding.is_some(),
                        "AAC edit-list encoder delay and end padding fit inside coded samples",
                        Some(json!({
                            "track_id": track.id,
                            "coded_samples": coded_samples,
                            "encoder_delay_samples": delay,
                            "presentation_samples": presentation_samples,
                            "end_padding_samples": end_padding
                        })),
                    ));
                }
                if !track.roll_distances.is_empty() {
                    xcheck.push(check(
                        "FORGE-ISOBMFF-AAC-ROLL-GROUP",
                        track.roll_distances.iter().all(|distance| *distance <= 0),
                        "AAC roll/prol sample-group distances describe non-positive decoder pre-roll",
                        Some(json!({
                            "track_id": track.id,
                            "grouping_types": track.sample_group_types,
                            "roll_distances": track.roll_distances
                        })),
                    ));
                }
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
    let timed_id3_aid3_brand = state.compatible_brands.iter().any(|brand| brand == "aid3");

    let properties = json!({
        "file_size_bytes": file_size,
        "top_level_boxes": state.top_level,
        "box_count": state.box_count,
        "major_brand": state.major_brand,
        "compatible_brands": state.compatible_brands,
        "fragmented": state.has_mvex || has_moof,
        "movie_fragments": state.fragments.len(),
        "fragment_sequences": state.fragments.iter()
            .filter_map(|fragment| fragment.sequence)
            .collect::<Vec<_>>(),
        "fragment_decode_times": state.fragments.iter()
            .flat_map(|fragment| fragment.decode_times.iter())
            .map(|(track_id, time)| json!({"track_id": track_id, "time": time}))
            .collect::<Vec<_>>(),
        "fragment_movie_relative": state.fragments.iter()
            .all(|fragment| fragment.movie_relative == Some(true)),
        "movie_duration": state.movie_duration,
        "movie_timescale": state.movie_timescale,
        "track_header_durations": state.tracks.iter()
            .map(|track| track.header_duration)
            .collect::<Vec<_>>(),
        "mvex_after_tracks": state.mvex_after_tracks,
        "media_data_boxes": state.mdat_ranges.len(),
        "tracks": state.tracks.iter().map(track_json).collect::<Vec<_>>(),
        "timed_id3": state.timed_id3.iter().map(|event| {
            serde_json::to_value(event).unwrap_or(Value::Null)
        }).collect::<Vec<_>>(),
        "timed_id3_aid3_compatible_brand": timed_id3_aid3_brand,
        "timed_id3_stored_bytes": state.timed_id3_bytes,
        "timed_id3_evidence_limit_hit": state.timed_id3_limit_hit
    });
    Ok(finish_audit(
        path, "isobmff", wrapper, bitstream, xcheck, properties,
    ))
}

fn parse_event_message(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
) -> Result<Option<TimedId3Event>, String> {
    let body = read_control(path, file, header)?;
    if body.len() < 4 {
        return Err("EventMessageBox is missing its FullBox header".into());
    }
    let version = body[0];
    let flags = u32::from_be_bytes([0, body[1], body[2], body[3]]);
    if flags != 0 || version > 1 {
        return Err(format!(
            "EventMessageBox has unsupported version {version} or non-zero flags {flags:#x}"
        ));
    }
    let (timescale, presentation_time, event_duration, id, scheme, value, message) = if version == 1
    {
        if body.len() < 24 {
            return Err("version 1 EventMessageBox fixed fields are truncated".into());
        }
        let timescale = u32::from_be_bytes(body[4..8].try_into().expect("four-byte slice"));
        let presentation_time =
            u64::from_be_bytes(body[8..16].try_into().expect("eight-byte slice"));
        let event_duration = u32::from_be_bytes(body[16..20].try_into().expect("four-byte slice"));
        let id = u32::from_be_bytes(body[20..24].try_into().expect("four-byte slice"));
        let (scheme, offset) = nul_string(&body, 24, "scheme_id_uri")?;
        let (value, offset) = nul_string(&body, offset, "value")?;
        (
            timescale,
            presentation_time,
            event_duration,
            id,
            scheme,
            value,
            &body[offset..],
        )
    } else {
        let (scheme, offset) = nul_string(&body, 4, "scheme_id_uri")?;
        let (value, offset) = nul_string(&body, offset, "value")?;
        if body.len().saturating_sub(offset) < 16 {
            return Err("version 0 EventMessageBox fixed fields are truncated".into());
        }
        let timescale = u32::from_be_bytes(
            body[offset..offset + 4]
                .try_into()
                .expect("four-byte slice"),
        );
        let presentation_delta = u32::from_be_bytes(
            body[offset + 4..offset + 8]
                .try_into()
                .expect("four-byte slice"),
        );
        let event_duration = u32::from_be_bytes(
            body[offset + 8..offset + 12]
                .try_into()
                .expect("four-byte slice"),
        );
        let id = u32::from_be_bytes(
            body[offset + 12..offset + 16]
                .try_into()
                .expect("four-byte slice"),
        );
        (
            timescale,
            u64::from(presentation_delta),
            event_duration,
            id,
            scheme,
            value,
            &body[offset + 16..],
        )
    };
    if scheme != "https://aomedia.org/emsg/ID3" {
        return Ok(None);
    }
    if version != 1 {
        return Err("AOMedia CMAF timed-ID3 requires EventMessageBox version 1".into());
    }
    if timescale == 0 {
        return Err("CMAF timed-ID3 EventMessageBox has a zero timescale".into());
    }
    let (tag, consumed) = crate::id3_qc::parse_prefix(message, true)?;
    if consumed != message.len() {
        return Err("CMAF timed-ID3 message_data contains bytes after the ID3v2.4 tag".into());
    }
    Ok(Some(TimedId3Event {
        version,
        timescale,
        presentation_time,
        event_duration,
        id,
        scheme_id_uri: scheme,
        value,
        tag,
    }))
}

fn nul_string(bytes: &[u8], offset: usize, field: &str) -> Result<(String, usize), String> {
    let relative = bytes
        .get(offset..)
        .ok_or_else(|| format!("EventMessageBox {field} starts outside the box"))?
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| format!("EventMessageBox {field} is not NUL-terminated"))?;
    let end = offset + relative;
    let value = std::str::from_utf8(&bytes[offset..end])
        .map_err(|_| format!("EventMessageBox {field} is not UTF-8"))?
        .to_owned();
    Ok((value, end + 1))
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
    let last_track = children.iter().rposition(|item| item.kind == *b"trak");
    let first_mvex = children.iter().position(|item| item.kind == *b"mvex");
    state.mvex_after_tracks = first_mvex
        .zip(last_track)
        .is_some_and(|(mvex, track)| mvex > track);
    for child in children {
        match &child.kind {
            b"mvhd" => {
                (state.movie_timescale, state.movie_duration) =
                    parse_mvhd(path, file, child, bitstream)?;
            }
            b"trak" => {
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
            _ => {}
        }
    }
    Ok(())
}

fn parse_mvhd(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    checks: &mut Vec<AuditCheck>,
) -> Result<(Option<u32>, Option<u64>), String> {
    let body = read_control(path, file, header)?;
    let (timescale, duration) = match body.first() {
        Some(0) => (
            body.get(12..16).map(be_u32),
            body.get(16..20).map(be_u32).map(u64::from),
        ),
        Some(1) => (body.get(20..24).map(be_u32), body.get(24..32).map(be_u64)),
        _ => (None, None),
    };
    checks.push(check(
        "FORGE-ISOBMFF-MOVIE-HEADER",
        timescale.is_some_and(|value| value > 0) && duration.is_some(),
        "movie header contains a positive timescale and duration",
        Some(json!({"timescale": timescale, "duration": duration})),
    ));
    Ok((timescale, duration))
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
            b"tkhd" => {
                (track.id, track.header_duration) = parse_tkhd(path, file, child, bitstream)?
            }
            b"mdia" => parse_mdia(path, file, child, box_count, &mut track, bitstream, xcheck)?,
            b"edts" => parse_edts(path, file, child, box_count, &mut track, bitstream)?,
            b"udta" => parse_udta(path, file, child, box_count, &mut track, bitstream)?,
            _ => {}
        }
    }
    Ok(track)
}

fn parse_edts(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    box_count: &mut usize,
    track: &mut Track,
    checks: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let children = list_boxes(path, file, header.body_start, header.end, box_count)?;
    let edit_lists: Vec<_> = children
        .into_iter()
        .filter(|child| child.kind == *b"elst")
        .collect();
    let mut valid = edit_lists.len() <= 1;
    if let Some(header) = edit_lists.first().copied() {
        let body = read_control(path, file, header)?;
        let version = body.first().copied();
        let width = match version {
            Some(0) => 12,
            Some(1) => 20,
            _ => 0,
        };
        let entries = (width > 0)
            .then(|| parse_counted_entries(&body, width))
            .flatten();
        if let Some(entries) = entries {
            let mut selected = None;
            for entry in entries {
                let (segment_duration, media_time, rate_offset) = if width == 12 {
                    (
                        u64::from(be_u32(&entry[..4])),
                        i64::from(i32::from_be_bytes(entry[4..8].try_into().unwrap())),
                        8,
                    )
                } else {
                    (
                        be_u64(&entry[..8]),
                        i64::from_be_bytes(entry[8..16].try_into().unwrap()),
                        16,
                    )
                };
                let rate_integer =
                    i16::from_be_bytes(entry[rate_offset..rate_offset + 2].try_into().unwrap());
                let rate_fraction =
                    i16::from_be_bytes(entry[rate_offset + 2..rate_offset + 4].try_into().unwrap());
                valid &= rate_integer == 1 && rate_fraction == 0;
                if media_time >= 0 && selected.is_none() {
                    selected = Some((media_time, segment_duration));
                }
            }
            if let Some((media_time, segment_duration)) = selected {
                track.edit_media_time = Some(media_time);
                track.edit_segment_duration = Some(segment_duration);
            }
        } else {
            valid = false;
        }
    }
    checks.push(check(
        "FORGE-ISOBMFF-EDIT-LIST",
        valid,
        "track edit list is unique, bounded, and uses normal playback rate",
        Some(json!({
            "count": edit_lists.len(),
            "media_time": track.edit_media_time,
            "segment_duration": track.edit_segment_duration
        })),
    ));
    Ok(())
}

fn parse_udta(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    box_count: &mut usize,
    track: &mut Track,
    checks: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    for child in list_boxes(path, file, header.body_start, header.end, box_count)? {
        if child.kind != *b"ludt" {
            continue;
        }
        track.ludt_count += 1;
        if let Err(error) = parse_ludt(path, file, child, box_count, track, checks) {
            checks.push(check(
                "FORGE-ISOBMFF-LOUDNESS-STRUCTURE",
                false,
                error,
                None,
            ));
        }
    }
    checks.push(check(
        "FORGE-ISOBMFF-LOUDNESS-BOX-COUNT",
        track.ludt_count <= 1,
        if track.ludt_count <= 1 {
            "track user data contains at most one LoudnessBox"
        } else {
            "track user data contains multiple LoudnessBox values"
        },
        Some(json!(track.ludt_count)),
    ));
    Ok(())
}

fn parse_ludt(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    box_count: &mut usize,
    track: &mut Track,
    checks: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let children = list_boxes(path, file, header.body_start, header.end, box_count)?;
    let mut known_children = true;
    let mut version_one_track = 0_usize;
    let mut version_one_album = 0_usize;
    let before = track.loudness.len();
    for child in children {
        let scope = match &child.kind {
            b"tlou" => "track",
            b"alou" => "album",
            _ => {
                known_children = false;
                continue;
            }
        };
        let entries = parse_loudness_base(path, file, child, scope)?;
        if entries.first().is_some_and(|entry| entry.version >= 1) {
            if scope == "track" {
                version_one_track += 1;
            } else {
                version_one_album += 1;
            }
        }
        track.loudness.extend(entries);
    }
    let added = &track.loudness[before..];
    let has_track = added.iter().any(|entry| entry.scope == "track");
    let mut keys = HashSet::new();
    let unique = added.iter().all(|entry| {
        keys.insert((
            entry.scope,
            entry.version,
            entry.eq_set_id,
            entry.downmix_id,
            entry.drc_set_id,
        ))
    });
    let valid =
        known_children && has_track && version_one_track <= 1 && version_one_album <= 1 && unique;
    checks.push(check(
        "FORGE-ISOBMFF-LOUDNESS-STRUCTURE",
        valid,
        if valid {
            "LoudnessBox has bounded, unique track/album loudness entries"
        } else {
            "LoudnessBox has unknown, missing, duplicate, or conflicting loudness entries"
        },
        Some(json!({
            "entries": added.len(),
            "track_entries": added.iter().filter(|entry| entry.scope == "track").count(),
            "album_entries": added.iter().filter(|entry| entry.scope == "album").count(),
            "version_1_track_boxes": version_one_track,
            "version_1_album_boxes": version_one_album
        })),
    ));
    Ok(())
}

fn parse_loudness_base(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    scope: &'static str,
) -> Result<Vec<LoudnessEntry>, String> {
    let body = read_control(path, file, header)?;
    if body.len() < 4 {
        return Err(format!("{} is truncated", header.name()));
    }
    let version = body[0];
    if body[1..4] != [0, 0, 0] {
        return Err(format!("{} FullBox flags must be zero", header.name()));
    }
    let mut offset = 4_usize;
    let count = if version >= 1 {
        let value = *body
            .get(offset)
            .ok_or_else(|| format!("{} is missing loudness_base_count", header.name()))?;
        offset += 1;
        if value & 0xc0 != 0 {
            return Err(format!(
                "{} loudness_base_count reserved bits are non-zero",
                header.name()
            ));
        }
        usize::from(value & 0x3f)
    } else {
        1
    };
    if count == 0 {
        return Err(format!(
            "{} contains no loudness base entries",
            header.name()
        ));
    }

    let mut output = Vec::with_capacity(count);
    for _ in 0..count {
        let eq_set_id = if version >= 1 {
            let value = *body
                .get(offset)
                .ok_or_else(|| format!("{} is missing EQ_set_ID", header.name()))?;
            offset += 1;
            if value & 0xc0 != 0 {
                return Err(format!(
                    "{} EQ_set_ID reserved bits are non-zero",
                    header.name()
                ));
            }
            Some(value & 0x3f)
        } else {
            None
        };
        let ids = body
            .get(offset..offset + 2)
            .ok_or_else(|| format!("{} is missing downmix/DRC IDs", header.name()))?;
        offset += 2;
        let ids = u16::from_be_bytes(ids.try_into().unwrap());
        if ids & 0xe000 != 0 {
            return Err(format!(
                "{} downmix/DRC reserved bits are non-zero",
                header.name()
            ));
        }
        let peaks = body
            .get(offset..offset + 3)
            .ok_or_else(|| format!("{} is missing peak metadata", header.name()))?;
        offset += 3;
        let peaks = (u32::from(peaks[0]) << 16) | (u32::from(peaks[1]) << 8) | u32::from(peaks[2]);
        let systems = *body
            .get(offset)
            .ok_or_else(|| format!("{} is missing true-peak provenance", header.name()))?;
        offset += 1;
        let measurement_count = usize::from(
            *body
                .get(offset)
                .ok_or_else(|| format!("{} is missing measurement_count", header.name()))?,
        );
        offset += 1;
        let measurement_bytes = measurement_count
            .checked_mul(3)
            .ok_or_else(|| format!("{} measurement count overflows", header.name()))?;
        let payload = body
            .get(offset..offset + measurement_bytes)
            .ok_or_else(|| format!("{} measurement list is truncated", header.name()))?;
        offset += measurement_bytes;
        let measurements = payload
            .chunks_exact(3)
            .map(|item| LoudnessMeasurement {
                method_definition: item[0],
                method_value: item[1],
                measurement_system: item[2] >> 4,
                reliability: item[2] & 0x0f,
            })
            .collect();
        output.push(LoudnessEntry {
            scope,
            version,
            eq_set_id,
            downmix_id: ((ids >> 6) & 0x7f) as u8,
            drc_set_id: (ids & 0x3f) as u8,
            sample_peak_code: sign_extend_12((peaks >> 12) as u16),
            true_peak_code: sign_extend_12((peaks & 0x0fff) as u16),
            true_peak_measurement_system: systems >> 4,
            true_peak_reliability: systems & 0x0f,
            measurements,
        });
    }
    if offset != body.len() {
        return Err(format!(
            "{} has {} trailing byte(s)",
            header.name(),
            body.len() - offset
        ));
    }
    Ok(output)
}

fn sign_extend_12(value: u16) -> i16 {
    if value & 0x0800 == 0 {
        value as i16
    } else {
        (value | 0xf000) as i16
    }
}

fn parse_tkhd(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    checks: &mut Vec<AuditCheck>,
) -> Result<(Option<u32>, Option<u64>), String> {
    let body = read_control(path, file, header)?;
    let version = body.first().copied();
    let (id_offset, duration_offset, duration_bytes) = match version {
        Some(0) => (12, 20, 4),
        Some(1) => (20, 28, 8),
        _ => (usize::MAX, usize::MAX, 0),
    };
    let id = body.get(id_offset..id_offset.saturating_add(4)).map(be_u32);
    let duration = match duration_bytes {
        4 => body
            .get(duration_offset..duration_offset + 4)
            .map(be_u32)
            .map(u64::from),
        8 => body.get(duration_offset..duration_offset + 8).map(be_u64),
        _ => None,
    };
    checks.push(check(
        "FORGE-ISOBMFF-TRACK-HEADER",
        id.is_some_and(|value| value != 0) && duration.is_some(),
        "track header has a non-zero track ID and duration",
        Some(json!({"track_id": id, "duration": duration})),
    ));
    Ok((id, duration))
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
            b"sgpd" => parse_sgpd(path, file, child, track, bitstream)?,
            b"sbgp" => parse_sbgp(path, file, child, track, bitstream)?,
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
            match sample_entry_child_boxes(&body[offset..offset + size]) {
                Ok(children) => {
                    let drc: Vec<_> = children
                        .iter()
                        .filter(|(kind, _)| {
                            matches!(
                                kind.as_str(),
                                "udc1" | "udc2" | "udi1" | "udi2" | "udex" | "dmix"
                            )
                        })
                        .map(|(kind, _)| kind.clone())
                        .collect();
                    if !drc.is_empty() {
                        checks.push(check(
                            "FORGE-ISOBMFF-MPEG-D-DRC-STRUCTURE",
                            true,
                            "MPEG-D DRC boxes have bounded FullBox payloads and zero flags",
                            Some(json!(drc)),
                        ));
                    }
                    track.drc_boxes.extend(drc);
                    if track.codecs.last().is_some_and(|codec| codec == "mp4a") {
                        let asc = children
                            .iter()
                            .find(|(kind, _)| kind == "esds")
                            .and_then(|(_, payload)| decoder_specific_info(payload).ok())
                            .and_then(|bytes| crate::aac_qc::parse_asc_bytes(bytes).ok());
                        checks.push(check(
                            "FORGE-ISOBMFF-AAC-ASC",
                            asc.is_some(),
                            "mp4a sample entry contains a bounded AudioSpecificConfig",
                            asc.as_ref().map(|value| json!(value)),
                        ));
                        track.aac_config = asc;
                    }
                }
                Err(()) => {
                    valid = false;
                    checks.push(check(
                        "FORGE-ISOBMFF-MPEG-D-DRC-STRUCTURE",
                        false,
                        "audio sample-entry child boxes are malformed or have invalid DRC FullBox fields",
                        None,
                    ));
                }
            }
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
    if !track.drc_boxes.is_empty() {
        let basic = track.drc_boxes.iter().any(|kind| kind == "udc1")
            == track.drc_boxes.iter().any(|kind| kind == "udi1");
        let unified = track.drc_boxes.iter().any(|kind| kind == "udc2")
            == track.drc_boxes.iter().any(|kind| kind == "udi2");
        checks.push(check(
            "FORGE-ISOBMFF-MPEG-D-DRC-PAIRING",
            basic && unified,
            if basic && unified {
                "MPEG-D DRC coefficient and instruction boxes are paired"
            } else {
                "MPEG-D DRC coefficient and instruction boxes must be paired by profile"
            },
            Some(json!(track.drc_boxes)),
        ));
    }
    Ok(())
}

fn sample_entry_child_boxes(entry: &[u8]) -> Result<Vec<(String, &[u8])>, ()> {
    if entry.len() < 36 {
        return Err(());
    }
    let version = u16::from_be_bytes(entry[16..18].try_into().unwrap());
    let mut offset = match version {
        0 => 36,
        1 => 52,
        2 => 72,
        _ => return Err(()),
    };
    if offset > entry.len() {
        return Err(());
    }
    let mut output = Vec::new();
    while offset < entry.len() {
        if entry.len() - offset < 8 {
            return Err(());
        }
        let size32 = be_u32(&entry[offset..offset + 4]);
        let kind = fourcc(&entry[offset + 4..offset + 8]);
        let (header_size, size) = if size32 == 1 {
            if entry.len() - offset < 16 {
                return Err(());
            }
            (
                16_usize,
                usize::try_from(be_u64(&entry[offset + 8..offset + 16])).map_err(|_| ())?,
            )
        } else if size32 == 0 {
            (8, entry.len() - offset)
        } else {
            (8, usize::try_from(size32).map_err(|_| ())?)
        };
        if size < header_size || size > entry.len() - offset {
            return Err(());
        }
        if matches!(
            kind.as_str(),
            "udc1" | "udc2" | "udi1" | "udi2" | "udex" | "dmix"
        ) && (size < header_size + 4
            || entry[offset + header_size + 1..offset + header_size + 4] != [0, 0, 0])
        {
            return Err(());
        }
        output.push((kind, &entry[offset + header_size..offset + size]));
        offset += size;
    }
    Ok(output)
}

fn decoder_specific_info(esds_body: &[u8]) -> Result<&[u8], ()> {
    if esds_body.len() < 4 || esds_body[1..4] != [0, 0, 0] {
        return Err(());
    }
    let (tag, payload, _) = descriptor(esds_body, 4)?;
    if tag != 0x03 || payload.len() < 3 {
        return Err(());
    }
    let flags = payload[2];
    let mut offset = 3;
    if flags & 0x80 != 0 {
        offset += 2;
    }
    if flags & 0x40 != 0 {
        let length = usize::from(*payload.get(offset).ok_or(())?);
        offset = offset.checked_add(1 + length).ok_or(())?;
    }
    if flags & 0x20 != 0 {
        offset += 2;
    }
    while offset < payload.len() {
        let (child_tag, child, end) = descriptor(payload, offset)?;
        if child_tag == 0x04 {
            if child.len() < 13 {
                return Err(());
            }
            let mut decoder_offset = 13;
            while decoder_offset < child.len() {
                let (decoder_tag, decoder_payload, decoder_end) =
                    descriptor(child, decoder_offset)?;
                if decoder_tag == 0x05 {
                    return Ok(decoder_payload);
                }
                decoder_offset = decoder_end;
            }
        }
        offset = end;
    }
    Err(())
}

fn descriptor(data: &[u8], offset: usize) -> Result<(u8, &[u8], usize), ()> {
    let tag = *data.get(offset).ok_or(())?;
    let mut cursor = offset + 1;
    let mut length = 0_usize;
    let mut complete = false;
    for _ in 0..4 {
        let byte = *data.get(cursor).ok_or(())?;
        cursor += 1;
        length = length.checked_shl(7).ok_or(())? | usize::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            complete = true;
            break;
        }
    }
    if !complete {
        return Err(());
    }
    let end = cursor.checked_add(length).ok_or(())?;
    Ok((tag, data.get(cursor..end).ok_or(())?, end))
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

fn parse_sgpd(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    track: &mut Track,
    checks: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let body = read_control(path, file, header)?;
    let version = body.first().copied();
    let grouping_type = body.get(4..8).map(fourcc);
    let (mut offset, default_length, entry_count) = match version {
        Some(0) if body.len() >= 12 => (12, None, be_u32(&body[8..12]) as usize),
        Some(1) if body.len() >= 16 => (
            16,
            Some(be_u32(&body[8..12]) as usize),
            be_u32(&body[12..16]) as usize,
        ),
        Some(2) if body.len() >= 20 => (
            20,
            Some(be_u32(&body[8..12]) as usize),
            be_u32(&body[16..20]) as usize,
        ),
        _ => (body.len(), None, 0),
    };
    let mut valid = grouping_type.is_some() && entry_count <= MAX_TABLE_ENTRIES;
    let mut roll_distances = Vec::new();
    for _ in 0..entry_count {
        let length = match default_length {
            Some(0) => {
                if body.len().saturating_sub(offset) < 4 {
                    valid = false;
                    break;
                }
                let value = be_u32(&body[offset..offset + 4]) as usize;
                offset += 4;
                value
            }
            Some(value) => value,
            None if matches!(grouping_type.as_deref(), Some("roll" | "prol")) => 2,
            None => {
                valid = false;
                break;
            }
        };
        if length > body.len().saturating_sub(offset) {
            valid = false;
            break;
        }
        if matches!(grouping_type.as_deref(), Some("roll" | "prol")) {
            if length != 2 {
                valid = false;
                break;
            }
            roll_distances.push(i16::from_be_bytes(
                body[offset..offset + 2].try_into().unwrap(),
            ));
        }
        offset += length;
    }
    valid &= offset == body.len();
    if let Some(grouping_type) = grouping_type {
        track.sample_group_types.push(grouping_type);
    }
    track.roll_distances.extend(roll_distances);
    checks.push(check(
        "FORGE-ISOBMFF-SAMPLE-GROUP-DESCRIPTION",
        valid,
        "sample-group descriptions are bounded and roll/prol entries have signed distances",
        Some(json!({
            "grouping_types": track.sample_group_types,
            "roll_distances": track.roll_distances
        })),
    ));
    Ok(())
}

fn parse_sbgp(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    track: &mut Track,
    checks: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let body = read_control(path, file, header)?;
    let version = body.first().copied();
    let grouping_type = body.get(4..8).map(fourcc);
    let offset = match version {
        Some(0) => 8,
        Some(1) => 12,
        _ => body.len(),
    };
    let entries = body.get(offset..).and_then(|tail| {
        let count = tail
            .get(..4)
            .map(be_u32)
            .and_then(|value| usize::try_from(value).ok())?;
        if count > MAX_TABLE_ENTRIES || tail.len() != 4_usize.checked_add(count.checked_mul(8)?)? {
            return None;
        }
        Some(tail[4..].chunks_exact(8).collect::<Vec<_>>())
    });
    let mut samples = 0_u64;
    let mut valid = grouping_type.is_some();
    if let Some(entries) = entries {
        for entry in entries {
            let count = u64::from(be_u32(&entry[..4]));
            let description_index = be_u32(&entry[4..8]);
            valid &= count > 0 && description_index <= MAX_TABLE_ENTRIES as u32;
            samples = samples.saturating_add(count);
        }
        track.sample_group_samples = Some(samples);
    } else {
        valid = false;
    }
    checks.push(check(
        "FORGE-ISOBMFF-SAMPLE-TO-GROUP",
        valid,
        "sample-to-group runs are bounded and use valid positive sample counts",
        Some(json!({"grouping_type": grouping_type, "samples": samples})),
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
                if body.len() >= 8 {
                    let flags =
                        (u32::from(body[1]) << 16) | (u32::from(body[2]) << 8) | u32::from(body[3]);
                    let relative = flags & 0x000001 == 0 && flags & 0x020000 != 0;
                    fragment.movie_relative =
                        Some(fragment.movie_relative.unwrap_or(true) && relative);
                }
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
        "track_header_duration": track.header_duration,
        "handler": track.handler.map(|value| fourcc(&value)),
        "timescale": track.timescale,
        "duration": track.duration,
        "codecs": track.codecs,
        "channels": track.channels,
        "sample_rate_hz": track.sample_rate,
        "sample_count": track.sample_count,
        "sample_bytes": track.sample_bytes,
        "chunk_count": track.chunk_offsets.len(),
        "chunk_samples": track.chunk_samples,
        "loudness_box_count": track.ludt_count,
        "loudness": track.loudness.iter().map(loudness_json).collect::<Vec<_>>(),
        "mpeg_d_drc_boxes": track.drc_boxes,
        "aac_audio_specific_config": track.aac_config,
        "edit_media_time": track.edit_media_time,
        "edit_segment_duration": track.edit_segment_duration,
        "sample_group_types": track.sample_group_types,
        "roll_distances": track.roll_distances,
        "sample_group_samples": track.sample_group_samples
    })
}

fn loudness_json(entry: &LoudnessEntry) -> Value {
    json!({
        "scope": entry.scope,
        "version": entry.version,
        "eq_set_id": entry.eq_set_id,
        "downmix_id": entry.downmix_id,
        "drc_set_id": entry.drc_set_id,
        "sample_peak_code": entry.sample_peak_code,
        "true_peak_code": entry.true_peak_code,
        "true_peak_measurement_system": entry.true_peak_measurement_system,
        "true_peak_reliability": entry.true_peak_reliability,
        "measurements": entry.measurements.iter().map(|measurement| json!({
            "method_definition": measurement.method_definition,
            "method_value": measurement.method_value,
            "value_lkfs": matches!(measurement.method_definition, 0..=5)
                .then(|| -57.75 + f64::from(measurement.method_value) * 0.25),
            "measurement_system": measurement.measurement_system,
            "reliability": measurement.reliability
        })).collect::<Vec<_>>()
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

    fn aac_esds() -> Vec<u8> {
        let decoder_config = [vec![0x40, 0x15], vec![0; 11], vec![0x05, 0x02, 0x11, 0x90]].concat();
        let es_descriptor = [
            vec![0x00, 0x01, 0x00],
            vec![0x04, u8::try_from(decoder_config.len()).unwrap()],
            decoder_config,
            vec![0x06, 0x01, 0x02],
        ]
        .concat();
        boxed(
            b"esds",
            full_box(
                0,
                [
                    vec![0x03, u8::try_from(es_descriptor.len()).unwrap()],
                    es_descriptor,
                ]
                .concat(),
            ),
        )
    }

    fn minimal_audio_mp4(chunk_offset: u32) -> Vec<u8> {
        minimal_audio_mp4_with_metadata(chunk_offset, Vec::new(), Vec::new())
    }

    fn minimal_audio_mp4_with_metadata(
        chunk_offset: u32,
        track_user_data: Vec<u8>,
        sample_entry_children: Vec<u8>,
    ) -> Vec<u8> {
        minimal_audio_mp4_advanced(
            chunk_offset,
            track_user_data,
            sample_entry_children,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    }

    fn minimal_audio_mp4_advanced(
        chunk_offset: u32,
        track_user_data: Vec<u8>,
        sample_entry_children: Vec<u8>,
        track_children: Vec<u8>,
        stbl_children: Vec<u8>,
        movie_children: Vec<u8>,
    ) -> Vec<u8> {
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
        sample_entry.extend(aac_esds());
        sample_entry.extend(sample_entry_children);
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
        let stbl = boxed(
            b"stbl",
            [stsd, stts, stsz, stsc, stco, stbl_children].concat(),
        );
        let minf = boxed(b"minf", stbl);
        let mdia = boxed(b"mdia", [mdhd, hdlr, minf].concat());
        let trak = boxed(
            b"trak",
            [tkhd, mdia, track_user_data, track_children].concat(),
        );
        let moov = boxed(b"moov", [movie_children, trak].concat());
        let mdat = boxed(b"mdat", vec![1, 2, 3, 4]);
        [ftyp, moov, mdat].concat()
    }

    fn media_fragment(sequence: u32, decode_time: u64) -> Vec<u8> {
        let mfhd = boxed(b"mfhd", full_box(0, sequence.to_be_bytes().to_vec()));
        let tfhd = boxed(
            b"tfhd",
            [vec![0, 2, 0, 0], 1_u32.to_be_bytes().to_vec()].concat(),
        );
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

    fn timed_id3_emsg(version: u8) -> Vec<u8> {
        let rva2_payload = [b"track".as_slice(), &[0, 1, 0xfe, 0x00, 0]].concat();
        let rva2 = [
            b"RVA2".as_slice(),
            &[0, 0, 0, rva2_payload.len() as u8, 0, 0],
            &rva2_payload,
        ]
        .concat();
        let id3 = [
            b"ID3\x04\x00\x00".as_slice(),
            &[0, 0, 0, rva2.len() as u8],
            &rva2,
        ]
        .concat();
        let body = if version == 1 {
            [
                full_box(
                    1,
                    [
                        1_000_u32.to_be_bytes().as_slice(),
                        500_u64.to_be_bytes().as_slice(),
                        u32::MAX.to_be_bytes().as_slice(),
                        7_u32.to_be_bytes().as_slice(),
                    ]
                    .concat(),
                ),
                b"https://aomedia.org/emsg/ID3\0".to_vec(),
                b"urn:example:loudness\0".to_vec(),
                id3,
            ]
            .concat()
        } else {
            [
                full_box(0, Vec::new()),
                b"https://aomedia.org/emsg/ID3\0".to_vec(),
                b"urn:example:loudness\0".to_vec(),
                [
                    1_000_u32.to_be_bytes().as_slice(),
                    500_u32.to_be_bytes().as_slice(),
                    u32::MAX.to_be_bytes().as_slice(),
                    7_u32.to_be_bytes().as_slice(),
                ]
                .concat(),
                id3,
            ]
            .concat()
        };
        boxed(b"emsg", body)
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
    fn parses_cmaf_timed_id3_rva2_and_aid3_brand() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("timed-id3.m4s");
        let bytes = [
            boxed(
                b"styp",
                [b"msdh".as_slice(), &[0; 4], b"msdh", b"aid3"].concat(),
            ),
            timed_id3_emsg(1),
            media_fragment(1, 0),
            boxed(b"mdat", vec![1, 2, 3, 4]),
        ]
        .concat();
        File::create(&path).unwrap().write_all(&bytes).unwrap();
        let result = crate::container_qc::audit(&path).unwrap();
        assert!(result.passed, "{result:#?}");
        assert_eq!(result.properties["timed_id3_aid3_compatible_brand"], true);
        assert_eq!(result.properties["timed_id3"][0]["version"], 1);
        assert_eq!(
            result.properties["timed_id3"][0]["tag"]["relative_volume_adjustments"][0]
                ["identification"],
            "track"
        );
    }

    #[test]
    fn treats_missing_recommended_aid3_brand_as_non_fatal() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("timed-id3-no-brand.m4s");
        let bytes = [
            boxed(b"styp", [b"msdh".as_slice(), &[0; 4], b"msdh"].concat()),
            timed_id3_emsg(1),
            media_fragment(1, 0),
            boxed(b"mdat", vec![1, 2, 3, 4]),
        ]
        .concat();
        File::create(&path).unwrap().write_all(&bytes).unwrap();
        let result = crate::container_qc::audit(&path).unwrap();
        assert!(result.passed, "{result:#?}");
        assert_eq!(result.properties["timed_id3_aid3_compatible_brand"], false);
    }

    #[test]
    fn rejects_version_zero_cmaf_timed_id3() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bad-timed-id3.m4s");
        let bytes = [
            boxed(
                b"styp",
                [b"msdh".as_slice(), &[0; 4], b"msdh", b"aid3"].concat(),
            ),
            timed_id3_emsg(0),
            media_fragment(1, 0),
            boxed(b"mdat", vec![1, 2, 3, 4]),
        ]
        .concat();
        File::create(&path).unwrap().write_all(&bytes).unwrap();
        let result = crate::container_qc::audit(&path).unwrap();
        assert!(!result.passed);
        assert!(result
            .layers
            .iter()
            .flat_map(|layer| &layer.checks)
            .any(|check| check.rule_id == "FORGE-ISOBMFF-EVENT-MESSAGE" && !check.passed));
    }

    #[test]
    fn reconciles_aac_edit_list_and_roll_sample_groups() {
        let mvhd = boxed(
            b"mvhd",
            full_box(
                0,
                [
                    vec![0; 8],
                    1_000_u32.to_be_bytes().to_vec(),
                    21_u32.to_be_bytes().to_vec(),
                ]
                .concat(),
            ),
        );
        let elst = boxed(
            b"elst",
            full_box(
                0,
                [
                    1_u32.to_be_bytes().to_vec(),
                    21_u32.to_be_bytes().to_vec(),
                    16_i32.to_be_bytes().to_vec(),
                    1_i16.to_be_bytes().to_vec(),
                    0_i16.to_be_bytes().to_vec(),
                ]
                .concat(),
            ),
        );
        let edts = boxed(b"edts", elst);
        let sgpd = boxed(
            b"sgpd",
            full_box(
                1,
                [
                    b"roll".to_vec(),
                    2_u32.to_be_bytes().to_vec(),
                    1_u32.to_be_bytes().to_vec(),
                    (-1_i16).to_be_bytes().to_vec(),
                ]
                .concat(),
            ),
        );
        let sbgp = boxed(
            b"sbgp",
            full_box(
                0,
                [
                    b"roll".to_vec(),
                    1_u32.to_be_bytes().to_vec(),
                    1_u32.to_be_bytes().to_vec(),
                    1_u32.to_be_bytes().to_vec(),
                ]
                .concat(),
            ),
        );
        let preliminary = minimal_audio_mp4_advanced(
            0,
            Vec::new(),
            Vec::new(),
            edts.clone(),
            [sgpd.clone(), sbgp.clone()].concat(),
            mvhd.clone(),
        );
        let mdat_start = preliminary
            .windows(4)
            .position(|window| window == b"mdat")
            .unwrap() as u32
            + 4;
        let bytes = minimal_audio_mp4_advanced(
            mdat_start,
            Vec::new(),
            Vec::new(),
            edts,
            [sgpd, sbgp].concat(),
            mvhd,
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("gapless.m4a");
        File::create(&path).unwrap().write_all(&bytes).unwrap();

        let result = crate::container_qc::audit(&path).unwrap();
        assert!(result.passed, "{result:#?}");
        let checks = result
            .layers
            .iter()
            .flat_map(|layer| &layer.checks)
            .collect::<Vec<_>>();
        assert!(checks
            .iter()
            .any(|check| check.rule_id == "FORGE-ISOBMFF-AAC-GAPLESS" && check.passed));
        assert!(checks
            .iter()
            .any(|check| check.rule_id == "FORGE-ISOBMFF-AAC-ROLL-GROUP" && check.passed));
    }

    #[test]
    fn parses_version_one_loudness_and_paired_mpeg_d_drc_boxes() {
        let mut loudness = vec![1, 0, 0, 0, 1, 0];
        loudness.extend_from_slice(&0_u16.to_be_bytes());
        loudness.extend_from_slice(&[0, 0, 0]);
        loudness.push(0x21);
        loudness.push(1);
        loudness.extend_from_slice(&[2, 100, 0x23]);
        let user_data = boxed(b"udta", boxed(b"ludt", boxed(b"tlou", loudness)));
        let drc = [
            boxed(b"udc2", full_box(0, Vec::new())),
            boxed(b"udi2", full_box(0, Vec::new())),
        ]
        .concat();
        let preliminary = minimal_audio_mp4_with_metadata(0, user_data.clone(), drc.clone());
        let mdat_start = preliminary
            .windows(4)
            .position(|window| window == b"mdat")
            .unwrap() as u32
            + 4;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("loudness.m4a");
        File::create(&path)
            .unwrap()
            .write_all(&minimal_audio_mp4_with_metadata(mdat_start, user_data, drc))
            .unwrap();

        let result = crate::container_qc::audit(&path).unwrap();
        assert!(result.passed, "{result:#?}");
        let track = &result.properties["tracks"][0];
        assert_eq!(track["loudness_box_count"], 1);
        assert_eq!(track["loudness"][0]["scope"], "track");
        assert_eq!(
            track["loudness"][0]["measurements"][0]["method_definition"],
            2
        );
        assert_eq!(track["mpeg_d_drc_boxes"], json!(["udc2", "udi2"]));
    }

    #[test]
    fn parses_version_zero_track_and_album_loudness() {
        let payload = full_box(
            0,
            [
                0_u16.to_be_bytes().as_slice(),
                &[0xff, 0xf8, 0x00],
                &[0x21, 1, 1, 96, 0x23],
            ]
            .concat(),
        );
        let user_data = boxed(
            b"udta",
            boxed(
                b"ludt",
                [boxed(b"tlou", payload.clone()), boxed(b"alou", payload)].concat(),
            ),
        );
        let preliminary = minimal_audio_mp4_with_metadata(0, user_data.clone(), Vec::new());
        let mdat_start = preliminary
            .windows(4)
            .position(|window| window == b"mdat")
            .unwrap() as u32
            + 4;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("album-loudness.m4a");
        File::create(&path)
            .unwrap()
            .write_all(&minimal_audio_mp4_with_metadata(
                mdat_start,
                user_data,
                Vec::new(),
            ))
            .unwrap();

        let result = crate::container_qc::audit(&path).unwrap();
        assert!(result.passed, "{result:#?}");
        let loudness = result.properties["tracks"][0]["loudness"]
            .as_array()
            .unwrap();
        assert_eq!(loudness.len(), 2);
        assert_eq!(loudness[0]["sample_peak_code"], -1);
        assert_eq!(loudness[0]["true_peak_code"], -2048);
        assert_eq!(loudness[1]["scope"], "album");
    }

    #[test]
    fn rejects_nonzero_loudness_reserved_bits() {
        let loudness = vec![1, 0, 0, 0, 0xc1];
        let user_data = boxed(b"udta", boxed(b"ludt", boxed(b"tlou", loudness)));
        let preliminary = minimal_audio_mp4_with_metadata(0, user_data.clone(), Vec::new());
        let mdat_start = preliminary
            .windows(4)
            .position(|window| window == b"mdat")
            .unwrap() as u32
            + 4;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invalid-loudness.m4a");
        File::create(&path)
            .unwrap()
            .write_all(&minimal_audio_mp4_with_metadata(
                mdat_start,
                user_data,
                Vec::new(),
            ))
            .unwrap();

        let result = crate::container_qc::audit(&path).unwrap();
        assert!(!result.passed);
        assert!(result
            .layers
            .iter()
            .flat_map(|layer| &layer.checks)
            .any(|item| item.rule_id == "FORGE-ISOBMFF-LOUDNESS-STRUCTURE" && !item.passed));
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
