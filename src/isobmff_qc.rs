//! Bounded-memory ISO Base Media File Format structural and audio-track QC.

use crate::container_qc::{check, finish_audit, AuditCheck, ContainerAudit};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom};
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
    sample_sizes: Vec<u32>,
    sample_durations: Vec<u32>,
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
    roll_default_group: bool,
    roll_default_description_index: Option<u32>,
    roll_sample_runs: Vec<(u64, u32)>,
    cenc_group_entries: Vec<CencSampleGroupEntry>,
    cenc_default_description_index: Option<u32>,
    cenc_sample_runs: Vec<(u64, u32)>,
    cenc_sbgp_seen: bool,
    has_sync_sample_box: bool,
    has_composition_offsets: bool,
    iamf_entries: Vec<Option<IamfSampleEntry>>,
    cenc_auxiliary: CencAuxiliary,
}

#[derive(Clone, Default)]
struct IamfSampleEntry {
    sample_entry_type: String,
    channel_count: Option<u16>,
    sample_rate: Option<u32>,
    configuration_version: Option<u8>,
    config_obus: Vec<u8>,
    config_trailing_bytes: usize,
    configuration_boxes: usize,
    has_sampling_rate_box: bool,
    protection: Option<CencProtection>,
}

#[derive(Clone, Default)]
struct CencProtection {
    original_format: Option<String>,
    scheme: Option<String>,
    scheme_version: Option<u32>,
    tenc_version: Option<u8>,
    default_is_protected: Option<u8>,
    per_sample_iv_size: Option<u8>,
    default_kid: Option<String>,
    crypt_byte_block: Option<u8>,
    skip_byte_block: Option<u8>,
    constant_iv_size: Option<u8>,
    valid: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CencSampleGroupEntry {
    is_protected: u8,
    per_sample_iv_size: u8,
    kid: String,
    crypt_byte_block: u8,
    skip_byte_block: u8,
    constant_iv_size: Option<u8>,
    valid: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EffectiveCencProtection {
    per_sample_iv_size: u8,
    kid: String,
    crypt_byte_block: u8,
    skip_byte_block: u8,
    constant_iv_size: Option<u8>,
    sample_group_override: bool,
}

#[derive(Clone, Default)]
struct CencAuxiliary {
    senc: Vec<CencSampleEncryption>,
    saiz: Vec<CencSampleAuxiliarySizes>,
    saio: Vec<CencSampleAuxiliaryOffsets>,
}

#[derive(Clone)]
struct CencSampleEncryption {
    flags: u32,
    sample_count: u32,
    iv_size_override: Option<u8>,
    entry_bytes: usize,
}

#[derive(Clone)]
struct CencSampleAuxiliarySizes {
    auxiliary_type: Option<String>,
    default_size: u8,
    sample_count: u32,
    sizes: Vec<u8>,
}

#[derive(Clone)]
struct CencSampleAuxiliaryOffsets {
    auxiliary_type: Option<String>,
    offsets: Vec<u64>,
}

#[derive(Clone, Copy)]
struct SampleLocation {
    offset: u64,
    size: u64,
    description_index: u32,
}

#[derive(Clone, Copy, Default)]
struct TrackExtendsDefaults {
    description_index: u32,
    duration: u32,
    size: u32,
    flags: u32,
}

#[derive(Default)]
struct TrackFragment {
    track_id: Option<u32>,
    decode_time: Option<u64>,
    declared_sample_count: u64,
    samples_resolved: bool,
    samples: Vec<SampleLocation>,
    sample_durations: Vec<u32>,
    sample_flags: Vec<u32>,
    roll_distances: Vec<i16>,
    roll_default_description_index: Option<u32>,
    roll_sample_runs: Vec<(u64, u32)>,
    cenc_group_entries: Vec<CencSampleGroupEntry>,
    cenc_default_description_index: Option<u32>,
    cenc_sample_runs: Vec<(u64, u32)>,
    cenc_sbgp_seen: bool,
    has_composition_offsets: bool,
    cenc_auxiliary: CencAuxiliary,
}

#[derive(Clone, Copy)]
struct IamfEntryTiming {
    duration_ticks: u64,
    roll_distance: Option<i64>,
}

enum IamfSegment {
    Memory(Vec<u8>),
    File { offset: u64, size: u64 },
}

struct IamfTrackReader<'a> {
    file: &'a mut File,
    segments: Vec<IamfSegment>,
    segment_index: usize,
    segment_offset: u64,
}

struct IamfContainerContext<'a> {
    mdat_ranges: &'a [(u64, u64)],
    compatible_brands: &'a [String],
    fragmented: bool,
    fragments: &'a [Fragment],
    movie_timescale: Option<u32>,
}

impl Read for IamfTrackReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            let Some(segment) = self.segments.get(self.segment_index) else {
                return Ok(0);
            };
            let segment_size = match segment {
                IamfSegment::Memory(bytes) => bytes.len() as u64,
                IamfSegment::File { size, .. } => *size,
            };
            if self.segment_offset == segment_size {
                self.segment_index += 1;
                self.segment_offset = 0;
                continue;
            }
            let remaining = segment_size - self.segment_offset;
            let amount = usize::try_from(remaining.min(output.len() as u64))
                .expect("read amount is bounded by the output buffer");
            match segment {
                IamfSegment::Memory(bytes) => {
                    let start = usize::try_from(self.segment_offset)
                        .expect("memory segment offset fits usize");
                    output[..amount].copy_from_slice(&bytes[start..start + amount]);
                }
                IamfSegment::File { offset, .. } => {
                    self.file
                        .seek(SeekFrom::Start(offset + self.segment_offset))?;
                    self.file.read_exact(&mut output[..amount])?;
                }
            }
            self.segment_offset += amount as u64;
            return Ok(amount);
        }
    }
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
    start: u64,
    sequence: Option<u32>,
    track_ids: Vec<u32>,
    decode_times: Vec<(u32, u64)>,
    sample_count: u64,
    movie_relative: Option<bool>,
    tracks: Vec<TrackFragment>,
}

struct FragmentParseContext<'a> {
    moof_start: u64,
    track_extends: &'a HashMap<u32, TrackExtendsDefaults>,
    box_count: &'a mut usize,
    implicit_data_offset: &'a mut Option<u64>,
    fragment: &'a mut Fragment,
    checks: &'a mut Vec<AuditCheck>,
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
    track_extends: HashMap<u32, TrackExtendsDefaults>,
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
            let iamf = track.codecs.iter().any(|codec| codec == "iamf");
            bitstream.push(check(
                "FORGE-ISOBMFF-AUDIO-DESCRIPTION",
                !track.codecs.is_empty()
                    && track.timescale.is_some_and(|value| value > 0)
                    && if iamf {
                        track.channels == Some(0) && track.sample_rate == Some(0)
                    } else {
                        track.channels.is_none_or(|value| value > 0)
                            && track.sample_rate.is_none_or(|value| value > 0)
                    },
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
            if !state.has_mvex && !has_moof {
                if let (Some(stts_duration), Some(duration)) = (track.stts_duration, track.duration)
                {
                    xcheck.push(check(
                        "FORGE-ISOBMFF-DURATION-XCHECK",
                        stts_duration == duration,
                        "media duration matches the time-to-sample table",
                        Some(
                            json!({"track_id": track.id, "mdhd": duration, "stts": stts_duration}),
                        ),
                    ));
                }
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
    let iamf_tracks = audit_iamf_tracks(
        path,
        &mut file,
        &state.tracks,
        IamfContainerContext {
            mdat_ranges: &state.mdat_ranges,
            compatible_brands: &state.compatible_brands,
            fragmented: state.has_mvex || has_moof,
            fragments: &state.fragments,
            movie_timescale: state.movie_timescale,
        },
        (&mut bitstream, &mut xcheck),
    );

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
        "timed_id3_evidence_limit_hit": state.timed_id3_limit_hit,
        "iamf_tracks": iamf_tracks
    });
    Ok(finish_audit(
        path, "isobmff", wrapper, bitstream, xcheck, properties,
    ))
}

fn audit_iamf_tracks(
    path: &Path,
    file: &mut File,
    tracks: &[Track],
    context: IamfContainerContext<'_>,
    checks: (&mut Vec<AuditCheck>, &mut Vec<AuditCheck>),
) -> Vec<Value> {
    let (bitstream, xcheck) = checks;
    let iamf_track_count = tracks
        .iter()
        .filter(|track| track.iamf_entries.iter().any(Option::is_some))
        .count();
    let iamf_brand = context
        .compatible_brands
        .iter()
        .any(|brand| brand == "iamf");
    if iamf_brand || iamf_track_count > 0 {
        bitstream.push(check(
            "FORGE-ISOBMFF-IAMF-TRACK",
            iamf_track_count > 0,
            if iamf_track_count > 0 {
                format!("{iamf_track_count} IAMF track(s) resolve through iamf or enca/frma")
            } else {
                "the iamf compatible brand requires an iamf or enca/frma=iamf sample entry"
                    .to_string()
            },
            Some(json!({
                "iamf_brand": iamf_brand,
                "iamf_tracks": iamf_track_count
            })),
        ));
    }
    if iamf_track_count == 0 {
        return Vec::new();
    }
    bitstream.push(check(
        "FORGE-ISOBMFF-IAMF-BRAND",
        iamf_brand,
        "ISO-BMFF IAMF files declare iamf in the compatible brands array",
        Some(json!({
            "compatible_brands": context.compatible_brands,
            "iamf_tracks": iamf_track_count
        })),
    ));

    let mut observations = Vec::new();
    for track in tracks
        .iter()
        .filter(|track| track.iamf_entries.iter().any(Option::is_some))
    {
        let iamf_entries: Vec<_> = track.iamf_entries.iter().flatten().collect();
        let encrypted_entries = iamf_entries
            .iter()
            .filter(|entry| entry.protection.is_some())
            .count();
        let encrypted = encrypted_entries > 0;
        let protection_declarations_valid = !encrypted
            || encrypted_entries == iamf_entries.len()
                && iamf_entries
                    .iter()
                    .all(|entry| entry.protection.as_ref().is_some_and(|item| item.valid));
        if context.fragmented && context.fragments.is_empty() {
            if encrypted {
                bitstream.push(check(
                    "FORGE-ISOBMFF-IAMF-CENC",
                    protection_declarations_valid,
                    "encrypted IAMF initialization segment declares one valid cenc/cbcs full-sample policy for every sample entry",
                    Some(json!({
                        "track_id": track.id,
                        "sample_entries": iamf_entries.len(),
                        "encrypted_sample_entries": encrypted_entries
                    })),
                ));
            }
            bitstream.push(check(
                "FORGE-ISOBMFF-IAMF-SAMPLE-DATA",
                protection_declarations_valid,
                if encrypted {
                    "encrypted IAMF initialization segment intentionally contains no IA Samples"
                } else {
                    "IAMF initialization segment intentionally contains no IA Samples"
                },
                Some(json!({"track_id": track.id, "initialization_segment": true})),
            ));
            observations.push(json!({
                "track_id": track.id,
                "fragmented": true,
                "initialization_segment": true,
                "encrypted": encrypted,
                "validated_samples": 0
            }));
            continue;
        }

        let samples = match iamf_sample_set(track, context.fragments, context.fragmented) {
            Ok(samples) => samples,
            Err(error) => {
                bitstream.push(check(
                    "FORGE-ISOBMFF-IAMF-SAMPLE-DATA",
                    false,
                    error,
                    Some(json!({"track_id": track.id})),
                ));
                continue;
            }
        };
        let locations = &samples.locations;
        let ranges_valid = locations.iter().all(|sample| {
            sample.offset.checked_add(sample.size).is_some_and(|end| {
                context
                    .mdat_ranges
                    .iter()
                    .any(|(start, limit)| sample.offset >= *start && end <= *limit)
            })
        });
        let mut sorted_ranges = locations
            .iter()
            .filter_map(|sample| {
                sample
                    .offset
                    .checked_add(sample.size)
                    .map(|end| (sample.offset, end))
            })
            .collect::<Vec<_>>();
        sorted_ranges.sort_unstable();
        let ranges_disjoint = sorted_ranges.len() == locations.len()
            && sorted_ranges.windows(2).all(|pair| pair[0].1 <= pair[1].0);
        let entries_valid = locations.iter().all(|sample| {
            usize::try_from(sample.description_index.saturating_sub(1))
                .ok()
                .and_then(|index| track.iamf_entries.get(index))
                .and_then(Option::as_ref)
                .is_some_and(|entry| !entry.config_obus.is_empty())
        });
        let cenc_result = if encrypted {
            cenc_auxiliary_for_iamf(track, context.fragments, context.fragmented, locations)
        } else {
            Ok(Value::Null)
        };
        let cenc_valid = protection_declarations_valid && cenc_result.is_ok();
        if encrypted {
            let cenc_error = cenc_result.as_ref().err().cloned();
            bitstream.push(check(
                "FORGE-ISOBMFF-IAMF-CENC",
                cenc_valid,
                cenc_error.unwrap_or_else(|| {
                    "IAMF samples use bounded cenc/cbcs full-sample encryption with matching IV auxiliary data"
                        .to_string()
                }),
                Some(json!({
                    "track_id": track.id,
                    "sample_entries": iamf_entries.len(),
                    "encrypted_sample_entries": encrypted_entries,
                    "samples": locations.len(),
                    "auxiliary": cenc_result.as_ref().ok()
                })),
            ));
        }
        let mut sample_obus = 0_u64;
        let mut sample_audio_frames = 0_u64;
        let mut sample_parameter_blocks = 0_u64;
        let mut sample_error = None;
        if !encrypted {
            for (index, sample) in locations.iter().enumerate() {
                match scan_iamf_sample(path, file, *sample) {
                    Ok((obus, audio_frames, parameter_blocks)) => {
                        sample_obus = sample_obus.saturating_add(obus);
                        sample_audio_frames = sample_audio_frames.saturating_add(audio_frames);
                        sample_parameter_blocks =
                            sample_parameter_blocks.saturating_add(parameter_blocks);
                    }
                    Err(error) => {
                        sample_error = Some(format!("IA Sample {}: {error}", index + 1));
                        break;
                    }
                }
            }
        }
        let samples_valid = ranges_valid
            && ranges_disjoint
            && entries_valid
            && cenc_valid
            && sample_error.is_none()
            && !locations.is_empty();
        bitstream.push(check(
            "FORGE-ISOBMFF-IAMF-SAMPLE-DATA",
            samples_valid,
            sample_error.unwrap_or_else(|| {
                if samples_valid && encrypted {
                    "every encrypted IA Sample is bounded inside MediaDataBox with complete full-sample CENC auxiliary signaling; ciphertext OBU validation requires keys"
                        .to_string()
                } else if samples_valid {
                    "every IA Sample is a bounded descriptor-free Temporal Unit without a Temporal Delimiter OBU"
                        .to_string()
                } else {
                    "IA Sample tables, MediaDataBox ranges, or sample-entry references are invalid"
                        .to_string()
                }
            }),
            Some(json!({
                "track_id": track.id,
                "samples": locations.len(),
                "fragments": samples.fragment_count,
                "sample_obus": sample_obus,
                "audio_frame_obus": sample_audio_frames,
                "parameter_block_obus": sample_parameter_blocks,
                "encrypted": encrypted,
                "ciphertext_obu_validation": if encrypted { "requires_keys" } else { "complete" },
                "ranges_inside_mdat": ranges_valid,
                "sample_ranges_disjoint": ranges_disjoint,
                "iamf_sample_entries_resolve": entries_valid
            })),
        ));
        if !samples_valid {
            observations.push(json!({
                "track_id": track.id,
                "fragmented": context.fragmented,
                "validated_samples": 0
            }));
            continue;
        }

        if encrypted {
            let fragment_non_sync_samples = samples
                .sample_flags
                .iter()
                .filter(|flags| **flags & 0x0001_0000 != 0)
                .count();
            xcheck.push(check(
                "FORGE-ISOBMFF-IAMF-SYNC-CTS",
                !track.has_sync_sample_box
                    && !track.has_composition_offsets
                    && !samples.has_composition_offsets
                    && fragment_non_sync_samples == 0,
                "encrypted IAMF omits stss and composition offsets so every IA Sample is sync with CTS equal to DTS",
                Some(json!({
                    "track_id": track.id,
                    "stss": track.has_sync_sample_box,
                    "ctts": track.has_composition_offsets,
                    "fragment_composition_offsets": samples.has_composition_offsets,
                    "fragment_non_sync_samples": fragment_non_sync_samples
                })),
            ));
            observations.push(json!({
                "track_id": track.id,
                "fragmented": context.fragmented,
                "fragments": samples.fragment_count,
                "encrypted": true,
                "protection_scheme": iamf_entries.first()
                    .and_then(|entry| entry.protection.as_ref())
                    .and_then(|item| item.scheme.clone()),
                "validated_samples": locations.len(),
                "ciphertext_obu_validation": "requires_keys",
                "cenc": cenc_result.as_ref().ok(),
                "configurations": iamf_entries.len()
            }));
            continue;
        }

        let mut segments = Vec::new();
        let mut total_bytes = 0_u64;
        let mut previous_description = None;
        for sample in locations {
            if previous_description != Some(sample.description_index) {
                let entry_index = usize::try_from(sample.description_index - 1)
                    .expect("validated sample-description index fits usize");
                let config = track.iamf_entries[entry_index]
                    .as_ref()
                    .expect("validated IAMF sample entry")
                    .config_obus
                    .clone();
                total_bytes = total_bytes.saturating_add(config.len() as u64);
                segments.push(IamfSegment::Memory(config));
                previous_description = Some(sample.description_index);
            }
            total_bytes = total_bytes.saturating_add(sample.size);
            segments.push(IamfSegment::File {
                offset: sample.offset,
                size: sample.size,
            });
        }
        let reader = IamfTrackReader {
            file,
            segments,
            segment_index: 0,
            segment_offset: 0,
        };
        match crate::iamf_qc::audit_reader(path, reader, total_bytes) {
            Ok(audit) => {
                let codec_configs = audit
                    .properties
                    .get("codec_configs")
                    .cloned()
                    .unwrap_or(Value::Null);
                let temporal_units = audit
                    .properties
                    .get("temporal_units")
                    .and_then(Value::as_u64);
                let trim_start = audit
                    .properties
                    .get("trim_at_start_samples")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let trim_end = audit
                    .properties
                    .get("trim_at_end_samples")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let frame_lengths: HashSet<u64> = codec_configs
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|config| config.get("num_samples_per_frame"))
                    .filter_map(Value::as_u64)
                    .collect();
                let sample_rates: HashSet<u64> = codec_configs
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|config| config.get("sample_rate_hz"))
                    .filter_map(Value::as_u64)
                    .collect();
                let entry_timings: Vec<Option<IamfEntryTiming>> = track
                    .iamf_entries
                    .iter()
                    .map(|entry| {
                        entry.as_ref().and_then(|entry| {
                            iamf_config_timing(path, &entry.config_obus, track.timescale?)
                        })
                    })
                    .collect();
                let sample_frame_lengths: Vec<Option<u64>> = locations
                    .iter()
                    .map(|sample| {
                        let index = usize::try_from(sample.description_index - 1).ok()?;
                        entry_timings
                            .get(index)
                            .copied()
                            .flatten()
                            .map(|timing| timing.duration_ticks)
                    })
                    .collect();
                let observed_duration = samples
                    .sample_durations
                    .iter()
                    .map(|duration| u64::from(*duration))
                    .sum::<u64>();
                let nominal_duration = sample_frame_lengths
                    .iter()
                    .try_fold(0_u64, |total, frame_length| {
                        total.checked_add((*frame_length)?)
                    });
                let expected_duration =
                    nominal_duration.and_then(|duration| duration.checked_sub(trim_end));
                let sample_timing_valid = samples.sample_durations.len() == locations.len()
                    && samples
                        .sample_durations
                        .iter()
                        .zip(&sample_frame_lengths)
                        .all(|(duration, frame_length)| {
                            *duration > 0
                                && frame_length.is_some_and(|frame_length| {
                                    u64::from(*duration) <= frame_length
                                })
                        })
                    && Some(observed_duration) == expected_duration
                    && temporal_units == Some(locations.len() as u64)
                    && samples.decode_timeline_contiguous;
                xcheck.push(check(
                    "FORGE-ISOBMFF-IAMF-SAMPLE-TIMING",
                    sample_timing_valid,
                    "sample durations and fragment decode times equal the IAMF timeline with one IA Sample per Temporal Unit",
                    Some(json!({
                        "track_id": track.id,
                        "fragmented": context.fragmented,
                        "sample_durations": samples.sample_durations,
                        "fragment_decode_times": samples.fragment_decode_times,
                        "fragment_decode_timeline_contiguous": samples.decode_timeline_contiguous,
                        "codec_frame_lengths": frame_lengths,
                        "codec_sample_rates": sample_rates,
                        "media_timescale": track.timescale,
                        "sample_decode_durations_in_media_ticks": sample_frame_lengths,
                        "observed_decode_duration": observed_duration,
                        "expected_decode_duration": expected_duration,
                        "trim_at_end_samples": trim_end,
                        "ia_samples": locations.len(),
                        "temporal_units": temporal_units
                    })),
                ));

                let sample_expected_rolls: Vec<Option<i64>> = locations
                    .iter()
                    .map(|sample| {
                        let index = usize::try_from(sample.description_index - 1).ok()?;
                        entry_timings
                            .get(index)
                            .copied()
                            .flatten()
                            .map(|timing| timing.roll_distance)
                    })
                    .collect::<Option<_>>()
                    .unwrap_or_default();
                let codec_requires_roll = sample_expected_rolls.iter().any(Option::is_some);
                let expected_rolls: HashSet<i64> =
                    sample_expected_rolls.iter().flatten().copied().collect();
                let actual_rolls: HashSet<i64> =
                    samples.roll_assignments.iter().flatten().copied().collect();
                let roll_assignments = samples.roll_assignments.as_slice();
                let roll_valid = sample_expected_rolls.len() == locations.len()
                    && roll_assignments.iter().zip(&sample_expected_rolls).all(
                        |(actual, expected)| expected.is_none_or(|value| *actual == Some(value)),
                    );
                xcheck.push(check(
                    "FORGE-ISOBMFF-IAMF-ROLL-GROUP",
                    roll_valid,
                    "Opus/mp4a IA Samples use complete roll sample groups matching Codec Config audio_roll_distance",
                    Some(json!({
                        "track_id": track.id,
                        "required": codec_requires_roll,
                        "expected_roll_distances": expected_rolls,
                        "observed_roll_distances": actual_rolls,
                        "grouped_samples": track.sample_group_samples,
                        "default_group_applies": track.roll_default_group,
                        "default_description_index": track.roll_default_description_index,
                        "sample_assignments_resolve": true,
                        "sample_count": locations.len()
                    })),
                ));
                let expected_presentation_media_ticks = observed_duration.checked_sub(trim_start);
                let edit_duration_matches = track
                    .edit_segment_duration
                    .zip(track.timescale)
                    .zip(context.movie_timescale)
                    .zip(expected_presentation_media_ticks)
                    .is_some_and(
                        |(((edit_duration, media_timescale), movie_timescale), expected)| {
                            u128::from(edit_duration) * u128::from(media_timescale)
                                == u128::from(expected) * u128::from(movie_timescale)
                        },
                    );
                let trim_valid = trim_start == 0 && trim_end == 0
                    || (track.edit_media_time == i64::try_from(trim_start).ok()
                        && edit_duration_matches);
                xcheck.push(check(
                    "FORGE-ISOBMFF-IAMF-TRIM",
                    trim_valid,
                    "IAMF audio-frame trimming is represented by an edit list",
                    Some(json!({
                        "track_id": track.id,
                        "trim_at_start_samples": trim_start,
                        "trim_at_end_samples": trim_end,
                        "edit_media_time": track.edit_media_time,
                        "edit_segment_duration": track.edit_segment_duration,
                        "movie_timescale": context.movie_timescale,
                        "expected_presentation_media_ticks": expected_presentation_media_ticks,
                        "edit_duration_matches": edit_duration_matches
                    })),
                ));
                let fragment_non_sync_samples = samples
                    .sample_flags
                    .iter()
                    .filter(|flags| **flags & 0x0001_0000 != 0)
                    .count();
                xcheck.push(check(
                    "FORGE-ISOBMFF-IAMF-SYNC-CTS",
                    !track.has_sync_sample_box
                        && !track.has_composition_offsets
                        && !samples.has_composition_offsets
                        && fragment_non_sync_samples == 0,
                    "IAMF omits stss and composition offsets so every IA Sample is sync with CTS equal to DTS",
                    Some(json!({
                        "track_id": track.id,
                        "stss": track.has_sync_sample_box,
                        "ctts": track.has_composition_offsets,
                        "fragment_composition_offsets": samples.has_composition_offsets,
                        "fragment_non_sync_samples": fragment_non_sync_samples
                    })),
                ));
                for layer in audit.layers {
                    if layer.layer == "x-check" {
                        xcheck.extend(layer.checks);
                    } else {
                        bitstream.extend(layer.checks);
                    }
                }
                observations.push(json!({
                    "track_id": track.id,
                    "fragmented": context.fragmented,
                    "fragments": samples.fragment_count,
                    "validated_samples": locations.len(),
                    "sample_obus": sample_obus,
                    "configurations": track.iamf_entries.iter().filter(|entry| entry.is_some()).count(),
                    "codec_configs": codec_configs,
                    "temporal_units": temporal_units,
                    "decapsulated_bytes": total_bytes
                }));
            }
            Err(error) => {
                bitstream.push(check(
                    "FORGE-ISOBMFF-IAMF-DECAPSULATION",
                    false,
                    error,
                    Some(json!({"track_id": track.id})),
                ));
            }
        }
    }
    observations
}

