//! Apple Lossless codec configuration and packet validation.
//!
//! ALAC does not carry a native frame checksum. Payload-validity checking therefore
//! consists of validating the complete magic cookie and strictly decoding each
//! container-delimited access unit.

use serde::Serialize;
use symphonia_alac_qc::AlacDecoder;
use symphonia_core_alac_qc::codecs::audio::well_known::CODEC_ID_ALAC;
use symphonia_core_alac_qc::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions,
};
use symphonia_core_alac_qc::packet::PacketRef;

const MAX_FRAME_LENGTH: u32 = 16_384;
const MAX_SAMPLE_RATE: u32 = 384_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct AlacConfig {
    pub(crate) frame_length: u32,
    pub(crate) compatible_version: u8,
    pub(crate) bit_depth: u8,
    pub(crate) pb: u8,
    pub(crate) mb: u8,
    pub(crate) kb: u8,
    pub(crate) channels: u8,
    pub(crate) max_run: u16,
    pub(crate) max_frame_bytes: u32,
    pub(crate) average_bit_rate: u32,
    pub(crate) sample_rate_hz: u32,
    pub(crate) channel_layout_tag: Option<u32>,
}

impl AlacConfig {
    pub(crate) fn parse(cookie: &[u8]) -> Result<Self, String> {
        if !matches!(cookie.len(), 24 | 48) {
            return Err(format!(
                "ALAC magic cookie is {} bytes; expected 24 or 48",
                cookie.len()
            ));
        }
        let frame_length = be_u32(&cookie[0..4]);
        let compatible_version = cookie[4];
        let bit_depth = cookie[5];
        let pb = cookie[6];
        let mb = cookie[7];
        let kb = cookie[8];
        let channels = cookie[9];
        let max_run = u16::from_be_bytes(cookie[10..12].try_into().unwrap());
        let max_frame_bytes = be_u32(&cookie[12..16]);
        let average_bit_rate = be_u32(&cookie[16..20]);
        let sample_rate_hz = be_u32(&cookie[20..24]);

        if !(1..=MAX_FRAME_LENGTH).contains(&frame_length) {
            return Err(format!(
                "ALAC frameLength {frame_length} is outside 1..={MAX_FRAME_LENGTH}"
            ));
        }
        if compatible_version != 0 {
            return Err(format!(
                "ALAC compatibleVersion is {compatible_version}; only version 0 is defined"
            ));
        }
        if !matches!(bit_depth, 16 | 20 | 24 | 32) {
            return Err(format!(
                "ALAC bitDepth is {bit_depth}; expected 16, 20, 24, or 32"
            ));
        }
        if kb > 32 {
            return Err(format!(
                "ALAC Rice-history limit kb={kb} exceeds the 32-bit code width"
            ));
        }
        if !(1..=8).contains(&channels) {
            return Err(format!("ALAC channel count {channels} is outside 1..=8"));
        }
        if !(1..=MAX_SAMPLE_RATE).contains(&sample_rate_hz) {
            return Err(format!(
                "ALAC sample rate {sample_rate_hz} is outside 1..={MAX_SAMPLE_RATE} Hz"
            ));
        }

        let channel_layout_tag = if cookie.len() == 48 {
            if be_u32(&cookie[24..28]) != 24
                || &cookie[28..32] != b"chan"
                || be_u32(&cookie[32..36]) != 0
                || be_u32(&cookie[40..44]) != 0
                || be_u32(&cookie[44..48]) != 0
            {
                return Err(
                    "ALAC channel-layout info has invalid size, type, version, or reserved fields"
                        .into(),
                );
            }
            let tag = be_u32(&cookie[36..40]);
            let expected_channels = match tag {
                0x0064_0001 => 1,
                0x0065_0002 => 2,
                0x0071_0003 => 3,
                0x0074_0004 => 4,
                0x0078_0005 => 5,
                0x007c_0006 => 6,
                0x008e_0007 => 7,
                0x007f_0008 => 8,
                _ => return Err(format!("unsupported ALAC channel-layout tag 0x{tag:08x}")),
            };
            if channels != expected_channels {
                return Err(format!(
                    "ALAC channel-layout tag declares {expected_channels} channels but the config declares {channels}"
                ));
            }
            Some(tag)
        } else {
            None
        };

        Ok(Self {
            frame_length,
            compatible_version,
            bit_depth,
            pb,
            mb,
            kb,
            channels,
            max_run,
            max_frame_bytes,
            average_bit_rate,
            sample_rate_hz,
            channel_layout_tag,
        })
    }
}

pub(crate) struct PacketDecoder {
    decoder: AlacDecoder,
    config: AlacConfig,
}

impl PacketDecoder {
    pub(crate) fn new(cookie: &[u8], config: AlacConfig) -> Result<Self, String> {
        let mut parameters = AudioCodecParameters::new();
        parameters
            .for_codec(CODEC_ID_ALAC)
            .with_extra_data(cookie.to_vec().into_boxed_slice());
        let decoder = AlacDecoder::try_new(&parameters, &AudioDecoderOptions::default())
            .map_err(|error| format!("create ALAC decoder: {error}"))?;
        Ok(Self { decoder, config })
    }

