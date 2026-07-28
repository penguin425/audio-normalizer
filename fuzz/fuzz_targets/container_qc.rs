#![no_main]

use forge_normalizer::container_qc;
use libfuzzer_sys::fuzz_target;
use ogg::writing::{PacketWriteEndInfo, PacketWriter};
use std::io::Write;

fn audit_bytes(bytes: &[u8]) {
    if let Ok(mut file) = tempfile::NamedTempFile::new() {
        if file.write_all(bytes).is_ok() {
            let _ = container_qc::audit(file.path());
        }
    }
}

fn append_wave_chunk(wave: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    wave.extend_from_slice(id);
    wave.extend_from_slice(&(body.len() as u32).to_le_bytes());
    wave.extend_from_slice(body);
    if body.len() % 2 != 0 {
        wave.push(0);
    }
}

fn wave_with_xml(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut wave = b"RIFF\0\0\0\0WAVE".to_vec();
    append_wave_chunk(
        &mut wave,
        b"fmt ",
        &[1, 0, 1, 0, 0x80, 0xbb, 0, 0, 0, 0x77, 1, 0, 2, 0, 16, 0],
    );
    append_wave_chunk(&mut wave, id, body);
    append_wave_chunk(&mut wave, b"data", &[]);
    let riff_size = (wave.len() as u32).saturating_sub(8);
    wave[4..8].copy_from_slice(&riff_size.to_le_bytes());
    wave
}

fn mp3_with_frame_payload(data: &[u8]) -> Vec<u8> {
    let protected = data.first().is_some_and(|byte| byte & 1 != 0);
    let free_format = data.first().is_some_and(|byte| byte & 2 != 0);
    if free_format {
        let mut stream = Vec::new();
        let payload = data.get(1..).unwrap_or_default();
        for (index, padded) in [false, true, false].into_iter().enumerate() {
            let frame_size = 64 + usize::from(padded);
            let start = stream.len();
            stream.resize(start + frame_size, 0);
            stream[start..start + 4].copy_from_slice(if protected {
                b"\xff\xfa\0\0"
            } else {
                b"\xff\xfb\0\0"
            });
            if padded {
                stream[start + 2] |= 0x02;
            }
            if !payload.is_empty() {
                for (payload_index, byte) in stream[start + 4..].iter_mut().enumerate() {
                    *byte = payload[(index + payload_index) % payload.len()];
                }
            }
        }
        return stream;
    }
    let mut frame = vec![0_u8; 417];
    frame[..4].copy_from_slice(if protected {
        b"\xff\xfa\x90\0"
    } else {
        b"\xff\xfb\x90\0"
    });
    let payload = data.get(1..).unwrap_or_default();
    let count = payload.len().min(frame.len() - 4);
    frame[4..4 + count].copy_from_slice(&payload[..count]);
    frame
}

fn ambisonic_opus(data: &[u8]) -> Vec<u8> {
    const CHANNEL_COUNTS: [u8; 8] = [1, 3, 4, 6, 9, 11, 16, 18];
    let channels = CHANNEL_COUNTS
        [usize::from(data.first().copied().unwrap_or_default()) % CHANNEL_COUNTS.len()];
    let mut mapping = Vec::with_capacity(usize::from(channels));
    for channel in 0..channels {
        let source = data
            .get(usize::from(channel) + 1)
            .copied()
            .unwrap_or(channel);
        mapping.push(if source & 0x0f == 0x0f {
            255
        } else {
            source % channels
        });
    }
    let mut head = b"OpusHead\x01".to_vec();
    head.push(channels);
    head.extend_from_slice(&0_u16.to_le_bytes());
    head.extend_from_slice(&48_000_u32.to_le_bytes());
    head.extend_from_slice(&0_i16.to_le_bytes());
    head.extend_from_slice(&[2, channels, 0]);
    head.extend_from_slice(&mapping);
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&4_u32.to_le_bytes());
    tags.extend_from_slice(b"fuzz");
    tags.extend_from_slice(&0_u32.to_le_bytes());

    let mut output = Vec::new();
    {
        let mut writer = PacketWriter::new(&mut output);
        let _ = writer.write_packet(head, 42, PacketWriteEndInfo::EndPage, 0);
        let _ = writer.write_packet(tags, 42, PacketWriteEndInfo::EndPage, 0);
        let _ = writer.write_packet(vec![0], 42, PacketWriteEndInfo::EndStream, 480);
    }
    output
}

fuzz_target!(|data: &[u8]| {
    audit_bytes(data);
    audit_bytes(&mp3_with_frame_payload(data));
    audit_bytes(&ambisonic_opus(data));
    let xml_id = match data.first().copied().unwrap_or_default() % 3 {
        0 => b"axml",
        1 => b"bxml",
        _ => b"sxml",
    };
    audit_bytes(&wave_with_xml(xml_id, data.get(1..).unwrap_or_default()));

    let mut container = match data.first().copied().unwrap_or_default() % 16 {
        0 => b"RIFF\0\0\0\0WAVE".to_vec(),
        1 => b"RF64\xff\xff\xff\xffWAVE".to_vec(),
        2 => b"BW64\xff\xff\xff\xffWAVE".to_vec(),
        3 => b"OggS".to_vec(),
        4 => b"FORM\0\0\0\0AIFF".to_vec(),
        5 => b"caff\0\x01\0\0".to_vec(),
        6 => b".snd".to_vec(),
        7 => b"fLaC".to_vec(),
        8 => b"\xff\xf1\x4c\x80\x00\xff\xfc".to_vec(),
        9 => b"\x56\xe0\x00".to_vec(),
        10 => b"\x0b\x77\0\0\x14\x40\x2c\x04".to_vec(),
        11 => b"\xf8\x06iamf\0\0".to_vec(),
        12 => b"\x1a\x45\xdf\xa3".to_vec(),
        13 => b"\x47\x40\x00\x10".to_vec(),
        14 => b"\x06\x0e\x2b\x34\x02\x05\x01\x01\x0d\x01\x02\x01\x01\x02\x04\x00".to_vec(),
        _ => b"\xff\xfb\x90\0".to_vec(),
    };
    container.extend_from_slice(data.get(1..).unwrap_or_default());
    audit_bytes(&container);
});