struct IamfSampleSet {
    locations: Vec<SampleLocation>,
    sample_durations: Vec<u32>,
    sample_flags: Vec<u32>,
    roll_assignments: Vec<Option<i64>>,
    has_composition_offsets: bool,
    fragment_decode_times: Vec<u64>,
    decode_timeline_contiguous: bool,
    fragment_count: usize,
}

fn cenc_auxiliary_for_iamf(
    track: &Track,
    fragments: &[Fragment],
    fragmented: bool,
    locations: &[SampleLocation],
) -> Result<Value, String> {
    let schemes: HashSet<_> = locations
        .iter()
        .filter_map(|sample| {
            let index = usize::try_from(sample.description_index.checked_sub(1)?).ok()?;
            track
                .iamf_entries
                .get(index)?
                .as_ref()?
                .protection
                .as_ref()?
                .scheme
                .clone()
        })
        .collect();
    if schemes.len() != 1 {
        return Err("encrypted IAMF samples must resolve to one CENC protection scheme".into());
    }
    let scheme = schemes.iter().next().expect("one scheme");
    if !fragmented {
        let groups = resolve_cenc_group_assignments(track, locations.len())?;
        let policies = cenc_effective_protections(track, locations, &groups)?;
        let evidence = validate_cenc_auxiliary(
            &track.cenc_auxiliary,
            &cenc_policy_iv_sizes(&policies),
            scheme,
        )?;
        return Ok(json!({
            "scope": "sample_table",
            "protection": cenc_policy_evidence(&policies),
            "auxiliary": evidence
        }));
    }

    let track_id = track
        .id
        .ok_or_else(|| "fragmented encrypted IAMF track has no track_ID".to_string())?;
    let mut fragment_evidence = Vec::new();
    let mut observed_samples = 0_usize;
    let has_fragment_groups =
        fragments
            .iter()
            .flat_map(|fragment| &fragment.tracks)
            .any(|track_fragment| {
                track_fragment.track_id == Some(track_id)
                    && (track_fragment.cenc_sbgp_seen
                        || !track_fragment.cenc_group_entries.is_empty()
                        || track_fragment.cenc_default_description_index.is_some())
            });
    if has_fragment_groups && !track.cenc_sample_runs.is_empty() {
        return Err(
            "CENC seig mappings cannot mix track-level and fragment-level sbgp scopes".into(),
        );
    }
    let global_groups = if !track.cenc_sample_runs.is_empty() {
        Some(resolve_cenc_group_assignments(track, locations.len())?)
    } else {
        None
    };
    for fragment in fragments {
        for track_fragment in &fragment.tracks {
            if track_fragment.track_id != Some(track_id) {
                continue;
            }
            let start = observed_samples;
            let end = start
                .checked_add(track_fragment.samples.len())
                .ok_or_else(|| "encrypted fragment sample count overflows memory".to_string())?;
            let groups = if let Some(global) = &global_groups {
                global
                    .get(start..end)
                    .ok_or_else(|| {
                        "track-level CENC seig mapping is shorter than fragment samples".to_string()
                    })?
                    .to_vec()
            } else {
                resolve_fragment_cenc_group_assignments(track, track_fragment)?
            };
            let policies = cenc_effective_protections(track, &track_fragment.samples, &groups)?;
            let sizes = cenc_policy_iv_sizes(&policies);
            observed_samples = observed_samples
                .checked_add(sizes.len())
                .ok_or_else(|| "encrypted fragment sample count overflows memory".to_string())?;
            fragment_evidence.push(json!({
                "fragment_offset": fragment.start,
                "samples": sizes.len(),
                "protection": cenc_policy_evidence(&policies),
                "auxiliary": validate_cenc_auxiliary(
                    &track_fragment.cenc_auxiliary,
                    &sizes,
                    scheme,
                )?
            }));
        }
    }
    if observed_samples != locations.len() {
        return Err(format!(
            "CENC fragment auxiliary data covers {observed_samples} samples, expected {}",
            locations.len()
        ));
    }
    Ok(json!({"scope": "track_fragments", "fragments": fragment_evidence}))
}

fn cenc_effective_protections(
    track: &Track,
    locations: &[SampleLocation],
    groups: &[Option<CencSampleGroupEntry>],
) -> Result<Vec<EffectiveCencProtection>, String> {
    if groups.len() != locations.len() {
        return Err("CENC seig assignments do not cover every encrypted IAMF sample".into());
    }
    locations
        .iter()
        .zip(groups)
        .map(|(sample, group)| {
            let index = usize::try_from(
                sample
                    .description_index
                    .checked_sub(1)
                    .ok_or_else(|| "CENC sample description index is zero".to_string())?,
            )
            .map_err(|_| "CENC sample description index does not fit memory".to_string())?;
            let entry = track
                .iamf_entries
                .get(index)
                .and_then(Option::as_ref)
                .ok_or_else(|| {
                    "CENC sample does not resolve to an IAMF sample entry".to_string()
                })?;
            let protection = entry.protection.as_ref().ok_or_else(|| {
                "IAMF track mixes clear and encrypted sample descriptions".to_string()
            })?;
            if !protection.valid {
                return Err("IAMF encrypted sample entry has invalid CENC signaling".into());
            }
            let effective = if let Some(group) = group {
                if !group.valid {
                    return Err("CENC seig entry has invalid field geometry".into());
                }
                if group.is_protected != 1 {
                    return Err(
                        "IAMF encrypted samples cannot be made clear by a CENC seig entry".into(),
                    );
                }
                EffectiveCencProtection {
                    per_sample_iv_size: group.per_sample_iv_size,
                    kid: group.kid.clone(),
                    crypt_byte_block: group.crypt_byte_block,
                    skip_byte_block: group.skip_byte_block,
                    constant_iv_size: group.constant_iv_size,
                    sample_group_override: true,
                }
            } else {
                EffectiveCencProtection {
                    per_sample_iv_size: protection.per_sample_iv_size.unwrap_or(0),
                    kid: protection
                        .default_kid
                        .clone()
                        .ok_or_else(|| "CENC tenc default_KID is missing".to_string())?,
                    crypt_byte_block: protection.crypt_byte_block.unwrap_or(0),
                    skip_byte_block: protection.skip_byte_block.unwrap_or(0),
                    constant_iv_size: protection.constant_iv_size,
                    sample_group_override: false,
                }
            };
            if effective.crypt_byte_block != 0 || effective.skip_byte_block != 0 {
                return Err(
                    "IAMF CENC seig/tenc policies must use full-sample encryption without pattern skipping"
                        .into(),
                );
            }
            if !matches!(effective.per_sample_iv_size, 0 | 8 | 16)
                || effective.per_sample_iv_size == 0
                    && !matches!(effective.constant_iv_size, Some(8 | 16))
            {
                return Err(
                    "IAMF CENC seig/tenc policy requires an 8/16-byte per-sample or constant IV"
                        .into(),
                );
            }
            Ok(effective)
        })
        .collect()
}

fn cenc_policy_iv_sizes(policies: &[EffectiveCencProtection]) -> Vec<u8> {
    policies
        .iter()
        .map(|policy| policy.per_sample_iv_size)
        .collect()
}

fn cenc_policy_evidence(policies: &[EffectiveCencProtection]) -> Value {
    let mut key_ids = policies
        .iter()
        .map(|policy| policy.kid.clone())
        .collect::<Vec<_>>();
    key_ids.sort();
    key_ids.dedup();
    json!({
        "key_ids": key_ids,
        "key_rotation": key_ids.len() > 1,
        "sample_group_overrides": policies.iter()
            .filter(|policy| policy.sample_group_override)
            .count(),
        "per_sample_iv_bytes": cenc_policy_iv_sizes(policies)
    })
}

fn validate_cenc_auxiliary(
    auxiliary: &CencAuxiliary,
    expected_iv_sizes: &[u8],
    scheme: &str,
) -> Result<Value, String> {
    if auxiliary.senc.len() > 1 || auxiliary.saiz.len() > 1 || auxiliary.saio.len() > 1 {
        return Err("CENC sample scope contains duplicate senc, saiz, or saio boxes".into());
    }
    if auxiliary.saiz.is_empty() != auxiliary.saio.is_empty() {
        return Err("CENC saiz and saio boxes must be present as a pair".into());
    }
    let type_valid = |kind: &Option<String>| kind.as_deref().is_none_or(|value| value == scheme);
    if auxiliary
        .saiz
        .iter()
        .any(|item| !type_valid(&item.auxiliary_type))
        || auxiliary
            .saio
            .iter()
            .any(|item| !type_valid(&item.auxiliary_type))
    {
        return Err("CENC auxiliary_info_type does not match the IAMF protection scheme".into());
    }

    let mut senc_valid = false;
    if let Some(senc) = auxiliary.senc.first() {
        if senc.flags & 0x000001 != 0 {
            return Err(
                "IAMF CENC key selection must use tenc/seig; senc track-encryption overrides are not allowed"
                    .into(),
            );
        }
        if senc.flags & 0x000002 != 0 {
            return Err("IAMF CENC forbids subsample encryption; senc flag 0x000002 is set".into());
        }
        if usize::try_from(senc.sample_count).ok() != Some(expected_iv_sizes.len()) {
            return Err(format!(
                "senc covers {} samples, expected {}",
                senc.sample_count,
                expected_iv_sizes.len()
            ));
        }
        let entry_bytes = if let Some(override_size) = senc.iv_size_override {
            if !matches!(override_size, 8 | 16) {
                return Err("senc IV_size override must be 8 or 16 bytes".into());
            }
            expected_iv_sizes
                .len()
                .checked_mul(usize::from(override_size))
        } else {
            expected_iv_sizes
                .iter()
                .try_fold(0_usize, |total, size| total.checked_add(usize::from(*size)))
        }
        .ok_or_else(|| "senc entry byte count overflows memory".to_string())?;
        if senc.entry_bytes != entry_bytes {
            return Err(format!(
                "senc contains {} entry bytes, expected {entry_bytes} full-sample IV bytes",
                senc.entry_bytes
            ));
        }
        senc_valid = true;
    }

    let mut external_valid = false;
    if let (Some(saiz), Some(saio)) = (auxiliary.saiz.first(), auxiliary.saio.first()) {
        if usize::try_from(saiz.sample_count).ok() != Some(expected_iv_sizes.len()) {
            return Err(format!(
                "saiz covers {} samples, expected {}",
                saiz.sample_count,
                expected_iv_sizes.len()
            ));
        }
        let observed_sizes = if saiz.default_size == 0 {
            saiz.sizes.clone()
        } else {
            vec![saiz.default_size; expected_iv_sizes.len()]
        };
        if observed_sizes != expected_iv_sizes {
            return Err(
                "saiz sizes include data beyond each full-sample IV or omit required IV bytes"
                    .into(),
            );
        }
        if saio.offsets.is_empty() {
            return Err("saio contains no auxiliary-data offset".into());
        }
        external_valid = true;
    }

    let needs_per_sample_iv = expected_iv_sizes.iter().any(|size| *size != 0);
    if needs_per_sample_iv && !senc_valid && !external_valid {
        return Err(
            "per-sample CENC IVs require a bounded senc box or paired saiz/saio boxes".into(),
        );
    }
    Ok(json!({
        "samples": expected_iv_sizes.len(),
        "per_sample_iv_bytes": expected_iv_sizes,
        "senc": senc_valid,
        "saiz_saio": external_valid,
        "constant_iv": !needs_per_sample_iv
    }))
}

