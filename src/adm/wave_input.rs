//! Strict, bounded WAVE chunk loading for the BS.2168 emission audit.

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const MAX_WAVE_CHUNKS: usize = 100_000;
const MAX_DS64_TABLE_ENTRIES: usize = 100_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WaveContainerKind {
    Riff,
    Rf64,
    Bw64,
}

impl WaveContainerKind {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Riff => "RIFF",
            Self::Rf64 => "RF64",
            Self::Bw64 => "BW64",
        }
    }

    const fn uses_ds64(self) -> bool {
        matches!(self, Self::Rf64 | Self::Bw64)
    }
}

#[derive(Debug)]
pub(super) struct EmissionWaveInput {
    pub(super) container: WaveContainerKind,
    pub(super) axml: Option<Vec<u8>>,
    pub(super) chna: Option<Vec<u8>>,
    pub(super) axml_count: usize,
    pub(super) chna_count: usize,
    pub(super) data_size: u64,
    pub(super) pcm: PcmGeometry,
    pub(super) ds64_sample_count: Option<u64>,
    pub(super) file_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PcmGeometry {
    pub(super) channels: u16,
    pub(super) sample_rate: u32,
    pub(super) container_bits_per_sample: u16,
    pub(super) valid_bits_per_sample: u16,
    pub(super) block_align: u16,
}

#[derive(Debug)]
struct Ds64 {
    riff_size: u64,
    data_size: u64,
    sample_count: u64,
    table: BTreeMap<[u8; 4], VecDeque<u64>>,
}

impl Ds64 {
    fn pop_size(&mut self, id: [u8; 4]) -> Option<u64> {
        self.table.get_mut(&id).and_then(VecDeque::pop_front)
    }

    fn unused_entries(&self) -> usize {
        self.table.values().map(VecDeque::len).sum()
    }

