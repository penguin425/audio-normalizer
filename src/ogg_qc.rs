//! RFC 3533 Ogg wrapper and Ogg Opus/Vorbis bitstream quality control.

use crate::container_qc::{check, finish_audit, ContainerAudit};
use ogg::reading::PacketReader;
use serde::Serialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

const MAX_OGG_PAGES: usize = 1_000_000;
const MAX_OGG_PACKET_BYTES: usize = 16 * 1024 * 1024;
const NO_GRANULE: u64 = u64::MAX;

#[derive(Debug, Clone, Serialize)]
struct RawOggInspection {
    pages: usize,
    chains: usize,
    serials: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    Opus,
    Vorbis,
}

impl Codec {
    fn format(self) -> &'static str {
        match self {
            Self::Opus => "ogg-opus",
            Self::Vorbis => "ogg-vorbis",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Opus => "Opus",
            Self::Vorbis => "Vorbis",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct ChainInspection {
    index: usize,
    serial: u32,
    codec: &'static str,
    channels: u8,
    sample_rate_hz: u32,
    audio_packet_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    mapping_family: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    original_sample_rate_hz: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_gain_q7_8: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    encoded_samples: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    initial_granule_offset_samples: Option<u64>,
    final_granule_position: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_trim_samples: Option<u64>,
    decoded_frames: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pre_skip_samples: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    r128_track_gain_q7_8: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    r128_album_gain_q7_8: Option<i16>,
}

#[derive(Debug, Clone)]
struct ActiveChain {
    index: usize,
    serial: u32,
    codec: Codec,
    channels: u8,
    sample_rate: u32,
    pre_skip: Option<u16>,
    mapping_family: Option<u8>,
    original_sample_rate: Option<u32>,
    output_gain_q7_8: Option<i16>,
    r128_track_gain_q7_8: Option<i16>,
    r128_album_gain_q7_8: Option<i16>,
    packet_index: usize,
    audio_packets: u64,
    encoded_samples: u64,
    page_samples: u64,
    first_audio_granule: Option<u64>,
    initial_granule_offset: u64,
    previous_granule: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
struct DecodedAudio {
    frames: u64,
    sample_rate_hz: u32,
    channels: usize,
}

struct OpusHead {
    channels: u8,
    pre_skip: u16,
    original_sample_rate: u32,
    output_gain_q7_8: i16,
    mapping_family: u8,
}

pub(crate) fn audit(path: &Path) -> Result<ContainerAudit, String> {
    let mut wrapper = Vec::new();
    let mut bitstream = Vec::new();
    let mut xcheck = Vec::new();
    let raw = match scan_pages(path) {
        Ok(value) => {
            wrapper.push(check(
                "FORGE-OGG-CRC",
                true,
                "every Ogg page checksum is valid",
                None,
            ));
            wrapper.push(check(
                "FORGE-OGG-PAGES",
                true,
                "RFC 3533 pages have valid bounds, flags, CRCs, sequences, and continuation state",
                Some(json!(&value)),
            ));
            wrapper.push(check(
                "FORGE-OGG-SEQUENTIAL-CHAINS",
                true,
                format!(
                    "{} unmultiplexed sequential logical stream(s)",
                    value.chains
                ),
                Some(json!(value.chains)),
            ));
            Some(value)
        }
        Err(error) => {
            wrapper.push(check(
                "FORGE-OGG-CRC",
                false,
                "page CRC validation could not complete because the Ogg wrapper is invalid",
                None,
            ));
            wrapper.push(check(
                "FORGE-OGG-SEQUENTIAL-CHAINS",
                false,
                "sequential-chain validation could not complete because the Ogg wrapper is invalid",
                None,
            ));
            wrapper.push(check("FORGE-OGG-WRAPPER", false, error, None));
            None
        }
    };

    let (codec, chains) = match raw.as_ref().map(|_| inspect_packets(path)) {
        Some(Ok((codec, chains))) => {
            bitstream.push(check(
                match codec {
                    Codec::Opus => "FORGE-OPUS-HEADERS",
                    Codec::Vorbis => "FORGE-VORBIS-HEADERS",
                },
                true,
                match codec {
                    Codec::Opus => {
                        "every chain has valid OpusHead and OpusTags headers".to_string()
                    }
                    Codec::Vorbis => {
                        "every chain has valid Vorbis identification, comment, and setup headers"
                            .to_string()
                    }
                },
                None,
            ));
            bitstream.push(check(
                match codec {
                    Codec::Opus => "FORGE-OPUS-GRANULES",
                    Codec::Vorbis => "FORGE-VORBIS-GRANULES",
                },
                true,
                format!(
                    "{} audio granule positions and end trimming are valid",
                    codec.name()
                ),
                Some(json!(&chains)),
            ));
            if codec == Codec::Opus {
                xcheck.push(check(
                    "FORGE-OPUS-CHAIN-LAYOUT",
                    true,
                    format!(
                        "all chains use the same {}-channel layout",
                        chains[0].channels
                    ),
                    Some(json!(chains[0].channels)),
                ));
            }
            (Some(codec), chains)
        }
        Some(Err(error)) => {
            bitstream.push(check("FORGE-OGG-CODEC", false, error, None));
            (None, Vec::new())
        }
        None => {
            bitstream.push(check(
                "FORGE-OGG-CODEC",
                false,
                "codec inspection skipped because wrapper validation failed",
                None,
            ));
            (None, Vec::new())
        }
    };

    let decoded = if codec == Some(Codec::Vorbis) && chains.len() == 1 {
        match verify_vorbis_decode(path) {
            Ok(decoded) => {
                let chain = &chains[0];
                let geometry_ok = decoded.sample_rate_hz == chain.sample_rate_hz
                    && decoded.channels == usize::from(chain.channels);
                xcheck.push(check(
                    "FORGE-VORBIS-DECODED-FORMAT",
                    geometry_ok,
                    "decoded channel count and sample rate match the identification header",
                    Some(json!({
                        "header": {"channels": chain.channels, "sample_rate_hz": chain.sample_rate_hz},
                        "decoded": &decoded
                    })),
                ));
                xcheck.push(check(
                    "FORGE-VORBIS-DECODED-AUDIO",
                    decoded.frames > 0,
                    "audio packets decode successfully; the final granule remains authoritative because Vorbis permits start offsets and end trimming",
                    Some(json!({
                        "raw_decoder_frames": decoded.frames,
                        "final_granule_position": chain.decoded_frames,
                        "difference": i128::from(decoded.frames) - i128::from(chain.decoded_frames)
                    })),
                ));
                Some(decoded)
            }
            Err(error) => {
                xcheck.push(check("FORGE-VORBIS-DECODE", false, error, None));
                None
            }
        }
    } else {
        None
    };

    let format = codec.map_or("ogg", Codec::format);
    let chain_count = chains.len();
    let channels = chains.first().map(|chain| chain.channels);
    let total_frames = chains
        .iter()
        .try_fold(0_u64, |total, chain| {
            total.checked_add(chain.decoded_frames)
        })
        .unwrap_or(u64::MAX);
    Ok(finish_audit(
        path,
        format,
        wrapper,
        bitstream,
        xcheck,
        json!({
            "wrapper": raw,
            "chain_count": chain_count,
            "channels": channels,
            "total_frames": total_frames,
            "chains": chains,
            "decoded": decoded
        }),
    ))
}

#[derive(Default)]
struct RawStreamState {
    next_sequence: u32,
    pending_packet: bool,
    pending_packet_bytes: usize,
    eos: bool,
}

fn scan_pages(path: &Path) -> Result<RawOggInspection, String> {
    let input = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut input = BufReader::new(input);
    let mut streams: HashMap<u32, RawStreamState> = HashMap::new();
    let mut serials = Vec::new();
    let mut active_serial = None;
    let mut pages = 0_usize;
    loop {
        let mut header = [0_u8; 27];
        match input.read(&mut header[..1]) {
            Ok(0) => break,
            Ok(1) => {}
            Ok(_) => unreachable!(),
            Err(error) => return Err(format!("read Ogg page header: {error}")),
        }
        input
            .read_exact(&mut header[1..])
            .map_err(|error| format!("truncated Ogg page header: {error}"))?;
        pages += 1;
        if pages > MAX_OGG_PAGES {
            return Err(format!(
                "Ogg page count exceeds safety limit {MAX_OGG_PAGES}"
            ));
        }
        if &header[..4] != b"OggS" {
            return Err(format!("page {pages} has no OggS capture pattern"));
        }
        if header[4] != 0 {
            return Err(format!(
                "page {pages} uses unsupported Ogg version {}",
                header[4]
            ));
        }
        let flags = header[5];
        if flags & !0x07 != 0 {
            return Err(format!("page {pages} has reserved header flags set"));
        }
        let continued = flags & 0x01 != 0;
        let bos = flags & 0x02 != 0;
        let eos = flags & 0x04 != 0;
        if bos && eos {
            return Err(format!("page {pages} sets both BOS and EOS"));
        }
        let granule = u64::from_le_bytes(header[6..14].try_into().unwrap());
        let serial = u32::from_le_bytes(header[14..18].try_into().unwrap());
        let sequence = u32::from_le_bytes(header[18..22].try_into().unwrap());
        let expected_crc = u32::from_le_bytes(header[22..26].try_into().unwrap());
        let segment_count = usize::from(header[26]);
        let mut lacing = vec![0_u8; segment_count];
        input
            .read_exact(&mut lacing)
            .map_err(|error| format!("page {pages} has a truncated segment table: {error}"))?;
        let body_size = lacing
            .iter()
            .map(|value| usize::from(*value))
            .sum::<usize>();
        let mut body = vec![0_u8; body_size];
        input
            .read_exact(&mut body)
            .map_err(|error| format!("page {pages} has a truncated body: {error}"))?;
        let mut crc_bytes = Vec::with_capacity(27 + segment_count + body_size);
        header[22..26].fill(0);
        crc_bytes.extend_from_slice(&header);
        crc_bytes.extend_from_slice(&lacing);
        crc_bytes.extend_from_slice(&body);
        let actual_crc = ogg_crc(&crc_bytes);
        if actual_crc != expected_crc {
            return Err(format!(
                "page {pages} CRC mismatch: stored {expected_crc:#010x}, calculated {actual_crc:#010x}"
            ));
        }

        if bos {
            if sequence != 0 || continued || streams.contains_key(&serial) {
                return Err(format!(
                    "page {pages} has an invalid or reused BOS stream {serial}"
                ));
            }
            if active_serial.is_some() {
                return Err(format!(
                    "page {pages} starts multiplexed stream {serial}; audio QC requires sequential chains"
                ));
            }
            active_serial = Some(serial);
            streams.insert(serial, RawStreamState::default());
            serials.push(serial);
        }
        if active_serial != Some(serial) {
            return Err(format!(
                "page {pages} belongs to unexpected stream {serial}; multiplexing is not supported"
            ));
        }
        let state = streams
            .get_mut(&serial)
            .ok_or_else(|| format!("page {pages} appears before a BOS page"))?;
        if state.eos || sequence != state.next_sequence || continued != state.pending_packet {
            return Err(format!(
                "page {pages} violates stream {serial} sequence or packet-continuation state"
            ));
        }
        state.next_sequence = state.next_sequence.wrapping_add(1);
        let completed_packets = lacing.iter().filter(|value| **value < 255).count();
        if !lacing.is_empty() && completed_packets == 0 && granule != NO_GRANULE {
            return Err(format!(
                "page {pages} completes no packet but does not use granule position -1"
            ));
        }
        for value in &lacing {
            state.pending_packet_bytes = state
                .pending_packet_bytes
                .checked_add(usize::from(*value))
                .ok_or_else(|| format!("page {pages} packet length overflow"))?;
            if state.pending_packet_bytes > MAX_OGG_PACKET_BYTES {
                return Err(format!(
                    "page {pages} creates a packet larger than the {MAX_OGG_PACKET_BYTES}-byte safety limit"
                ));
            }
            if *value < 255 {
                state.pending_packet_bytes = 0;
            }
        }
        state.pending_packet = lacing.last().is_some_and(|value| *value == 255);
        if eos {
            if state.pending_packet {
                return Err(format!("EOS page {pages} ends with an incomplete packet"));
            }
            state.eos = true;
            active_serial = None;
        }
    }
    if pages == 0 {
        return Err("empty Ogg file".into());
    }
    if active_serial.is_some()
        || streams
            .values()
            .any(|state| !state.eos || state.pending_packet)
    {
        return Err("Ogg stream ends without a complete EOS page".into());
    }
    Ok(RawOggInspection {
        pages,
        chains: streams.len(),
        serials,
    })
}

fn ogg_crc(bytes: &[u8]) -> u32 {
    let mut crc = 0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn inspect_packets(path: &Path) -> Result<(Codec, Vec<ChainInspection>), String> {
    let input = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut reader = PacketReader::new(BufReader::new(input));
    let mut codec = None;
    let mut channels = None;
    let mut serials = HashSet::new();
    let mut active = None;
    let mut chains = Vec::new();
    while let Some(packet) = reader
        .read_packet()
        .map_err(|error| format!("read Ogg packet: {error}"))?
    {
        if packet.data.len() > MAX_OGG_PACKET_BYTES
            && active
                .as_ref()
                .is_none_or(|chain: &ActiveChain| chain.packet_index < 3)
        {
            return Err("Ogg codec header packet exceeds the bounded-read safety limit".into());
        }
        if packet.first_in_stream() {
            if active.is_some() || !packet.first_in_page() || !packet.last_in_page() {
                return Err(
                    "logical stream does not begin with a standalone identification packet".into(),
                );
            }
            let detected = detect_codec(&packet.data)?;
            if codec.is_some_and(|value| value != detected) {
                return Err("chained Ogg streams change codec".into());
            }
            codec.get_or_insert(detected);
            let (chain_channels, sample_rate, pre_skip, opus_head) = match detected {
                Codec::Opus => {
                    let head = parse_opus_head(&packet.data)?;
                    (head.channels, 48_000, Some(head.pre_skip), Some(head))
                }
                Codec::Vorbis => {
                    let (channels, sample_rate) = validate_vorbis_identification(&packet.data)?;
                    (channels, sample_rate, None, None)
                }
            };
            if channels.is_some_and(|value| value != chain_channels) {
                return Err("chained Ogg streams change channel count".into());
            }
            channels.get_or_insert(chain_channels);
            let serial = packet.stream_serial();
            if !serials.insert(serial) || packet.absgp_page() != 0 {
                return Err("identification page reuses a serial or has non-zero granule".into());
            }
            active = Some(ActiveChain {
                index: chains.len() + 1,
                serial,
                codec: detected,
                channels: chain_channels,
                sample_rate,
                pre_skip,
                mapping_family: opus_head.as_ref().map(|head| head.mapping_family),
                original_sample_rate: opus_head.as_ref().map(|head| head.original_sample_rate),
                output_gain_q7_8: opus_head.as_ref().map(|head| head.output_gain_q7_8),
                r128_track_gain_q7_8: None,
                r128_album_gain_q7_8: None,
                packet_index: 1,
                audio_packets: 0,
                encoded_samples: 0,
                page_samples: 0,
                first_audio_granule: None,
                initial_granule_offset: 0,
                previous_granule: None,
            });
            continue;
        }
        let chain = active
            .as_mut()
            .ok_or_else(|| "Ogg packet appears outside a logical stream".to_string())?;
        if packet.stream_serial() != chain.serial {
            return Err("multiplexed Ogg logical streams are unsupported".into());
        }
        match (chain.codec, chain.packet_index) {
            (Codec::Opus, 1) => {
                let (track_gain, album_gain) =
                    parse_comment_packet(&packet.data, b"OpusTags", false)?;
                chain.r128_track_gain_q7_8 = track_gain;
                chain.r128_album_gain_q7_8 = album_gain;
                if packet.absgp_page() != 0 || !packet.last_in_page() {
                    return Err("OpusTags must finish a zero-granule header page".into());
                }
            }
            (Codec::Vorbis, 1) => {
                parse_comment_packet(&packet.data, b"\x03vorbis", true)?;
                if packet.absgp_page() != 0 {
                    return Err("Vorbis comment header page has non-zero granule".into());
                }
            }
            (Codec::Vorbis, 2) => {
                if !packet.data.starts_with(b"\x05vorbis") || packet.data.len() < 8 {
                    return Err("invalid or truncated Vorbis setup header".into());
                }
                if packet.absgp_page() != 0 || !packet.last_in_page() {
                    return Err("Vorbis setup header must finish a zero-granule page".into());
                }
            }
            (Codec::Opus, _) => inspect_opus_audio(chain, &packet)?,
            (Codec::Vorbis, _) => {
                if chain.packet_index == 3 && !packet.first_in_page() {
                    return Err("first Vorbis audio packet must begin a fresh page".into());
                }
                inspect_vorbis_audio(chain, &packet)?;
            }
        }
        chain.packet_index += 1;
        if packet.last_in_stream() {
            let finished = active.take().unwrap();
            chains.push(finish_chain(finished)?);
        }
    }
    if active.is_some() {
        return Err("logical stream has no packet-bearing EOS page".into());
    }
    let codec = codec.ok_or_else(|| "Ogg file contains no codec packet".to_string())?;
    if chains.is_empty() {
        return Err("Ogg file contains no complete logical stream".into());
    }
    chains.iter().try_fold(0_u64, |total, chain| {
        total
            .checked_add(chain.decoded_frames)
            .ok_or_else(|| "chained Ogg duration overflow".to_string())
    })?;
    Ok((codec, chains))
}

fn detect_codec(packet: &[u8]) -> Result<Codec, String> {
    if packet.starts_with(b"OpusHead") {
        Ok(Codec::Opus)
    } else if packet.starts_with(b"\x01vorbis") {
        Ok(Codec::Vorbis)
    } else {
        Err("unsupported Ogg codec; expected OpusHead or Vorbis identification".into())
    }
}

fn parse_opus_head(packet: &[u8]) -> Result<OpusHead, String> {
    if packet.len() < 19 || &packet[..8] != b"OpusHead" || packet[8] & 0xf0 != 0 {
        return Err("invalid OpusHead signature, size, or version".into());
    }
    let channels = packet[9];
    if channels == 0 {
        return Err("OpusHead channel count is zero".into());
    }
    let pre_skip = u16::from_le_bytes(packet[10..12].try_into().unwrap());
    let input_rate = u32::from_le_bytes(packet[12..16].try_into().unwrap());
    let output_gain_q7_8 = i16::from_le_bytes(packet[16..18].try_into().unwrap());
    let family = packet[18];
    if family == 0 {
        if (packet[8] == 1 && packet.len() != 19) || channels > 2 {
            return Err("Opus mapping family 0 requires a 19-byte mono/stereo header".into());
        }
    } else {
        let required = 21_usize
            .checked_add(usize::from(channels))
            .ok_or_else(|| "OpusHead mapping size overflow".to_string())?;
        if packet.len() < required || (packet[8] == 1 && packet.len() != required) {
            return Err("OpusHead mapping table length does not match channel count".into());
        }
        if family == 1 && channels > 8 {
            return Err("Opus mapping family 1 supports at most 8 channels".into());
        }
        let streams = packet[19];
        let coupled = packet[20];
        if streams == 0 || coupled > streams || u16::from(streams) + u16::from(coupled) > 255 {
            return Err("OpusHead stream/coupled counts are invalid".into());
        }
        let coded_channels = streams.saturating_add(coupled);
        if packet[21..]
            .iter()
            .any(|mapping| *mapping != 255 && *mapping >= coded_channels)
        {
            return Err("OpusHead channel mapping index is out of range".into());
        }
    }
    Ok(OpusHead {
        channels,
        pre_skip,
        original_sample_rate: input_rate,
        output_gain_q7_8,
        mapping_family: family,
    })
}

pub(crate) fn validate_opus_identification(packet: &[u8]) -> Result<(u8, u16), String> {
    let header = parse_opus_head(packet)?;
    Ok((header.channels, header.pre_skip))
}

pub(crate) fn validate_vorbis_identification(packet: &[u8]) -> Result<(u8, u32), String> {
    if packet.len() != 30 || &packet[..7] != b"\x01vorbis" {
        return Err("Vorbis identification header must be exactly 30 bytes".into());
    }
    let version = u32::from_le_bytes(packet[7..11].try_into().unwrap());
    let channels = packet[11];
    let sample_rate = u32::from_le_bytes(packet[12..16].try_into().unwrap());
    let block_sizes = packet[28];
    let small = block_sizes & 0x0f;
    let large = block_sizes >> 4;
    if version != 0
        || channels == 0
        || sample_rate == 0
        || !(6..=13).contains(&small)
        || !(6..=13).contains(&large)
        || small > large
        || packet[29] != 1
    {
        return Err("Vorbis identification fields or framing bit are invalid".into());
    }
    Ok((channels, sample_rate))
}

fn parse_comment_packet(
    packet: &[u8],
    signature: &[u8],
    framing: bool,
) -> Result<(Option<i16>, Option<i16>), String> {
    if !packet.starts_with(signature) {
        return Err("missing or misplaced codec comment header".into());
    }
    let mut cursor = signature.len();
    let vendor = take_le_vector(packet, &mut cursor)?;
    std::str::from_utf8(vendor).map_err(|_| "comment vendor is not UTF-8")?;
    let count = take_le_u32(packet, &mut cursor)?;
    let mut r128_track_gain = false;
    let mut r128_album_gain = false;
    let mut track_gain = None;
    let mut album_gain = None;
    for _ in 0..count {
        let comment = take_le_vector(packet, &mut cursor)?;
        let text = std::str::from_utf8(comment).map_err(|_| "codec comment is not UTF-8")?;
        let (name, _) = text
            .split_once('=')
            .ok_or_else(|| "codec comment has no field-name separator".to_string())?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| (0x20..=0x7d).contains(&byte) && byte != b'=')
        {
            return Err("codec comment field name is invalid".into());
        }
        if !framing {
            let value = text.split_once('=').unwrap().1;
            let seen = if name.eq_ignore_ascii_case("R128_TRACK_GAIN") {
                Some(&mut r128_track_gain)
            } else if name.eq_ignore_ascii_case("R128_ALBUM_GAIN") {
                Some(&mut r128_album_gain)
            } else {
                None
            };
            if let Some(seen) = seen {
                let parsed = value.parse::<i16>().map_err(|_| {
                    "Opus R128 gain tag is duplicated or outside signed Q7.8".to_string()
                })?;
                if *seen {
                    return Err("Opus R128 gain tag is duplicated or outside signed Q7.8".into());
                }
                *seen = true;
                if name.eq_ignore_ascii_case("R128_TRACK_GAIN") {
                    track_gain = Some(parsed);
                } else {
                    album_gain = Some(parsed);
                }
            }
        }
    }
    if framing {
        if packet.get(cursor) != Some(&1) || cursor + 1 != packet.len() {
            return Err("Vorbis comment framing bit or packet length is invalid".into());
        }
    } else if cursor != packet.len() {
        return Err("OpusTags contains trailing bytes".into());
    }
    Ok((track_gain, album_gain))
}

fn take_le_u32(packet: &[u8], cursor: &mut usize) -> Result<u32, String> {
    let end = cursor
        .checked_add(4)
        .ok_or_else(|| "comment length overflow".to_string())?;
    let bytes = packet
        .get(*cursor..end)
        .ok_or_else(|| "truncated codec comment header".to_string())?;
    *cursor = end;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn take_le_vector<'a>(packet: &'a [u8], cursor: &mut usize) -> Result<&'a [u8], String> {
    let length = usize::try_from(take_le_u32(packet, cursor)?)
        .map_err(|_| "comment vector length does not fit memory".to_string())?;
    let end = cursor
        .checked_add(length)
        .ok_or_else(|| "comment vector length overflow".to_string())?;
    let value = packet
        .get(*cursor..end)
        .ok_or_else(|| "truncated codec comment vector".to_string())?;
    *cursor = end;
    Ok(value)
}

fn inspect_opus_audio(chain: &mut ActiveChain, packet: &ogg::Packet) -> Result<(), String> {
    if packet.data.is_empty() {
        return Err("empty Opus audio packet".into());
    }
    let samples = opus_packet_samples(&packet.data)?;
    chain.audio_packets += 1;
    chain.encoded_samples = chain
        .encoded_samples
        .checked_add(samples)
        .ok_or_else(|| "Opus encoded sample count overflow".to_string())?;
    chain.page_samples = chain
        .page_samples
        .checked_add(samples)
        .ok_or_else(|| "Opus page sample count overflow".to_string())?;
    if packet.last_in_page() {
        let granule = packet.absgp_page();
        if granule == NO_GRANULE {
            return Err("completed Opus page has no granule position".into());
        }
        if let Some(previous) = chain.previous_granule {
            let expected = previous
                .checked_add(chain.page_samples)
                .ok_or_else(|| "Opus granule overflow".to_string())?;
            if packet.last_in_stream() {
                if granule < previous || granule > expected {
                    return Err("Opus EOS granule is outside the permitted end-trim range".into());
                }
            } else if granule != expected {
                return Err("Opus page granule does not match completed packet duration".into());
            }
        } else if !packet.last_in_stream() && granule < chain.page_samples {
            return Err("first Opus audio granule is smaller than its completed packets".into());
        }
        if chain.first_audio_granule.is_none() {
            chain.initial_granule_offset = granule.saturating_sub(chain.page_samples);
            chain.first_audio_granule = Some(granule);
        }
        chain.previous_granule = Some(granule);
        chain.page_samples = 0;
    }
    Ok(())
}

fn opus_packet_samples(packet: &[u8]) -> Result<u64, String> {
    let toc = packet[0];
    let config = toc >> 3;
    let frame_samples = if config >= 16 {
        120_u64 << (config & 0x03)
    } else if config >= 12 {
        480_u64 << (config & 0x01)
    } else {
        match config & 0x03 {
            0 => 480,
            1 => 960,
            2 => 1_920,
            _ => 2_880,
        }
    };
    let frames = match toc & 0x03 {
        0 => 1_u64,
        1 | 2 => 2,
        _ => u64::from(
            packet
                .get(1)
                .ok_or_else(|| "truncated Opus frame-count byte".to_string())?
                & 0x3f,
        ),
    };
    let total = frame_samples
        .checked_mul(frames)
        .ok_or_else(|| "Opus packet duration overflow".to_string())?;
    if frames == 0 || total > 5_760 {
        return Err("Opus packet duration is zero or exceeds 120 ms".into());
    }
    Ok(total)
}

fn inspect_vorbis_audio(chain: &mut ActiveChain, packet: &ogg::Packet) -> Result<(), String> {
    if packet.data.is_empty() || packet.data[0] & 1 != 0 {
        return Err("invalid Vorbis audio packet type".into());
    }
    chain.audio_packets += 1;
    if packet.last_in_page() {
        let granule = packet.absgp_page();
        if granule == NO_GRANULE {
            return Err("Vorbis page completing audio packets has no granule position".into());
        }
        if chain
            .previous_granule
            .is_some_and(|previous| granule < previous)
        {
            return Err("Vorbis audio granule positions decrease".into());
        }
        chain.first_audio_granule.get_or_insert(granule);
        chain.previous_granule = Some(granule);
    }
    Ok(())
}

fn finish_chain(chain: ActiveChain) -> Result<ChainInspection, String> {
    if chain.audio_packets == 0 {
        return Err(format!("chain {} contains no audio packets", chain.index));
    }
    let final_granule = chain
        .previous_granule
        .ok_or_else(|| format!("chain {} contains no final granule", chain.index))?;
    let (decoded_frames, end_trim) = match chain.codec {
        Codec::Vorbis => (final_granule, None),
        Codec::Opus => {
            let playable_origin = chain
                .initial_granule_offset
                .saturating_add(u64::from(chain.pre_skip.unwrap_or(0)));
            if final_granule < playable_origin {
                return Err("Opus final granule precedes pre-skip".into());
            }
            (
                final_granule - playable_origin,
                Some(
                    chain
                        .initial_granule_offset
                        .saturating_add(chain.encoded_samples)
                        .saturating_sub(final_granule),
                ),
            )
        }
    };
    Ok(ChainInspection {
        index: chain.index,
        serial: chain.serial,
        codec: chain.codec.name(),
        channels: chain.channels,
        sample_rate_hz: chain.sample_rate,
        audio_packet_count: chain.audio_packets,
        mapping_family: chain.mapping_family,
        original_sample_rate_hz: chain.original_sample_rate,
        output_gain_q7_8: chain.output_gain_q7_8,
        encoded_samples: (chain.codec == Codec::Opus).then_some(chain.encoded_samples),
        initial_granule_offset_samples: (chain.codec == Codec::Opus)
            .then_some(chain.initial_granule_offset),
        final_granule_position: final_granule,
        end_trim_samples: end_trim,
        decoded_frames,
        pre_skip_samples: chain.pre_skip,
        r128_track_gain_q7_8: chain.r128_track_gain_q7_8,
        r128_album_gain_q7_8: chain.r128_album_gain_q7_8,
    })
}

fn verify_vorbis_decode(path: &Path) -> Result<DecodedAudio, String> {
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::errors::Error;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;
    use symphonia::default::{get_codecs, get_probe};

    let input = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let source = MediaSourceStream::new(Box::new(input), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    hint.with_extension("ogg");
    let probed = get_probe()
        .format(
            &hint,
            source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|error| format!("probe Ogg Vorbis: {error}"))?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| "Ogg Vorbis contains no default audio track".to_string())?
        .clone();
    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| "Vorbis decoder found no sample rate".to_string())?;
    let channels = track
        .codec_params
        .channels
        .map(|value| value.count())
        .ok_or_else(|| "Vorbis decoder found no channel layout".to_string())?;
    let mut decoder = get_codecs()
        .make(&track.codec_params, &DecoderOptions { verify: true })
        .map_err(|error| format!("create Vorbis decoder: {error}"))?;
    let mut frames = 0_u64;
    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(Error::IoError(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(error) => return Err(format!("read Vorbis packet: {error}")),
        };
        if packet.track_id() != track.id {
            continue;
        }
        let decoded = decoder
            .decode(&packet)
            .map_err(|error| format!("decode Vorbis packet: {error}"))?;
        frames = frames
            .checked_add(decoded.frames() as u64)
            .ok_or_else(|| "decoded Vorbis sample count overflow".to_string())?;
    }
    Ok(DecodedAudio {
        frames,
        sample_rate_hz: sample_rate,
        channels,
    })
}

#[cfg(test)]
mod tests {
    use ogg::writing::{PacketWriteEndInfo, PacketWriter};
    use std::io::Write;

