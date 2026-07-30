//! Cross-container audio metadata preservation.

use lofty::aac::AacFile;
use lofty::config::WriteOptions;
use lofty::config::{ParseOptions, ParsingMode};
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::id3::v2::{Frame, Id3v2Tag};
use lofty::iff::aiff::AiffFile;
use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst, Mp4File};
use lofty::mpeg::MpegFile;
use lofty::probe::Probe;
use lofty::tag::{ItemKey, Tag, TagExt};
use std::borrow::Cow;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::normalize::Analysis;
use crate::wav::WaveChunk;

/// Copy the source's primary metadata tag to the destination container.
///
/// Lofty's generic tag representation retains common text fields and artwork.
/// Remapping discards only fields that the destination tag format cannot
/// represent.
pub fn copy_metadata(input: &Path, output: &Path) -> Result<(), String> {
    if matches!(
        input
            .extension()
            .and_then(|value| value.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("dsf" | "dff")
    ) {
        // DSF ID3 and DSDIFF DIIN/COMT metadata have no lossless, standardized
        // mapping to Forge's PCM output containers. Keep DSD handling
        // read-only instead of silently translating or dropping fields into a
        // partially equivalent tag model.
        return Ok(());
    }
    let source = lofty::read_from_path(input)
        .map_err(|error| format!("read metadata {}: {error}", input.display()))?;
    let Some(mut tag) = source.primary_tag().or_else(|| source.first_tag()).cloned() else {
        return Ok(());
    };

    let destination = match lofty::read_from_path(output) {
        Ok(destination) => destination,
        Err(_) if is_wave_family(output)? => return Ok(()),
        Err(error) => {
            return Err(format!(
                "read output metadata {}: {error}",
                output.display()
            ))
        }
    };
    tag.re_map(destination.primary_tag_type());
    tag.save_to_path(output, WriteOptions::default())
        .map_err(|error| format!("write metadata {}: {error}", output.display()))
}

/// Read the Broadcast Wave `bext` chunk without interpreting vendor fields.
pub fn read_bext(path: &Path) -> Result<Option<Vec<u8>>, String> {
    read_wave_chunk(path, *b"bext")
}

pub fn read_wave_chunk(path: &Path, wanted: [u8; 4]) -> Result<Option<Vec<u8>>, String> {
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let Some((offset, size)) = scan_wave_chunks(&mut file, |id, offset, size| {
        Ok((id == wanted).then_some((offset, size)))
    })?
    else {
        return Ok(None);
    };
    let size = usize::try_from(size).map_err(|_| "bext chunk is too large".to_string())?;
    let mut body = vec![0; size];
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek bext: {error}"))?;
    file.read_exact(&mut body)
        .map_err(|error| format!("read bext: {error}"))?;
    Ok(Some(body))
}

/// Preserve production metadata required by common BWF/ADM workflows.
pub fn prepare_broadcast_chunks(input: &Path) -> Result<Vec<WaveChunk>, String> {
    let mut chunks = vec![WaveChunk {
        id: *b"bext",
        body: prepare_bext(input)?,
    }];
    for id in [*b"axml", *b"bxml", *b"sxml", *b"chna", *b"iXML"] {
        if let Some(body) = read_wave_chunk(input, id)? {
            chunks.push(WaveChunk { id, body });
        }
    }
    Ok(chunks)
}

/// Prepare a BWF v2 `bext` body, preserving source production metadata.
pub fn prepare_bext(input: &Path) -> Result<Vec<u8>, String> {
    let mut bext = read_bext(input)?.unwrap_or_else(blank_bext);
    bext.resize(bext.len().max(602), 0);
    let version = u16::from_le_bytes([bext[346], bext[347]]).max(2);
    bext[346..348].copy_from_slice(&version.to_le_bytes());
    Ok(bext)
}

pub fn blank_bext() -> Vec<u8> {
    let mut bext = vec![0; 602];
    bext[346..348].copy_from_slice(&2u16.to_le_bytes());
    bext
}

/// Update the five EBU R 128 fields in an existing BWF v2 `bext` chunk.
pub fn update_bwf_loudness(path: &Path, analysis: &Analysis) -> Result<(), String> {
    let mut file = File::options()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    let bext = scan_wave_chunks(&mut file, |id, offset, size| {
        if id == *b"bext" {
            Ok(Some((offset, size)))
        } else {
            Ok(None)
        }
    })?
    .ok_or_else(|| format!("{}: missing BWF bext chunk", path.display()))?;
    if bext.1 < 422 {
        return Err(format!("{}: BWF bext chunk is too short", path.display()));
    }
    file.seek(SeekFrom::Start(bext.0 + 346))
        .map_err(|error| format!("seek BWF version: {error}"))?;
    file.write_all(&2u16.to_le_bytes())
        .map_err(|error| format!("write BWF version: {error}"))?;
    file.seek(SeekFrom::Start(bext.0 + 412))
        .map_err(|error| format!("seek BWF loudness metadata: {error}"))?;
    for value in [
        analysis.lufs,
        analysis.loudness_range_lu,
        analysis.true_peak_db(),
        analysis.max_momentary_lufs,
        analysis.max_short_term_lufs,
    ] {
        file.write_all(&bwf_value(value).to_le_bytes())
            .map_err(|error| format!("write BWF loudness metadata: {error}"))?;
    }
    file.flush()
        .map_err(|error| format!("flush BWF loudness metadata: {error}"))
}

fn bwf_value(value: f64) -> i16 {
    if value.is_finite() {
        (value * 100.0)
            .round()
            .clamp(i16::MIN as f64, (i16::MAX - 1) as f64) as i16
    } else {
        i16::MAX
    }
}

fn is_wave_family(path: &Path) -> Result<bool, String> {
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut magic = [0; 4];
    file.read_exact(&mut magic)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(matches!(&magic, b"RIFF" | b"RF64" | b"BW64"))
}

fn scan_wave_chunks<T>(
    file: &mut File,
    mut inspect: impl FnMut([u8; 4], u64, u64) -> Result<Option<T>, String>,
) -> Result<Option<T>, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek WAVE header: {error}"))?;
    let mut header = [0; 12];
    file.read_exact(&mut header)
        .map_err(|error| format!("read WAVE header: {error}"))?;
    if !matches!(&header[..4], b"RIFF" | b"RF64" | b"BW64") || &header[8..] != b"WAVE" {
        return Ok(None);
    }
    loop {
        let mut chunk = [0; 8];
        match file.read_exact(&mut chunk) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(format!("read WAVE chunk: {error}")),
        }
        let id: [u8; 4] = chunk[..4].try_into().unwrap();
        let size = u32::from_le_bytes(chunk[4..].try_into().unwrap()) as u64;
        let offset = file
            .stream_position()
            .map_err(|error| format!("locate WAVE chunk: {error}"))?;
        if let Some(result) = inspect(id, offset, size)? {
            return Ok(Some(result));
        }
        if id == *b"data" {
            return Ok(None);
        }
        file.seek(SeekFrom::Start(
            offset
                .checked_add(size)
                .and_then(|position| position.checked_add(size & 1))
                .ok_or_else(|| "WAVE chunk offset overflow".to_string())?,
        ))
        .map_err(|error| format!("skip WAVE chunk: {error}"))?;
    }
}