    fn unused_ids(&self) -> String {
        self.table
            .iter()
            .filter(|(_, sizes)| !sizes.is_empty())
            .map(|(id, sizes)| format!("{} ({})", fourcc(*id), sizes.len()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Open and read the metadata and essence extent needed by the emission audit.
#[cfg(test)]
pub(super) fn read(
    path: &Path,
    max_axml_bytes: usize,
    max_chna_bytes: usize,
) -> Result<EmissionWaveInput, String> {
    let mut file = File::open(path)
        .map_err(|error| format!("open ADM WAVE input {}: {error}", path.display()))?;
    read_from(&mut file, path, max_axml_bytes, max_chna_bytes)
}

/// Read the emission-audit WAVE structure from an already-open descriptor.
///
/// The scanner starts from byte zero, checks the complete RIFF/RF64/BW64 chunk
/// layout, and seeks over chunk bodies which are not needed by the audit. In
/// particular, PCM data is never loaded into memory here. `display_path` is
/// used only to identify the descriptor in diagnostics; it is never reopened.
pub(super) fn read_from(
    file: &mut File,
    display_path: &Path,
    max_axml_bytes: usize,
    max_chna_bytes: usize,
) -> Result<EmissionWaveInput, String> {
    let path = display_path;
    let metadata = file
        .metadata()
        .map_err(|error| format!("stat ADM WAVE input {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "ADM WAVE input {} is not a regular file",
            path.display()
        ));
    }
    let file_bytes = metadata.len();
    if file_bytes < 12 {
        return Err(format!(
            "ADM WAVE input {} has a truncated 12-byte container header",
            path.display()
        ));
    }

    file.seek(SeekFrom::Start(0)).map_err(|error| {
        format!(
            "seek ADM WAVE input {} to byte zero: {error}",
            path.display()
        )
    })?;
    let mut header = [0_u8; 12];
    file.read_exact(&mut header)
        .map_err(|error| format!("read ADM WAVE header {}: {error}", path.display()))?;
    let container = match &header[..4] {
        b"RIFF" => WaveContainerKind::Riff,
        b"RF64" => WaveContainerKind::Rf64,
        b"BW64" => WaveContainerKind::Bw64,
        _ => {
            return Err(format!(
                "ADM input {} is not a RIFF, RF64, or BW64 WAVE file",
                path.display()
            ));
        }
    };
    if &header[8..12] != b"WAVE" {
        return Err(format!(
            "ADM input {} has an invalid WAVE signature",
            path.display()
        ));
    }

    let declared_riff_size = u32::from_le_bytes(header[4..8].try_into().unwrap());
    if container.uses_ds64() {
        if declared_riff_size != u32::MAX {
            return Err(format!(
                "{} input {} must use 0xffffffff for its RIFF size",
                container.as_str(),
                path.display()
            ));
        }
    } else {
        let declared_file_bytes = u64::from(declared_riff_size)
            .checked_add(8)
            .ok_or_else(|| "RIFF container size overflow".to_string())?;
        if declared_file_bytes != file_bytes {
            return Err(format!(
                "RIFF size declares {declared_file_bytes} file bytes, but {} contains {file_bytes}",
                path.display()
            ));
        }
    }

    let mut offset = 12_u64;
    let mut chunk_count = 0_usize;
    let mut ds64_count = 0_usize;
    let mut ds64 = None;
    let mut axml = None;
    let mut chna = None;
    let mut axml_count = 0_usize;
    let mut bxml_count = 0_usize;
    let mut chna_count = 0_usize;
    let mut fmt_count = 0_usize;
    let mut pcm_geometry = None;
    let mut data_count = 0_usize;
    let mut first_data = None;
    let mut first_data_declared_size = None;

    while offset < file_bytes {
        if chunk_count >= MAX_WAVE_CHUNKS {
            return Err(format!(
                "ADM WAVE input {} exceeds the hard limit of {MAX_WAVE_CHUNKS} chunks",
                path.display()
            ));
        }
        let remaining = file_bytes
            .checked_sub(offset)
            .ok_or_else(|| "WAVE chunk offset exceeds the file length".to_string())?;
        if remaining < 8 {
            return Err(format!(
                "ADM WAVE input {} has a trailing partial chunk header of {remaining} byte(s) at offset {offset}",
                path.display()
            ));
        }

        file.seek(SeekFrom::Start(offset)).map_err(|error| {
            format!(
                "seek ADM WAVE input {} to chunk at {offset}: {error}",
                path.display()
            )
        })?;
        let mut chunk_header = [0_u8; 8];
        file.read_exact(&mut chunk_header).map_err(|error| {
            format!(
                "read ADM WAVE chunk header {} at {offset}: {error}",
                path.display()
            )
        })?;
        let id: [u8; 4] = chunk_header[..4].try_into().unwrap();
        let declared_size = u32::from_le_bytes(chunk_header[4..8].try_into().unwrap());
        chunk_count = chunk_count
            .checked_add(1)
            .ok_or_else(|| "WAVE chunk count overflow".to_string())?;

        if container.uses_ds64() && chunk_count == 1 && id != *b"ds64" {
            return Err(format!(
                "{} input {} requires ds64 as its first chunk, found {}",
                container.as_str(),
                path.display(),
                fourcc(id)
            ));
        }
        if id == *b"ds64" {
            if !container.uses_ds64() {
                return Err(format!(
                    "RIFF input {} must not contain a ds64 chunk",
                    path.display()
                ));
            }
            if ds64_count != 0 {
                return Err(format!(
                    "ADM WAVE input {} contains multiple ds64 chunks",
                    path.display()
                ));
            }
            if declared_size == u32::MAX {
                return Err(format!(
                    "ADM WAVE input {} has a ds64 chunk with an unresolved 0xffffffff size",
                    path.display()
                ));
            }
        }

        if declared_size == u32::MAX && !container.uses_ds64() {
            return Err(format!(
                "RIFF input {} contains an invalid 0xffffffff {} chunk size",
                path.display(),
                fourcc(id)
            ));
        }
        let effective_size = if declared_size != u32::MAX {
            u64::from(declared_size)
        } else if id == *b"data" {
            ds64.as_ref()
                .map(|value: &Ds64| value.data_size)
                .ok_or_else(|| {
                    format!(
                        "0xffffffff data chunk in {} appears before a usable ds64 dataSize",
                        path.display()
                    )
                })?
        } else {
            ds64.as_mut()
                .and_then(|value: &mut Ds64| value.pop_size(id))
                .ok_or_else(|| {
                    format!(
                        "0xffffffff {} chunk in {} has no preceding ds64 table entry",
                        fourcc(id),
                        path.display()
                    )
                })?
        };
        let body_offset = offset
            .checked_add(8)
            .ok_or_else(|| format!("{} chunk body offset overflow", fourcc(id)))?;
        let body_end = body_offset
            .checked_add(effective_size)
            .ok_or_else(|| format!("{} chunk end offset overflow", fourcc(id)))?;
        if body_end > file_bytes {
            return Err(format!(
                "{} chunk at {offset} ends at {body_end}, beyond the {file_bytes}-byte ADM WAVE input {}",
                fourcc(id),
                path.display()
            ));
        }
        let pad_bytes = effective_size & 1;
        let next_offset = body_end
            .checked_add(pad_bytes)
            .ok_or_else(|| format!("{} chunk padded end offset overflow", fourcc(id)))?;
        if next_offset > file_bytes {
            return Err(format!(
                "odd-sized {} chunk at {offset} in {} is missing its pad byte",
                fourcc(id),
                path.display()
            ));
        }
        if pad_bytes != 0 {
            file.seek(SeekFrom::Start(body_end)).map_err(|error| {
                format!(
                    "seek to {} pad byte in {}: {error}",
                    fourcc(id),
                    path.display()
                )
            })?;
            let mut pad = [0_u8; 1];
            file.read_exact(&mut pad).map_err(|error| {
                format!(
                    "read {} pad byte in {}: {error}",
                    fourcc(id),
                    path.display()
                )
            })?;
            if pad[0] != 0 {
                return Err(format!(
                    "odd-sized {} chunk at {offset} in {} has a non-zero pad byte",
                    fourcc(id),
                    path.display()
                ));
            }
        }

        match &id {
            b"ds64" => {
                ds64_count = ds64_count
                    .checked_add(1)
                    .ok_or_else(|| "ds64 chunk count overflow".to_string())?;
                ds64 = Some(read_ds64(path, file, body_offset, effective_size)?);
            }
            b"axml" => {
                axml_count = axml_count
                    .checked_add(1)
                    .ok_or_else(|| "axml chunk count overflow".to_string())?;
                if axml_count == 1 {
                    axml = Some(read_limited_chunk(
                        path,
                        file,
                        id,
                        body_offset,
                        effective_size,
                        max_axml_bytes,
                    )?);
                }
            }
            b"bxml" => {
                bxml_count = bxml_count
                    .checked_add(1)
                    .ok_or_else(|| "bxml chunk count overflow".to_string())?;
            }
            b"chna" => {
                chna_count = chna_count
                    .checked_add(1)
                    .ok_or_else(|| "chna chunk count overflow".to_string())?;
                if chna_count == 1 {
                    chna = Some(read_limited_chunk(
                        path,
                        file,
                        id,
                        body_offset,
                        effective_size,
                        max_chna_bytes,
                    )?);
                }
            }
            b"fmt " => {
                fmt_count = fmt_count
                    .checked_add(1)
                    .ok_or_else(|| "fmt chunk count overflow".to_string())?;
                if data_count != 0 {
                    return Err(format!(
                        "fmt chunk in {} appears after the data chunk",
                        path.display()
                    ));
                }
                if fmt_count == 1 {
                    pcm_geometry = Some(read_pcm_fmt(path, file, body_offset, effective_size)?);
                }
            }
            b"data" => {
                data_count = data_count
                    .checked_add(1)
                    .ok_or_else(|| "data chunk count overflow".to_string())?;
                if data_count == 1 {
                    first_data = Some((body_offset, effective_size));
                    first_data_declared_size = Some(declared_size);
                }
            }
            _ => {}
        }

        offset = next_offset;
    }

    if container.uses_ds64() {
        let value = ds64.as_ref().ok_or_else(|| {
            format!(
                "{} input {} is missing its required first ds64 chunk",
                container.as_str(),
                path.display()
            )
        })?;
        if ds64_count != 1 {
            return Err(format!(
                "{} input {} must contain exactly one ds64 chunk",
                container.as_str(),
                path.display()
            ));
        }
        let declared_file_bytes = value
            .riff_size
            .checked_add(8)
            .ok_or_else(|| "ds64 RIFF size overflow".to_string())?;
        if declared_file_bytes != file_bytes {
            return Err(format!(
                "ds64 riffSize declares {declared_file_bytes} file bytes, but {} contains {file_bytes}",
                path.display()
            ));
        }
    }

    if data_count != 1 {
        return Err(format!(
            "ADM WAVE input {} must contain exactly one data chunk, found {data_count}",
            path.display()
        ));
    }
    if axml_count > 1 {
        return Err(format!(
            "ADM WAVE input {} contains {axml_count} axml chunks; the BS.2088 AXML carrier must be unique",
            path.display()
        ));
    }
    if bxml_count != 0 {
        return Err(format!(
            "ADM WAVE input {} contains {bxml_count} BS.2088 BXML carrier chunk(s), which are unsupported by this AXML-subset validator",
            path.display()
        ));
    }
    if fmt_count != 1 {
        return Err(format!(
            "ADM WAVE input {} must contain exactly one fmt chunk before data, found {fmt_count}",
            path.display()
        ));
    }
    let (_, data_size) = first_data.ok_or_else(|| {
        format!(
            "ADM WAVE input {} did not retain its required data chunk",
            path.display()
        )
    })?;
    let pcm = pcm_geometry.ok_or_else(|| {
        format!(
            "ADM WAVE input {} did not retain its required fmt chunk",
            path.display()
        )
    })?;
    if data_size % u64::from(pcm.block_align) != 0 {
        return Err(format!(
            "data chunk size {data_size} in {} is not divisible by PCM blockAlign {}",
            pcm.block_align,
            path.display()
        ));
    }
    if let (Some(value), Some(declared)) = (ds64.as_ref(), first_data_declared_size) {
        if declared != u32::MAX && value.data_size != data_size {
            return Err(format!(
                "ds64 dataSize {} does not match the data chunk size {data_size} in {}",
                value.data_size,
                path.display()
            ));
        }
    }

    if let Some(value) = ds64.as_ref() {
        let unused = value.unused_entries();
        if unused != 0 {
            return Err(format!(
                "ds64 in {} contains {unused} unused table entry or entries: {}",
                path.display(),
                value.unused_ids()
            ));
        }
    }

    let final_file_bytes = file
        .metadata()
        .map_err(|error| format!("restat ADM WAVE input {}: {error}", path.display()))?
        .len();
    if final_file_bytes != file_bytes {
        return Err(format!(
            "ADM WAVE input {} changed size while it was being scanned ({file_bytes} to {final_file_bytes} bytes)",
            path.display()
        ));
    }

    Ok(EmissionWaveInput {
        container,
        axml,
        chna,
        axml_count,
        chna_count,
        data_size,
        pcm,
        ds64_sample_count: (container == WaveContainerKind::Rf64)
            .then(|| ds64.as_ref().map(|value| value.sample_count))
            .flatten(),
        file_bytes,
    })
}

fn read_pcm_fmt(
    path: &Path,
    file: &mut File,
    body_offset: u64,
    size: u64,
) -> Result<PcmGeometry, String> {
    if size < 16 {
        return Err(format!(
            "fmt chunk in {} is truncated: expected at least 16 bytes, found {size}",
            path.display()
        ));
    }
    file.seek(SeekFrom::Start(body_offset))
        .map_err(|error| format!("seek fmt chunk in {}: {error}", path.display()))?;
    let mut fixed = [0_u8; 16];
    file.read_exact(&mut fixed)
        .map_err(|error| format!("read fmt fields in {}: {error}", path.display()))?;
    let channels = u16::from_le_bytes(fixed[2..4].try_into().unwrap());
    let sample_rate = u32::from_le_bytes(fixed[4..8].try_into().unwrap());
    let byte_rate = u32::from_le_bytes(fixed[8..12].try_into().unwrap());
    let block_align = u16::from_le_bytes(fixed[12..14].try_into().unwrap());
    let bits_per_sample = u16::from_le_bytes(fixed[14..16].try_into().unwrap());
    if channels == 0 || sample_rate == 0 || bits_per_sample == 0 {
        return Err(format!(
            "fmt chunk in {} has zero channels, sampleRate, or bitsPerSample",
            path.display()
        ));
    }
    let format_tag = u16::from_le_bytes(fixed[0..2].try_into().unwrap());
    let valid_bits_per_sample = if format_tag == 0xfffe {
        if size < 40 {
            return Err(format!(
                "WAVE_FORMAT_EXTENSIBLE fmt chunk in {} is shorter than 40 bytes",
                path.display()
            ));
        }
        let mut extension = [0_u8; 24];
        file.read_exact(&mut extension).map_err(|error| {
            format!(
                "read WAVE_FORMAT_EXTENSIBLE fields in {}: {error}",
                path.display()
            )
        })?;
        let extension_size = u16::from_le_bytes(extension[0..2].try_into().unwrap());
        if extension_size < 22 || size != 18 + u64::from(extension_size) {
            return Err(format!(
                "WAVE_FORMAT_EXTENSIBLE cbSize {extension_size} in {} does not match the {size}-byte fmt chunk",
                path.display()
            ));
        }
        let valid_bits = u16::from_le_bytes(extension[2..4].try_into().unwrap());
        if valid_bits == 0 || valid_bits > bits_per_sample {
            return Err(format!(
                "WAVE_FORMAT_EXTENSIBLE validBitsPerSample {valid_bits} in {} must be between 1 and container bits {bits_per_sample}",
                path.display()
            ));
        }
        const PCM_SUBFORMAT: [u8; 16] = [
            1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71,
        ];
        if extension[8..24] != PCM_SUBFORMAT {
            return Err(format!(
                "WAVE_FORMAT_EXTENSIBLE SubFormat in {} is not the PCM GUID 00000001-0000-0010-8000-00aa00389b71",
                path.display()
            ));
        }
        valid_bits
    } else if format_tag == 0x0001 {
        if !matches!(size, 16 | 18) {
            return Err(format!(
                "legacy PCM fmt chunk in {} must contain 16 bytes or an 18-byte zero cbSize form, found {size}",
                path.display()
            ));
        }
        if size == 18 {
            let mut cb_size = [0_u8; 2];
            file.read_exact(&mut cb_size).map_err(|error| {
                format!("read legacy PCM cbSize in {}: {error}", path.display())
            })?;
            let cb_size = u16::from_le_bytes(cb_size);
            if cb_size != 0 {
                return Err(format!(
                    "legacy PCM fmt extension in {} must have cbSize zero, found {cb_size}",
                    path.display()
                ));
            }
        }
        bits_per_sample
    } else {
        return Err(format!(
            "fmt chunk in {} is not integer PCM (format tag 0x{format_tag:04x})",
            path.display()
        ));
    };
    let bytes_per_sample = bits_per_sample
        .checked_add(7)
        .ok_or_else(|| "fmt bitsPerSample overflow".to_string())?
        / 8;
    let expected_block_align = channels
        .checked_mul(bytes_per_sample)
        .ok_or_else(|| "fmt PCM blockAlign overflow".to_string())?;
    if block_align != expected_block_align {
        return Err(format!(
            "fmt blockAlign {block_align} in {} does not match PCM channels × bytesPerSample ({expected_block_align})",
            path.display()
        ));
    }
    let expected_byte_rate = sample_rate
        .checked_mul(u32::from(block_align))
        .ok_or_else(|| "fmt PCM byteRate overflow".to_string())?;
    if byte_rate != expected_byte_rate {
        return Err(format!(
            "fmt byteRate {byte_rate} in {} does not match PCM sampleRate × blockAlign ({expected_byte_rate})",
            path.display()
        ));
    }
    Ok(PcmGeometry {
        channels,
        sample_rate,
        container_bits_per_sample: bits_per_sample,
        valid_bits_per_sample,
        block_align,
    })
}

fn read_ds64(path: &Path, file: &mut File, body_offset: u64, size: u64) -> Result<Ds64, String> {
    if size < 28 {
        return Err(format!(
            "ds64 chunk in {} is truncated: expected at least 28 bytes, found {size}",
            path.display()
        ));
    }
    file.seek(SeekFrom::Start(body_offset))
        .map_err(|error| format!("seek ds64 chunk in {}: {error}", path.display()))?;
    let mut fixed = [0_u8; 28];
    file.read_exact(&mut fixed)
        .map_err(|error| format!("read ds64 fixed fields in {}: {error}", path.display()))?;

    let table_length_u32 = u32::from_le_bytes(fixed[24..28].try_into().unwrap());
    let table_length = usize::try_from(table_length_u32)
        .map_err(|_| "ds64 table count does not fit this platform".to_string())?;
    if table_length > MAX_DS64_TABLE_ENTRIES {
        return Err(format!(
            "ds64 in {} declares {table_length} table entries, exceeding the hard limit of {MAX_DS64_TABLE_ENTRIES}",
            path.display()
        ));
    }
    let table_bytes = u64::from(table_length_u32)
        .checked_mul(12)
        .ok_or_else(|| "ds64 table byte count overflow".to_string())?;
    let required_size = 28_u64
        .checked_add(table_bytes)
        .ok_or_else(|| "ds64 chunk size overflow".to_string())?;
    if size != required_size {
        return Err(format!(
            "ds64 chunk in {} has size {size}, but its tableLength requires exactly {required_size} bytes",
            path.display()
        ));
    }

    let mut table: BTreeMap<[u8; 4], VecDeque<u64>> = BTreeMap::new();
    for index in 0..table_length {
        let mut entry = [0_u8; 12];
        file.read_exact(&mut entry).map_err(|error| {
            format!(
                "read ds64 table entry {index} in {}: {error}",
                path.display()
            )
        })?;
        let id: [u8; 4] = entry[..4].try_into().unwrap();
        let entry_size = u64::from_le_bytes(entry[4..12].try_into().unwrap());
        table.entry(id).or_default().push_back(entry_size);
    }

    Ok(Ds64 {
        riff_size: u64::from_le_bytes(fixed[0..8].try_into().unwrap()),
        data_size: u64::from_le_bytes(fixed[8..16].try_into().unwrap()),
        sample_count: u64::from_le_bytes(fixed[16..24].try_into().unwrap()),
        table,
    })
}

fn read_limited_chunk(
    path: &Path,
    file: &mut File,
    id: [u8; 4],
    body_offset: u64,
    size: u64,
    maximum_bytes: usize,
) -> Result<Vec<u8>, String> {
    if size > maximum_bytes as u64 {
        return Err(format!(
            "WAVE {} chunk contains {size} bytes, exceeding the configured limit {maximum_bytes}",
            fourcc(id)
        ));
    }
    let length = usize::try_from(size).map_err(|_| {
        format!(
            "WAVE {} chunk in {} is too large for this platform",
            fourcc(id),
            path.display()
        )
    })?;
    let mut body = Vec::new();
    body.try_reserve_exact(length).map_err(|error| {
        format!(
            "reserve {length} bytes for WAVE {} chunk in {}: {error}",
            fourcc(id),
            path.display()
        )
    })?;
    body.resize(length, 0);
    file.seek(SeekFrom::Start(body_offset)).map_err(|error| {
        format!(
            "seek WAVE {} chunk in {}: {error}",
            fourcc(id),
            path.display()
        )
    })?;
    file.read_exact(&mut body).map_err(|error| {
        format!(
            "read WAVE {} chunk in {}: {error}",
            fourcc(id),
            path.display()
        )
    })?;
    Ok(body)
}

fn fourcc(id: [u8; 4]) -> String {
    String::from_utf8_lossy(&id).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        path: PathBuf,
    }

    impl Fixture {
        fn new(label: &str, bytes: &[u8]) -> Self {
            let sequence = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "forge-wave-input-{label}-{}-{sequence}.wav",
                std::process::id()
            ));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            file.write_all(bytes).unwrap();
            file.flush().unwrap();
            Self { path }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[derive(Clone)]
    struct TestChunk {
        id: [u8; 4],
        body: Vec<u8>,
        declared_size: u32,
        include_pad: bool,
    }

    impl TestChunk {
        fn regular(id: [u8; 4], body: impl Into<Vec<u8>>) -> Self {
            let body = body.into();
            Self {
                id,
                declared_size: u32::try_from(body.len()).unwrap(),
                body,
                include_pad: true,
            }
        }

        fn sentinel(id: [u8; 4], body: impl Into<Vec<u8>>) -> Self {
            Self {
                id,
                body: body.into(),
                declared_size: u32::MAX,
                include_pad: true,
            }
        }
    }

    fn riff(chunks: &[TestChunk]) -> Vec<u8> {
        let mut bytes = Vec::from(&b"RIFF\0\0\0\0WAVE"[..]);
        if !chunks.iter().any(|chunk| chunk.id == *b"fmt ") {
            append_chunks(&mut bytes, &[fmt_chunk()]);
        }
        append_chunks(&mut bytes, chunks);
        let riff_size = u32::try_from(bytes.len() - 8).unwrap();
        bytes[4..8].copy_from_slice(&riff_size.to_le_bytes());
        bytes
    }

    fn large_wave(
        container: [u8; 4],
        table: &[([u8; 4], u64)],
        chunks: &[TestChunk],
        data_size: u64,
        sample_count: u64,
    ) -> Vec<u8> {
        assert!(matches!(&container, b"RF64" | b"BW64"));
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&container);
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        let mut body = vec![0_u8; 28];
        body[8..16].copy_from_slice(&data_size.to_le_bytes());
        body[16..24].copy_from_slice(&sample_count.to_le_bytes());
        body[24..28].copy_from_slice(&u32::try_from(table.len()).unwrap().to_le_bytes());
        for (id, size) in table {
            body.extend_from_slice(id);
            body.extend_from_slice(&size.to_le_bytes());
        }
        append_chunks(&mut bytes, &[TestChunk::regular(*b"ds64", body)]);
        if !chunks.iter().any(|chunk| chunk.id == *b"fmt ") {
            append_chunks(&mut bytes, &[fmt_chunk()]);
        }
        append_chunks(&mut bytes, chunks);
        let riff_size = u64::try_from(bytes.len() - 8).unwrap();
        bytes[20..28].copy_from_slice(&riff_size.to_le_bytes());
        bytes
    }

    fn append_chunks(bytes: &mut Vec<u8>, chunks: &[TestChunk]) {
        for chunk in chunks {
            bytes.extend_from_slice(&chunk.id);
            bytes.extend_from_slice(&chunk.declared_size.to_le_bytes());
            bytes.extend_from_slice(&chunk.body);
            if chunk.include_pad && chunk.body.len() & 1 == 1 {
                bytes.push(0);
            }
        }
    }

    fn fmt_chunk() -> TestChunk {
        TestChunk::regular(
            *b"fmt ",
            vec![1, 0, 1, 0, 0x80, 0xbb, 0, 0, 0, 0x77, 1, 0, 2, 0, 16, 0],
        )
    }

    fn data_chunk() -> TestChunk {
        TestChunk::regular(*b"data", vec![0; 4])
    }

    fn packed_20_bit_fmt_chunk() -> TestChunk {
        let mut body = Vec::new();
        body.extend_from_slice(&1_u16.to_le_bytes());
        body.extend_from_slice(&1_u16.to_le_bytes());
        body.extend_from_slice(&48_000_u32.to_le_bytes());
        body.extend_from_slice(&144_000_u32.to_le_bytes());
        body.extend_from_slice(&3_u16.to_le_bytes());
        body.extend_from_slice(&20_u16.to_le_bytes());
        TestChunk::regular(*b"fmt ", body)
    }

    #[test]
    fn retains_packed_20_bit_pcm_geometry_for_adm_metadata_qc() {
        let bytes = riff(&[
            packed_20_bit_fmt_chunk(),
            TestChunk::regular(*b"data", vec![0; 3]),
        ]);
        let fixture = Fixture::new("packed-20", &bytes);
        let input = read(&fixture.path, 1024, 1024).unwrap();
        assert_eq!(
            input.pcm,
            PcmGeometry {
                channels: 1,
                sample_rate: 48_000,
                container_bits_per_sample: 20,
                valid_bits_per_sample: 20,
                block_align: 3,
            }
        );
    }

    #[test]
    fn finds_metadata_in_all_positions_around_data() {
        let cases = [
            vec![
                fmt_chunk(),
                TestChunk::regular(*b"axml", b"<a/>".to_vec()),
                TestChunk::regular(*b"chna", b"chna".to_vec()),
                data_chunk(),
            ],
            vec![
                fmt_chunk(),
                data_chunk(),
                TestChunk::regular(*b"axml", b"<a/>".to_vec()),
                TestChunk::regular(*b"chna", b"chna".to_vec()),
            ],
            vec![
                fmt_chunk(),
                TestChunk::regular(*b"axml", b"<a/>".to_vec()),
                data_chunk(),
                TestChunk::regular(*b"chna", b"chna".to_vec()),
            ],
            vec![
                fmt_chunk(),
                TestChunk::regular(*b"chna", b"chna".to_vec()),
                data_chunk(),
                TestChunk::regular(*b"axml", b"<a/>".to_vec()),
            ],
        ];
        for (index, chunks) in cases.iter().enumerate() {
            let fixture = Fixture::new(&format!("ordering-{index}"), &riff(chunks));
            let input = read(&fixture.path, 1024, 1024).unwrap();
            assert_eq!(input.container, WaveContainerKind::Riff);
            assert_eq!(input.axml.as_deref(), Some(&b"<a/>"[..]));
            assert_eq!(input.chna.as_deref(), Some(&b"chna"[..]));
            assert_eq!(input.axml_count, 1);
            assert_eq!(input.chna_count, 1);
            assert_eq!(input.data_size, 4);
            assert_eq!(
                input.file_bytes,
                std::fs::metadata(&fixture.path).unwrap().len()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn read_from_keeps_scanning_the_open_descriptor_after_path_replacement() {
        let original = riff(&[
            TestChunk::regular(*b"axml", b"<original/>".to_vec()),
            TestChunk::regular(*b"chna", b"original-map".to_vec()),
            data_chunk(),
        ]);
        let replacement = riff(&[
            TestChunk::regular(*b"axml", b"<replacement/>".to_vec()),
            TestChunk::regular(*b"chna", b"replacement-map".to_vec()),
            TestChunk::regular(*b"data", vec![0; 8]),
        ]);
        let fixture = Fixture::new("descriptor-replacement", &original);
        let mut opened = File::open(&fixture.path).unwrap();
        opened.seek(SeekFrom::End(0)).unwrap();

        let moved_path = fixture.path.with_extension("opened");
        std::fs::rename(&fixture.path, &moved_path).unwrap();
        let moved = Fixture { path: moved_path };
        std::fs::write(&fixture.path, replacement).unwrap();

        let input = read_from(&mut opened, &fixture.path, 1024, 1024).unwrap();
        assert_eq!(input.axml.as_deref(), Some(&b"<original/>"[..]));
        assert_eq!(input.chna.as_deref(), Some(&b"original-map"[..]));
        assert_eq!(input.data_size, 4);
        assert_eq!(input.file_bytes, u64::try_from(original.len()).unwrap());

        drop(moved);
    }

    #[test]
    fn rejects_duplicate_axml_carriers() {
        let bytes = riff(&[
            TestChunk::regular(*b"axml", b"first-axml".to_vec()),
            TestChunk::regular(*b"chna", b"first-chna".to_vec()),
            data_chunk(),
            TestChunk::regular(*b"axml", b"second-axml".to_vec()),
            TestChunk::regular(*b"chna", b"second-chna".to_vec()),
        ]);
        let fixture = Fixture::new("duplicates", &bytes);
        let error = read(&fixture.path, 1024, 1024).unwrap_err();
        assert!(error.contains("contains 2 axml chunks"), "{error}");
    }

    #[test]
    fn resolves_ds64_data_and_scans_metadata_after_it() {
        let bytes = large_wave(
            *b"BW64",
            &[],
            &[
                fmt_chunk(),
                TestChunk::sentinel(*b"data", vec![0; 4]),
                TestChunk::regular(*b"axml", b"<a/>".to_vec()),
                TestChunk::regular(*b"chna", b"chna".to_vec()),
            ],
            4,
            2,
        );
        let fixture = Fixture::new("ds64-data", &bytes);
        let input = read(&fixture.path, 1024, 1024).unwrap();
        assert_eq!(input.container, WaveContainerKind::Bw64);
        assert_eq!(input.data_size, 4);
        assert_eq!(input.ds64_sample_count, None);
        assert_eq!(input.axml.as_deref(), Some(&b"<a/>"[..]));
        assert_eq!(input.chna.as_deref(), Some(&b"chna"[..]));
    }

    #[test]
    fn riff_rejects_ds64_and_sentinels_but_accepts_regular_ancillary_chunks() {
        let mut disguised = large_wave(
            *b"BW64",
            &[],
            &[TestChunk::sentinel(*b"data", vec![0; 4])],
            4,
            2,
        );
        disguised[..4].copy_from_slice(b"RIFF");
        let riff_size = u32::try_from(disguised.len() - 8).unwrap();
        disguised[4..8].copy_from_slice(&riff_size.to_le_bytes());
        let fixture = Fixture::new("riff-with-ds64", &disguised);
        assert!(read(&fixture.path, 1024, 1024).is_err());

        let regular = riff(&[TestChunk::regular(*b"JUNK", vec![0; 3]), data_chunk()]);
        let fixture = Fixture::new("riff-with-junk", &regular);
        assert_eq!(
            read(&fixture.path, 1024, 1024).unwrap().container,
            WaveContainerKind::Riff
        );
    }

    #[test]
    fn rejects_ds64_data_sizes_that_do_not_cover_the_stored_body() {
        let too_large = large_wave(
            *b"BW64",
            &[],
            &[TestChunk::sentinel(*b"data", vec![0; 4])],
            64,
            2,
        );
        let fixture = Fixture::new("ds64-data-too-large", &too_large);
        let error = read(&fixture.path, 1024, 1024).unwrap_err();
        assert!(error.contains("beyond the"), "{error}");

        let too_small = large_wave(
            *b"BW64",
            &[],
            &[
                TestChunk::sentinel(*b"data", vec![0; 4]),
                TestChunk::regular(*b"axml", b"<a/>".to_vec()),
            ],
            2,
            1,
        );
        let fixture = Fixture::new("ds64-data-too-small", &too_small);
        assert!(read(&fixture.path, 1024, 1024).is_err());
    }

    #[test]
    fn consumes_repeated_ds64_table_ids_in_fifo_order() {
        let bytes = large_wave(
            *b"RF64",
            &[(*b"ABCD", 3), (*b"ABCD", 5)],
            &[
                TestChunk::sentinel(*b"ABCD", b"one".to_vec()),
                TestChunk::sentinel(*b"ABCD", b"three".to_vec()),
                TestChunk::sentinel(*b"data", vec![0; 2]),
            ],
            2,
            1,
        );
        let fixture = Fixture::new("ds64-fifo", &bytes);
        let input = read(&fixture.path, 1024, 1024).unwrap();
        assert_eq!(input.container, WaveContainerKind::Rf64);
        assert_eq!(input.axml, None);
    }

    #[test]
    fn rejects_missing_and_unused_ds64_table_entries() {
        let missing = large_wave(
            *b"BW64",
            &[],
            &[
                TestChunk::sentinel(*b"chna", b"map".to_vec()),
                TestChunk::sentinel(*b"data", Vec::new()),
            ],
            0,
            0,
        );
        let fixture = Fixture::new("ds64-missing", &missing);
        let error = read(&fixture.path, 1024, 1024).unwrap_err();
        assert!(error.contains("no preceding ds64 table entry"), "{error}");

        let unused = large_wave(
            *b"BW64",
            &[(*b"axml", 3)],
            &[TestChunk::sentinel(*b"data", Vec::new())],
            0,
            0,
        );
        let fixture = Fixture::new("ds64-unused", &unused);
        let error = read(&fixture.path, 1024, 1024).unwrap_err();
        assert!(error.contains("unused table entry"), "{error}");
    }

    #[test]
    fn rejects_late_duplicate_and_malformed_ds64_chunks() {
        let mut late = Vec::from(&b"BW64\xff\xff\xff\xffWAVE"[..]);
        append_chunks(&mut late, &[fmt_chunk()]);
        let fixture = Fixture::new("ds64-late", &late);
        let error = read(&fixture.path, 1024, 1024).unwrap_err();
        assert!(
            error.contains("requires ds64 as its first chunk"),
            "{error}"
        );

        let mut duplicate = large_wave(
            *b"BW64",
            &[],
            &[TestChunk::sentinel(*b"data", Vec::new())],
            0,
            0,
        );
        let second_ds64 = TestChunk::regular(*b"ds64", vec![0; 28]);
        append_chunks(&mut duplicate, &[second_ds64]);
        let riff_size = u64::try_from(duplicate.len() - 8).unwrap();
        duplicate[20..28].copy_from_slice(&riff_size.to_le_bytes());
        let fixture = Fixture::new("ds64-duplicate", &duplicate);
        let error = read(&fixture.path, 1024, 1024).unwrap_err();
        assert!(error.contains("multiple ds64 chunks"), "{error}");

        let mut malformed_body = vec![0_u8; 28];
        malformed_body[24..28].copy_from_slice(&1_u32.to_le_bytes());
        let mut malformed = Vec::from(&b"BW64\xff\xff\xff\xffWAVE"[..]);
        append_chunks(
            &mut malformed,
            &[TestChunk::regular(*b"ds64", malformed_body)],
        );
        let fixture = Fixture::new("ds64-malformed", &malformed);
        let error = read(&fixture.path, 1024, 1024).unwrap_err();
        assert!(
            error.contains("tableLength requires exactly 40 bytes"),
            "{error}"
        );

        let mut short = Vec::from(&b"BW64\xff\xff\xff\xffWAVE"[..]);
        append_chunks(&mut short, &[TestChunk::regular(*b"ds64", vec![0; 27])]);
        let fixture = Fixture::new("ds64-short", &short);
        let error = read(&fixture.path, 1024, 1024).unwrap_err();
        assert!(error.contains("is truncated"), "{error}");
    }

    #[test]
    fn rejects_truncation_partial_headers_and_missing_odd_pad() {
        let mut body_overrun = Vec::from(&b"RIFF\0\0\0\0WAVEdata\x04\0\0\0\x01\x02"[..]);
        let riff_size = u32::try_from(body_overrun.len() - 8).unwrap();
        body_overrun[4..8].copy_from_slice(&riff_size.to_le_bytes());
        let fixture = Fixture::new("body-overrun", &body_overrun);
        let error = read(&fixture.path, 1024, 1024).unwrap_err();
        assert!(error.contains("beyond the"), "{error}");

        let mut partial = riff(&[data_chunk()]);
        partial.extend_from_slice(&[1, 2, 3]);
        let riff_size = u32::try_from(partial.len() - 8).unwrap();
        partial[4..8].copy_from_slice(&riff_size.to_le_bytes());
        let fixture = Fixture::new("partial-header", &partial);
        let error = read(&fixture.path, 1024, 1024).unwrap_err();
        assert!(error.contains("trailing partial chunk header"), "{error}");

        let mut odd = TestChunk::regular(*b"data", vec![1, 2, 3]);
        odd.include_pad = false;
        let fixture = Fixture::new("missing-pad", &riff(&[odd]));
        let error = read(&fixture.path, 1024, 1024).unwrap_err();
        assert!(error.contains("missing its pad byte"), "{error}");
    }

    #[test]
    fn rejects_zero_or_multiple_data_chunks_and_bad_container_lengths() {
        let fixture = Fixture::new(
            "no-data",
            &riff(&[TestChunk::regular(*b"axml", b"<a/>".to_vec())]),
        );
        let error = read(&fixture.path, 1024, 1024).unwrap_err();
        assert!(error.contains("exactly one data chunk, found 0"), "{error}");

        let fixture = Fixture::new("two-data", &riff(&[data_chunk(), data_chunk()]));
        let error = read(&fixture.path, 1024, 1024).unwrap_err();
        assert!(error.contains("exactly one data chunk, found 2"), "{error}");

        let mut bad_riff = riff(&[data_chunk()]);
        bad_riff[4..8].copy_from_slice(&12_u32.to_le_bytes());
        let fixture = Fixture::new("bad-riff-size", &bad_riff);
        let error = read(&fixture.path, 1024, 1024).unwrap_err();
        assert!(error.contains("RIFF size declares"), "{error}");

        let mut bad_bw64 = large_wave(
            *b"BW64",
            &[],
            &[TestChunk::sentinel(*b"data", Vec::new())],
            0,
            0,
        );
        bad_bw64[20..28].copy_from_slice(&12_u64.to_le_bytes());
        let fixture = Fixture::new("bad-ds64-riff-size", &bad_bw64);
        let error = read(&fixture.path, 1024, 1024).unwrap_err();
        assert!(error.contains("ds64 riffSize declares"), "{error}");
    }

    #[test]
    fn rejects_the_first_metadata_chunk_before_allocating_over_its_limit() {
        let bytes = riff(&[
            TestChunk::regular(*b"axml", vec![0; 8]),
            TestChunk::regular(*b"axml", vec![0; 1]),
            data_chunk(),
        ]);
        let fixture = Fixture::new("metadata-limit", &bytes);
        let error = read(&fixture.path, 4, 1024).unwrap_err();
        assert!(error.contains("configured limit 4"), "{error}");
    }
}