fn iamf_sample_set(
    track: &Track,
    fragments: &[Fragment],
    fragmented: bool,
) -> Result<IamfSampleSet, String> {
    if !fragmented {
        let locations = sample_locations(track)?;
        let roll_assignments = resolve_roll_assignments(track, locations.len())?;
        return Ok(IamfSampleSet {
            locations,
            sample_durations: track.sample_durations.clone(),
            sample_flags: Vec::new(),
            roll_assignments,
            has_composition_offsets: false,
            fragment_decode_times: Vec::new(),
            decode_timeline_contiguous: true,
            fragment_count: 0,
        });
    }
    let track_id = track
        .id
        .ok_or_else(|| "fragmented IAMF track has no track_ID".to_string())?;
    let mut locations = Vec::new();
    let mut sample_durations = Vec::new();
    let mut sample_flags = Vec::new();
    let mut roll_assignments = Vec::new();
    let mut has_composition_offsets = false;
    let mut fragment_decode_times = Vec::new();
    let mut expected_decode_time = 0_u64;
    let mut decode_timeline_contiguous = true;
    let mut fragment_count = 0_usize;
    let mut uses_fragment_groups = false;
    let mut uses_global_track_runs = false;
    for fragment in fragments {
        for track_fragment in &fragment.tracks {
            if track_fragment.track_id != Some(track_id) {
                continue;
            }
            fragment_count += 1;
            if !track_fragment.samples_resolved {
                return Err(format!(
                    "IAMF fragment at {} cannot resolve positive sample description, duration, and size values from trun/tfhd/trex",
                    fragment.start
                ));
            }
            let decode_time = track_fragment.decode_time.ok_or_else(|| {
                format!(
                    "IAMF fragment at {} has no base decode time",
                    fragment.start
                )
            })?;
            fragment_decode_times.push(decode_time);
            decode_timeline_contiguous &= decode_time == expected_decode_time;
            for duration in &track_fragment.sample_durations {
                expected_decode_time = expected_decode_time
                    .checked_add(u64::from(*duration))
                    .ok_or_else(|| "IAMF fragment decode timeline overflows uint64".to_string())?;
            }
            locations.extend_from_slice(&track_fragment.samples);
            sample_durations.extend_from_slice(&track_fragment.sample_durations);
            sample_flags.extend_from_slice(&track_fragment.sample_flags);
            has_composition_offsets |= track_fragment.has_composition_offsets;
            let has_local_groups = !track_fragment.roll_distances.is_empty()
                || track_fragment.roll_default_description_index.is_some()
                || !track_fragment.roll_sample_runs.is_empty();
            if has_local_groups {
                if uses_global_track_runs {
                    return Err(
                        "IAMF fragments mix whole-track and fragment roll sample-group mappings"
                            .into(),
                    );
                }
                uses_fragment_groups = true;
                roll_assignments.extend(resolve_fragment_roll_assignments(track, track_fragment)?);
            } else if track.roll_sample_runs.is_empty() {
                roll_assignments.extend(resolve_fragment_roll_assignments(track, track_fragment)?);
            } else {
                if uses_fragment_groups {
                    return Err(
                        "IAMF fragments mix fragment and whole-track roll sample-group mappings"
                            .into(),
                    );
                }
                uses_global_track_runs = true;
            }
        }
    }
    if fragment_count == 0 || locations.is_empty() {
        return Err(format!(
            "fragmented IAMF track {track_id} has no bounded TrackRunBox samples"
        ));
    }
    if uses_global_track_runs {
        roll_assignments = resolve_roll_assignments(track, locations.len())?;
    } else {
        if roll_assignments.len() != locations.len() {
            return Err(
                "fragment roll sample-group assignments do not cover every IA Sample".into(),
            );
        }
    }
    Ok(IamfSampleSet {
        locations,
        sample_durations,
        sample_flags,
        roll_assignments,
        has_composition_offsets,
        fragment_decode_times,
        decode_timeline_contiguous,
        fragment_count,
    })
}

fn resolve_fragment_roll_assignments(
    track: &Track,
    fragment: &TrackFragment,
) -> Result<Vec<Option<i64>>, String> {
    let resolve = |index: u32| -> Result<Option<i64>, String> {
        let effective = if index == 0 {
            fragment
                .roll_default_description_index
                .or(track.roll_default_description_index)
                .unwrap_or(0)
        } else {
            index
        };
        if effective == 0 {
            return Ok(None);
        }
        if effective >= 0x1_0000 {
            let local = effective - 0x1_0000;
            if local == 0 {
                return Err(
                    "fragment-local roll group description index 0x10000 is reserved".into(),
                );
            }
            let index = usize::try_from(local - 1)
                .map_err(|_| "fragment roll group index does not fit memory".to_string())?;
            return fragment
                .roll_distances
                .get(index)
                .map(|distance| Some(i64::from(*distance)))
                .ok_or_else(|| {
                    format!(
                        "fragment-local roll group description index {effective:#x} is undefined"
                    )
                });
        }
        let index = usize::try_from(effective - 1)
            .map_err(|_| "track roll group index does not fit memory".to_string())?;
        track
            .roll_distances
            .get(index)
            .map(|distance| Some(i64::from(*distance)))
            .ok_or_else(|| format!("track roll group description index {effective} is undefined"))
    };
    if fragment.roll_sample_runs.is_empty() {
        let value = resolve(0)?;
        return Ok(std::iter::repeat_n(value, fragment.samples.len()).collect());
    }
    let mut assignments = Vec::with_capacity(fragment.samples.len());
    for &(count, index) in &fragment.roll_sample_runs {
        let count = usize::try_from(count)
            .map_err(|_| "fragment roll group run does not fit memory".to_string())?;
        if count > fragment.samples.len().saturating_sub(assignments.len()) {
            return Err("fragment roll group runs exceed the IA Sample count".into());
        }
        assignments.extend(std::iter::repeat_n(resolve(index)?, count));
    }
    if assignments.len() != fragment.samples.len() {
        return Err(format!(
            "fragment roll group runs cover {} IA Samples, expected {}",
            assignments.len(),
            fragment.samples.len()
        ));
    }
    Ok(assignments)
}

fn resolve_fragment_cenc_group_assignments(
    track: &Track,
    fragment: &TrackFragment,
) -> Result<Vec<Option<CencSampleGroupEntry>>, String> {
    let resolve = |index: u32| -> Result<Option<CencSampleGroupEntry>, String> {
        let effective = if index == 0 {
            fragment
                .cenc_default_description_index
                .or(track.cenc_default_description_index)
                .unwrap_or(0)
        } else {
            index
        };
        if effective == 0 {
            return Ok(None);
        }
        if effective >= 0x1_0000 {
            let local = effective - 0x1_0000;
            if local == 0 {
                return Err(
                    "fragment-local CENC seig description index 0x10000 is reserved".into(),
                );
            }
            let index = usize::try_from(local - 1)
                .map_err(|_| "fragment CENC seig index does not fit memory".to_string())?;
            return fragment
                .cenc_group_entries
                .get(index)
                .cloned()
                .map(Some)
                .ok_or_else(|| {
                    format!(
                        "fragment-local CENC seig description index {effective:#x} is undefined"
                    )
                });
        }
        let index = usize::try_from(effective - 1)
            .map_err(|_| "track CENC seig index does not fit memory".to_string())?;
        track
            .cenc_group_entries
            .get(index)
            .cloned()
            .map(Some)
            .ok_or_else(|| format!("track CENC seig description index {effective} is undefined"))
    };
    if fragment.cenc_sample_runs.is_empty() {
        let value = resolve(0)?;
        return Ok(std::iter::repeat_n(value, fragment.samples.len()).collect());
    }
    let mut assignments = Vec::with_capacity(fragment.samples.len());
    for &(count, index) in &fragment.cenc_sample_runs {
        let count = usize::try_from(count)
            .map_err(|_| "fragment CENC seig run count does not fit memory".to_string())?;
        if count > fragment.samples.len().saturating_sub(assignments.len()) {
            return Err("fragment CENC seig runs exceed the IAMF sample count".into());
        }
        assignments.extend(std::iter::repeat_n(resolve(index)?, count));
    }
    if assignments.len() != fragment.samples.len() {
        return Err(format!(
            "fragment CENC seig runs cover {} IAMF samples, expected {}",
            assignments.len(),
            fragment.samples.len()
        ));
    }
    Ok(assignments)
}

fn sample_locations(track: &Track) -> Result<Vec<SampleLocation>, String> {
    if track.sample_sizes.is_empty()
        || track.chunk_offsets.is_empty()
        || track.sample_to_chunk.is_empty()
    {
        return Err("IAMF track requires complete stsz/stz2, stco/co64, and stsc tables".into());
    }
    if track.sample_to_chunk.first().map(|entry| entry.0) != Some(1) {
        return Err("IAMF stsc first_chunk must start at 1".into());
    }
    if expand_chunk_samples(&track.sample_to_chunk, track.chunk_offsets.len())
        != Some(track.sample_sizes.len() as u64)
    {
        return Err(
            "IAMF stsc mappings must be ordered, bounded by the chunk table, and cover every sample"
                .into(),
        );
    }
    let mut locations = Vec::with_capacity(track.sample_sizes.len());
    let mut sample_index = 0_usize;
    let mut mapping_index = 0_usize;
    for (chunk_index, chunk_offset) in track.chunk_offsets.iter().copied().enumerate() {
        let one_based = u32::try_from(chunk_index + 1)
            .map_err(|_| "IAMF chunk index exceeds uint32".to_string())?;
        while mapping_index + 1 < track.sample_to_chunk.len()
            && track.sample_to_chunk[mapping_index + 1].0 <= one_based
        {
            mapping_index += 1;
        }
        let (_, samples_per_chunk, description_index) = track.sample_to_chunk[mapping_index];
        let mut offset = chunk_offset;
        for _ in 0..samples_per_chunk {
            let size = *track
                .sample_sizes
                .get(sample_index)
                .ok_or_else(|| "stsc expands beyond the IAMF sample-size table".to_string())?;
            if description_index == 0 {
                return Err("stsc references sample description index 0".into());
            }
            locations.push(SampleLocation {
                offset,
                size: u64::from(size),
                description_index,
            });
            offset = offset
                .checked_add(u64::from(size))
                .ok_or_else(|| "IAMF sample offset overflows uint64".to_string())?;
            sample_index += 1;
        }
    }
    if sample_index != track.sample_sizes.len() {
        return Err(format!(
            "stsc expands to {sample_index} IAMF samples, expected {}",
            track.sample_sizes.len()
        ));
    }
    Ok(locations)
}

fn resolve_roll_assignments(
    track: &Track,
    sample_count: usize,
) -> Result<Vec<Option<i64>>, String> {
    let resolve = |index: u32| -> Result<Option<i64>, String> {
        let effective = if index == 0 {
            track.roll_default_description_index.unwrap_or(0)
        } else {
            index
        };
        if effective == 0 {
            return Ok(None);
        }
        let offset = usize::try_from(effective - 1)
            .map_err(|_| "roll group description index does not fit memory".to_string())?;
        track
            .roll_distances
            .get(offset)
            .map(|distance| Some(i64::from(*distance)))
            .ok_or_else(|| format!("roll group description index {effective} is undefined"))
    };

    if track.roll_sample_runs.is_empty() {
        let value = resolve(0)?;
        return Ok(std::iter::repeat_n(value, sample_count).collect());
    }
    let mut assignments = Vec::with_capacity(sample_count);
    for &(count, index) in &track.roll_sample_runs {
        let count = usize::try_from(count)
            .map_err(|_| "roll sample-group run count does not fit memory".to_string())?;
        if count > sample_count.saturating_sub(assignments.len()) {
            return Err("roll sample-group runs exceed the IA Sample count".into());
        }
        assignments.extend(std::iter::repeat_n(resolve(index)?, count));
    }
    if assignments.len() != sample_count {
        return Err(format!(
            "roll sample-group runs cover {} IA Samples, expected {sample_count}",
            assignments.len()
        ));
    }
    Ok(assignments)
}

fn resolve_cenc_group_assignments(
    track: &Track,
    sample_count: usize,
) -> Result<Vec<Option<CencSampleGroupEntry>>, String> {
    let resolve = |index: u32| -> Result<Option<CencSampleGroupEntry>, String> {
        let effective = if index == 0 {
            track.cenc_default_description_index.unwrap_or(0)
        } else {
            index
        };
        if effective == 0 {
            return Ok(None);
        }
        let offset = usize::try_from(effective - 1)
            .map_err(|_| "CENC seig description index does not fit memory".to_string())?;
        track
            .cenc_group_entries
            .get(offset)
            .cloned()
            .map(Some)
            .ok_or_else(|| format!("CENC seig description index {effective} is undefined"))
    };
    if track.cenc_sample_runs.is_empty() {
        let value = resolve(0)?;
        return Ok(std::iter::repeat_n(value, sample_count).collect());
    }
    let mut assignments = Vec::with_capacity(sample_count);
    for &(count, index) in &track.cenc_sample_runs {
        let count = usize::try_from(count)
            .map_err(|_| "CENC seig run count does not fit memory".to_string())?;
        if count > sample_count.saturating_sub(assignments.len()) {
            return Err("CENC seig runs exceed the IAMF sample count".into());
        }
        assignments.extend(std::iter::repeat_n(resolve(index)?, count));
    }
    if assignments.len() != sample_count {
        return Err(format!(
            "CENC seig runs cover {} IAMF samples, expected {sample_count}",
            assignments.len()
        ));
    }
    Ok(assignments)
}

fn scan_iamf_sample(
    path: &Path,
    file: &mut File,
    sample: SampleLocation,
) -> Result<(u64, u64, u64), String> {
    let end = sample
        .offset
        .checked_add(sample.size)
        .ok_or_else(|| "sample range overflows uint64".to_string())?;
    if sample.size == 0 {
        return Err("sample is empty".into());
    }
    let mut offset = sample.offset;
    let mut obus = 0_u64;
    let mut audio_frames = 0_u64;
    let mut parameter_blocks = 0_u64;
    while offset < end {
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| format!("seek {} to IA Sample OBU: {error}", path.display()))?;
        let mut header = [0_u8; 1];
        file.read_exact(&mut header)
            .map_err(|error| format!("read {} IA Sample OBU: {error}", path.display()))?;
        let obu_type = header[0] >> 3;
        let (body_size, leb_bytes) = read_iamf_leb_reader(file)
            .ok_or_else(|| "OBU size is invalid bounded LEB128".to_string())?;
        let obu_size = 1_u64
            .checked_add(leb_bytes as u64)
            .and_then(|size| size.checked_add(body_size))
            .ok_or_else(|| "OBU size overflows uint64".to_string())?;
        if obu_size > 1 << 21 || offset.checked_add(obu_size).is_none_or(|limit| limit > end) {
            return Err("OBU exceeds the 2 MiB profile limit or IA Sample boundary".into());
        }
        if matches!(obu_type, 0..=2 | 31) {
            return Err(format!(
                "descriptor OBU type {obu_type} is stored in an IA Sample"
            ));
        }
        if obu_type == 4 {
            return Err("Temporal Delimiter OBU is stored in an IA Sample".into());
        }
        if obu_type == 3 {
            parameter_blocks += 1;
        } else if (5..=23).contains(&obu_type) {
            audio_frames += 1;
        }
        obus += 1;
        offset += obu_size;
    }
    if offset != end || audio_frames == 0 {
        return Err("IA Sample is not one complete Temporal Unit with audio frames".into());
    }
    Ok((obus, audio_frames, parameter_blocks))
}

fn read_iamf_leb_reader(file: &mut File) -> Option<(u64, usize)> {
    let mut value = 0_u64;
    for index in 0..8 {
        let mut byte = [0_u8; 1];
        file.read_exact(&mut byte).ok()?;
        value |= u64::from(byte[0] & 0x7f).checked_shl(index * 7)?;
        if byte[0] & 0x80 == 0 {
            return Some((value, index as usize + 1));
        }
    }
    None
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
            b"mvex" => parse_mvex(path, file, child, state, bitstream)?,
            _ => {}
        }
    }
    Ok(())
}