/// Write ReplayGain 2.0 fields while leaving encoded audio untouched.
///
/// ReplayGain 2.0 uses EBU R128 measurement with a -18 LUFS reference.
pub fn write_replaygain(
    path: &Path,
    track_lufs: f64,
    track_peak: f32,
    album: Option<(f64, f32)>,
) -> Result<(), String> {
    let tagged = lofty::read_from_path(path)
        .map_err(|error| format!("read metadata {}: {error}", path.display()))?;
    let mut tag = tagged
        .primary_tag()
        .or_else(|| tagged.first_tag())
        .cloned()
        .unwrap_or_else(|| Tag::new(tagged.primary_tag_type()));
    tag.re_map(tagged.primary_tag_type());
    tag.insert_text(
        ItemKey::ReplayGainTrackGain,
        format!("{:+.2} dB", -18.0 - track_lufs),
    );
    tag.insert_text(ItemKey::ReplayGainTrackPeak, format!("{:.8}", track_peak));
    if let Some((album_lufs, album_peak)) = album {
        tag.insert_text(
            ItemKey::ReplayGainAlbumGain,
            format!("{:+.2} dB", -18.0 - album_lufs),
        );
        tag.insert_text(ItemKey::ReplayGainAlbumPeak, format!("{:.8}", album_peak));
    }
    tag.save_to_path(path, WriteOptions::default())
        .map_err(|error| format!("write metadata {}: {error}", path.display()))
}

