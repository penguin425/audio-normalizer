//! Ogg Opus streaming I/O with RFC 7845 loudness metadata.

use crate::decoder::StreamInfo;
use crate::wav::{default_channel_roles, named_channel_layout, ChannelRole, PcmKind};
use ::opus::{Application, Bitrate, Channels, Decoder, Encoder, MSDecoder, MSEncoder};
use ogg::{Packet, PacketReader, PacketWriteEndInfo, PacketWriter};
use rubato::{FastFixedIn, PolynomialDegree, Resampler};
use serde::Serialize;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

const OPUS_RATE: u32 = 48_000;
const FRAME_SIZE: usize = 960;
const RESAMPLE_CHUNK: usize = 1024;
static NEXT_SERIAL: AtomicU32 = AtomicU32::new(0x464f_5247);

#[derive(Debug, Clone, Serialize)]
pub struct OpusInspection {
    pub chain_count: usize,
    pub channels: u16,
    pub total_frames: u64,
    pub chains: Vec<OpusChainInspection>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpusChainInspection {
    pub index: usize,
    pub serial: u32,
    pub channels: u16,
    pub mapping_family: u8,
    pub pre_skip_samples: u16,
    pub original_sample_rate_hz: u32,
    pub output_gain_q7_8: i16,
    pub audio_packet_count: u64,
    pub final_granule_position: u64,
    pub decoded_frames: u64,
    pub r128_track_gain_q7_8: Option<i16>,
    pub r128_album_gain_q7_8: Option<i16>,
}

pub struct OpusStreamWriter {
    packets: PacketWriter<'static, BufWriter<File>>,
    encoder: OpusEncoder,
    channels: usize,
    channel_order: Vec<usize>,
    max_packet_bytes: usize,
    serial: u32,
    pre_skip: usize,
    expected_output_frames: u64,
    queued_signal_frames: u64,
    resampler_delay: usize,
    encoded_frames: u64,
    input_pending: Vec<Vec<f32>>,
    encode_pending: Vec<f32>,
    resampler: Option<FastFixedIn<f32>>,
}

impl OpusStreamWriter {
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        path: &Path,
        input_rate: u32,
        input_frames: usize,
        channels: u16,
        channel_roles: &[ChannelRole],
        bitrate_kbps: i32,
        track_lufs: f64,
        album_lufs: Option<f64>,
    ) -> Result<Self, String> {
        let layout = opus_layout(channels, channel_roles)?;
        if bitrate_kbps <= 0 {
            return Err("Opus bitrate must be positive".into());
        }
        let mut encoder = OpusEncoder::new(&layout)?;
        encoder
            .set_bitrate(Bitrate::Bits(bitrate_kbps.saturating_mul(1000)))
            .map_err(|error| format!("set Opus bitrate: {error}"))?;
        let pre_skip = encoder
            .get_lookahead()
            .map_err(|error| format!("query Opus look-ahead: {error}"))?
            .max(0) as usize;
        let file =
            File::create(path).map_err(|error| format!("create {}: {error}", path.display()))?;
        let serial = NEXT_SERIAL.fetch_add(1, Ordering::Relaxed);
        let mut packets = PacketWriter::new(BufWriter::new(file));
        packets
            .write_packet(
                opus_head(channels, pre_skip as u16, input_rate, &layout),
                serial,
                PacketWriteEndInfo::EndPage,
                0,
            )
            .map_err(|error| format!("write OpusHead: {error}"))?;
        packets
            .write_packet(
                opus_tags(track_lufs, album_lufs),
                serial,
                PacketWriteEndInfo::EndPage,
                0,
            )
            .map_err(|error| format!("write OpusTags: {error}"))?;

        let resampler = if input_rate == OPUS_RATE {
            None
        } else {
            Some(
                FastFixedIn::<f32>::new(
                    OPUS_RATE as f64 / input_rate as f64,
                    1.0,
                    PolynomialDegree::Septic,
                    RESAMPLE_CHUNK,
                    channels as usize,
                )
                .map_err(|error| format!("create Opus resampler: {error}"))?,
            )
        };
        let resampler_delay = resampler.as_ref().map_or(0, Resampler::output_delay);
        let expected_output_frames = ((input_frames as u128 * OPUS_RATE as u128
            + input_rate as u128 / 2)
            / input_rate as u128) as u64;
        Ok(Self {
            packets,
            encoder,
            channels: channels as usize,
            channel_order: layout.to_opus_order,
            max_packet_bytes: 1275 * layout.streams as usize,
            serial,
            pre_skip,
            expected_output_frames,
            queued_signal_frames: 0,
            resampler_delay,
            encoded_frames: 0,
            input_pending: vec![Vec::new(); channels as usize],
            encode_pending: Vec::new(),
            resampler,
        })
    }