    fn opus_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fixture.opus");
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = PacketWriter::new(std::io::BufWriter::new(file));
        let mut head = b"OpusHead\x01\x01".to_vec();
        head.extend_from_slice(&0_u16.to_le_bytes());
        head.extend_from_slice(&48_000_u32.to_le_bytes());
        head.extend_from_slice(&0_i16.to_le_bytes());
        head.push(0);
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&4_u32.to_le_bytes());
        tags.extend_from_slice(b"test");
        tags.extend_from_slice(&0_u32.to_le_bytes());
        writer
            .write_packet(head, 42, PacketWriteEndInfo::EndPage, 0)
            .unwrap();
        writer
            .write_packet(tags, 42, PacketWriteEndInfo::EndPage, 0)
            .unwrap();
        writer
            .write_packet(vec![0], 42, PacketWriteEndInfo::EndStream, 480)
            .unwrap();
        writer.into_inner().flush().unwrap();
        (directory, path)
    }

    #[test]
    fn pure_opus_duration_parser_covers_packet_codes() {
        assert_eq!(super::opus_packet_samples(&[0]).unwrap(), 480);
        assert_eq!(super::opus_packet_samples(&[1]).unwrap(), 960);
        assert_eq!(super::opus_packet_samples(&[3, 3]).unwrap(), 1_440);
        assert!(super::opus_packet_samples(&[3, 0]).is_err());
        assert!(super::opus_packet_samples(&[0x1b, 63]).is_err());
    }

    #[test]
    fn default_build_audits_opus_and_detects_crc_corruption() {
        let (_directory, path) = opus_fixture();
        let audit = crate::container_qc::audit(&path).unwrap();
        assert!(audit.passed, "{audit:#?}");
        assert_eq!(audit.format, "ogg-opus");
        assert_eq!(audit.properties["chains"][0]["decoded_frames"], 480);

        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        let audit = crate::container_qc::audit(&path).unwrap();
        assert!(!audit.passed);
        assert!(!audit.layers[0].passed);
    }
}