    pub(crate) fn decode(&mut self, packet_bytes: &[u8], duration: u64) -> Result<usize, String> {
        let packet = PacketRef::new(0, 0_i64.into(), duration.into(), packet_bytes);
        let decoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.decoder.decode_ref(&packet).map(|decoded| {
                let frames = decoded.frames();
                let channels = decoded.spec().channels().count();
                (frames, channels)
            })
        }))
        .map_err(|_| "decode ALAC access unit: decoder rejected unsafe frame geometry".to_string())?
        .map_err(|error| format!("decode ALAC access unit: {error}"))?;
        let (frames, channels) = decoded;
        if frames == 0 || frames > self.config.frame_length as usize {
            return Err(format!(
                "decoded ALAC frame count {frames} is outside 1..={}",
                self.config.frame_length
            ));
        }
        if channels != self.config.channels as usize {
            return Err(format!(
                "decoded ALAC channel count {channels} differs from {}",
                self.config.channels
            ));
        }
        Ok(frames)
    }
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(channels: u8) -> Vec<u8> {
        [
            4096_u32.to_be_bytes().as_slice(),
            &[0, 24, 40, 10, 14, channels],
            &255_u16.to_be_bytes(),
            &0_u32.to_be_bytes(),
            &0_u32.to_be_bytes(),
            &48_000_u32.to_be_bytes(),
        ]
        .concat()
    }

    fn write_bits(output: &mut Vec<u8>, bit_len: &mut usize, value: u64, width: usize) {
        for shift in (0..width).rev() {
            if (*bit_len).is_multiple_of(8) {
                output.push(0);
            }
            let bit = ((value >> shift) & 1) as u8;
            let byte = output.last_mut().unwrap();
            *byte |= bit << (7 - *bit_len % 8);
            *bit_len += 1;
        }
    }

    fn uncompressed_mono_packet(samples: u32) -> Vec<u8> {
        let mut output = Vec::new();
        let mut bits = 0;
        write_bits(&mut output, &mut bits, 0, 3); // SCE
        write_bits(&mut output, &mut bits, 0, 4); // instance
        write_bits(&mut output, &mut bits, 0, 12); // reserved
        write_bits(&mut output, &mut bits, 1, 1); // partial frame
        write_bits(&mut output, &mut bits, 0, 2); // no shifted bytes
        write_bits(&mut output, &mut bits, 1, 1); // uncompressed
        write_bits(&mut output, &mut bits, u64::from(samples), 32);
        for _ in 0..samples.min(4096) {
            write_bits(&mut output, &mut bits, 0, 16);
        }
        write_bits(&mut output, &mut bits, 7, 3); // END
        output
    }

    #[test]
    fn parses_current_magic_cookie() {
        let parsed = AlacConfig::parse(&cookie(2)).unwrap();
        assert_eq!(parsed.frame_length, 4096);
        assert_eq!(parsed.bit_depth, 24);
        assert_eq!(parsed.channels, 2);
        assert_eq!(parsed.sample_rate_hz, 48_000);
    }

    #[test]
    fn parses_explicit_stereo_channel_layout() {
        let mut value = cookie(2);
        value.extend(24_u32.to_be_bytes());
        value.extend(b"chan");
        value.extend(0_u32.to_be_bytes());
        value.extend(0x0065_0002_u32.to_be_bytes());
        value.extend(0_u32.to_be_bytes());
        value.extend(0_u32.to_be_bytes());
        let parsed = AlacConfig::parse(&value).unwrap();
        assert_eq!(parsed.channel_layout_tag, Some(0x0065_0002));
    }

    #[test]
    fn rejects_invalid_version_bit_depth_and_layout() {
        let mut value = cookie(2);
        value[4] = 1;
        assert!(AlacConfig::parse(&value).is_err());
        value[4] = 0;
        value[5] = 12;
        assert!(AlacConfig::parse(&value).is_err());

        let mut value = cookie(2);
        value.extend(24_u32.to_be_bytes());
        value.extend(b"chan");
        value.extend(0_u32.to_be_bytes());
        value.extend(0x0064_0001_u32.to_be_bytes());
        value.extend(0_u32.to_be_bytes());
        value.extend(0_u32.to_be_bytes());
        assert!(AlacConfig::parse(&value).is_err());
    }

    #[test]
    fn strictly_decodes_an_uncompressed_access_unit() {
        let cookie = cookie(1);
        let config = AlacConfig::parse(&cookie).unwrap();
        let mut decoder = PacketDecoder::new(&cookie, config).unwrap();
        assert_eq!(decoder.decode(&uncompressed_mono_packet(1), 1).unwrap(), 1);
    }

    #[test]
    fn rejects_access_units_larger_than_the_configured_frame() {
        let cookie = cookie(1);
        let config = AlacConfig::parse(&cookie).unwrap();
        let mut decoder = PacketDecoder::new(&cookie, config).unwrap();
        assert!(decoder
            .decode(&uncompressed_mono_packet(4097), 4097)
            .is_err());
    }
}