fn parse_mvex(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
    state: &mut State,
    checks: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let children = list_boxes(
        path,
        file,
        header.body_start,
        header.end,
        &mut state.box_count,
    )?;
    let mut valid = true;
    let mut count = 0_usize;
    for child in children {
        if child.kind != *b"trex" {
            continue;
        }
        count += 1;
        let body = read_control(path, file, child)?;
        if body.len() != 24 || body[0] != 0 || body[1..4] != [0, 0, 0] {
            valid = false;
            continue;
        }
        let track_id = be_u32(&body[4..8]);
        let defaults = TrackExtendsDefaults {
            description_index: be_u32(&body[8..12]),
            duration: be_u32(&body[12..16]),
            size: be_u32(&body[16..20]),
            flags: be_u32(&body[20..24]),
        };
        valid &= track_id > 0
            && defaults.description_index > 0
            && state.track_extends.insert(track_id, defaults).is_none();
    }
    checks.push(check(
        "FORGE-ISOBMFF-TRACK-EXTENDS",
        valid && count > 0,
        "MovieExtendsBox has unique, bounded TrackExtendsBox defaults",
        Some(json!({
            "entries": count,
            "track_ids": state.track_extends.keys().copied().collect::<Vec<_>>()
        })),
    ));
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
            b"ctts" => track.has_composition_offsets = true,
            b"stss" => track.has_sync_sample_box = true,
            b"stsz" => parse_stsz(path, file, child, track, bitstream)?,
            b"stz2" => parse_stz2(path, file, child, track, bitstream)?,
            b"stsc" => parse_stsc(path, file, child, track, bitstream)?,
            b"sgpd" => parse_sgpd(path, file, child, track, bitstream)?,
            b"sbgp" => parse_sbgp(path, file, child, track, bitstream)?,
            b"senc" => track
                .cenc_auxiliary
                .senc
                .push(parse_senc(path, file, child)?),
            b"saiz" => track
                .cenc_auxiliary
                .saiz
                .push(parse_saiz(path, file, child)?),
            b"saio" => track
                .cenc_auxiliary
                .saio
                .push(parse_saio(path, file, child)?),
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
        let sample_entry_type = fourcc(&body[offset + 4..offset + 8]);
        let mut logical_codec = sample_entry_type.clone();
        if track.handler == Some(*b"soun") && size >= 36 {
            track.channels = Some(u16::from_be_bytes(
                body[offset + 24..offset + 26].try_into().unwrap(),
            ));
            track.sample_rate = Some(be_u32(&body[offset + 32..offset + 36]) >> 16);
            match sample_entry_child_boxes(&body[offset..offset + size]) {
                Ok(children) => {
                    let protection = (sample_entry_type == "enca")
                        .then(|| parse_cenc_protection(&children, checks));
                    let is_iamf = sample_entry_type == "iamf"
                        || protection
                            .as_ref()
                            .and_then(|item| item.original_format.as_deref())
                            == Some("iamf");
                    if is_iamf {
                        logical_codec = "iamf".to_string();
                    }
                    track.iamf_entries.push(if is_iamf {
                        Some(parse_iamf_sample_entry(
                            &body[offset..offset + size],
                            &sample_entry_type,
                            &children,
                            protection,
                            checks,
                        ))
                    } else {
                        None
                    });
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
                    if logical_codec == "mp4a" {
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
        track.codecs.push(logical_codec);
        if track.handler == Some(*b"soun") && track.iamf_entries.len() < track.codecs.len() {
            track.iamf_entries.push(None);
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

fn bounded_child_boxes(bytes: &[u8]) -> Result<Vec<(String, &[u8])>, String> {
    let mut offset = 0_usize;
    let mut output = Vec::new();
    while offset < bytes.len() {
        if bytes.len() - offset < 8 {
            return Err("nested ISO-BMFF box header is truncated".into());
        }
        let size32 = be_u32(&bytes[offset..offset + 4]);
        let kind = fourcc(&bytes[offset + 4..offset + 8]);
        let (header_size, size) = if size32 == 1 {
            if bytes.len() - offset < 16 {
                return Err(format!("{kind} extended-size header is truncated"));
            }
            (
                16_usize,
                usize::try_from(be_u64(&bytes[offset + 8..offset + 16]))
                    .map_err(|_| format!("{kind} size does not fit memory"))?,
            )
        } else if size32 == 0 {
            (8_usize, bytes.len() - offset)
        } else {
            (
                8_usize,
                usize::try_from(size32).map_err(|_| format!("{kind} size does not fit memory"))?,
            )
        };
        if size < header_size || size > bytes.len() - offset {
            return Err(format!("{kind} box exceeds its parent"));
        }
        output.push((kind, &bytes[offset + header_size..offset + size]));
        offset += size;
    }
    Ok(output)
}

fn parse_cenc_protection(
    children: &[(String, &[u8])],
    checks: &mut Vec<AuditCheck>,
) -> CencProtection {
    let sinf: Vec<_> = children
        .iter()
        .filter(|(kind, _)| kind == "sinf")
        .map(|(_, payload)| *payload)
        .collect();
    let mut result = CencProtection::default();
    let mut errors = Vec::new();
    if sinf.len() != 1 {
        errors.push(format!(
            "encrypted audio sample entry contains {} ProtectionSchemeInfoBox values; exactly one is required",
            sinf.len()
        ));
    }
    let nested = sinf
        .first()
        .map(|payload| bounded_child_boxes(payload))
        .transpose();
    let nested = match nested {
        Ok(Some(items)) => items,
        Ok(None) => Vec::new(),
        Err(error) => {
            errors.push(error);
            Vec::new()
        }
    };

    let original_formats: Vec<_> = nested
        .iter()
        .filter(|(kind, _)| kind == "frma")
        .map(|(_, payload)| *payload)
        .collect();
    result.original_format = original_formats
        .iter()
        .find(|payload| payload.len() == 4)
        .map(|payload| fourcc(payload));
    if original_formats.len() != 1 || original_formats[0].len() != 4 {
        errors.push("ProtectionSchemeInfoBox requires one four-byte OriginalFormatBox".to_string());
    }

    let schemes: Vec<_> = nested
        .iter()
        .filter(|(kind, _)| kind == "schm")
        .map(|(_, payload)| *payload)
        .collect();
    if schemes.len() == 1 {
        let body = schemes[0];
        if body.len() < 12 || body[0] != 0 {
            errors.push("SchemeTypeBox is truncated or has unsupported version".to_string());
        } else {
            let flags = full_box_flags(body);
            result.scheme = Some(fourcc(&body[4..8]));
            result.scheme_version = Some(be_u32(&body[8..12]));
            if flags & !1 != 0
                || flags == 0 && body.len() != 12
                || flags == 1 && (body.len() == 12 || body.last() != Some(&0))
            {
                errors.push("SchemeTypeBox flags, URI, or trailing bytes are invalid".to_string());
            }
            if !matches!(result.scheme.as_deref(), Some("cenc" | "cbcs")) {
                errors.push("IAMF protection scheme must be cenc or cbcs".to_string());
            }
            if result.scheme_version != Some(0x0001_0000) {
                errors.push("CENC scheme_version must be 0x00010000".to_string());
            }
        }
    } else {
        errors.push(format!(
            "ProtectionSchemeInfoBox contains {} SchemeTypeBox values; exactly one is required",
            schemes.len()
        ));
    }

    let scheme_information: Vec<_> = nested
        .iter()
        .filter(|(kind, _)| kind == "schi")
        .map(|(_, payload)| *payload)
        .collect();
    if scheme_information.len() == 1 {
        match bounded_child_boxes(scheme_information[0]) {
            Ok(items) => {
                let tenc: Vec<_> = items
                    .iter()
                    .filter(|(kind, _)| kind == "tenc")
                    .map(|(_, payload)| *payload)
                    .collect();
                if tenc.len() == 1 {
                    parse_track_encryption(tenc[0], &mut result, &mut errors);
                } else {
                    errors.push(format!(
                        "SchemeInformationBox contains {} TrackEncryptionBox values; exactly one is required",
                        tenc.len()
                    ));
                }
            }
            Err(error) => errors.push(error),
        }
    } else {
        errors.push(format!(
            "ProtectionSchemeInfoBox contains {} SchemeInformationBox values; exactly one is required",
            scheme_information.len()
        ));
    }
    result.valid = sinf.len() == 1 && errors.is_empty();
    if result.original_format.as_deref() == Some("iamf") {
        checks.push(check(
            "FORGE-ISOBMFF-IAMF-CENC-SIGNALING",
            result.valid,
            if errors.is_empty() {
                "encrypted IAMF sample entry has bounded CENC full-sample protection signaling"
                    .to_string()
            } else {
                errors.join("; ")
            },
            Some(cenc_protection_json(&result)),
        ));
    }
    result
}

fn parse_track_encryption(body: &[u8], result: &mut CencProtection, errors: &mut Vec<String>) {
    if body.len() < 24 || !matches!(body[0], 0 | 1) || full_box_flags(body) != 0 {
        errors.push("TrackEncryptionBox is truncated or has unsupported fields".to_string());
        return;
    }
    result.tenc_version = Some(body[0]);
    if body[4] != 0 {
        errors.push("TrackEncryptionBox reserved field is non-zero".to_string());
    }
    if body[0] == 0 {
        if body[5] != 0 {
            errors.push("TrackEncryptionBox version 0 reserved fields are non-zero".to_string());
        }
        result.crypt_byte_block = Some(0);
        result.skip_byte_block = Some(0);
    } else {
        result.crypt_byte_block = Some(body[5] >> 4);
        result.skip_byte_block = Some(body[5] & 0x0f);
    }
    result.default_is_protected = Some(body[6]);
    result.per_sample_iv_size = Some(body[7]);
    result.default_kid = Some(hex_bytes(&body[8..24]));
    if result.default_is_protected != Some(1) {
        errors.push("IAMF encrypted sample entry must set default_isProtected to 1".to_string());
    }
    if result.crypt_byte_block != Some(0) || result.skip_byte_block != Some(0) {
        errors.push(
            "IAMF cenc/cbcs protection must encrypt the full sample without a pattern skip"
                .to_string(),
        );
    }
    match result.per_sample_iv_size {
        Some(8 | 16) if body.len() == 24 => {}
        Some(0) if body.len() >= 25 => {
            let size = body[24];
            result.constant_iv_size = Some(size);
            if !matches!(size, 8 | 16) || body.len() != 25 + usize::from(size) {
                errors.push(
                    "TrackEncryptionBox constant IV must be exactly 8 or 16 bytes".to_string(),
                );
            }
        }
        _ => errors.push(
            "TrackEncryptionBox requires an 8/16-byte per-sample IV or constant IV".to_string(),
        ),
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

fn cenc_protection_json(protection: &CencProtection) -> Value {
    json!({
        "original_format": protection.original_format,
        "scheme": protection.scheme,
        "scheme_version": protection.scheme_version,
        "tenc_version": protection.tenc_version,
        "default_is_protected": protection.default_is_protected,
        "per_sample_iv_size": protection.per_sample_iv_size,
        "default_kid": protection.default_kid,
        "crypt_byte_block": protection.crypt_byte_block,
        "skip_byte_block": protection.skip_byte_block,
        "constant_iv_size": protection.constant_iv_size,
        "valid": protection.valid
    })
}

fn parse_iamf_sample_entry(
    entry: &[u8],
    sample_entry_type: &str,
    children: &[(String, &[u8])],
    protection: Option<CencProtection>,
    checks: &mut Vec<AuditCheck>,
) -> IamfSampleEntry {
    let channel_count = entry
        .get(24..26)
        .map(|value| u16::from_be_bytes(value.try_into().unwrap()));
    let sample_rate = entry.get(32..36).map(be_u32);
    let has_sampling_rate_box = children.iter().any(|(kind, _)| kind == "srat");
    let configurations: Vec<_> = children
        .iter()
        .filter(|(kind, _)| kind == "iacb")
        .map(|(_, payload)| *payload)
        .collect();
    let mut result = IamfSampleEntry {
        sample_entry_type: sample_entry_type.to_string(),
        channel_count,
        sample_rate,
        configuration_boxes: configurations.len(),
        has_sampling_rate_box,
        protection,
        ..IamfSampleEntry::default()
    };
    checks.push(check(
        "FORGE-ISOBMFF-IAMF-SAMPLE-ENTRY",
        channel_count == Some(0) && sample_rate == Some(0) && !has_sampling_rate_box,
        "IAMF AudioSampleEntry uses zero channelcount/samplerate fields and no SamplingRateBox",
        Some(json!({
            "channel_count": channel_count,
            "sample_rate_fixed_16_16": sample_rate,
            "sampling_rate_box": has_sampling_rate_box
        })),
    ));

    let mut valid = configurations.len() == 1;
    let mut error = (configurations.len() != 1).then(|| {
        format!(
            "IAMF sample entry contains {} IAConfigurationBox values; exactly one is required",
            configurations.len()
        )
    });
    if let Some(payload) = configurations.first() {
        result.configuration_version = payload.first().copied();
        if result.configuration_version != Some(1) {
            valid = false;
            error = Some("IAConfigurationBox configurationVersion must be 1".to_string());
        } else {
            match parse_iamf_leb(&payload[1..]) {
                Some((size, leb_bytes)) => {
                    let start = 1 + leb_bytes;
                    match usize::try_from(size)
                        .ok()
                        .and_then(|size| start.checked_add(size).map(|end| (size, end)))
                    {
                        Some((size, end)) if size > 0 && end <= payload.len() => {
                            result.config_obus = payload[start..end].to_vec();
                            result.config_trailing_bytes = payload.len() - end;
                            if let Err(config_error) =
                                validate_iamf_config_obus(&result.config_obus)
                            {
                                valid = false;
                                error = Some(config_error);
                            }
                        }
                        _ => {
                            valid = false;
                            error = Some(
                                "IAConfigurationBox configOBUs_size is zero, overflowing, or truncated"
                                    .to_string(),
                            );
                        }
                    }
                }
                None => {
                    valid = false;
                    error =
                        Some("IAConfigurationBox configOBUs_size is invalid LEB128".to_string());
                }
            }
        }
    }
    checks.push(check(
        "FORGE-ISOBMFF-IAMF-CONFIG",
        valid,
        error.unwrap_or_else(|| {
            "IAConfigurationBox v1 contains a bounded non-empty configOBUs sequence".to_string()
        }),
        Some(json!({
            "configuration_boxes": result.configuration_boxes,
            "configuration_version": result.configuration_version,
            "config_obus_bytes": result.config_obus.len(),
            "ignored_trailing_bytes": result.config_trailing_bytes
        })),
    ));
    result
}

fn validate_iamf_config_obus(bytes: &[u8]) -> Result<(), String> {
    let mut offset = 0_usize;
    let mut obus = 0_usize;
    while offset < bytes.len() {
        let header = *bytes
            .get(offset)
            .ok_or_else(|| "configOBUs has a truncated OBU header".to_string())?;
        let obu_type = header >> 3;
        let (body_size, leb_bytes) = parse_iamf_leb(&bytes[offset + 1..])
            .ok_or_else(|| "configOBUs has an invalid OBU size".to_string())?;
        let body_size = usize::try_from(body_size)
            .map_err(|_| "configOBUs OBU size does not fit memory".to_string())?;
        let size = 1_usize
            .checked_add(leb_bytes)
            .and_then(|size| size.checked_add(body_size))
            .ok_or_else(|| "configOBUs OBU size overflows".to_string())?;
        if size > 1 << 21 || offset.checked_add(size).is_none_or(|end| end > bytes.len()) {
            return Err("configOBUs OBU exceeds its bound or the 2 MiB profile limit".into());
        }
        if (3..=23).contains(&obu_type) {
            return Err(format!(
                "configOBUs contains IA data OBU type {obu_type}; only Descriptors and Reserved OBUs are allowed"
            ));
        }
        obus += 1;
        offset += size;
    }
    if offset != bytes.len() || obus == 0 {
        return Err("configOBUs is empty or does not end on an OBU boundary".into());
    }
    Ok(())
}

fn iamf_config_timing(
    path: &Path,
    config_obus: &[u8],
    media_timescale: u32,
) -> Option<IamfEntryTiming> {
    let audit =
        crate::iamf_qc::audit_reader(path, Cursor::new(config_obus), config_obus.len() as u64)
            .ok()?;
    let configs = audit.properties.get("codec_configs")?.as_array()?;
    let timings: Vec<(u64, u64)> = configs
        .iter()
        .map(|config| {
            Some((
                config.get("num_samples_per_frame")?.as_u64()?,
                config.get("sample_rate_hz")?.as_u64()?,
            ))
        })
        .collect::<Option<_>>()?;
    let required_rolls: Vec<i64> = configs
        .iter()
        .filter(|config| {
            matches!(
                config.get("codec_id").and_then(Value::as_str),
                Some("Opus" | "mp4a")
            )
        })
        .map(|config| config.get("audio_roll_distance")?.as_i64())
        .collect::<Option<_>>()?;
    let unique_rolls: HashSet<i64> = required_rolls.iter().copied().collect();
    if unique_rolls.len() > 1 {
        return None;
    }
    let roll_distance = unique_rolls.into_iter().next();
    let (reference_samples, reference_rate) = *timings.first()?;
    if reference_rate == 0
        || timings.iter().any(|(samples, rate)| {
            u128::from(*samples) * u128::from(reference_rate)
                != u128::from(reference_samples) * u128::from(*rate)
        })
    {
        return None;
    }
    let scaled = reference_samples.checked_mul(u64::from(media_timescale))?;
    (scaled % reference_rate == 0).then(|| IamfEntryTiming {
        duration_ticks: scaled / reference_rate,
        roll_distance,
    })
}

fn parse_iamf_leb(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0_u64;
    for (index, byte) in bytes.iter().copied().take(8).enumerate() {
        let shift = index * 7;
        value |= u64::from(byte & 0x7f).checked_shl(shift as u32)?;
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
    }
    None
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
            if samples <= MAX_TABLE_ENTRIES as u64 && delta <= u64::from(u32::MAX) {
                track
                    .sample_durations
                    .extend(std::iter::repeat_n(delta as u32, count as usize));
            }
        }
        if samples > MAX_TABLE_ENTRIES as u64 {
            track.sample_durations.clear();
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
        valid &= count <= MAX_TABLE_ENTRIES as u64;
        if uniform == 0 {
            let required = 12_u64.saturating_add(count.saturating_mul(4));
            valid &= required == body.len() as u64;
            if valid {
                track.sample_sizes = body[12..].chunks_exact(4).map(be_u32).collect();
                track.sample_bytes =
                    Some(track.sample_sizes.iter().map(|item| u64::from(*item)).sum());
            }
        } else {
            valid &= body.len() == 12;
            track.sample_bytes = Some(uniform.saturating_mul(count));
            if valid {
                track.sample_sizes = vec![uniform as u32; count as usize];
            }
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
            valid &= count <= MAX_TABLE_ENTRIES;
            track.sample_sizes = match field_size {
                4 => (0..count)
                    .map(|index| {
                        let value = payload[index / 2];
                        u32::from(if index % 2 == 0 {
                            value >> 4
                        } else {
                            value & 0x0f
                        })
                    })
                    .collect(),
                8 => payload.iter().map(|value| u32::from(*value)).collect(),
                16 => payload
                    .chunks_exact(2)
                    .map(|value| u32::from(u16::from_be_bytes(value.try_into().unwrap())))
                    .collect(),
                _ => Vec::new(),
            };
            let bytes = track
                .sample_sizes
                .iter()
                .map(|value| u64::from(*value))
                .sum();
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
    let default_description_index = if version == Some(2) {
        body.get(12..16).map(be_u32)
    } else {
        None
    };
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
    let duplicate_roll = grouping_type.as_deref() == Some("roll")
        && track.sample_group_types.iter().any(|kind| kind == "roll");
    let duplicate_cenc = grouping_type.as_deref() == Some("seig")
        && track.sample_group_types.iter().any(|kind| kind == "seig");
    let mut valid = grouping_type.is_some()
        && entry_count <= MAX_TABLE_ENTRIES
        && !duplicate_roll
        && !duplicate_cenc
        && (grouping_type.as_deref() != Some("seig") || matches!(version, Some(1 | 2)));
    let mut roll_distances = Vec::new();
    let mut cenc_entries = Vec::new();
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
            if grouping_type.as_deref() == Some("roll") {
                roll_distances.push(i16::from_be_bytes(
                    body[offset..offset + 2].try_into().unwrap(),
                ));
            }
        } else if grouping_type.as_deref() == Some("seig") {
            match parse_cenc_sample_group_entry(&body[offset..offset + length]) {
                Ok(entry) => cenc_entries.push(entry),
                Err(()) => valid = false,
            }
        }
        offset += length;
    }
    valid &= offset == body.len();
    if let Some(default_description_index) = default_description_index {
        valid &= default_description_index <= entry_count as u32;
        if grouping_type.as_deref() == Some("roll") {
            track.roll_default_description_index = Some(default_description_index);
            track.roll_default_group = default_description_index > 0;
        } else if grouping_type.as_deref() == Some("seig") {
            track.cenc_default_description_index = Some(default_description_index);
        }
    }
    if let Some(grouping_type) = grouping_type {
        track.sample_group_types.push(grouping_type);
    }
    track.roll_distances.extend(roll_distances);
    track.cenc_group_entries.extend(cenc_entries);
    checks.push(check(
        "FORGE-ISOBMFF-SAMPLE-GROUP-DESCRIPTION",
        valid,
        "sample-group descriptions are bounded; roll/prol distances and CENC seig entries have valid field geometry",
        Some(json!({
            "grouping_types": track.sample_group_types,
            "roll_distances": track.roll_distances,
            "cenc_key_ids": track.cenc_group_entries.iter()
                .map(|entry| &entry.kid)
                .collect::<Vec<_>>(),
            "default_description_index": default_description_index
        })),
    ));
    Ok(())
}

fn parse_cenc_sample_group_entry(body: &[u8]) -> Result<CencSampleGroupEntry, ()> {
    if body.len() < 20 {
        return Err(());
    }
    let reserved = body[0];
    let pattern = body[1];
    let is_protected = body[2];
    let per_sample_iv_size = body[3];
    let kid = hex_bytes(&body[4..20]);
    let crypt_byte_block = pattern >> 4;
    let skip_byte_block = pattern & 0x0f;
    let mut offset = 20_usize;
    let constant_iv_size = if is_protected == 1 && per_sample_iv_size == 0 {
        let size = *body.get(offset).ok_or(())?;
        offset += 1;
        let end = offset.checked_add(usize::from(size)).ok_or(())?;
        if body.get(offset..end).is_none() {
            return Err(());
        }
        offset = end;
        Some(size)
    } else {
        None
    };
    let valid = reserved == 0
        && matches!(is_protected, 0 | 1)
        && (is_protected == 1 || per_sample_iv_size == 0)
        && matches!(per_sample_iv_size, 0 | 8 | 16)
        && constant_iv_size.is_none_or(|size| matches!(size, 8 | 16))
        && offset == body.len();
    Ok(CencSampleGroupEntry {
        is_protected,
        per_sample_iv_size,
        kid,
        crypt_byte_block,
        skip_byte_block,
        constant_iv_size,
        valid,
    })
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
    let duplicate_roll =
        grouping_type.as_deref() == Some("roll") && !track.roll_sample_runs.is_empty();
    let duplicate_cenc = grouping_type.as_deref() == Some("seig") && track.cenc_sbgp_seen;
    let mut samples = 0_u64;
    let grouping_parameter_valid = version != Some(1)
        || grouping_type.as_deref() != Some("seig")
        || body.get(8..12).is_some_and(|value| be_u32(value) == 0);
    let mut valid =
        grouping_type.is_some() && !duplicate_roll && !duplicate_cenc && grouping_parameter_valid;
    let mut runs = Vec::new();
    if let Some(entries) = entries {
        for entry in entries {
            let count = u64::from(be_u32(&entry[..4]));
            let description_index = be_u32(&entry[4..8]);
            let local_index = description_index.checked_sub(0x1_0000);
            let index_bounded = description_index == 0
                || description_index <= MAX_TABLE_ENTRIES as u32
                || local_index.is_some_and(|index| index > 0 && index <= MAX_TABLE_ENTRIES as u32);
            valid &= count > 0 && index_bounded;
            samples = samples.saturating_add(count);
            runs.push((count, description_index));
        }
        if grouping_type.as_deref() == Some("roll") {
            track.sample_group_samples = Some(samples);
            track.roll_sample_runs = runs;
        } else if grouping_type.as_deref() == Some("seig") {
            track.cenc_sample_runs = runs;
            track.cenc_sbgp_seen = true;
        }
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
    let mut fragment = Fragment {
        start: header.start,
        ..Fragment::default()
    };
    let mut implicit_data_offset = None;
    let track_extends = state.track_extends.clone();
    let mut mfhd_seen = false;
    for child in children {
        match &child.kind {
            b"mfhd" => {
                if mfhd_seen {
                    return Err(
                        "MovieFragmentBox contains multiple MovieFragmentHeaderBox values".into(),
                    );
                }
                mfhd_seen = true;
                let body = read_control(path, file, child)?;
                if body.len() != 8 || body[0] != 0 || body[1..4] != [0, 0, 0] {
                    return Err(
                        "MovieFragmentHeaderBox is truncated or has unsupported fields".into(),
                    );
                }
                fragment.sequence = Some(be_u32(&body[4..8]));
            }
            b"traf" => parse_traf(
                path,
                file,
                child,
                &mut FragmentParseContext {
                    moof_start: header.start,
                    track_extends: &track_extends,
                    box_count: &mut state.box_count,
                    implicit_data_offset: &mut implicit_data_offset,
                    fragment: &mut fragment,
                    checks,
                },
            )?,
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
    context: &mut FragmentParseContext<'_>,
) -> Result<(), String> {
    let children = list_boxes(path, file, header.body_start, header.end, context.box_count)?;
    let mut tfhd_body = None;
    let mut decode_time = None;
    let mut tfdt_seen = false;
    let mut trun_bodies = Vec::new();
    let mut groups = Track::default();
    for child in children {
        match &child.kind {
            b"tfhd" => {
                if tfhd_body.is_some() {
                    return Err(
                        "TrackFragmentBox contains multiple TrackFragmentHeaderBox values".into(),
                    );
                }
                tfhd_body = Some(read_control(path, file, child)?);
            }
            b"tfdt" => {
                if tfdt_seen {
                    return Err(
                        "TrackFragmentBox contains multiple TrackFragmentDecodeTimeBox values"
                            .into(),
                    );
                }
                tfdt_seen = true;
                let body = read_control(path, file, child)?;
                decode_time = match body.as_slice() {
                    [0, 0, 0, 0, rest @ ..] if rest.len() == 4 => Some(u64::from(be_u32(rest))),
                    [1, 0, 0, 0, rest @ ..] if rest.len() == 8 => Some(be_u64(rest)),
                    _ => {
                        return Err(
                            "TrackFragmentDecodeTimeBox is truncated or has unsupported fields"
                                .into(),
                        )
                    }
                };
            }
            b"trun" => {
                trun_bodies.push(read_control(path, file, child)?);
            }
            b"sgpd" => parse_sgpd(path, file, child, &mut groups, context.checks)?,
            b"sbgp" => parse_sbgp(path, file, child, &mut groups, context.checks)?,
            b"senc" => groups
                .cenc_auxiliary
                .senc
                .push(parse_senc(path, file, child)?),
            b"saiz" => groups
                .cenc_auxiliary
                .saiz
                .push(parse_saiz(path, file, child)?),
            b"saio" => groups
                .cenc_auxiliary
                .saio
                .push(parse_saio(path, file, child)?),
            _ => {}
        }
    }

    let body = tfhd_body
        .as_deref()
        .ok_or_else(|| "TrackFragmentBox is missing TrackFragmentHeaderBox".to_string())?;
    if body.len() < 8 || body[0] != 0 {
        return Err("TrackFragmentHeaderBox is truncated or has unsupported version".into());
    }
    let flags = full_box_flags(body);
    let known_flags = 0x000001 | 0x000002 | 0x000008 | 0x000010 | 0x000020 | 0x010000 | 0x020000;
    if flags & !known_flags != 0 || flags & 0x010000 != 0 && flags & 0x020000 != 0 {
        return Err(format!(
            "TrackFragmentHeaderBox has unsupported flags {flags:#08x}"
        ));
    }
    let track_id = be_u32(&body[4..8]);
    if track_id == 0 {
        return Err("TrackFragmentHeaderBox uses track_ID 0".into());
    }
    let mut offset = 8_usize;
    let base_data_offset = if flags & 0x000001 != 0 {
        take_u64(body, &mut offset, "tfhd base_data_offset")?
    } else if flags & 0x020000 != 0 {
        context.moof_start
    } else {
        context.implicit_data_offset.unwrap_or(context.moof_start)
    };
    let defaults = context
        .track_extends
        .get(&track_id)
        .copied()
        .unwrap_or_default();
    let description_index = if flags & 0x000002 != 0 {
        take_u32(body, &mut offset, "tfhd sample_description_index")?
    } else {
        defaults.description_index
    };
    let default_duration = if flags & 0x000008 != 0 {
        take_u32(body, &mut offset, "tfhd default_sample_duration")?
    } else {
        defaults.duration
    };
    let default_size = if flags & 0x000010 != 0 {
        take_u32(body, &mut offset, "tfhd default_sample_size")?
    } else {
        defaults.size
    };
    let default_flags = if flags & 0x000020 != 0 {
        take_u32(body, &mut offset, "tfhd default_sample_flags")?
    } else {
        defaults.flags
    };
    if offset != body.len() {
        return Err("TrackFragmentHeaderBox fields are incomplete".into());
    }

    let movie_relative = flags & 0x000001 == 0 && flags & 0x020000 != 0;
    context.fragment.movie_relative =
        Some(context.fragment.movie_relative.unwrap_or(true) && movie_relative);
    let mut track_fragment = TrackFragment {
        track_id: Some(track_id),
        decode_time,
        samples_resolved: true,
        roll_distances: groups.roll_distances,
        roll_default_description_index: groups.roll_default_description_index,
        roll_sample_runs: groups.roll_sample_runs,
        cenc_group_entries: groups.cenc_group_entries,
        cenc_default_description_index: groups.cenc_default_description_index,
        cenc_sample_runs: groups.cenc_sample_runs,
        cenc_sbgp_seen: groups.cenc_sbgp_seen,
        cenc_auxiliary: groups.cenc_auxiliary,
        ..TrackFragment::default()
    };
    let mut run_data_offset = None;
    for body in &trun_bodies {
        parse_track_run(
            body,
            base_data_offset,
            description_index,
            default_duration,
            default_size,
            default_flags,
            &mut run_data_offset,
            &mut track_fragment,
        )?;
    }
    if trun_bodies.is_empty() {
        return Err("TrackFragmentBox does not contain a TrackRunBox".into());
    }
    if let Some(end) = run_data_offset {
        *context.implicit_data_offset = Some(end);
    }
    context.fragment.sample_count = context
        .fragment
        .sample_count
        .saturating_add(track_fragment.declared_sample_count);
    context.fragment.track_ids.push(track_id);
    if let Some(time) = decode_time {
        context.fragment.decode_times.push((track_id, time));
    }
    context.checks.push(check(
        "FORGE-ISOBMFF-TRACK-FRAGMENT",
        decode_time.is_some() && track_fragment.declared_sample_count > 0,
        "track fragment identifies its track, base decode time, and bounded samples",
        Some(json!({
            "track_id": track_id,
            "decode_time": decode_time,
            "samples": track_fragment.declared_sample_count,
            "sample_fields_resolve": track_fragment.samples_resolved,
            "sample_description_index": description_index,
            "movie_relative": movie_relative
        })),
    ));
    context.fragment.tracks.push(track_fragment);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn parse_track_run(
    body: &[u8],
    base_data_offset: u64,
    description_index: u32,
    default_duration: u32,
    default_size: u32,
    default_flags: u32,
    previous_data_end: &mut Option<u64>,
    track_fragment: &mut TrackFragment,
) -> Result<(), String> {
    if body.len() < 8 || !matches!(body[0], 0 | 1) {
        return Err("TrackRunBox is truncated or has unsupported version".into());
    }
    let flags = full_box_flags(body);
    let known_flags = 0x000001 | 0x000004 | 0x000100 | 0x000200 | 0x000400 | 0x000800;
    if flags & !known_flags != 0 {
        return Err(format!("TrackRunBox has unsupported flags {flags:#08x}"));
    }
    let sample_count = usize::try_from(be_u32(&body[4..8]))
        .map_err(|_| "trun sample_count does not fit memory".to_string())?;
    if sample_count == 0 || sample_count > MAX_TABLE_ENTRIES {
        return Err(format!(
            "TrackRunBox sample_count {sample_count} is zero or exceeds the safety limit"
        ));
    }
    if sample_count > MAX_TABLE_ENTRIES.saturating_sub(track_fragment.samples.len()) {
        return Err("fragment sample total exceeds the safety limit".into());
    }
    track_fragment.declared_sample_count = track_fragment
        .declared_sample_count
        .checked_add(sample_count as u64)
        .ok_or_else(|| "fragment declared sample count overflows uint64".to_string())?;
    let mut offset = 8_usize;
    let data_offset = if flags & 0x000001 != 0 {
        Some(take_i32(body, &mut offset, "trun data_offset")?)
    } else {
        None
    };
    let first_sample_flags = if flags & 0x000004 != 0 {
        Some(take_u32(body, &mut offset, "trun first_sample_flags")?)
    } else {
        None
    };
    if first_sample_flags.is_some() && flags & 0x000400 != 0 {
        return Err("TrackRunBox combines first_sample_flags with per-sample flags".into());
    }
    let mut data_cursor = if let Some(relative) = data_offset {
        checked_signed_offset(base_data_offset, relative)?
    } else {
        previous_data_end.unwrap_or(base_data_offset)
    };
    for index in 0..sample_count {
        let duration = if flags & 0x000100 != 0 {
            take_u32(body, &mut offset, "trun sample_duration")?
        } else {
            default_duration
        };
        let size = if flags & 0x000200 != 0 {
            take_u32(body, &mut offset, "trun sample_size")?
        } else {
            default_size
        };
        let sample_flags = if flags & 0x000400 != 0 {
            take_u32(body, &mut offset, "trun sample_flags")?
        } else if index == 0 {
            first_sample_flags.unwrap_or(default_flags)
        } else {
            default_flags
        };
        if flags & 0x000800 != 0 {
            let _ = take_u32(body, &mut offset, "trun sample_composition_time_offset")?;
            track_fragment.has_composition_offsets = true;
        }
        let resolved = duration > 0 && size > 0 && description_index > 0;
        track_fragment.samples_resolved &= resolved;
        track_fragment.samples.push(SampleLocation {
            offset: data_cursor,
            size: u64::from(size),
            description_index,
        });
        track_fragment.sample_durations.push(duration);
        track_fragment.sample_flags.push(sample_flags);
        if resolved {
            data_cursor = data_cursor
                .checked_add(u64::from(size))
                .ok_or_else(|| "fragment sample data offset overflows uint64".to_string())?;
        }
    }
    if offset != body.len() {
        return Err("TrackRunBox has trailing or incomplete sample fields".into());
    }
    *previous_data_end = Some(data_cursor);
    Ok(())
}

fn full_box_flags(body: &[u8]) -> u32 {
    (u32::from(body[1]) << 16) | (u32::from(body[2]) << 8) | u32::from(body[3])
}

fn parse_senc(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
) -> Result<CencSampleEncryption, String> {
    let body = read_control(path, file, header)?;
    if body.len() < 8 || body[0] != 0 {
        return Err("SampleEncryptionBox is truncated or has unsupported version".into());
    }
    let flags = full_box_flags(&body);
    if flags & !0x000003 != 0 {
        return Err(format!(
            "SampleEncryptionBox has unsupported flags {flags:#08x}"
        ));
    }
    let mut offset = 4_usize;
    let iv_size_override = if flags & 0x000001 != 0 {
        let fields = body
            .get(offset..offset + 20)
            .ok_or_else(|| "SampleEncryptionBox override fields are truncated".to_string())?;
        offset += 20;
        Some(fields[3])
    } else {
        None
    };
    let sample_count = take_u32(&body, &mut offset, "senc sample_count")?;
    if usize::try_from(sample_count)
        .ok()
        .is_none_or(|count| count > MAX_TABLE_ENTRIES)
    {
        return Err("SampleEncryptionBox sample_count exceeds the safety limit".into());
    }
    Ok(CencSampleEncryption {
        flags,
        sample_count,
        iv_size_override,
        entry_bytes: body.len() - offset,
    })
}

fn parse_saiz(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
) -> Result<CencSampleAuxiliarySizes, String> {
    let body = read_control(path, file, header)?;
    if body.len() < 9 || body[0] != 0 {
        return Err(
            "SampleAuxiliaryInformationSizesBox is truncated or has unsupported version".into(),
        );
    }
    let flags = full_box_flags(&body);
    if flags & !1 != 0 {
        return Err(format!(
            "SampleAuxiliaryInformationSizesBox has unsupported flags {flags:#08x}"
        ));
    }
    let mut offset = 4_usize;
    let auxiliary_type = if flags == 1 {
        let kind = body
            .get(offset..offset + 4)
            .map(fourcc)
            .ok_or_else(|| "saiz auxiliary_info_type is truncated".to_string())?;
        offset += 8;
        Some(kind)
    } else {
        None
    };
    let default_size = *body
        .get(offset)
        .ok_or_else(|| "saiz default_sample_info_size is truncated".to_string())?;
    offset += 1;
    let sample_count = take_u32(&body, &mut offset, "saiz sample_count")?;
    let count = usize::try_from(sample_count)
        .map_err(|_| "saiz sample_count does not fit memory".to_string())?;
    if count > MAX_TABLE_ENTRIES {
        return Err("saiz sample_count exceeds the safety limit".into());
    }
    let sizes = if default_size == 0 {
        let sizes = body
            .get(offset..offset + count)
            .ok_or_else(|| "saiz sample_info_size array is truncated".to_string())?
            .to_vec();
        offset += count;
        sizes
    } else {
        Vec::new()
    };
    if offset != body.len() {
        return Err("SampleAuxiliaryInformationSizesBox has trailing bytes".into());
    }
    Ok(CencSampleAuxiliarySizes {
        auxiliary_type,
        default_size,
        sample_count,
        sizes,
    })
}

fn parse_saio(
    path: &Path,
    file: &mut File,
    header: BoxHeader,
) -> Result<CencSampleAuxiliaryOffsets, String> {
    let body = read_control(path, file, header)?;
    if body.len() < 8 || !matches!(body[0], 0 | 1) {
        return Err(
            "SampleAuxiliaryInformationOffsetsBox is truncated or has unsupported version".into(),
        );
    }
    let flags = full_box_flags(&body);
    if flags & !1 != 0 {
        return Err(format!(
            "SampleAuxiliaryInformationOffsetsBox has unsupported flags {flags:#08x}"
        ));
    }
    let mut offset = 4_usize;
    let auxiliary_type = if flags == 1 {
        let kind = body
            .get(offset..offset + 4)
            .map(fourcc)
            .ok_or_else(|| "saio auxiliary_info_type is truncated".to_string())?;
        offset += 8;
        Some(kind)
    } else {
        None
    };
    let entry_count = take_u32(&body, &mut offset, "saio entry_count")?;
    let count = usize::try_from(entry_count)
        .map_err(|_| "saio entry_count does not fit memory".to_string())?;
    if count == 0 || count > MAX_TABLE_ENTRIES {
        return Err("saio entry_count is zero or exceeds the safety limit".into());
    }
    let mut offsets = Vec::with_capacity(count);
    for _ in 0..count {
        offsets.push(if body[0] == 0 {
            u64::from(take_u32(&body, &mut offset, "saio offset")?)
        } else {
            take_u64(&body, &mut offset, "saio offset")?
        });
    }
    if offset != body.len() {
        return Err("SampleAuxiliaryInformationOffsetsBox has trailing bytes".into());
    }
    Ok(CencSampleAuxiliaryOffsets {
        auxiliary_type,
        offsets,
    })
}

fn take_u32(body: &[u8], offset: &mut usize, field: &str) -> Result<u32, String> {
    let value = body
        .get(*offset..*offset + 4)
        .map(be_u32)
        .ok_or_else(|| format!("{field} is truncated"))?;
    *offset += 4;
    Ok(value)
}

fn take_i32(body: &[u8], offset: &mut usize, field: &str) -> Result<i32, String> {
    let value = body
        .get(*offset..*offset + 4)
        .map(|bytes| i32::from_be_bytes(bytes.try_into().expect("four-byte slice")))
        .ok_or_else(|| format!("{field} is truncated"))?;
    *offset += 4;
    Ok(value)
}

fn take_u64(body: &[u8], offset: &mut usize, field: &str) -> Result<u64, String> {
    let value = body
        .get(*offset..*offset + 8)
        .map(be_u64)
        .ok_or_else(|| format!("{field} is truncated"))?;
    *offset += 8;
    Ok(value)
}

fn checked_signed_offset(base: u64, relative: i32) -> Result<u64, String> {
    if relative >= 0 {
        base.checked_add(relative as u64)
    } else {
        base.checked_sub(u64::from(relative.unsigned_abs()))
    }
    .ok_or_else(|| "fragment data offset overflows or precedes the file".to_string())
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
        "sample_group_samples": track.sample_group_samples,
        "roll_default_group": track.roll_default_group,
        "roll_default_description_index": track.roll_default_description_index,
        "roll_sample_runs": track.roll_sample_runs,
        "cenc_sample_groups": {
            "default_description_index": track.cenc_default_description_index,
            "entries": track.cenc_group_entries.iter().map(|entry| json!({
                "is_protected": entry.is_protected,
                "per_sample_iv_size": entry.per_sample_iv_size,
                "kid": entry.kid,
                "crypt_byte_block": entry.crypt_byte_block,
                "skip_byte_block": entry.skip_byte_block,
                "constant_iv_size": entry.constant_iv_size,
                "valid": entry.valid
            })).collect::<Vec<_>>(),
            "sample_runs": track.cenc_sample_runs
        },
        "sync_sample_box": track.has_sync_sample_box,
        "composition_offsets": track.has_composition_offsets,
        "cenc_auxiliary": cenc_auxiliary_json(&track.cenc_auxiliary),
        "iamf_sample_entries": track.iamf_entries.iter().enumerate().filter_map(|(index, entry)| {
            entry.as_ref().map(|entry| json!({
                "sample_description_index": index + 1,
                "sample_entry_type": entry.sample_entry_type,
                "channel_count": entry.channel_count,
                "sample_rate_fixed_16_16": entry.sample_rate,
                "configuration_boxes": entry.configuration_boxes,
                "configuration_version": entry.configuration_version,
                "config_obus_bytes": entry.config_obus.len(),
                "ignored_trailing_bytes": entry.config_trailing_bytes,
                "sampling_rate_box": entry.has_sampling_rate_box,
                "protection": entry.protection.as_ref().map(cenc_protection_json)
            }))
        }).collect::<Vec<_>>()
    })
}

fn cenc_auxiliary_json(auxiliary: &CencAuxiliary) -> Value {
    json!({
        "senc": auxiliary.senc.iter().map(|item| json!({
            "flags": item.flags,
            "sample_count": item.sample_count,
            "iv_size_override": item.iv_size_override,
            "entry_bytes": item.entry_bytes
        })).collect::<Vec<_>>(),
        "saiz": auxiliary.saiz.iter().map(|item| json!({
            "auxiliary_type": item.auxiliary_type,
            "default_size": item.default_size,
            "sample_count": item.sample_count,
            "sizes": item.sizes
        })).collect::<Vec<_>>(),
        "saio": auxiliary.saio.iter().map(|item| json!({
            "auxiliary_type": item.auxiliary_type,
            "offsets": item.offsets
        })).collect::<Vec<_>>()
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

    fn full_box_with_flags(version: u8, flags: u32, payload: Vec<u8>) -> Vec<u8> {
        let mut body = vec![
            version,
            ((flags >> 16) & 0xff) as u8,
            ((flags >> 8) & 0xff) as u8,
            (flags & 0xff) as u8,
        ];
        body.extend(payload);
        body
    }

    #[derive(Clone, Copy)]
    struct TestCenc {
        scheme: [u8; 4],
        pattern: u8,
        iv_size: u8,
        subsample: bool,
    }

    fn test_cenc_sinf(config: TestCenc) -> Vec<u8> {
        let mut tenc = [vec![0, config.pattern, 1, config.iv_size], vec![0x11; 16]].concat();
        if config.iv_size == 0 {
            tenc.extend([vec![16], vec![0x22; 16]].concat());
        }
        boxed(
            b"sinf",
            [
                boxed(b"frma", b"iamf".to_vec()),
                boxed(
                    b"schm",
                    full_box(
                        0,
                        [
                            config.scheme.to_vec(),
                            0x0001_0000_u32.to_be_bytes().to_vec(),
                        ]
                        .concat(),
                    ),
                ),
                boxed(b"schi", boxed(b"tenc", full_box(1, tenc))),
            ]
            .concat(),
        )
    }

    fn test_cenc_auxiliary(config: TestCenc) -> Vec<u8> {
        if config.iv_size == 0 && !config.subsample {
            return Vec::new();
        }
        let mut entries = if config.iv_size == 0 {
            Vec::new()
        } else {
            vec![0x33; usize::from(config.iv_size)]
        };
        if config.subsample {
            entries.extend([1_u16.to_be_bytes().to_vec(), vec![0; 6]].concat());
        }
        let flags = if config.subsample { 0x000002 } else { 0 };
        let senc = boxed(
            b"senc",
            full_box_with_flags(0, flags, [1_u32.to_be_bytes().to_vec(), entries].concat()),
        );
        let auxiliary_size = config
            .iv_size
            .saturating_add(if config.subsample { 8 } else { 0 });
        let saiz = boxed(
            b"saiz",
            full_box(
                0,
                [vec![auxiliary_size], 1_u32.to_be_bytes().to_vec()].concat(),
            ),
        );
        let saio = boxed(
            b"saio",
            full_box(0, [1_u32.to_be_bytes(), 1_u32.to_be_bytes()].concat()),
        );
        [senc, saiz, saio].concat()
    }

    fn test_seig_entry(kid_byte: u8, iv_size: u8) -> Vec<u8> {
        let mut entry = vec![0, 0, 1, iv_size];
        entry.extend(vec![kid_byte; 16]);
        if iv_size == 0 {
            entry.push(16);
            entry.extend(vec![0x77; 16]);
        }
        entry
    }

    fn test_cenc_key_rotation_groups() -> Vec<u8> {
        let entries = [test_seig_entry(0x44, 8), test_seig_entry(0x55, 8)];
        let descriptions = entries
            .into_iter()
            .flat_map(|entry| {
                [
                    u32::try_from(entry.len()).unwrap().to_be_bytes().to_vec(),
                    entry,
                ]
                .concat()
            })
            .collect::<Vec<_>>();
        let sgpd = boxed(
            b"sgpd",
            full_box(
                1,
                [
                    b"seig".to_vec(),
                    0_u32.to_be_bytes().to_vec(),
                    2_u32.to_be_bytes().to_vec(),
                    descriptions,
                ]
                .concat(),
            ),
        );
        let sbgp = boxed(
            b"sbgp",
            full_box(
                0,
                [
                    b"seig".to_vec(),
                    2_u32.to_be_bytes().to_vec(),
                    1_u32.to_be_bytes().to_vec(),
                    1_u32.to_be_bytes().to_vec(),
                    1_u32.to_be_bytes().to_vec(),
                    2_u32.to_be_bytes().to_vec(),
                ]
                .concat(),
            ),
        );
        [sgpd, sbgp].concat()
    }

    fn test_fragment_cenc_group(kid_byte: u8) -> Vec<u8> {
        let entry = test_seig_entry(kid_byte, 8);
        let sgpd = boxed(
            b"sgpd",
            full_box(
                1,
                [
                    b"seig".to_vec(),
                    u32::try_from(entry.len()).unwrap().to_be_bytes().to_vec(),
                    1_u32.to_be_bytes().to_vec(),
                    entry,
                ]
                .concat(),
            ),
        );
        let sbgp = boxed(
            b"sbgp",
            full_box(
                0,
                [
                    b"seig".to_vec(),
                    1_u32.to_be_bytes().to_vec(),
                    1_u32.to_be_bytes().to_vec(),
                    0x1_0001_u32.to_be_bytes().to_vec(),
                ]
                .concat(),
            ),
        );
        [sgpd, sbgp].concat()
    }

    fn test_default_cenc_group(kid_byte: u8) -> Vec<u8> {
        let entry = test_seig_entry(kid_byte, 8);
        boxed(
            b"sgpd",
            full_box(
                2,
                [
                    b"seig".to_vec(),
                    u32::try_from(entry.len()).unwrap().to_be_bytes().to_vec(),
                    1_u32.to_be_bytes().to_vec(),
                    1_u32.to_be_bytes().to_vec(),
                    entry,
                ]
                .concat(),
            ),
        )
    }

    fn test_two_sample_cenc_auxiliary() -> Vec<u8> {
        let senc = boxed(
            b"senc",
            full_box(0, [2_u32.to_be_bytes().to_vec(), vec![0x33; 16]].concat()),
        );
        let saiz = boxed(
            b"saiz",
            full_box(0, [vec![8], 2_u32.to_be_bytes().to_vec()].concat()),
        );
        let saio = boxed(
            b"saio",
            full_box(0, [1_u32.to_be_bytes(), 1_u32.to_be_bytes()].concat()),
        );
        [senc, saiz, saio].concat()
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

    fn iamf_obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() < 128);
        [vec![obu_type << 3, payload.len() as u8], payload.to_vec()].concat()
    }

    fn minimal_iamf_mix() -> Vec<u8> {
        vec![
            0, 0, // Mix ID and no localized labels.
            1, 1, 0, // One sub-mix with one audio element.
            0, 0, // Stereo rendering mode and no rendering extension.
            100, 1, 0x80, 0, 0, // Element mix gain.
            100, 1, 0x80, 0, 0, // Output mix gain.
            1, 0x80, // One Sound System A stereo layout.
            0, 0, 0, 0, 0, // Base loudness fields.
        ]
    }

    fn minimal_iamf_mp4(compatible_brand: &[u8; 4], sample: Vec<u8>) -> Vec<u8> {
        minimal_iamf_mp4_protected(compatible_brand, sample, None)
    }

    fn minimal_iamf_mp4_protected(
        compatible_brand: &[u8; 4],
        sample: Vec<u8>,
        protection: Option<TestCenc>,
    ) -> Vec<u8> {
        minimal_iamf_mp4_protected_with_groups(
            compatible_brand,
            vec![sample],
            protection,
            Vec::new(),
            None,
        )
    }

    fn minimal_iamf_mp4_protected_with_groups(
        compatible_brand: &[u8; 4],
        samples: Vec<Vec<u8>>,
        protection: Option<TestCenc>,
        sample_groups: Vec<u8>,
        cenc_auxiliary: Option<Vec<u8>>,
    ) -> Vec<u8> {
        assert!(!samples.is_empty());
        let mut config = iamf_obu(31, b"iamf\x00\x00");
        config.extend(iamf_obu(
            0,
            &[0, b'i', b'p', b'c', b'm', 1, 0, 0, 0, 16, 0, 0, 187, 128],
        ));
        config.extend(iamf_obu(1, &[0, 0, 0, 1, 0, 0, 0x20, 0, 1, 0]));
        config.extend(iamf_obu(2, &minimal_iamf_mix()));
        let iacb = boxed(
            b"iacb",
            [vec![1, u8::try_from(config.len()).unwrap()], config.clone()].concat(),
        );
        let mut sample_entry = vec![0_u8; 28];
        sample_entry[6..8].copy_from_slice(&1_u16.to_be_bytes());
        sample_entry.extend(iacb);
        if let Some(protection) = protection {
            sample_entry.extend(test_cenc_sinf(protection));
        }
        let stsd = boxed(
            b"stsd",
            full_box(
                0,
                [
                    1_u32.to_be_bytes().to_vec(),
                    boxed(
                        if protection.is_some() {
                            b"enca"
                        } else {
                            b"iamf"
                        },
                        sample_entry,
                    ),
                ]
                .concat(),
            ),
        );
        let stts = boxed(
            b"stts",
            full_box(
                0,
                [
                    1_u32.to_be_bytes(),
                    u32::try_from(samples.len()).unwrap().to_be_bytes(),
                    1_u32.to_be_bytes(),
                ]
                .concat(),
            ),
        );
        let sample_sizes = samples
            .iter()
            .map(|sample| u32::try_from(sample.len()).unwrap())
            .collect::<Vec<_>>();
        let stsz = boxed(
            b"stsz",
            full_box(
                0,
                [
                    0_u32.to_be_bytes().to_vec(),
                    u32::try_from(samples.len()).unwrap().to_be_bytes().to_vec(),
                    sample_sizes
                        .iter()
                        .flat_map(|size| size.to_be_bytes())
                        .collect::<Vec<_>>(),
                ]
                .concat(),
            ),
        );
        let stsc = boxed(
            b"stsc",
            full_box(
                0,
                [
                    1_u32.to_be_bytes(),
                    1_u32.to_be_bytes(),
                    u32::try_from(samples.len()).unwrap().to_be_bytes(),
                    1_u32.to_be_bytes(),
                ]
                .concat(),
            ),
        );
        let ftyp = boxed(
            b"ftyp",
            [b"iamf".as_slice(), &[0, 0, 0, 0], compatible_brand, b"iso6"].concat(),
        );
        let make_moov = |chunk_offset: u32| {
            let stco = boxed(
                b"stco",
                full_box(
                    0,
                    [1_u32.to_be_bytes(), chunk_offset.to_be_bytes()].concat(),
                ),
            );
            let stbl = boxed(
                b"stbl",
                [
                    stsd.clone(),
                    stts.clone(),
                    stsz.clone(),
                    stsc.clone(),
                    stco,
                    sample_groups.clone(),
                    cenc_auxiliary
                        .clone()
                        .unwrap_or_else(|| protection.map(test_cenc_auxiliary).unwrap_or_default()),
                ]
                .concat(),
            );
            let mdhd = boxed(
                b"mdhd",
                full_box(
                    0,
                    [
                        vec![0; 8],
                        48_000_u32.to_be_bytes().to_vec(),
                        u32::try_from(samples.len()).unwrap().to_be_bytes().to_vec(),
                        vec![0; 4],
                    ]
                    .concat(),
                ),
            );
            let hdlr = boxed(
                b"hdlr",
                full_box(0, [vec![0; 4], b"soun".to_vec(), vec![0; 12]].concat()),
            );
            let tkhd = boxed(
                b"tkhd",
                full_box(
                    0,
                    [vec![0; 8], 1_u32.to_be_bytes().to_vec(), vec![0; 68]].concat(),
                ),
            );
            let mdia = boxed(b"mdia", [mdhd, hdlr, boxed(b"minf", stbl)].concat());
            boxed(b"moov", boxed(b"trak", [tkhd, mdia].concat()))
        };
        let placeholder = make_moov(0);
        let chunk_offset = u32::try_from(ftyp.len() + placeholder.len() + 8).unwrap();
        let moov = make_moov(chunk_offset);
        [
            ftyp,
            moov,
            boxed(b"mdat", samples.into_iter().flatten().collect()),
        ]
        .concat()
    }

    fn minimal_fragmented_iamf(
        sample: Vec<u8>,
        data_offset_adjustment: i32,
        composition_offset: bool,
        initialization_only: bool,
    ) -> Vec<u8> {
        minimal_fragmented_iamf_protected(
            sample,
            data_offset_adjustment,
            composition_offset,
            initialization_only,
            None,
        )
    }

    fn minimal_fragmented_iamf_protected(
        sample: Vec<u8>,
        data_offset_adjustment: i32,
        composition_offset: bool,
        initialization_only: bool,
        protection: Option<TestCenc>,
    ) -> Vec<u8> {
        minimal_fragmented_iamf_protected_with_groups(
            sample,
            data_offset_adjustment,
            composition_offset,
            initialization_only,
            protection,
            Vec::new(),
            None,
        )
    }

    fn minimal_fragmented_iamf_protected_with_groups(
        sample: Vec<u8>,
        data_offset_adjustment: i32,
        composition_offset: bool,
        initialization_only: bool,
        protection: Option<TestCenc>,
        sample_groups: Vec<u8>,
        cenc_auxiliary: Option<Vec<u8>>,
    ) -> Vec<u8> {
        let mut config = iamf_obu(31, b"iamf\x00\x00");
        config.extend(iamf_obu(
            0,
            &[0, b'i', b'p', b'c', b'm', 1, 0, 0, 0, 16, 0, 0, 187, 128],
        ));
        config.extend(iamf_obu(1, &[0, 0, 0, 1, 0, 0, 0x20, 0, 1, 0]));
        config.extend(iamf_obu(2, &minimal_iamf_mix()));
        let iacb = boxed(
            b"iacb",
            [vec![1, u8::try_from(config.len()).unwrap()], config].concat(),
        );
        let mut sample_entry = vec![0_u8; 28];
        sample_entry[6..8].copy_from_slice(&1_u16.to_be_bytes());
        sample_entry.extend(iacb);
        if let Some(protection) = protection {
            sample_entry.extend(test_cenc_sinf(protection));
        }
        let stsd = boxed(
            b"stsd",
            full_box(
                0,
                [
                    1_u32.to_be_bytes().to_vec(),
                    boxed(
                        if protection.is_some() {
                            b"enca"
                        } else {
                            b"iamf"
                        },
                        sample_entry,
                    ),
                ]
                .concat(),
            ),
        );
        let empty_table = |kind: &[u8; 4], tail: Vec<u8>| {
            boxed(
                kind,
                full_box(0, [0_u32.to_be_bytes().to_vec(), tail].concat()),
            )
        };
        let stbl = boxed(
            b"stbl",
            [
                stsd,
                empty_table(b"stts", Vec::new()),
                empty_table(b"stsc", Vec::new()),
                empty_table(b"stco", Vec::new()),
                boxed(
                    b"stsz",
                    full_box(0, [0_u32.to_be_bytes(), 0_u32.to_be_bytes()].concat()),
                ),
            ]
            .concat(),
        );
        let mdhd = boxed(
            b"mdhd",
            full_box(
                0,
                [
                    vec![0; 8],
                    48_000_u32.to_be_bytes().to_vec(),
                    1_u32.to_be_bytes().to_vec(),
                    vec![0; 4],
                ]
                .concat(),
            ),
        );
        let hdlr = boxed(
            b"hdlr",
            full_box(0, [vec![0; 4], b"soun".to_vec(), vec![0; 12]].concat()),
        );
        let tkhd = boxed(
            b"tkhd",
            full_box(
                0,
                [vec![0; 8], 1_u32.to_be_bytes().to_vec(), vec![0; 68]].concat(),
            ),
        );
        let trak = boxed(
            b"trak",
            [
                tkhd,
                boxed(b"mdia", [mdhd, hdlr, boxed(b"minf", stbl)].concat()),
            ]
            .concat(),
        );
        let mvhd = boxed(
            b"mvhd",
            full_box(
                0,
                [
                    vec![0; 8],
                    48_000_u32.to_be_bytes().to_vec(),
                    1_u32.to_be_bytes().to_vec(),
                    vec![0; 80],
                ]
                .concat(),
            ),
        );
        let trex = boxed(
            b"trex",
            full_box(
                0,
                [
                    1_u32.to_be_bytes(),
                    1_u32.to_be_bytes(),
                    0_u32.to_be_bytes(),
                    0_u32.to_be_bytes(),
                    0_u32.to_be_bytes(),
                ]
                .concat(),
            ),
        );
        let ftyp = boxed(
            b"ftyp",
            [b"dash".as_slice(), &[0, 0, 0, 0], b"iso6", b"iamf"].concat(),
        );
        let moov = boxed(b"moov", [mvhd, trak, boxed(b"mvex", trex)].concat());
        if initialization_only {
            return [ftyp, moov].concat();
        }
        let make_moof = |data_offset: i32| {
            let tfhd = boxed(
                b"tfhd",
                [
                    vec![0, 2, 0, 2],
                    1_u32.to_be_bytes().to_vec(),
                    1_u32.to_be_bytes().to_vec(),
                ]
                .concat(),
            );
            let tfdt = boxed(b"tfdt", full_box(0, 0_u32.to_be_bytes().to_vec()));
            let mut run_fields = [
                1_u32.to_be_bytes().to_vec(),
                data_offset.to_be_bytes().to_vec(),
                1_u32.to_be_bytes().to_vec(),
                u32::try_from(sample.len()).unwrap().to_be_bytes().to_vec(),
            ]
            .concat();
            if composition_offset {
                run_fields.extend(0_u32.to_be_bytes());
            }
            let trun_flags = if composition_offset { 0x0b01 } else { 0x0301 };
            let trun = boxed(
                b"trun",
                [
                    vec![
                        0,
                        ((trun_flags >> 16) & 0xff) as u8,
                        ((trun_flags >> 8) & 0xff) as u8,
                        (trun_flags & 0xff) as u8,
                    ],
                    run_fields,
                ]
                .concat(),
            );
            boxed(
                b"moof",
                [
                    boxed(b"mfhd", full_box(0, 1_u32.to_be_bytes().to_vec())),
                    boxed(
                        b"traf",
                        [
                            tfhd,
                            tfdt,
                            trun,
                            sample_groups.clone(),
                            cenc_auxiliary.clone().unwrap_or_else(|| {
                                protection.map(test_cenc_auxiliary).unwrap_or_default()
                            }),
                        ]
                        .concat(),
                    ),
                ]
                .concat(),
            )
        };
        let placeholder = make_moof(0);
        let data_offset = i32::try_from(placeholder.len() + 8)
            .unwrap()
            .checked_add(data_offset_adjustment)
            .unwrap();
        let moof = make_moof(data_offset);
        [ftyp, moov, moof, boxed(b"mdat", sample)].concat()
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
    fn audits_iso_bmff_iamf_configuration_samples_and_timing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("presentation.mp4");
        let bytes = minimal_iamf_mp4(b"iamf", iamf_obu(6, &[0]));
        File::create(&path).unwrap().write_all(&bytes).unwrap();
        let result = crate::container_qc::audit(&path).unwrap();
        assert!(result.passed, "{result:#?}");
        assert_eq!(result.properties["iamf_tracks"][0]["validated_samples"], 1);
        for rule_id in [
            "FORGE-ISOBMFF-IAMF-BRAND",
            "FORGE-ISOBMFF-IAMF-SAMPLE-ENTRY",
            "FORGE-ISOBMFF-IAMF-CONFIG",
            "FORGE-ISOBMFF-IAMF-SAMPLE-DATA",
            "FORGE-ISOBMFF-IAMF-SAMPLE-TIMING",
            "FORGE-ISOBMFF-IAMF-ROLL-GROUP",
            "FORGE-ISOBMFF-IAMF-TRIM",
            "FORGE-ISOBMFF-IAMF-SYNC-CTS",
            "FORGE-IAMF-TIMELINE",
        ] {
            assert!(result
                .layers
                .iter()
                .flat_map(|layer| &layer.checks)
                .any(|item| item.rule_id == rule_id && item.passed));
        }
    }

    #[test]
    fn audits_fragmented_iso_bmff_iamf_samples_and_timing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fragmented-presentation.mp4");
        let bytes = minimal_fragmented_iamf(iamf_obu(6, &[0]), 0, false, false);
        File::create(&path).unwrap().write_all(&bytes).unwrap();
        let result = crate::container_qc::audit(&path).unwrap();
        assert!(result.passed, "{result:#?}");
        assert_eq!(result.properties["fragmented"], true);
        assert_eq!(result.properties["iamf_tracks"][0]["fragments"], 1);
        assert_eq!(result.properties["iamf_tracks"][0]["validated_samples"], 1);
        for rule_id in [
            "FORGE-ISOBMFF-TRACK-EXTENDS",
            "FORGE-ISOBMFF-IAMF-SAMPLE-DATA",
            "FORGE-ISOBMFF-IAMF-SAMPLE-TIMING",
            "FORGE-ISOBMFF-IAMF-SYNC-CTS",
            "FORGE-IAMF-TIMELINE",
        ] {
            assert!(result
                .layers
                .iter()
                .flat_map(|layer| &layer.checks)
                .any(|item| item.rule_id == rule_id && item.passed));
        }
    }

    #[test]
    fn audits_unfragmented_cenc_encrypted_iamf_without_parsing_ciphertext() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("encrypted-presentation.mp4");
        let protection = TestCenc {
            scheme: *b"cenc",
            pattern: 0,
            iv_size: 8,
            subsample: false,
        };
        let bytes = minimal_iamf_mp4_protected(b"iamf", vec![0xff; 32], Some(protection));
        File::create(&path).unwrap().write_all(&bytes).unwrap();
        let result = crate::container_qc::audit(&path).unwrap();
        assert!(result.passed, "{result:#?}");
        assert_eq!(result.properties["tracks"][0]["codecs"][0], "iamf");
        assert_eq!(
            result.properties["tracks"][0]["iamf_sample_entries"][0]["sample_entry_type"],
            "enca"
        );
        assert_eq!(result.properties["iamf_tracks"][0]["encrypted"], true);
        assert_eq!(
            result.properties["iamf_tracks"][0]["ciphertext_obu_validation"],
            "requires_keys"
        );
        for rule_id in [
            "FORGE-ISOBMFF-IAMF-CENC-SIGNALING",
            "FORGE-ISOBMFF-IAMF-CENC",
            "FORGE-ISOBMFF-IAMF-SAMPLE-DATA",
            "FORGE-ISOBMFF-IAMF-SYNC-CTS",
        ] {
            assert!(result
                .layers
                .iter()
                .flat_map(|layer| &layer.checks)
                .any(|item| item.rule_id == rule_id && item.passed));
        }
    }

    #[test]
    fn audits_fragmented_cbcs_encrypted_iamf_with_constant_iv() {
        let directory = tempfile::tempdir().unwrap();
        let protection = TestCenc {
            scheme: *b"cbcs",
            pattern: 0,
            iv_size: 0,
            subsample: false,
        };
        for (name, initialization_only) in [("init.mp4", true), ("media.mp4", false)] {
            let path = directory.path().join(name);
            let bytes = minimal_fragmented_iamf_protected(
                vec![0xee; 32],
                0,
                false,
                initialization_only,
                Some(protection),
            );
            File::create(&path).unwrap().write_all(&bytes).unwrap();
            let result = crate::container_qc::audit(&path).unwrap();
            assert!(result.passed, "{result:#?}");
            assert_eq!(result.properties["iamf_tracks"][0]["encrypted"], true);
            assert!(result
                .layers
                .iter()
                .flat_map(|layer| &layer.checks)
                .any(|item| item.rule_id == "FORGE-ISOBMFF-IAMF-CENC" && item.passed));
        }
    }

    #[test]
    fn rejects_pattern_and_subsample_encryption_for_iamf() {
        let directory = tempfile::tempdir().unwrap();
        let pattern = directory.path().join("pattern.mp4");
        let bytes = minimal_iamf_mp4_protected(
            b"iamf",
            vec![0xaa; 32],
            Some(TestCenc {
                scheme: *b"cbcs",
                pattern: 0x19,
                iv_size: 16,
                subsample: false,
            }),
        );
        File::create(&pattern).unwrap().write_all(&bytes).unwrap();
        let result = crate::container_qc::audit(&pattern).unwrap();
        assert!(!result.passed);
        assert!(result
            .layers
            .iter()
            .flat_map(|layer| &layer.checks)
            .any(|item| { item.rule_id == "FORGE-ISOBMFF-IAMF-CENC-SIGNALING" && !item.passed }));

        let subsample = directory.path().join("subsample.mp4");
        let bytes = minimal_iamf_mp4_protected(
            b"iamf",
            vec![0xbb; 32],
            Some(TestCenc {
                scheme: *b"cenc",
                pattern: 0,
                iv_size: 8,
                subsample: true,
            }),
        );
        File::create(&subsample).unwrap().write_all(&bytes).unwrap();
        let result = crate::container_qc::audit(&subsample).unwrap();
        assert!(!result.passed);
        assert!(result
            .layers
            .iter()
            .flat_map(|layer| &layer.checks)
            .any(|item| item.rule_id == "FORGE-ISOBMFF-IAMF-CENC" && !item.passed));

        let unsupported = directory.path().join("unsupported-scheme.mp4");
        let bytes = minimal_iamf_mp4_protected(
            b"iamf",
            vec![0xcc; 32],
            Some(TestCenc {
                scheme: *b"cbc1",
                pattern: 0,
                iv_size: 16,
                subsample: false,
            }),
        );
        File::create(&unsupported)
            .unwrap()
            .write_all(&bytes)
            .unwrap();
        let result = crate::container_qc::audit(&unsupported).unwrap();
        assert!(!result.passed);
        assert!(result
            .layers
            .iter()
            .flat_map(|layer| &layer.checks)
            .any(|item| { item.rule_id == "FORGE-ISOBMFF-IAMF-CENC-SIGNALING" && !item.passed }));

        let missing_original_format = directory.path().join("missing-frma.mp4");
        let mut bytes = minimal_iamf_mp4_protected(
            b"iamf",
            vec![0xdd; 32],
            Some(TestCenc {
                scheme: *b"cenc",
                pattern: 0,
                iv_size: 8,
                subsample: false,
            }),
        );
        let frma = bytes
            .windows(4)
            .position(|window| window == b"frma")
            .unwrap();
        bytes[frma..frma + 4].copy_from_slice(b"xxxx");
        File::create(&missing_original_format)
            .unwrap()
            .write_all(&bytes)
            .unwrap();
        let result = crate::container_qc::audit(&missing_original_format).unwrap();
        assert!(!result.passed);
        assert!(result
            .layers
            .iter()
            .flat_map(|layer| &layer.checks)
            .any(|item| item.rule_id == "FORGE-ISOBMFF-IAMF-TRACK" && !item.passed));
    }

    #[test]
    fn validates_cenc_auxiliary_counts_and_iv_geometry() {
        let senc = CencAuxiliary {
            senc: vec![CencSampleEncryption {
                flags: 0,
                sample_count: 2,
                iv_size_override: None,
                entry_bytes: 16,
            }],
            ..CencAuxiliary::default()
        };
        assert!(validate_cenc_auxiliary(&senc, &[8, 8], "cenc").is_ok());
        assert!(validate_cenc_auxiliary(&senc, &[8], "cenc").is_err());

        let external = CencAuxiliary {
            saiz: vec![CencSampleAuxiliarySizes {
                auxiliary_type: Some("cenc".to_string()),
                default_size: 8,
                sample_count: 2,
                sizes: Vec::new(),
            }],
            saio: vec![CencSampleAuxiliaryOffsets {
                auxiliary_type: Some("cenc".to_string()),
                offsets: vec![128],
            }],
            ..CencAuxiliary::default()
        };
        assert!(validate_cenc_auxiliary(&external, &[8, 8], "cenc").is_ok());
        assert!(validate_cenc_auxiliary(&external, &[16, 16], "cenc").is_err());
        assert!(validate_cenc_auxiliary(&CencAuxiliary::default(), &[0], "cbcs").is_ok());
    }

    #[test]
    fn parses_and_resolves_cenc_seig_key_rotation() {
        let first = parse_cenc_sample_group_entry(&test_seig_entry(0x44, 8)).unwrap();
        let second = parse_cenc_sample_group_entry(&test_seig_entry(0x55, 0)).unwrap();
        assert_eq!(first.kid, "44".repeat(16));
        assert_eq!(first.per_sample_iv_size, 8);
        assert_eq!(second.constant_iv_size, Some(16));

        let track = Track {
            cenc_group_entries: vec![first.clone(), second.clone()],
            cenc_sample_runs: vec![(1, 1), (1, 2)],
            cenc_sbgp_seen: true,
            ..Track::default()
        };
        assert_eq!(
            resolve_cenc_group_assignments(&track, 2).unwrap(),
            vec![Some(first), Some(second)]
        );
        assert!(resolve_cenc_group_assignments(&track, 1).is_err());
        let underfilled = Track {
            cenc_group_entries: track.cenc_group_entries.clone(),
            cenc_sample_runs: vec![(1, 1)],
            cenc_sbgp_seen: true,
            ..Track::default()
        };
        assert!(resolve_cenc_group_assignments(&underfilled, 2).is_err());

        let local = parse_cenc_sample_group_entry(&test_seig_entry(0x66, 8)).unwrap();
        let fragment = TrackFragment {
            samples: vec![SampleLocation {
                offset: 0,
                size: 1,
                description_index: 1,
            }],
            cenc_group_entries: vec![local],
            cenc_sample_runs: vec![(1, 0x1_0001)],
            cenc_sbgp_seen: true,
            ..TrackFragment::default()
        };
        assert_eq!(
            resolve_fragment_cenc_group_assignments(&track, &fragment).unwrap()[0]
                .as_ref()
                .unwrap()
                .kid,
            "66".repeat(16)
        );
        let undefined_fragment = TrackFragment {
            samples: fragment.samples.clone(),
            cenc_sample_runs: vec![(1, 0x1_0002)],
            cenc_sbgp_seen: true,
            ..TrackFragment::default()
        };
        assert!(resolve_fragment_cenc_group_assignments(&track, &undefined_fragment).is_err());

        let default_group = Track {
            cenc_group_entries: vec![
                parse_cenc_sample_group_entry(&test_seig_entry(0x77, 8)).unwrap()
            ],
            cenc_default_description_index: Some(1),
            ..Track::default()
        };
        assert_eq!(
            resolve_cenc_group_assignments(&default_group, 2)
                .unwrap()
                .iter()
                .filter(|entry| entry.is_some())
                .count(),
            2
        );
        assert!(parse_cenc_sample_group_entry(&[0; 19]).is_err());
        let mut malformed_constant_iv = test_seig_entry(0x77, 0);
        malformed_constant_iv.pop();
        assert!(parse_cenc_sample_group_entry(&malformed_constant_iv).is_err());
    }

    #[test]
    fn accepts_iso_bmff_iamf_cenc_seig_key_rotation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("key-rotation.mp4");
        let protection = TestCenc {
            scheme: *b"cenc",
            pattern: 0,
            iv_size: 8,
            subsample: false,
        };
        let bytes = minimal_iamf_mp4_protected_with_groups(
            b"iamf",
            vec![vec![0xdd; 32], vec![0xee; 32]],
            Some(protection),
            test_cenc_key_rotation_groups(),
            Some(test_two_sample_cenc_auxiliary()),
        );
        File::create(&path).unwrap().write_all(&bytes).unwrap();
        let result = crate::container_qc::audit(&path).unwrap();
        assert!(result.passed, "{result:#?}");
        let evidence = &result.properties["iamf_tracks"][0]["cenc"]["protection"];
        assert_eq!(evidence["key_rotation"], true);
        assert_eq!(evidence["sample_group_overrides"], 2);
        assert_eq!(evidence["key_ids"].as_array().unwrap().len(), 2);

        let invalid_path = directory.path().join("pattern-key-rotation.mp4");
        let mut groups = test_cenc_key_rotation_groups();
        let entry = groups
            .windows(5)
            .position(|window| window == [0, 0, 1, 8, 0x44])
            .unwrap();
        groups[entry + 1] = 0x11;
        let bytes = minimal_iamf_mp4_protected_with_groups(
            b"iamf",
            vec![vec![0xdd; 32], vec![0xee; 32]],
            Some(protection),
            groups,
            Some(test_two_sample_cenc_auxiliary()),
        );
        File::create(&invalid_path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();
        let result = crate::container_qc::audit(&invalid_path).unwrap();
        assert!(!result.passed);
        assert!(result
            .layers
            .iter()
            .flat_map(|layer| &layer.checks)
            .any(|check| check.rule_id == "FORGE-ISOBMFF-IAMF-CENC" && !check.passed));
    }

    #[test]
    fn accepts_fragment_local_cenc_seig_override() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fragment-key-rotation.mp4");
        let protection = TestCenc {
            scheme: *b"cenc",
            pattern: 0,
            iv_size: 8,
            subsample: false,
        };
        let bytes = minimal_fragmented_iamf_protected_with_groups(
            vec![0xdd; 32],
            0,
            false,
            false,
            Some(protection),
            test_fragment_cenc_group(0x66),
            None,
        );
        File::create(&path).unwrap().write_all(&bytes).unwrap();
        let result = crate::container_qc::audit(&path).unwrap();
        assert!(result.passed, "{result:#?}");
        let protection = &result.properties["iamf_tracks"][0]["cenc"]["fragments"][0]["protection"];
        assert_eq!(protection["sample_group_overrides"], 1);
        assert_eq!(protection["key_ids"][0], "66".repeat(16));
    }

    #[test]
    fn accepts_version_two_default_cenc_seig_override() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("default-key-group.mp4");
        let protection = TestCenc {
            scheme: *b"cenc",
            pattern: 0,
            iv_size: 8,
            subsample: false,
        };
        let bytes = minimal_iamf_mp4_protected_with_groups(
            b"iamf",
            vec![vec![0xdd; 32], vec![0xee; 32]],
            Some(protection),
            test_default_cenc_group(0x77),
            Some(test_two_sample_cenc_auxiliary()),
        );
        File::create(&path).unwrap().write_all(&bytes).unwrap();
        let result = crate::container_qc::audit(&path).unwrap();
        assert!(result.passed, "{result:#?}");
        let protection = &result.properties["iamf_tracks"][0]["cenc"]["protection"];
        assert_eq!(protection["sample_group_overrides"], 2);
        assert_eq!(protection["key_ids"][0], "77".repeat(16));
    }

    #[test]
    fn parses_iso_cenc_track_encryption_field_layout() {
        let mut version_zero = vec![0, 0, 0, 0, 0, 0, 1, 8];
        version_zero.extend(vec![0x44; 16]);
        let mut parsed = CencProtection::default();
        let mut errors = Vec::new();
        parse_track_encryption(&version_zero, &mut parsed, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(parsed.tenc_version, Some(0));
        assert_eq!(parsed.default_is_protected, Some(1));
        assert_eq!(parsed.per_sample_iv_size, Some(8));
        assert_eq!(parsed.default_kid, Some("44".repeat(16)));

        let mut version_one = vec![1, 0, 0, 0, 0, 0, 1, 0];
        version_one.extend(vec![0x55; 16]);
        version_one.extend([16]);
        version_one.extend(vec![0x66; 16]);
        let mut parsed = CencProtection::default();
        let mut errors = Vec::new();
        parse_track_encryption(&version_one, &mut parsed, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(parsed.constant_iv_size, Some(16));
        assert_eq!(parsed.crypt_byte_block, Some(0));
        assert_eq!(parsed.skip_byte_block, Some(0));
    }

    #[test]
    fn accepts_iamf_initialization_segment_without_media_samples() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("init.mp4");
        let bytes = minimal_fragmented_iamf(iamf_obu(6, &[0]), 0, false, true);
        File::create(&path).unwrap().write_all(&bytes).unwrap();
        let result = crate::container_qc::audit(&path).unwrap();
        assert!(result.passed, "{result:#?}");
        assert_eq!(
            result.properties["iamf_tracks"][0]["initialization_segment"],
            true
        );
        assert_eq!(result.properties["iamf_tracks"][0]["validated_samples"], 0);
    }

    #[test]
    fn rejects_fragmented_iamf_out_of_mdat_and_composition_offsets() {
        let directory = tempfile::tempdir().unwrap();
        let outside = directory.path().join("outside-mdat.mp4");
        let bytes = minimal_fragmented_iamf(iamf_obu(6, &[0]), 1, false, false);
        File::create(&outside).unwrap().write_all(&bytes).unwrap();
        let result = crate::container_qc::audit(&outside).unwrap();
        assert!(!result.passed);
        assert!(result
            .layers
            .iter()
            .flat_map(|layer| &layer.checks)
            .any(|item| item.rule_id == "FORGE-ISOBMFF-IAMF-SAMPLE-DATA" && !item.passed));

        let composition = directory.path().join("composition-offset.mp4");
        let bytes = minimal_fragmented_iamf(iamf_obu(6, &[0]), 0, true, false);
        File::create(&composition)
            .unwrap()
            .write_all(&bytes)
            .unwrap();
        let result = crate::container_qc::audit(&composition).unwrap();
        assert!(!result.passed);
        assert!(result
            .layers
            .iter()
            .flat_map(|layer| &layer.checks)
            .any(|item| item.rule_id == "FORGE-ISOBMFF-IAMF-SYNC-CTS" && !item.passed));
    }

    #[test]
    fn rejects_iso_bmff_iamf_missing_brand_and_sample_delimiter() {
        let directory = tempfile::tempdir().unwrap();
        let missing_brand = directory.path().join("missing-brand.mp4");
        let bytes = minimal_iamf_mp4(b"isom", iamf_obu(6, &[0]));
        File::create(&missing_brand)
            .unwrap()
            .write_all(&bytes)
            .unwrap();
        let result = crate::container_qc::audit(&missing_brand).unwrap();
        assert!(!result.passed);
        assert!(result
            .layers
            .iter()
            .flat_map(|layer| &layer.checks)
            .any(|item| item.rule_id == "FORGE-ISOBMFF-IAMF-BRAND" && !item.passed));

        let delimiter = directory.path().join("delimiter.mp4");
        let sample = [iamf_obu(4, &[]), iamf_obu(6, &[0])].concat();
        let bytes = minimal_iamf_mp4(b"iamf", sample);
        File::create(&delimiter).unwrap().write_all(&bytes).unwrap();
        let result = crate::container_qc::audit(&delimiter).unwrap();
        assert!(!result.passed);
        assert!(result
            .layers
            .iter()
            .flat_map(|layer| &layer.checks)
            .any(|item| item.rule_id == "FORGE-ISOBMFF-IAMF-SAMPLE-DATA" && !item.passed));
    }

    #[test]
    fn rejects_invalid_iamf_configuration_boxes_and_stsc_mappings() {
        assert!(validate_iamf_config_obus(&iamf_obu(4, &[])).is_err());
        assert!(validate_iamf_config_obus(&[31 << 3, 2, 0]).is_err());

        let config = iamf_obu(31, b"iamf\x00\x00");
        let invalid_version = [vec![2, config.len() as u8], config.clone()].concat();
        let children = vec![("iacb".to_string(), invalid_version.as_slice())];
        let mut checks = Vec::new();
        let entry = vec![0_u8; 36];
        parse_iamf_sample_entry(&entry, "iamf", &children, None, &mut checks);
        assert!(checks
            .iter()
            .any(|item| item.rule_id == "FORGE-ISOBMFF-IAMF-CONFIG" && !item.passed));

        let valid_configuration = [vec![1, config.len() as u8], config].concat();
        let duplicate_children = vec![
            ("iacb".to_string(), valid_configuration.as_slice()),
            ("iacb".to_string(), valid_configuration.as_slice()),
        ];
        let mut checks = Vec::new();
        parse_iamf_sample_entry(&entry, "iamf", &duplicate_children, None, &mut checks);
        assert!(checks
            .iter()
            .any(|item| item.rule_id == "FORGE-ISOBMFF-IAMF-CONFIG" && !item.passed));

        let track = Track {
            sample_sizes: vec![1],
            chunk_offsets: vec![0],
            sample_to_chunk: vec![(1, 1, 1), (2, 1, 1)],
            ..Track::default()
        };
        assert!(sample_locations(&track).is_err());

        let default_roll = Track {
            roll_distances: vec![-4],
            roll_default_description_index: Some(1),
            ..Track::default()
        };
        assert_eq!(
            resolve_roll_assignments(&default_roll, 2).unwrap(),
            vec![Some(-4), Some(-4)]
        );
        let explicit_roll = Track {
            roll_distances: vec![-4],
            roll_default_description_index: Some(1),
            roll_sample_runs: vec![(1, 1), (1, 0)],
            ..Track::default()
        };
        assert_eq!(
            resolve_roll_assignments(&explicit_roll, 2).unwrap(),
            vec![Some(-4), Some(-4)]
        );
        let undefined_roll = Track {
            roll_distances: vec![-4],
            roll_sample_runs: vec![(1, 2)],
            ..Track::default()
        };
        assert!(resolve_roll_assignments(&undefined_roll, 1).is_err());
    }

    #[test]
    fn reconciles_multi_fragment_decode_timeline_and_local_roll_groups() {
        let track = Track {
            id: Some(1),
            roll_distances: vec![-4],
            ..Track::default()
        };
        let fragment_sample = |offset, description_index| SampleLocation {
            offset,
            size: 2,
            description_index,
        };
        let first = Fragment {
            start: 100,
            tracks: vec![TrackFragment {
                track_id: Some(1),
                decode_time: Some(0),
                declared_sample_count: 1,
                samples_resolved: true,
                samples: vec![fragment_sample(200, 1)],
                sample_durations: vec![1],
                sample_flags: vec![0],
                roll_sample_runs: vec![(1, 1)],
                ..TrackFragment::default()
            }],
            ..Fragment::default()
        };
        let second = Fragment {
            start: 300,
            tracks: vec![TrackFragment {
                track_id: Some(1),
                decode_time: Some(1),
                declared_sample_count: 1,
                samples_resolved: true,
                samples: vec![fragment_sample(400, 2)],
                sample_durations: vec![1],
                sample_flags: vec![0],
                roll_distances: vec![-2],
                roll_sample_runs: vec![(1, 0x1_0001)],
                ..TrackFragment::default()
            }],
            ..Fragment::default()
        };
        assert_eq!(
            resolve_fragment_roll_assignments(&track, &first.tracks[0]).unwrap(),
            vec![Some(-4)]
        );
        assert_eq!(
            resolve_fragment_roll_assignments(&track, &second.tracks[0]).unwrap(),
            vec![Some(-2)]
        );

        let mixed_groups = iamf_sample_set(&track, &[first, second], true).unwrap();
        assert_eq!(mixed_groups.roll_assignments, vec![Some(-4), Some(-2)]);

        let fragments = vec![
            Fragment {
                start: 100,
                tracks: vec![TrackFragment {
                    track_id: Some(1),
                    decode_time: Some(0),
                    declared_sample_count: 1,
                    samples_resolved: true,
                    samples: vec![fragment_sample(200, 1)],
                    sample_durations: vec![1],
                    sample_flags: vec![0],
                    ..TrackFragment::default()
                }],
                ..Fragment::default()
            },
            Fragment {
                start: 300,
                tracks: vec![TrackFragment {
                    track_id: Some(1),
                    decode_time: Some(1),
                    declared_sample_count: 1,
                    samples_resolved: true,
                    samples: vec![fragment_sample(400, 2)],
                    sample_durations: vec![1],
                    sample_flags: vec![0],
                    ..TrackFragment::default()
                }],
                ..Fragment::default()
            },
        ];
        let samples = iamf_sample_set(&track, &fragments, true).unwrap();
        assert!(samples.decode_timeline_contiguous);
        assert_eq!(samples.fragment_decode_times, vec![0, 1]);
        assert_eq!(
            samples
                .locations
                .iter()
                .map(|sample| sample.description_index)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );

        let mut gap = fragments;
        gap[1].tracks[0].decode_time = Some(2);
        assert!(
            !iamf_sample_set(&track, &gap, true)
                .unwrap()
                .decode_timeline_contiguous
        );
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