const SOUND_CHECK_DESCRIPTION: &str = "iTunNORM";
const SOUND_CHECK_REFERENCE_LUFS: f64 = -18.0;
const SOUND_CHECK_MAX_ENGINEERING_WORD: f64 = 65_534.0;

/// Parsed Apple Sound Check compatibility metadata.
///
/// Apple documents Sound Check's playback behaviour but does not publish the
/// `iTunNORM` field layout or its analysis algorithm. Forge therefore exposes
/// the ten observed words without assigning meaning to the undocumented pairs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoundCheck {
    words: [u32; 10],
}

impl SoundCheck {
    /// Parse the de-facto `iTunNORM` representation: ten eight-digit
    /// hexadecimal words separated by ASCII whitespace.
    pub fn parse(value: &str) -> Result<Self, String> {
        if !value.is_ascii() {
            return Err("Sound Check metadata must contain only ASCII".into());
        }
        let fields: Vec<_> = value.split_ascii_whitespace().collect();
        if fields.len() != 10 {
            return Err(format!(
                "Sound Check metadata has {} hexadecimal words, expected 10",
                fields.len()
            ));
        }
        let mut words = [0_u32; 10];
        for (index, field) in fields.into_iter().enumerate() {
            if field.len() != 8 || !field.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(format!(
                    "Sound Check word {} must contain exactly eight hexadecimal digits",
                    index + 1
                ));
            }
            words[index] = u32::from_str_radix(field, 16)
                .map_err(|error| format!("parse Sound Check word {}: {error}", index + 1))?;
        }
        if words[..4].contains(&0) {
            return Err("Sound Check gain words must be non-zero".into());
        }
        Ok(Self { words })
    }

    /// Create a conservative interoperability value from an R128 measurement.
    ///
    /// This is explicitly an engineering mapping, not Apple's private
    /// analyser: Forge maps the same -18 LUFS reference used for ReplayGain
    /// 2.0 into the observed 1000/2500 word pairs and records sample peak in
    /// the observed 16-bit scale. Undocumented word pairs remain zero.
    pub fn from_r128(track_lufs: f64, sample_peak: f32) -> Result<Self, String> {
        if !track_lufs.is_finite() {
            return Err("Sound Check loudness must be finite".into());
        }
        if !sample_peak.is_finite() || sample_peak < 0.0 {
            return Err("Sound Check sample peak must be finite and non-negative".into());
        }
        let gain_db = SOUND_CHECK_REFERENCE_LUFS - track_lufs;
        let gain_word = |reference: f64| {
            (reference * 10_f64.powf(-gain_db / 10.0))
                .round()
                .clamp(1.0, SOUND_CHECK_MAX_ENGINEERING_WORD) as u32
        };
        let peak_word = (f64::from(sample_peak) * 32_768.0)
            .round()
            .clamp(0.0, u32::MAX as f64) as u32;
        Ok(Self {
            words: [
                gain_word(1_000.0),
                gain_word(1_000.0),
                gain_word(2_500.0),
                gain_word(2_500.0),
                0,
                0,
                peak_word,
                peak_word,
                0,
                0,
            ],
        })
    }

    pub fn words(&self) -> &[u32; 10] {
        &self.words
    }

    /// Gain implied by the more conservative of the first stereo pair.
    pub fn engineering_gain_db(&self) -> f64 {
        -10.0 * (f64::from(self.words[0].max(self.words[1])) / 1_000.0).log10()
    }

    /// Sample-peak ratio implied by the observed 16-bit peak pair.
    pub fn engineering_sample_peak(&self) -> f64 {
        f64::from(self.words[6].max(self.words[7])) / 32_768.0
    }

    pub fn canonical_value(&self) -> String {
        self.words.iter().map(|word| format!("{word:08X}")).fold(
            String::new(),
            |mut value, word| {
                value.push(' ');
                value.push_str(&word);
                value
            },
        )
    }
}