    pub fn write_chunk(&mut self, planar: &[Vec<f32>]) -> Result<(), String> {
        validate_planar(planar, self.channels)?;
        if self.resampler.is_none() {
            return self.queue_for_encoding(planar);
        }
        for (pending, channel) in self.input_pending.iter_mut().zip(planar) {
            pending.extend_from_slice(channel);
        }
        while self.input_pending[0].len() >= RESAMPLE_CHUNK {
            let block: Vec<Vec<f32>> = self
                .input_pending
                .iter_mut()
                .map(|channel| channel.drain(..RESAMPLE_CHUNK).collect())
                .collect();
            let output = self
                .resampler
                .as_mut()
                .unwrap()
                .process(&block, None)
                .map_err(|error| format!("resample for Opus: {error}"))?;
            self.queue_for_encoding(&output)?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<(), String> {
        if let Some(resampler) = self.resampler.as_mut() {
            if !self.input_pending[0].is_empty() {
                let output = resampler
                    .process_partial(Some(&self.input_pending), None)
                    .map_err(|error| format!("finish Opus resampling: {error}"))?;
                self.queue_for_encoding(&output)?;
            }
        }

        self.encode_pending.resize(
            self.encode_pending.len() + self.pre_skip * self.channels,
            0.0,
        );
        let frame_samples = FRAME_SIZE * self.channels;
        let remainder = self.encode_pending.len() % frame_samples;
        if remainder != 0 {
            self.encode_pending
                .resize(self.encode_pending.len() + frame_samples - remainder, 0.0);
        }
        if self.encode_pending.is_empty() {
            self.encode_pending.resize(frame_samples, 0.0);
        }
        while self.encode_pending.len() > frame_samples {
            self.encode_one(PacketWriteEndInfo::NormalPacket, None)?;
        }
        if self.queued_signal_frames != self.expected_output_frames {
            return Err(format!(
                "Opus resampler produced {} frames, expected {}",
                self.queued_signal_frames, self.expected_output_frames
            ));
        }
        let final_granule = self.pre_skip as u64 + self.expected_output_frames;
        self.encode_one(PacketWriteEndInfo::EndStream, Some(final_granule))?;
        self.packets
            .inner_mut()
            .flush()
            .map_err(|error| format!("flush Ogg Opus: {error}"))
    }

    fn queue_for_encoding(&mut self, planar: &[Vec<f32>]) -> Result<(), String> {
        validate_planar(planar, self.channels)?;
        let frames = planar.first().map_or(0, Vec::len);
        let start = self.resampler_delay.min(frames);
        self.resampler_delay -= start;
        let remaining = self
            .expected_output_frames
            .saturating_sub(self.queued_signal_frames) as usize;
        let end = frames.min(start.saturating_add(remaining));
        if end <= start {
            return Ok(());
        }
        self.queued_signal_frames += (end - start) as u64;
        self.encode_pending.reserve((end - start) * self.channels);
        let channel_order = &self.channel_order;
        self.encode_pending.extend((start..end).flat_map(|frame| {
            channel_order
                .iter()
                .map(move |&channel| planar[channel][frame])
        }));
        let frame_samples = FRAME_SIZE * self.channels;
        while self.encode_pending.len() >= frame_samples * 2 {
            self.encode_one(PacketWriteEndInfo::NormalPacket, None)?;
        }
        Ok(())
    }

    fn encode_one(&mut self, end: PacketWriteEndInfo, granule: Option<u64>) -> Result<(), String> {
        let samples = FRAME_SIZE * self.channels;
        let frame: Vec<f32> = self.encode_pending.drain(..samples).collect();
        let packet = self
            .encoder
            .encode_vec_float(&frame, self.max_packet_bytes)
            .map_err(|error| format!("encode Opus packet: {error}"))?;
        self.encoded_frames += FRAME_SIZE as u64;
        self.packets
            .write_packet(
                packet,
                self.serial,
                end,
                granule.unwrap_or(self.encoded_frames),
            )
            .map_err(|error| format!("write Ogg Opus packet: {error}"))
    }
}

pub fn decode_stream<F>(path: &Path, mut consume: F) -> Result<StreamInfo, String>
where
    F: FnMut(&StreamInfo, &mut [Vec<f32>]) -> Result<(), String>,
{
    let file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut packets = PacketReader::new(BufReader::new(file));
    let mut primary_info: Option<StreamInfo> = None;
    let mut chain_index = 0_usize;
    while let Some(head) = read_ogg_packet(&mut packets, path, "OpusHead")? {
        chain_index += 1;
        if !head.first_in_stream() || !head.data.starts_with(b"OpusHead") {
            return Err(format!(
                "{}: chain {chain_index} does not start with OpusHead",
                path.display()
            ));
        }
        let parsed = parse_opus_head(&head.data)
            .map_err(|error| format!("{}: chain {chain_index}: {error}", path.display()))?;
        let info = StreamInfo {
            sample_rate: OPUS_RATE,
            channels: parsed.channels,
            channel_roles: internal_channel_roles(parsed.channels),
            source_kind: PcmKind::F32,
        };
        if let Some(primary) = &primary_info {
            if primary.channels != info.channels || primary.channel_roles != info.channel_roles {
                return Err(format!(
                    "{}: chain {chain_index} changes the Opus channel layout",
                    path.display()
                ));
            }
        } else {
            primary_info = Some(info.clone());
        }
        decode_opus_chain(
            path,
            chain_index,
            head,
            &parsed,
            &info,
            &mut packets,
            &mut consume,
        )?;
    }
    primary_info.ok_or_else(|| format!("{}: missing OpusHead", path.display()))
}

fn read_ogg_packet(
    packets: &mut PacketReader<BufReader<File>>,
    path: &Path,
    label: &str,
) -> Result<Option<Packet>, String> {
    packets
        .read_packet()
        .map_err(|error| format!("{}: read {label}: {error}", path.display()))
}

fn decode_opus_chain<F>(
    path: &Path,
    chain_index: usize,
    head: Packet,
    parsed: &ParsedOpusHead,
    info: &StreamInfo,
    packets: &mut PacketReader<BufReader<File>>,
    consume: &mut F,
) -> Result<(), String>
where
    F: FnMut(&StreamInfo, &mut [Vec<f32>]) -> Result<(), String>,
{
    let serial = head.stream_serial();
    let tags = read_ogg_packet(packets, path, "OpusTags")?.ok_or_else(|| {
        format!(
            "{}: chain {chain_index} is missing OpusTags",
            path.display()
        )
    })?;
    if tags.stream_serial() != serial
        || tags.first_in_stream()
        || tags.last_in_stream()
        || !tags.data.starts_with(b"OpusTags")
    {
        return Err(format!(
            "{}: chain {chain_index} has invalid OpusTags",
            path.display()
        ));
    }
    parse_r128_comments(&tags.data)
        .map_err(|error| format!("{}: chain {chain_index}: {error}", path.display()))?;

    let channels = parsed.channels as usize;
    let mut decoder = OpusDecoder::new(parsed)?;
    let output_gain = 10.0_f32.powf(parsed.output_gain_q7_8 as f32 / (20.0 * 256.0));
    let from_opus_order = invert_permutation(&parsed.to_opus_order);
    let mut skip = parsed.pre_skip as usize;
    let mut raw_frames = 0_u64;
    let mut audio_packets = 0_u64;
    loop {
        let packet = read_ogg_packet(packets, path, "Ogg Opus packet")?.ok_or_else(|| {
            format!(
                "{}: chain {chain_index} ended without an Ogg EOS page",
                path.display()
            )
        })?;
        if packet.stream_serial() != serial {
            return Err(format!(
                "{}: multiplexed Ogg streams are unsupported; expected serial {serial}, found {}",
                path.display(),
                packet.stream_serial()
            ));
        }
        audio_packets += 1;
        let mut interleaved = vec![0.0_f32; 5760 * channels];
        let frames = decoder
            .decode_float(&packet.data, &mut interleaved, false)
            .map_err(|error| {
                format!(
                    "{}: chain {chain_index} decode Opus packet {audio_packets}: {error}",
                    path.display()
                )
            })?;
        let packet_start = raw_frames;
        raw_frames = raw_frames
            .checked_add(frames as u64)
            .ok_or_else(|| "decoded Opus duration overflow".to_string())?;
        let start = skip.min(frames);
        skip -= start;
        let mut end = frames;
        if packet.last_in_stream() {
            let target_raw_frames = packet.absgp_page();
            if target_raw_frames < parsed.pre_skip as u64 || target_raw_frames > raw_frames {
                return Err(format!(
                    "{}: chain {chain_index} has invalid final granule position {target_raw_frames}",
                    path.display()
                ));
            }
            end = end.min(target_raw_frames.saturating_sub(packet_start) as usize);
        }
        if end > start {
            let mut planar = vec![Vec::with_capacity(end - start); channels];
            for frame in start..end {
                for (internal, &opus_channel) in from_opus_order.iter().enumerate() {
                    planar[internal]
                        .push(interleaved[frame * channels + opus_channel] * output_gain);
                }
            }
            consume(info, &mut planar)?;
        }
        if packet.last_in_stream() {
            if skip != 0 {
                return Err(format!(
                    "{}: chain {chain_index} ends before its pre-skip",
                    path.display()
                ));
            }
            break;
        }
    }
    Ok(())
}

pub fn read_r128_tags(path: &Path) -> Result<(Option<i16>, Option<i16>), String> {
    let file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut packets = PacketReader::new(BufReader::new(file));
    let _head = packets
        .read_packet()
        .map_err(|error| format!("{}: read OpusHead: {error}", path.display()))?;
    let tags = packets
        .read_packet()
        .map_err(|error| format!("{}: read OpusTags: {error}", path.display()))?
        .ok_or_else(|| format!("{}: missing OpusTags", path.display()))?;
    parse_r128_comments(&tags.data)
}

/// Validate the Ogg wrapper and every sequential Opus logical stream without
/// decoding sample payloads. `PacketReader` verifies every page CRC.
pub fn inspect(path: &Path) -> Result<OpusInspection, String> {
    let file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut packets = PacketReader::new(BufReader::new(file));
    let mut chains = Vec::new();
    let mut serials = std::collections::BTreeSet::new();
    let mut expected_channels = None;
    while let Some(head) = read_ogg_packet(&mut packets, path, "OpusHead")? {
        let index = chains.len() + 1;
        if !head.first_in_stream() || !head.data.starts_with(b"OpusHead") {
            return Err(format!("chain {index} does not start with OpusHead"));
        }
        let serial = head.stream_serial();
        if !serials.insert(serial) {
            return Err(format!("chain {index} reuses Ogg serial number {serial}"));
        }
        let parsed =
            parse_opus_head(&head.data).map_err(|error| format!("chain {index}: {error}"))?;
        if expected_channels.is_some_and(|channels| channels != parsed.channels) {
            return Err(format!("chain {index} changes the Opus channel count"));
        }
        expected_channels.get_or_insert(parsed.channels);
        let tags = read_ogg_packet(&mut packets, path, "OpusTags")?
            .ok_or_else(|| format!("chain {index} is missing OpusTags"))?;
        if tags.stream_serial() != serial
            || tags.first_in_stream()
            || tags.last_in_stream()
            || !tags.data.starts_with(b"OpusTags")
        {
            return Err(format!("chain {index} has invalid OpusTags"));
        }
        let (track_gain, album_gain) =
            parse_r128_comments(&tags.data).map_err(|error| format!("chain {index}: {error}"))?;
        let mut audio_packet_count = 0_u64;
        let mut previous_page_granule = 0_u64;
        let final_granule_position = loop {
            let packet = read_ogg_packet(&mut packets, path, "Ogg Opus packet")?
                .ok_or_else(|| format!("chain {index} ended without an Ogg EOS page"))?;
            if packet.stream_serial() != serial {
                return Err(format!(
                    "multiplexed Ogg streams are unsupported; expected serial {serial}, found {}",
                    packet.stream_serial()
                ));
            }
            if packet.data.is_empty() {
                return Err(format!("chain {index} contains an empty Opus packet"));
            }
            audio_packet_count += 1;
            if packet.last_in_page() {
                let granule = packet.absgp_page();
                if granule < previous_page_granule {
                    return Err(format!("chain {index} has a decreasing granule position"));
                }
                previous_page_granule = granule;
            }
            if packet.last_in_stream() {
                break packet.absgp_page();
            }
        };
        if final_granule_position < u64::from(parsed.pre_skip) {
            return Err(format!("chain {index} final granule precedes its pre-skip"));
        }
        chains.push(OpusChainInspection {
            index,
            serial,
            channels: parsed.channels,
            mapping_family: parsed.mapping_family,
            pre_skip_samples: parsed.pre_skip,
            original_sample_rate_hz: parsed.original_sample_rate,
            output_gain_q7_8: parsed.output_gain_q7_8,
            audio_packet_count,
            final_granule_position,
            decoded_frames: final_granule_position - u64::from(parsed.pre_skip),
            r128_track_gain_q7_8: track_gain,
            r128_album_gain_q7_8: album_gain,
        });
    }
    let channels = expected_channels.ok_or_else(|| "missing OpusHead".to_string())?;
    let total_frames = chains
        .iter()
        .try_fold(0_u64, |total, chain| {
            total.checked_add(chain.decoded_frames)
        })
        .ok_or_else(|| "chained Opus duration overflow".to_string())?;
    Ok(OpusInspection {
        chain_count: chains.len(),
        channels,
        total_frames,
        chains,
    })
}

pub fn rewrite_r128_tags(
    path: &Path,
    track_lufs: f64,
    album_lufs: Option<f64>,
) -> Result<(), String> {
    let file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut reader = PacketReader::new(BufReader::new(file));
    let mut packets = Vec::new();
    while let Some(packet) = reader
        .read_packet()
        .map_err(|error| format!("{}: read Ogg packet: {error}", path.display()))?
    {
        packets.push(packet);
    }
    let mut rewritten = 0_usize;
    for index in 0..packets.len().saturating_sub(1) {
        if packets[index].first_in_stream() && packets[index].data.starts_with(b"OpusHead") {
            let serial = packets[index].stream_serial();
            let tags = &mut packets[index + 1];
            if tags.stream_serial() != serial || !tags.data.starts_with(b"OpusTags") {
                return Err(format!(
                    "{}: missing OpusTags after OpusHead",
                    path.display()
                ));
            }
            tags.data = replace_r128_comments(&tags.data, track_lufs, album_lufs)?;
            rewritten += 1;
        }
    }
    if rewritten == 0 {
        return Err(format!("{}: missing OpusTags", path.display()));
    }

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::Builder::new()
        .prefix(".forge-opus-tags-")
        .suffix(".opus")
        .tempfile_in(parent)
        .map_err(|error| format!("create temporary OpusTags file: {error}"))?;
    {
        let mut writer = PacketWriter::new(temporary.as_file_mut());
        for packet in packets {
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
    }
    temporary.persist(path).map_err(|error| {
        format!(
            "replace {} after OpusTags update: {}",
            path.display(),
            error.error
        )
    })?;
    Ok(())
}

fn opus_channels(channels: u16) -> Result<Channels, String> {
    match channels {
        1 => Ok(Channels::Mono),
        2 => Ok(Channels::Stereo),
        _ => Err(format!(
            "single-stream Opus requires 1 or 2 channels, got {channels}"
        )),
    }
}

fn validate_planar(planar: &[Vec<f32>], channels: usize) -> Result<(), String> {
    if planar.len() != channels {
        return Err("Opus stream channel count changed".into());
    }
    let frames = planar.first().map_or(0, Vec::len);
    if planar.iter().any(|channel| channel.len() != frames) {
        return Err("Opus stream channel length mismatch".into());
    }
    Ok(())
}

fn opus_head(channels: u16, pre_skip: u16, original_rate: u32, layout: &OpusLayout) -> Vec<u8> {
    let mut head = b"OpusHead".to_vec();
    head.push(1);
    head.push(channels as u8);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&original_rate.to_le_bytes());
    head.extend_from_slice(&0_i16.to_le_bytes());
    head.push(layout.mapping_family);
    if layout.mapping_family != 0 {
        head.push(layout.streams);
        head.push(layout.coupled_streams);
        head.extend_from_slice(&layout.mapping);
    }
    head
}

fn parse_opus_head(data: &[u8]) -> Result<ParsedOpusHead, String> {
    if data.len() < 19 || &data[..8] != b"OpusHead" || data[8] == 0 {
        return Err("invalid OpusHead".into());
    }
    let channels = data[9] as u16;
    let pre_skip = u16::from_le_bytes([data[10], data[11]]);
    let original_sample_rate = u32::from_le_bytes(data[12..16].try_into().unwrap());
    let output_gain_q7_8 = i16::from_le_bytes(data[16..18].try_into().unwrap());
    let mapping_family = data[18];
    if mapping_family == 0 {
        opus_channels(channels)?;
        return Ok(ParsedOpusHead {
            channels,
            pre_skip,
            original_sample_rate,
            output_gain_q7_8,
            mapping_family,
            streams: 1,
            coupled_streams: (channels == 2) as u8,
            mapping: (0..channels as u8).collect(),
            to_opus_order: (0..channels as usize).collect(),
        });
    }
    if mapping_family != 1 || !(1..=8).contains(&channels) || data.len() < 21 + channels as usize {
        return Err("unsupported Opus channel mapping family".into());
    }
    let standard = family_one_layout(channels)?;
    let streams = data[19];
    let coupled_streams = data[20];
    let mapping = data[21..21 + channels as usize].to_vec();
    let decoded_channels = streams as u16 + coupled_streams as u16;
    if streams == 0
        || coupled_streams > streams
        || decoded_channels > 255
        || mapping
            .iter()
            .any(|value| *value != 255 && u16::from(*value) >= decoded_channels)
        || mapping != standard.mapping
    {
        return Err("invalid Opus mapping family 1 table".into());
    }
    Ok(ParsedOpusHead {
        channels,
        pre_skip,
        original_sample_rate,
        output_gain_q7_8,
        mapping_family,
        streams,
        coupled_streams,
        mapping,
        to_opus_order: standard.to_opus_order,
    })
}

#[derive(Debug, Clone)]
struct OpusLayout {
    mapping_family: u8,
    streams: u8,
    coupled_streams: u8,
    mapping: Vec<u8>,
    /// Opus/Vorbis output channel index -> Forge/WAVE input channel index.
    to_opus_order: Vec<usize>,
}

struct ParsedOpusHead {
    channels: u16,
    pre_skip: u16,
    original_sample_rate: u32,
    output_gain_q7_8: i16,
    mapping_family: u8,
    streams: u8,
    coupled_streams: u8,
    mapping: Vec<u8>,
    to_opus_order: Vec<usize>,
}

fn opus_layout(channels: u16, roles: &[ChannelRole]) -> Result<OpusLayout, String> {
    if roles.len() != channels as usize {
        return Err("Opus channel-role count does not match channel count".into());
    }
    if channels <= 2 {
        opus_channels(channels)?;
        return Ok(OpusLayout {
            mapping_family: 0,
            streams: 1,
            coupled_streams: (channels == 2) as u8,
            mapping: (0..channels as u8).collect(),
            to_opus_order: (0..channels as usize).collect(),
        });
    }
    if channels >= 7 {
        let layout_name = if channels == 7 { "6.1" } else { "7.1" };
        if named_channel_layout(layout_name).as_deref() != Some(roles) {
            return Err(format!(
                "{channels}-channel Opus output requires the {layout_name} channel layout"
            ));
        }
    }
    if channels < 7 && default_channel_roles(channels) != roles {
        return Err(format!(
            "{channels}-channel Opus output requires the conventional channel layout"
        ));
    }
    family_one_layout(channels)
}

fn family_one_layout(channels: u16) -> Result<OpusLayout, String> {
    let (streams, coupled_streams, mapping, to_opus_order): (u8, u8, &[u8], &[usize]) =
        match channels {
            1 => (1, 0, &[0], &[0]),
            2 => (1, 1, &[0, 1], &[0, 1]),
            3 => (2, 1, &[0, 2, 1], &[0, 2, 1]),
            4 => (2, 2, &[0, 1, 2, 3], &[0, 1, 2, 3]),
            5 => (3, 2, &[0, 4, 1, 2, 3], &[0, 2, 1, 3, 4]),
            6 => (4, 2, &[0, 4, 1, 2, 3, 5], &[0, 2, 1, 4, 5, 3]),
            7 => (4, 3, &[0, 4, 1, 2, 3, 5, 6], &[0, 2, 1, 5, 6, 4, 3]),
            8 => (5, 3, &[0, 6, 1, 2, 3, 4, 5, 7], &[0, 2, 1, 6, 7, 4, 5, 3]),
            _ => {
                return Err(format!(
                    "Ogg Opus supports 1 through 7.1, got {channels} channels"
                ))
            }
        };
    Ok(OpusLayout {
        mapping_family: 1,
        streams,
        coupled_streams,
        mapping: mapping.to_vec(),
        to_opus_order: to_opus_order.to_vec(),
    })
}

fn invert_permutation(permutation: &[usize]) -> Vec<usize> {
    let mut inverse = vec![0; permutation.len()];
    for (output, &input) in permutation.iter().enumerate() {
        inverse[input] = output;
    }
    inverse
}

fn internal_channel_roles(channels: u16) -> Vec<ChannelRole> {
    if channels >= 7 {
        named_channel_layout(if channels == 7 { "6.1" } else { "7.1" })
            .expect("built-in surround layout")
    } else {
        default_channel_roles(channels)
    }
}

enum OpusEncoder {
    Single(Encoder),
    Multi(MSEncoder),
}

impl OpusEncoder {
    fn new(layout: &OpusLayout) -> Result<Self, String> {
        if layout.mapping_family == 0 {
            Encoder::new(
                OPUS_RATE,
                opus_channels(layout.mapping.len() as u16)?,
                Application::Audio,
            )
            .map(Self::Single)
            .map_err(|error| format!("create Opus encoder: {error}"))
        } else {
            MSEncoder::new(
                OPUS_RATE,
                layout.streams,
                layout.coupled_streams,
                &layout.mapping,
                Application::Audio,
            )
            .map(Self::Multi)
            .map_err(|error| format!("create multistream Opus encoder: {error}"))
        }
    }

    fn set_bitrate(&mut self, bitrate: Bitrate) -> Result<(), ::opus::Error> {
        match self {
            Self::Single(encoder) => encoder.set_bitrate(bitrate),
            Self::Multi(encoder) => encoder.set_bitrate(bitrate),
        }
    }

    fn get_lookahead(&mut self) -> Result<i32, ::opus::Error> {
        match self {
            Self::Single(encoder) => encoder.get_lookahead(),
            Self::Multi(encoder) => encoder.get_lookahead(),
        }
    }

    fn encode_vec_float(
        &mut self,
        samples: &[f32],
        max_size: usize,
    ) -> Result<Vec<u8>, ::opus::Error> {
        match self {
            Self::Single(encoder) => encoder.encode_vec_float(samples, max_size),
            Self::Multi(encoder) => encoder.encode_vec_float(samples, max_size),
        }
    }
}

enum OpusDecoder {
    Single(Decoder),
    Multi(MSDecoder),
}

impl OpusDecoder {
    fn new(head: &ParsedOpusHead) -> Result<Self, String> {
        if head.mapping_family == 0 {
            Decoder::new(OPUS_RATE, opus_channels(head.channels)?)
                .map(Self::Single)
                .map_err(|error| format!("create Opus decoder: {error}"))
        } else {
            MSDecoder::new(OPUS_RATE, head.streams, head.coupled_streams, &head.mapping)
                .map(Self::Multi)
                .map_err(|error| format!("create multistream Opus decoder: {error}"))
        }
    }

    fn decode_float(
        &mut self,
        packet: &[u8],
        output: &mut [f32],
        fec: bool,
    ) -> Result<usize, ::opus::Error> {
        match self {
            Self::Single(decoder) => decoder.decode_float(packet, output, fec),
            Self::Multi(decoder) => decoder.decode_float(packet, output, fec),
        }
    }
}

fn opus_tags(track_lufs: f64, album_lufs: Option<f64>) -> Vec<u8> {
    let vendor = b"Forge audio normalizer";
    let mut comments = vec![format!("R128_TRACK_GAIN={}", r128_gain(track_lufs))];
    if let Some(album_lufs) = album_lufs {
        comments.push(format!("R128_ALBUM_GAIN={}", r128_gain(album_lufs)));
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

fn r128_gain(lufs: f64) -> i16 {
    if !lufs.is_finite() {
        return 0;
    }
    ((-23.0 - lufs) * 256.0)
        .round()
        .clamp(i16::MIN as f64, i16::MAX as f64) as i16
}

fn parse_r128_comments(data: &[u8]) -> Result<(Option<i16>, Option<i16>), String> {
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
    comments.push(format!("R128_TRACK_GAIN={}", r128_gain(track_lufs)).into_bytes());
    if let Some(album_lufs) = album_lufs {
        comments.push(format!("R128_ALBUM_GAIN={}", r128_gain(album_lufs)).into_bytes());
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
