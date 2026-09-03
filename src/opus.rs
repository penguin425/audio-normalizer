//! Ogg Opus streaming I/O with RFC 7845 loudness metadata.

use crate::decoder::StreamInfo;
use crate::opus_tags::{build_opus_tags as opus_tags, parse_r128_comments};
use crate::wav::{default_channel_roles, named_channel_layout, ChannelRole, PcmKind};
use ::opus::{Application, Bitrate, Channels, Decoder, Encoder, MSDecoder, MSEncoder};
use ogg::{Packet, PacketReader, PacketWriteEndInfo, PacketWriter};
use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Async, FixedAsync, Indexing, PolynomialDegree, Resampler};
use serde::Serialize;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

pub use crate::opus_tags::{read_r128_tags, rewrite_r128_tags};

const OPUS_RATE: u32 = 48_000;
const FRAME_SIZE: usize = 960;
const MAX_PACKET_FRAMES: usize = 5760;
const RESAMPLE_CHUNK: usize = 1024;
const MAX_RESAMPLE_FLUSH_PASSES: usize = 8;
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
    pub encoded_samples: u64,
    pub initial_granule_offset_samples: u64,
    pub final_granule_position: u64,
    pub end_trim_samples: u64,
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
    input_offset: usize,
    resample_output: Vec<Vec<f32>>,
    encode_pending: Vec<f32>,
    encode_offset: usize,
    resampler: Option<AssertUnwindSafe<Async<f32>>>,
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
        if input_rate == 0 {
            return Err("Opus input sample rate must be positive".into());
        }
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

        // Construct and validate every input-dependent component before
        // opening the destination. In particular, a zero/unsupported input
        // rate must neither truncate an existing file nor reach the frame
        // count division below.
        let resampler = if input_rate == OPUS_RATE {
            None
        } else {
            Some(AssertUnwindSafe(
                Async::<f32>::new_poly(
                    OPUS_RATE as f64 / input_rate as f64,
                    1.0,
                    PolynomialDegree::Septic,
                    RESAMPLE_CHUNK,
                    channels as usize,
                    FixedAsync::Input,
                )
                .map_err(|error| format!("create Opus resampler: {error}"))?,
            ))
        };
        let resampler_delay = resampler
            .as_ref()
            .map_or(0, |resampler| resampler.output_delay());
        let expected_output_frames = u64::try_from(
            (input_frames as u128 * OPUS_RATE as u128 + input_rate as u128 / 2)
                / input_rate as u128,
        )
        .map_err(|_| "Opus output frame count exceeds the supported range".to_string())?;
        let head = opus_head(channels, pre_skip as u16, input_rate, &layout);
        let tags = opus_tags(track_lufs, album_lufs);

        let file =
            File::create(path).map_err(|error| format!("create {}: {error}", path.display()))?;
        let serial = NEXT_SERIAL.fetch_add(1, Ordering::Relaxed);
        let mut packets = PacketWriter::new(BufWriter::new(file));
        packets
            .write_packet(head, serial, PacketWriteEndInfo::EndPage, 0)
            .map_err(|error| format!("write OpusHead: {error}"))?;
        packets
            .write_packet(tags, serial, PacketWriteEndInfo::EndPage, 0)
            .map_err(|error| format!("write OpusTags: {error}"))?;
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
            input_offset: 0,
            resample_output: vec![Vec::new(); channels as usize],
            encode_pending: Vec::new(),
            encode_offset: 0,
            resampler,
        })
    }

    pub fn write_chunk(&mut self, planar: &[Vec<f32>]) -> Result<(), String> {
        validate_planar(planar, self.channels)?;
        if self.resampler.is_none() {
            return self.queue_for_encoding(planar);
        }
        self.compact_input_pending();
        for (pending, channel) in self.input_pending.iter_mut().zip(planar) {
            pending.extend_from_slice(channel);
        }
        let input_frames = self.resampler.as_ref().unwrap().input_frames_next();
        while self.input_pending[0].len() - self.input_offset >= input_frames {
            self.resample_into_output(None, "resample for Opus")?;
            self.input_offset += input_frames;
            self.queue_resample_output()?;
        }
        self.compact_input_pending();
        Ok(())
    }

    pub fn finish(mut self) -> Result<(), String> {
        if self.resampler.is_some() {
            self.compact_input_pending();
            let pending_frames = self.input_pending[0].len() - self.input_offset;
            if pending_frames > 0 {
                self.resample_into_output(Some(pending_frames), "finish Opus resampling")?;
                self.input_offset += pending_frames;
                self.queue_resample_output()?;
                for channel in &mut self.input_pending {
                    channel.clear();
                }
                self.input_offset = 0;
            }
            for _ in 0..MAX_RESAMPLE_FLUSH_PASSES {
                if self.queued_signal_frames == self.expected_output_frames {
                    break;
                }
                let produced = self.resample_into_output(Some(0), "flush Opus resampling")?;
                if produced == 0 {
                    break;
                }
                self.queue_resample_output()?;
            }
        }

        self.compact_encode_pending();
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
        while self.encode_pending.len() - self.encode_offset > frame_samples {
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

    fn resample_into_output(
        &mut self,
        partial_len: Option<usize>,
        operation: &str,
    ) -> Result<usize, String> {
        let input_offset = self.input_offset;
        let resampler = self
            .resampler
            .as_mut()
            .ok_or_else(|| "Opus resampler is not configured".to_string())?;
        let input_frames = partial_len.unwrap_or_else(|| resampler.input_frames_next());
        let output_frames = resampler.output_frames_next();
        let input = SequentialSliceOfVecs::new(
            self.input_pending.as_slice(),
            self.channels,
            input_offset + input_frames,
        )
        .map_err(|error| format!("prepare Opus resampler input: {error}"))?;
        for channel in &mut self.resample_output {
            channel.resize(output_frames, 0.0);
        }
        let indexing = if input_offset > 0 || partial_len.is_some() {
            let mut indexing = Indexing::new().input_offset(input_offset);
            if let Some(frames) = partial_len {
                indexing = indexing.partial_len(frames);
            }
            Some(indexing)
        } else {
            None
        };
        let produced = {
            let mut output_adapter = SequentialSliceOfVecs::new_mut(
                self.resample_output.as_mut_slice(),
                self.channels,
                output_frames,
            )
            .map_err(|error| format!("prepare Opus resampler output: {error}"))?;
            resampler
                .process_into_buffer(&input, &mut output_adapter, indexing.as_ref())
                .map_err(|error| format!("{operation}: {error}"))?
                .1
        };
        for channel in &mut self.resample_output {
            channel.truncate(produced);
        }
        Ok(produced)
    }

    fn queue_resample_output(&mut self) -> Result<(), String> {
        let output = std::mem::take(&mut self.resample_output);
        let result = self.queue_for_encoding(&output);
        self.resample_output = output;
        result
    }

    fn compact_input_pending(&mut self) {
        if self.input_offset == 0 {
            return;
        }
        for channel in &mut self.input_pending {
            let remaining = channel.len() - self.input_offset;
            channel.copy_within(self.input_offset.., 0);
            channel.truncate(remaining);
        }
        self.input_offset = 0;
    }

    fn queue_for_encoding(&mut self, planar: &[Vec<f32>]) -> Result<(), String> {
        validate_planar(planar, self.channels)?;
        self.compact_encode_pending();
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
        while self.encode_pending.len() - self.encode_offset >= frame_samples * 2 {
            self.encode_one(PacketWriteEndInfo::NormalPacket, None)?;
        }
        self.compact_encode_pending();
        Ok(())
    }

    fn compact_encode_pending(&mut self) {
        if self.encode_offset == 0 {
            return;
        }
        let remaining = self.encode_pending.len() - self.encode_offset;
        self.encode_pending.copy_within(self.encode_offset.., 0);
        self.encode_pending.truncate(remaining);
        self.encode_offset = 0;
    }

    fn encode_one(&mut self, end: PacketWriteEndInfo, granule: Option<u64>) -> Result<(), String> {
        let samples = FRAME_SIZE * self.channels;
        let start = self.encode_offset;
        let frame_end = start + samples;
        let packet = self
            .encoder
            .encode_vec_float(
                &self.encode_pending[start..frame_end],
                self.max_packet_bytes,
            )
            .map_err(|error| format!("encode Opus packet: {error}"))?;
        self.encode_offset = frame_end;
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
            channel_roles: internal_channel_roles(parsed.mapping_family, parsed.channels),
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
    let mut interleaved = vec![0.0_f32; MAX_PACKET_FRAMES * channels];
    let mut planar = (0..channels)
        .map(|_| Vec::with_capacity(MAX_PACKET_FRAMES))
        .collect::<Vec<_>>();
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
            let output_frames = end - start;
            for channel in &mut planar {
                channel.clear();
                channel.reserve(output_frames);
            }
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
        if !head.first_in_stream()
            || !head.last_in_page()
            || head.absgp_page() != 0
            || !head.data.starts_with(b"OpusHead")
        {
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
            || !tags.last_in_page()
            || tags.absgp_page() != 0
            || !tags.data.starts_with(b"OpusTags")
        {
            return Err(format!("chain {index} has invalid OpusTags"));
        }
        let (track_gain, album_gain) =
            parse_r128_comments(&tags.data).map_err(|error| format!("chain {index}: {error}"))?;
        let mut audio_packet_count = 0_u64;
        let mut encoded_samples = 0_u64;
        let mut page_samples = 0_u64;
        let mut previous_page_granule = None;
        let mut initial_granule_offset_samples = 0_u64;
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
            let packet_samples = opus_packet_samples(&packet.data).map_err(|error| {
                format!("chain {index}: packet {}: {error}", audio_packet_count + 1)
            })?;
            audio_packet_count += 1;
            encoded_samples = encoded_samples
                .checked_add(packet_samples)
                .ok_or_else(|| format!("chain {index} encoded duration overflow"))?;
            page_samples = page_samples
                .checked_add(packet_samples)
                .ok_or_else(|| format!("chain {index} page duration overflow"))?;
            if packet.last_in_page() {
                let granule = packet.absgp_page();
                if previous_page_granule.is_none() && !packet.last_in_stream() {
                    initial_granule_offset_samples = granule.saturating_sub(page_samples);
                }
                validate_page_granule(
                    index,
                    previous_page_granule,
                    granule,
                    page_samples,
                    packet.last_in_stream(),
                )?;
                previous_page_granule = Some(granule);
                page_samples = 0;
            }
            if packet.last_in_stream() {
                break packet.absgp_page();
            }
        };
        let playable_origin =
            initial_granule_offset_samples.saturating_add(u64::from(parsed.pre_skip));
        if final_granule_position < playable_origin {
            return Err(format!("chain {index} final granule precedes its pre-skip"));
        }
        let end_trim_samples = initial_granule_offset_samples
            .saturating_add(encoded_samples)
            .saturating_sub(final_granule_position);
        chains.push(OpusChainInspection {
            index,
            serial,
            channels: parsed.channels,
            mapping_family: parsed.mapping_family,
            pre_skip_samples: parsed.pre_skip,
            original_sample_rate_hz: parsed.original_sample_rate,
            output_gain_q7_8: parsed.output_gain_q7_8,
            audio_packet_count,
            encoded_samples,
            initial_granule_offset_samples,
            final_granule_position,
            end_trim_samples,
            decoded_frames: final_granule_position - playable_origin,
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

fn opus_packet_samples(packet: &[u8]) -> Result<u64, String> {
    let length = i32::try_from(packet.len()).map_err(|_| "Opus packet is too large")?;
    // SAFETY: libopus only reads `length` bytes from the non-empty packet slice.
    let samples = unsafe {
        audiopus_sys::opus_packet_get_nb_samples(packet.as_ptr(), length, OPUS_RATE as i32)
    };
    if samples <= 0 {
        return Err(format!("invalid RFC 6716 packet (libopus error {samples})"));
    }
    Ok(samples as u64)
}

fn validate_page_granule(
    chain_index: usize,
    previous: Option<u64>,
    granule: u64,
    completed_samples: u64,
    eos: bool,
) -> Result<(), String> {
    let Some(previous) = previous else {
        if !eos && granule < completed_samples {
            return Err(format!(
                "chain {chain_index} first audio-page granule {granule} is smaller than its {completed_samples} completed sample(s)"
            ));
        }
        return Ok(());
    };
    let expected = previous
        .checked_add(completed_samples)
        .ok_or_else(|| format!("chain {chain_index} granule position overflow"))?;
    if eos {
        if granule < previous || granule > expected {
            return Err(format!(
                "chain {chain_index} EOS granule {granule} is outside the RFC 7845 end-trim range {previous}..={expected}"
            ));
        }
    } else if granule != expected {
        return Err(format!(
            "chain {chain_index} granule {granule} does not equal previous {previous} plus {completed_samples} completed sample(s)"
        ));
    }
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
    let exact_roles = family_one_channel_roles(channels).expect("validated Opus channel count");
    if channels >= 7 {
        let layout_name = if channels == 7 { "6.1" } else { "7.1" };
        if named_channel_layout(layout_name).as_deref() != Some(roles) && exact_roles != roles {
            return Err(format!(
                "{channels}-channel Opus output requires the {layout_name} channel layout"
            ));
        }
    }
    if channels < 7 && default_channel_roles(channels) != roles && exact_roles != roles {
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

/// Speaker roles that a stream created by [`OpusStreamWriter`] persists.
///
/// The writer uses mapping family 0 for mono/stereo and RFC 7845 mapping
/// family 1 for 3 through 8 channels. Keep accepting the historical generic
/// roles at the public writer boundary, but expose the exact post-decode roles
/// so normalization can verify that a requested output preserves semantics.
pub(crate) fn persisted_channel_roles(channels: u16) -> Option<Vec<ChannelRole>> {
    family_one_channel_roles(channels)
}

fn internal_channel_roles(mapping_family: u8, channels: u16) -> Vec<ChannelRole> {
    if mapping_family == 1 {
        family_one_channel_roles(channels).expect("validated mapping-family-1 channel count")
    } else {
        default_channel_roles(channels)
    }
}

/// RFC 7845 section 5.1.1.2 speaker locations in Forge/WAVE channel order.
/// Mono/stereo retain their unambiguous legacy representation so mapping
/// families 0 and 1 remain interchangeable for those channel counts.
/// Five-channel rear beds use the conventional +/-110 degree positions, which
/// preserve the BS.1770 +1.5 dB weighting while remaining distinct from side
/// speakers. Seven- and eight-channel layouts retain separate rear/side beds.
fn family_one_channel_roles(channels: u16) -> Option<Vec<ChannelRole>> {
    use ChannelRole::Lfe;
    let p = ChannelRole::positioned;
    Some(match channels {
        1 | 2 => default_channel_roles(channels),
        3 => vec![p(-30, 0), p(30, 0), p(0, 0)],
        4 => vec![p(-30, 0), p(30, 0), p(-110, 0), p(110, 0)],
        5 => vec![p(-30, 0), p(30, 0), p(0, 0), p(-110, 0), p(110, 0)],
        6 => vec![p(-30, 0), p(30, 0), p(0, 0), Lfe, p(-110, 0), p(110, 0)],
        7 => vec![
            p(-30, 0),
            p(30, 0),
            p(0, 0),
            Lfe,
            p(180, 0),
            p(-90, 0),
            p(90, 0),
        ],
        8 => vec![
            p(-30, 0),
            p(30, 0),
            p(0, 0),
            Lfe,
            p(-135, 0),
            p(135, 0),
            p(-90, 0),
            p(90, 0),
        ],
        _ => return None,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    #[test]
    fn stream_writer_preserves_unwind_safety_traits() {
        fn assert_traits<T: std::panic::UnwindSafe + std::panic::RefUnwindSafe>() {}

        assert_traits::<OpusStreamWriter>();
    }

    #[test]
    fn invalid_writer_inputs_do_not_panic_or_modify_the_destination() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("existing.opus");
        let sentinel = b"existing destination";
        let invalid_side_layout = crate::wav::reader::roles_from_wave_mask(0x060f, 6);
        let cases = [
            (0, 1, default_channel_roles(1), 64, "sample rate"),
            (OPUS_RATE, 0, Vec::new(), 64, "requires 1 or 2 channels"),
            (OPUS_RATE, 2, default_channel_roles(1), 64, "role count"),
            (
                OPUS_RATE,
                6,
                invalid_side_layout,
                384,
                "conventional channel layout",
            ),
            (OPUS_RATE, 1, default_channel_roles(1), 0, "positive"),
        ];

        for (input_rate, channels, roles, bitrate, diagnostic) in cases {
            std::fs::write(&path, sentinel).unwrap();
            let result = std::panic::catch_unwind(|| {
                OpusStreamWriter::create(
                    &path, input_rate, FRAME_SIZE, channels, &roles, bitrate, -18.0, None,
                )
                .err()
            });
            let error = result
                .expect("invalid Opus writer input must not panic")
                .expect("invalid Opus writer input must return an error");
            assert!(error.contains(diagnostic), "{error}");
            assert_eq!(std::fs::read(&path).unwrap(), sentinel);
        }
    }

    #[test]
    fn mapping_family_one_roles_retain_rear_and_side_positions() {
        assert_eq!(
            internal_channel_roles(1, 1),
            default_channel_roles(1),
            "mapping-family-1 mono is semantically identical to family 0"
        );
        assert_eq!(
            internal_channel_roles(1, 2),
            default_channel_roles(2),
            "mapping-family-1 stereo is semantically identical to family 0"
        );
        assert_eq!(persisted_channel_roles(2), Some(default_channel_roles(2)));
        assert!(opus_layout(6, &default_channel_roles(6)).is_ok());
        assert!(opus_layout(7, &named_channel_layout("6.1").unwrap()).is_ok());

        let rear_five_one = family_one_channel_roles(6).unwrap();
        assert_eq!(
            rear_five_one,
            crate::wav::reader::roles_from_wave_mask(0x003f, 6),
            "RFC 7845 5.1 and the conventional rear-bed WAVE layout agree"
        );
        assert_ne!(
            rear_five_one,
            crate::wav::reader::roles_from_wave_mask(0x060f, 6),
            "rear and side 5.1 beds must not collapse to the same roles"
        );
        assert_eq!(crate::dsp::lufs::channel_weight(rear_five_one[4]), 1.41);

        let six_one = family_one_channel_roles(7).unwrap();
        assert_eq!(six_one[4], ChannelRole::positioned(180, 0));
        assert_eq!(six_one[5], ChannelRole::positioned(-90, 0));
        assert_eq!(six_one[6], ChannelRole::positioned(90, 0));

        let seven_one = family_one_channel_roles(8).unwrap();
        assert_eq!(seven_one[4], ChannelRole::positioned(-135, 0));
        assert_eq!(seven_one[5], ChannelRole::positioned(135, 0));
        assert_eq!(seven_one[6], ChannelRole::positioned(-90, 0));
        assert_eq!(seven_one[7], ChannelRole::positioned(90, 0));
    }

    #[test]
    fn six_channel_writer_round_trips_exact_mapping_family_one_roles() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("exact-roles.opus");
        let roles = persisted_channel_roles(6).unwrap();
        let mut writer =
            OpusStreamWriter::create(&path, OPUS_RATE, FRAME_SIZE, 6, &roles, 384, -18.0, None)
                .unwrap();
        writer.write_chunk(&vec![vec![0.0; FRAME_SIZE]; 6]).unwrap();
        writer.finish().unwrap();

        let mut callback_roles = None;
        let info = decode_stream(&path, |info, _| {
            callback_roles.get_or_insert_with(|| info.channel_roles.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(info.channel_roles, roles);
        assert_eq!(callback_roles, Some(roles));
    }

    #[test]
    fn granules_follow_completed_packet_durations() {
        validate_page_granule(1, None, 1_200, 960, false).unwrap();
        validate_page_granule(1, Some(1_200), 3_120, 1_920, false).unwrap();
        validate_page_granule(1, Some(3_120), 3_700, 960, true).unwrap();

        assert!(validate_page_granule(1, Some(1_200), 3_000, 1_920, false).is_err());
        assert!(validate_page_granule(1, Some(3_120), 4_200, 960, true).is_err());
        assert!(validate_page_granule(1, None, 800, 960, false).is_err());
    }

    #[test]
    fn libopus_reports_rfc_6716_packet_duration() {
        // A one-byte code-0 TOC packet has one 20 ms frame at 48 kHz.
        assert_eq!(opus_packet_samples(&[0x98]).unwrap(), 960);
        assert!(opus_packet_samples(&[]).is_err());
    }

    #[test]
    fn opus_resampler_flushes_an_exact_number_of_input_chunks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("exact-chunks.opus");
        let input_rate = 44_100;
        let input_frames = RESAMPLE_CHUNK * 2;
        let expected_frames = ((input_frames as u128 * OPUS_RATE as u128 + input_rate as u128 / 2)
            / input_rate as u128) as u64;
        let mut writer = OpusStreamWriter::create(
            &path,
            input_rate,
            input_frames,
            1,
            &default_channel_roles(1),
            64,
            -18.0,
            None,
        )
        .unwrap();
        let samples = (0..input_frames)
            .map(|frame| (TAU * 997.0 * frame as f32 / input_rate as f32).sin() * 0.1)
            .collect::<Vec<_>>();
        writer.write_chunk(&[samples]).unwrap();
        writer.finish().unwrap();

        let inspection = inspect(&path).unwrap();
        assert_eq!(inspection.chain_count, 1);
        assert_eq!(inspection.total_frames, expected_frames);
    }

    #[test]
    fn stream_writer_reuses_pending_allocation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reuse.opus");
        let large_frames = FRAME_SIZE * 8 + 137;
        let small_frames = FRAME_SIZE + 137;
        let total_frames = large_frames + small_frames;
        let mut writer = OpusStreamWriter::create(
            &path,
            OPUS_RATE,
            total_frames,
            2,
            &default_channel_roles(2),
            128,
            -18.0,
            None,
        )
        .unwrap();

        let large = vec![vec![0.0; large_frames]; 2];
        writer.write_chunk(&large).unwrap();
        let pending_allocation = writer.encode_pending.as_ptr();
        let pending_capacity = writer.encode_pending.capacity();

        let small = vec![vec![0.0; small_frames]; 2];
        writer.write_chunk(&small).unwrap();
        assert_eq!(writer.encode_offset, 0);
        assert_eq!(writer.encode_pending.as_ptr(), pending_allocation);
        assert_eq!(writer.encode_pending.capacity(), pending_capacity);
        writer.finish().unwrap();

        assert_eq!(inspect(&path).unwrap().total_frames, total_frames as u64);
    }

    #[test]
    fn resampler_reuses_input_and_output_allocations() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("resample-reuse.opus");
        let input_rate = 44_100;
        let large_frames = RESAMPLE_CHUNK * 8 + 31;
        let small_frames = RESAMPLE_CHUNK + 31;
        let total_frames = large_frames + small_frames;
        let mut writer = OpusStreamWriter::create(
            &path,
            input_rate,
            total_frames,
            1,
            &default_channel_roles(1),
            64,
            -18.0,
            None,
        )
        .unwrap();

        writer.write_chunk(&[vec![0.0; large_frames]]).unwrap();
        let input_allocation = writer.input_pending[0].as_ptr();
        let input_capacity = writer.input_pending[0].capacity();
        let output_allocation = writer.resample_output[0].as_ptr();
        let output_capacity = writer.resample_output[0].capacity();

        writer.write_chunk(&[vec![0.0; small_frames]]).unwrap();
        assert_eq!(writer.input_offset, 0);
        assert_eq!(writer.input_pending[0].as_ptr(), input_allocation);
        assert_eq!(writer.input_pending[0].capacity(), input_capacity);
        assert_eq!(writer.resample_output[0].as_ptr(), output_allocation);
        assert_eq!(writer.resample_output[0].capacity(), output_capacity);
        writer.finish().unwrap();

        let expected_frames = ((total_frames as u128 * OPUS_RATE as u128 + input_rate as u128 / 2)
            / input_rate as u128) as u64;
        assert_eq!(inspect(&path).unwrap().total_frames, expected_frames);
    }

    #[test]
    fn decoder_reuses_packet_and_planar_allocations() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("decode-reuse.opus");
        let input_frames = FRAME_SIZE * 20;
        let mut writer = OpusStreamWriter::create(
            &path,
            OPUS_RATE,
            input_frames,
            2,
            &default_channel_roles(2),
            128,
            -18.0,
            None,
        )
        .unwrap();
        let samples = (0..input_frames)
            .map(|frame| (TAU * 997.0 * frame as f32 / OPUS_RATE as f32).sin() * 0.1)
            .collect::<Vec<_>>();
        writer.write_chunk(&[samples.clone(), samples]).unwrap();
        writer.finish().unwrap();

        let mut allocations: Option<Vec<*const f32>> = None;
        let mut callbacks = 0;
        let mut decoded_frames = 0;
        decode_stream(&path, |_, planar| {
            callbacks += 1;
            decoded_frames += planar[0].len();
            let current = planar.iter().map(Vec::as_ptr).collect::<Vec<_>>();
            if let Some(expected) = &allocations {
                assert_eq!(&current, expected);
            } else {
                allocations = Some(current);
            }
            Ok(())
        })
        .unwrap();

        assert!(callbacks > 1);
        assert_eq!(decoded_frames, input_frames);
    }
}