/// Read `iTunNORM` from MP4 `ilst` or an ID3v2 comment.
///
/// MP4 uses `----:com.apple.iTunes:iTunNORM`; MPEG audio, AIFF and raw AAC
/// use a `COMM` frame whose description is `iTunNORM`.
pub fn read_sound_check(path: &Path) -> Result<Option<SoundCheck>, String> {
    match probe_file_type(path)? {
        FileType::Mp4 => read_mp4_sound_check(path),
        file_type @ (FileType::Mpeg | FileType::Aiff | FileType::Aac) => {
            let Some(tag) = read_id3v2(path, file_type)? else {
                return Ok(None);
            };
            sound_check_from_id3(&tag)
        }
        other => Err(format!(
            "{}: Sound Check metadata is unsupported for {other:?}; use MP4/M4A, MP3, AIFF, or AAC",
            path.display()
        )),
    }
}

/// Write `iTunNORM` without changing encoded audio, then read it back exactly.
pub fn write_sound_check(path: &Path, value: &SoundCheck) -> Result<SoundCheck, String> {
    match probe_file_type(path)? {
        FileType::Mp4 => write_mp4_sound_check(path, value)?,
        file_type @ (FileType::Mpeg | FileType::Aiff | FileType::Aac) => {
            let mut tag = read_id3v2(path, file_type)?.unwrap_or_default();
            tag.retain(|frame| {
                !matches!(
                    frame,
                    Frame::Comment(comment) if comment.description == SOUND_CHECK_DESCRIPTION
                )
            });
            tag.insert(Frame::Comment(lofty::id3::v2::CommentFrame::new(
                lofty::TextEncoding::UTF8,
                lofty::tag::items::ENGLISH,
                SOUND_CHECK_DESCRIPTION,
                value.canonical_value(),
            )));
            tag.save_to_path(path, WriteOptions::default())
                .map_err(|error| {
                    format!("write Sound Check metadata {}: {error}", path.display())
                })?;
        }
        other => {
            return Err(format!(
                "{}: Sound Check metadata is unsupported for {other:?}; use MP4/M4A, MP3, AIFF, or AAC",
                path.display()
            ));
        }
    }
    let round_trip = read_sound_check(path)?.ok_or_else(|| {
        format!(
            "{}: Sound Check metadata disappeared after writing",
            path.display()
        )
    })?;
    if &round_trip != value {
        return Err(format!(
            "{}: Sound Check metadata changed during write/read round trip",
            path.display()
        ));
    }
    Ok(round_trip)
}

fn probe_file_type(path: &Path) -> Result<FileType, String> {
    Probe::open(path)
        .map_err(|error| format!("open metadata {}: {error}", path.display()))?
        .guess_file_type()
        .map_err(|error| format!("identify metadata container {}: {error}", path.display()))?
        .file_type()
        .ok_or_else(|| format!("cannot identify metadata container {}", path.display()))
}

fn parse_options() -> ParseOptions {
    ParseOptions::new().parsing_mode(ParsingMode::Strict)
}

fn read_id3v2(path: &Path, file_type: FileType) -> Result<Option<Id3v2Tag>, String> {
    let mut file =
        File::open(path).map_err(|error| format!("open metadata {}: {error}", path.display()))?;
    let tag =
        match file_type {
            FileType::Mpeg => MpegFile::read_from(&mut file, parse_options())
                .map(|parsed| parsed.id3v2().cloned()),
            FileType::Aiff => AiffFile::read_from(&mut file, parse_options())
                .map(|parsed| parsed.id3v2().cloned()),
            FileType::Aac => {
                AacFile::read_from(&mut file, parse_options()).map(|parsed| parsed.id3v2().cloned())
            }
            _ => unreachable!("read_id3v2 called for a non-ID3 Sound Check container"),
        }
        .map_err(|error| format!("read ID3v2 metadata {}: {error}", path.display()))?;
    Ok(tag)
}

fn sound_check_from_id3(tag: &Id3v2Tag) -> Result<Option<SoundCheck>, String> {
    let mut values = tag.into_iter().filter_map(|frame| match frame {
        Frame::Comment(comment) if comment.description == SOUND_CHECK_DESCRIPTION => {
            Some(comment.content.as_ref())
        }
        _ => None,
    });
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err("ID3v2 contains multiple iTunNORM comments".into());
    }
    SoundCheck::parse(value).map(Some)
}

fn sound_check_ident() -> AtomIdent<'static> {
    AtomIdent::Freeform {
        mean: Cow::Borrowed("com.apple.iTunes"),
        name: Cow::Borrowed(SOUND_CHECK_DESCRIPTION),
    }
}

