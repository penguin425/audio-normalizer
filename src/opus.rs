//! Ogg Opus streaming I/O with RFC 7845 loudness metadata.

use crate::decoder::StreamInfo;
use crate::wav::{default_channel_roles, PcmKind};
use ::opus::{Application, Bitrate, Channels, Decoder, Encoder};
use ogg::{PacketReader, PacketWriteEndInfo, PacketWriter};
use rubato::{FastFixedIn, PolynomialDegree, Resampler};
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

const OPUS_RATE: u32 = 48_000;
const FRAME_SIZE: usize = 960;
const RESAMPLE_CHUNK: usize = 1024;
static NEXT_SERIAL: AtomicU32 = AtomicU32::new(0x464f_5247);

pub struct OpusStreamWriter {
    packets: PacketWriter<'static, BufWriter<File>>,
    encoder: Encoder,
    channels: usize,
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
    pub fn create(
        path: &Path,
        input_rate: u32,
        input_frames: usize,
        channels: u16,
        bitrate_kbps: i32,
        track_lufs: f64,
        album_lufs: Option<f64>,
    ) -> Result<Self, String> {
        let channel_mode = opus_channels(channels)?;
        if bitrate_kbps <= 0 {
            return Err("Opus bitrate must be positive".into());
        }
        let mut encoder = Encoder::new(OPUS_RATE, channel_mode, Application::Audio)
            .map_err(|error| format!("create Opus encoder: {error}"))?;
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
                opus_head(channels, pre_skip as u16, input_rate),
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
        for frame in start..end {
            for channel in planar {
                self.encode_pending.push(channel[frame]);
            }
        }
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
            .encode_vec_float(&frame, 4000)
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
    let head = packets
        .read_packet()
        .map_err(|error| format!("{}: read OpusHead: {error}", path.display()))?
        .ok_or_else(|| format!("{}: missing OpusHead", path.display()))?;
    let (channels, pre_skip) = parse_opus_head(&head.data)?;
    let serial = head.stream_serial();
    let tags = packets
        .read_packet()
        .map_err(|error| format!("{}: read OpusTags: {error}", path.display()))?
        .ok_or_else(|| format!("{}: missing OpusTags", path.display()))?;
    if tags.stream_serial() != serial || !tags.data.starts_with(b"OpusTags") {
        return Err(format!("{}: invalid OpusTags", path.display()));
    }
    let info = StreamInfo {
        sample_rate: OPUS_RATE,
        channels,
        channel_roles: default_channel_roles(channels),
        source_kind: PcmKind::F32,
    };
    let mut decoder = Decoder::new(OPUS_RATE, opus_channels(channels)?)
        .map_err(|error| format!("create Opus decoder: {error}"))?;
    let mut skip = pre_skip as usize;
    let mut raw_frames = 0_u64;

    while let Some(packet) = packets
        .read_packet()
        .map_err(|error| format!("{}: read Ogg packet: {error}", path.display()))?
    {
        if packet.stream_serial() != serial {
            continue;
        }
        let mut interleaved = vec![0.0_f32; 5760 * channels as usize];
        let frames = decoder
            .decode_float(&packet.data, &mut interleaved, false)
            .map_err(|error| format!("{}: decode Opus: {error}", path.display()))?;
        let packet_start = raw_frames;
        raw_frames += frames as u64;
        let start = skip.min(frames);
        skip -= start;
        let mut end = frames;
        if packet.last_in_stream() {
            let target_raw_frames = packet.absgp_page();
            end = end.min(target_raw_frames.saturating_sub(packet_start) as usize);
        }
        if end <= start {
            continue;
        }
        let mut planar = vec![Vec::with_capacity(end - start); channels as usize];
        for frame in start..end {
            for channel in 0..channels as usize {
                planar[channel].push(interleaved[frame * channels as usize + channel]);
            }
        }
        consume(&info, &mut planar)?;
    }
    Ok(info)
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
    if packets.len() < 2 || !packets[1].data.starts_with(b"OpusTags") {
        return Err(format!("{}: missing OpusTags", path.display()));
    }
    packets[1].data = replace_r128_comments(&packets[1].data, track_lufs, album_lufs)?;

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
            "Ogg Opus output currently supports mono or stereo, got {channels} channels"
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

fn opus_head(channels: u16, pre_skip: u16, original_rate: u32) -> Vec<u8> {
    let mut head = b"OpusHead".to_vec();
    head.push(1);
    head.push(channels as u8);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&original_rate.to_le_bytes());
    head.extend_from_slice(&0_i16.to_le_bytes());
    head.push(0);
    head
}

fn parse_opus_head(data: &[u8]) -> Result<(u16, u16), String> {
    if data.len() < 19 || &data[..8] != b"OpusHead" || data[8] == 0 {
        return Err("invalid OpusHead".into());
    }
    let channels = data[9] as u16;
    opus_channels(channels)?;
    Ok((channels, u16::from_le_bytes([data[10], data[11]])))
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