fn read_mp4_ilst(path: &Path) -> Result<Option<Ilst>, String> {
    let mut file =
        File::open(path).map_err(|error| format!("open metadata {}: {error}", path.display()))?;
    Mp4File::read_from(&mut file, parse_options())
        .map(|parsed| parsed.ilst().cloned())
        .map_err(|error| format!("read MP4 metadata {}: {error}", path.display()))
}

fn read_mp4_sound_check(path: &Path) -> Result<Option<SoundCheck>, String> {
    let Some(ilst) = read_mp4_ilst(path)? else {
        return Ok(None);
    };
    let Some(atom) = ilst.get(&sound_check_ident()) else {
        return Ok(None);
    };
    let mut values = atom.data();
    let Some(data) = values.next() else {
        return Err("MP4 iTunNORM atom has no data".into());
    };
    if values.next().is_some() {
        return Err("MP4 contains multiple iTunNORM values".into());
    }
    let text = match data {
        AtomData::UTF8(value) | AtomData::UTF16(value) => value,
        _ => return Err("MP4 iTunNORM atom is not text".into()),
    };
    SoundCheck::parse(text).map(Some)
}

fn write_mp4_sound_check(path: &Path, value: &SoundCheck) -> Result<(), String> {
    let mut ilst = read_mp4_ilst(path)?.unwrap_or_default();
    ilst.replace_atom(Atom::new(
        sound_check_ident(),
        AtomData::UTF8(value.canonical_value()),
    ));
    ilst.save_to_path(path, WriteOptions::default())
        .map_err(|error| format!("write Sound Check metadata {}: {error}", path.display()))
}

#[cfg(test)]
mod sound_check_tests {
    use super::*;

    #[test]
    fn sound_check_parser_accepts_canonical_value() {
        let value = SoundCheck::parse(
            " 000003E8 000003E8 000009C4 000009C4 00000000 \
             00000000 00008000 00008000 00000000 00000000",
        )
        .unwrap();
        assert_eq!(value.engineering_gain_db(), 0.0);
        assert_eq!(value.engineering_sample_peak(), 1.0);
        assert_eq!(
            value.canonical_value(),
            " 000003E8 000003E8 000009C4 000009C4 00000000 \
             00000000 00008000 00008000 00000000 00000000"
        );
    }

    #[test]
    fn sound_check_parser_rejects_ambiguous_or_inactive_values() {
        assert!(SoundCheck::parse("000003E8").is_err());
        assert!(SoundCheck::parse(
            " 00000000 000003E8 000009C4 000009C4 00000000 \
             00000000 00008000 00008000 00000000 00000000"
        )
        .is_err());
        assert!(SoundCheck::parse(
            " 000003E8 000003E8 000009C4 000009C4 00000000 \
             00000000 0000800Z 00008000 00000000 00000000"
        )
        .is_err());
    }

    #[test]
    fn r128_engineering_mapping_round_trips_gain_and_peak() {
        let value = SoundCheck::from_r128(-14.0, 0.5).unwrap();
        assert!((value.engineering_gain_db() + 4.0).abs() < 0.01);
        assert!((value.engineering_sample_peak() - 0.5).abs() < f64::EPSILON);
        assert_eq!(SoundCheck::parse(&value.canonical_value()).unwrap(), value);
    }

    #[test]
    fn format_specific_in_memory_round_trip_is_exact() {
        let value = SoundCheck::from_r128(-16.0, 0.75).unwrap();

        let mut id3 = Id3v2Tag::new();
        id3.insert(Frame::Comment(lofty::id3::v2::CommentFrame::new(
            lofty::TextEncoding::UTF8,
            lofty::tag::items::ENGLISH,
            SOUND_CHECK_DESCRIPTION,
            value.canonical_value(),
        )));
        assert_eq!(sound_check_from_id3(&id3).unwrap(), Some(value.clone()));

        let mut ilst = Ilst::new();
        ilst.replace_atom(Atom::new(
            sound_check_ident(),
            AtomData::UTF8(value.canonical_value()),
        ));
        let atom = ilst.get(&sound_check_ident()).unwrap();
        let text = match atom.data().next().unwrap() {
            AtomData::UTF8(text) => text,
            _ => panic!("expected UTF-8 iTunNORM atom"),
        };
        assert_eq!(SoundCheck::parse(text).unwrap(), value);
    }
}
