//! Dependency-free RFC 7845 loudness metadata for existing Ogg Opus files.
//!
//! This module intentionally does not depend on libopus. Metadata-only
//! normalization must remain available when Opus encoding is not compiled in.

use crate::stable_input::identity_from_open_file;
use ogg::{PacketReader, PacketWriteEndInfo, PacketWriter};
use std::fs::{File, OpenOptions};
use std::io::BufReader;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

pub type R128Tags = (Option<i16>, Option<i16>);
const MAX_OPUS_CHAINS: usize = 1_024;

/// Build an `OpusTags` packet containing RFC 7845 loudness comments.
#[cfg(any(feature = "opus-encoding", test))]
pub(crate) fn build_opus_tags(track_lufs: f64, album_lufs: Option<f64>) -> Vec<u8> {
    let vendor = b"Forge audio normalizer";
    let mut comments = Vec::new();
    if let Some(track_gain) = r128_gain(track_lufs) {
        comments.push(format!("R128_TRACK_GAIN={track_gain}"));
    }
    if let Some(album_gain) = album_lufs.and_then(r128_gain) {
        comments.push(format!("R128_ALBUM_GAIN={album_gain}"));
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

/// Read the first logical stream's RFC 7845 track and album gains.
pub fn read_r128_tags(path: &Path) -> Result<R128Tags, String> {
    read_all_r128_tags(path)?
        .into_iter()
        .next()
        .ok_or_else(|| format!("{}: missing OpusTags", path.display()))
}

/// Rewrite every sequential Opus logical stream through a validated sibling.
///
/// The final path component must be a regular file rather than a link or
/// Windows reparse point. Publication also verifies that it still identifies
/// the file that was parsed. As with other pathname-based atomic replacement,
/// the containing directory must be trusted against a hostile rename in the
/// small interval between that verification and the final rename.
pub fn rewrite_r128_tags(
    path: &Path,
    track_lufs: f64,
    album_lufs: Option<f64>,
) -> Result<(), String> {
    rewrite_r128_tags_with_hooks(path, track_lufs, album_lufs, || Ok(()), |_| Ok(()))
}

fn rewrite_r128_tags_with_hooks<F, G>(
    path: &Path,
    track_lufs: f64,
    album_lufs: Option<f64>,
    before_source_revalidation: F,
    before_stage_revalidation: G,
) -> Result<(), String>
where
    F: FnOnce() -> Result<(), String>,
    G: FnOnce(&Path) -> Result<(), String>,
{
    let file = open_regular_opus_file(path)?;
    let source_identity = identity_from_open_file(&file, path)
        .map_err(|error| format!("identify opened Opus file {}: {error}", path.display()))?;
    let source_permissions = file
        .metadata()
        .map_err(|error| format!("inspect {} permissions: {error}", path.display()))?
        .permissions();
    let mut reader = PacketReader::new(BufReader::new(file));
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::Builder::new()
        .prefix(".forge-opus-tags-")
        .suffix(".opus")
        .tempfile_in(parent)
        .map_err(|error| format!("create temporary OpusTags file: {error}"))?;
    let mut rewritten = 0_usize;
    {
        let mut writer = PacketWriter::new(temporary.as_file_mut());
        let mut expected_tags_serial = None;
        while let Some(mut packet) = reader
            .read_packet()
            .map_err(|error| format!("{}: read Ogg packet: {error}", path.display()))?
        {
            if let Some(serial) = expected_tags_serial.take() {
                if packet.stream_serial() != serial || !packet.data.starts_with(b"OpusTags") {
                    return Err(format!(
                        "{}: missing OpusTags after OpusHead",
                        path.display()
                    ));
                }
                packet.data = replace_r128_comments(&packet.data, track_lufs, album_lufs)?;
                rewritten += 1;
                if rewritten > MAX_OPUS_CHAINS {
                    return Err(format!(
                        "{}: more than {MAX_OPUS_CHAINS} chained Opus streams",
                        path.display()
                    ));
                }
            } else if packet.first_in_stream() && packet.data.starts_with(b"OpusHead") {
                expected_tags_serial = Some(packet.stream_serial());
            }
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
        if expected_tags_serial.is_some() {
            return Err(format!("{}: missing OpusTags", path.display()));
        }
    }
    if rewritten == 0 {
        return Err(format!("{}: missing OpusTags", path.display()));
    }

    // Validate the complete replacement before it can displace the caller's
    // file. This keeps a metadata-only rewrite transactional even if an Ogg
    // writer regression produces malformed tags.
    let expected = (r128_gain(track_lufs), album_lufs.and_then(r128_gain));
    let round_trip = read_all_r128_tags(temporary.path())?;
    if round_trip.len() != rewritten || round_trip.iter().any(|tags| *tags != expected) {
        return Err(format!(
            "{}: RFC 7845 loudness metadata changed during write/read round trip",
            path.display()
        ));
    }

    // Synchronize the actual inode that will be renamed into place, rather
    // than relying on a stale handle held by an outer staging transaction.
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| format!("sync OpusTags replacement for {}: {error}", path.display()))?;

    before_source_revalidation()?;
    let current = open_regular_opus_file(path)?;
    let current_identity = identity_from_open_file(&current, path)
        .map_err(|error| format!("re-identify Opus file {}: {error}", path.display()))?;
    if current_identity != source_identity {
        return Err(format!(
            "refuse to replace {}: source path changed while OpusTags were rewritten",
            path.display()
        ));
    }

    before_stage_revalidation(temporary.path())?;

    // Bind the still-accessible private stage before applying a preserved mode
    // that may remove read access. Retain the no-follow handle until persist
    // so ACL-backed source readability is not accidentally required on the
    // newly-created replacement.
    let _bound_stage = verify_owned_opus_stage(&temporary)?;
    temporary
        .as_file()
        .set_permissions(source_permissions.clone())
        .map_err(|error| format!("preserve {} permissions: {error}", path.display()))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| format!("sync OpusTags permissions for {}: {error}", path.display()))?;

    // MoveFileEx cannot replace a read-only destination. Temporarily clear
    // only that bit through an attribute-capable handle bound to the verified
    // source identity. Restore it on failure and apply it to the published
    // handle on success. `tempfile::persist` deliberately resets its Windows
    // temporary file to FILE_ATTRIBUTE_NORMAL.
    #[cfg(windows)]
    let readonly_source_handle =
        make_readonly_source_replaceable(path, &source_identity, &source_permissions)?;

    let _persisted = match temporary.persist(path) {
        Ok(file) => file,
        Err(error) => {
            let error = format!(
                "replace {} after OpusTags update: {}",
                path.display(),
                error.error
            );
            #[cfg(windows)]
            let error = restore_readonly_after_error(
                readonly_source_handle.as_ref(),
                &source_permissions,
                path,
                error,
            );
            return Err(error);
        }
    };

    #[cfg(windows)]
    {
        _persisted
            .set_permissions(source_permissions)
            .map_err(|error| {
                format!(
                    "restore {} permissions after OpusTags update: {error}",
                    path.display()
                )
            })?;
        _persisted.sync_all().map_err(|error| {
            format!(
                "sync restored permissions after OpusTags update for {}: {error}",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn verify_owned_opus_stage(temporary: &tempfile::NamedTempFile) -> Result<File, String> {
    let path = temporary.path();
    let current = open_regular_opus_file(path)?;
    let owned_identity = identity_from_open_file(temporary.as_file(), path)
        .map_err(|error| format!("identify owned OpusTags stage {}: {error}", path.display()))?;
    let current_identity = identity_from_open_file(&current, path).map_err(|error| {
        format!(
            "identify current OpusTags stage {}: {error}",
            path.display()
        )
    })?;
    if current_identity != owned_identity {
        return Err(format!(
            "refuse to publish {}: OpusTags staging path no longer identifies the owned file",
            path.display()
        ));
    }
    Ok(current)
}

fn open_regular_opus_file(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(windows)]
    options.custom_flags(0x0020_0000);

    let file = options
        .open(path)
        .map_err(|error| format!("open Opus file {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect opened Opus file {}: {error}", path.display()))?;
    #[cfg(windows)]
    if metadata.file_attributes() & 0x0000_0400 != 0 {
        return Err(format!(
            "refuse OpusTags rewrite through reparse point {}",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!(
            "refuse OpusTags rewrite of non-regular file {}",
            path.display()
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn open_regular_opus_attribute_file(path: &Path) -> Result<File, String> {
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    let mut options = OpenOptions::new();
    options
        .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options
        .open(path)
        .map_err(|error| format!("open Opus file attributes {}: {error}", path.display()))?;
    let metadata = file.metadata().map_err(|error| {
        format!(
            "inspect opened Opus attribute handle {}: {error}",
            path.display()
        )
    })?;
    if metadata.file_attributes() & 0x0000_0400 != 0 {
        return Err(format!(
            "refuse OpusTags rewrite through reparse point {}",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!(
            "refuse OpusTags rewrite of non-regular file {}",
            path.display()
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn make_readonly_source_replaceable(
    path: &Path,
    source_identity: &crate::stable_input::StableFileIdentity,
    source_permissions: &std::fs::Permissions,
) -> Result<Option<File>, String> {
    if !source_permissions.readonly() {
        return Ok(None);
    }
    let attributes = open_regular_opus_attribute_file(path)?;
    let attribute_identity = identity_from_open_file(&attributes, path)
        .map_err(|error| format!("identify Opus attribute handle {}: {error}", path.display()))?;
    if &attribute_identity != source_identity {
        return Err(format!(
            "refuse to replace {}: source path changed before updating its read-only attribute",
            path.display()
        ));
    }
    let mut replaceable_permissions = source_permissions.clone();
    replaceable_permissions.set_readonly(false);
    attributes
        .set_permissions(replaceable_permissions)
        .map_err(|error| {
            format!(
                "temporarily make {} replaceable for OpusTags update: {error}",
                path.display()
            )
        })?;
    Ok(Some(attributes))
}

#[cfg(windows)]
fn restore_readonly_after_error(
    source_handle: Option<&File>,
    source_permissions: &std::fs::Permissions,
    path: &Path,
    primary_error: String,
) -> String {
    let Some(source_handle) = source_handle else {
        return primary_error;
    };
    match source_handle.set_permissions(source_permissions.clone()) {
        Ok(()) => primary_error,
        Err(restore_error) => format!(
            "{primary_error}; additionally failed to restore the read-only attribute for {}: {restore_error}",
            path.display()
        ),
    }
}

fn read_all_r128_tags(path: &Path) -> Result<Vec<R128Tags>, String> {
    let file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut reader = PacketReader::new(BufReader::new(file));
    let mut result = Vec::new();
    let mut expected_tags_serial = None;
    while let Some(packet) = reader
        .read_packet()
        .map_err(|error| format!("{}: read Ogg packet: {error}", path.display()))?
    {
        if let Some(serial) = expected_tags_serial.take() {
            if packet.stream_serial() != serial || !packet.data.starts_with(b"OpusTags") {
                return Err(format!(
                    "{}: missing OpusTags after OpusHead",
                    path.display()
                ));
            }
            result.push(parse_r128_comments(&packet.data)?);
            if result.len() > MAX_OPUS_CHAINS {
                return Err(format!(
                    "{}: more than {MAX_OPUS_CHAINS} chained Opus streams",
                    path.display()
                ));
            }
        } else if packet.first_in_stream() && packet.data.starts_with(b"OpusHead") {
            expected_tags_serial = Some(packet.stream_serial());
        }
    }
    if expected_tags_serial.is_some() {
        return Err(format!("{}: missing OpusTags", path.display()));
    }
    Ok(result)
}

fn r128_gain(lufs: f64) -> Option<i16> {
    if !lufs.is_finite() {
        return None;
    }
    Some(
        ((-23.0 - lufs) * 256.0)
            .round()
            .clamp(i16::MIN as f64, i16::MAX as f64) as i16,
    )
}

pub(crate) fn parse_r128_comments(data: &[u8]) -> Result<R128Tags, String> {
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
    if let Some(track_gain) = r128_gain(track_lufs) {
        comments.push(format!("R128_TRACK_GAIN={track_gain}").into_bytes());
    }
    if let Some(album_gain) = album_lufs.and_then(r128_gain) {
        comments.push(format!("R128_ALBUM_GAIN={album_gain}").into_bytes());
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufWriter;

    fn write_stream(
        writer: &mut PacketWriter<'_, BufWriter<File>>,
        serial: u32,
        track_lufs: f64,
        album_lufs: Option<f64>,
    ) {
        let mut head = b"OpusHead".to_vec();
        head.extend_from_slice(&[1, 2]);
        head.extend_from_slice(&312_u16.to_le_bytes());
        head.extend_from_slice(&48_000_u32.to_le_bytes());
        head.extend_from_slice(&0_i16.to_le_bytes());
        head.push(0);
        writer
            .write_packet(head, serial, PacketWriteEndInfo::EndPage, 0)
            .unwrap();
        writer
            .write_packet(
                build_opus_tags(track_lufs, album_lufs),
                serial,
                PacketWriteEndInfo::EndPage,
                0,
            )
            .unwrap();
        writer
            .write_packet(
                vec![0xF8, 0xFF, 0xFE],
                serial,
                PacketWriteEndInfo::EndStream,
                960,
            )
            .unwrap();
    }

    #[test]
    fn rewrites_and_verifies_every_chained_stream_without_libopus() {
        let temporary = tempfile::NamedTempFile::new().unwrap();
        {
            let file = temporary.reopen().unwrap();
            let mut writer = PacketWriter::new(BufWriter::new(file));
            write_stream(&mut writer, 11, -18.0, None);
            write_stream(&mut writer, 22, -19.0, Some(-20.0));
        }

        rewrite_r128_tags(temporary.path(), -16.0, Some(-18.0)).unwrap();
        assert_eq!(
            read_r128_tags(temporary.path()).unwrap(),
            (Some(-1792), Some(-1280))
        );
        assert_eq!(
            read_all_r128_tags(temporary.path()).unwrap(),
            vec![(Some(-1792), Some(-1280)); 2]
        );
    }

    #[test]
    fn rejects_ogg_streams_without_opus_tags() {
        let temporary = tempfile::NamedTempFile::new().unwrap();
        {
            let file = temporary.reopen().unwrap();
            let mut writer = PacketWriter::new(BufWriter::new(file));
            writer
                .write_packet(
                    b"OpusHead-invalid".to_vec(),
                    33,
                    PacketWriteEndInfo::EndPage,
                    0,
                )
                .unwrap();
            writer
                .write_packet(b"not-tags".to_vec(), 33, PacketWriteEndInfo::EndStream, 0)
                .unwrap();
        }
        let before = std::fs::read(temporary.path()).unwrap();
        let error = rewrite_r128_tags(temporary.path(), -18.0, None).unwrap_err();
        assert!(error.contains("missing OpusTags"));
        assert_eq!(std::fs::read(temporary.path()).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn rewrite_preserves_unix_access_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let temporary = tempfile::NamedTempFile::new().unwrap();
        {
            let file = temporary.reopen().unwrap();
            let mut writer = PacketWriter::new(BufWriter::new(file));
            write_stream(&mut writer, 55, -18.0, None);
        }
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o640)).unwrap();

        rewrite_r128_tags(temporary.path(), -16.0, None).unwrap();

        assert_eq!(
            std::fs::metadata(temporary.path()).unwrap().mode() & 0o777,
            0o640
        );
    }

    #[cfg(unix)]
    #[test]
    fn rewrite_rejects_source_path_replacement_before_publish() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.opus");
        let original = directory.path().join("original.opus");
        let replacement = directory.path().join("replacement.opus");
        {
            let file = File::create(&path).unwrap();
            let mut writer = PacketWriter::new(BufWriter::new(file));
            write_stream(&mut writer, 66, -18.0, None);
        }
        {
            let file = File::create(&replacement).unwrap();
            let mut writer = PacketWriter::new(BufWriter::new(file));
            write_stream(&mut writer, 77, -20.0, Some(-21.0));
        }
        let original_bytes = std::fs::read(&path).unwrap();
        let replacement_bytes = std::fs::read(&replacement).unwrap();

        let error = rewrite_r128_tags_with_hooks(
            &path,
            -16.0,
            None,
            || {
                std::fs::rename(&path, &original).map_err(|error| error.to_string())?;
                std::fs::rename(&replacement, &path).map_err(|error| error.to_string())?;
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap_err();

        assert!(error.contains("source path changed"), "{error}");
        assert_eq!(std::fs::read(&original).unwrap(), original_bytes);
        assert_eq!(std::fs::read(&path).unwrap(), replacement_bytes);
    }

    #[test]
    fn rewrite_rejects_replaced_private_stage_before_publish() {
        use std::io::Write as _;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.opus");
        {
            let file = File::create(&path).unwrap();
            let mut writer = PacketWriter::new(BufWriter::new(file));
            write_stream(&mut writer, 78, -18.0, None);
        }
        let source_bytes = std::fs::read(&path).unwrap();

        let error = rewrite_r128_tags_with_hooks(
            &path,
            -16.0,
            None,
            || Ok(()),
            |stage_path| {
                let mut replacement = tempfile::NamedTempFile::new_in(stage_path.parent().unwrap())
                    .map_err(|error| error.to_string())?;
                replacement
                    .write_all(b"swapped private stage")
                    .map_err(|error| error.to_string())?;
                replacement
                    .persist(stage_path)
                    .map_err(|error| error.error.to_string())?;
                Ok(())
            },
        )
        .unwrap_err();

        assert!(
            error.contains("staging path no longer identifies the owned file"),
            "{error}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), source_bytes);
    }

    #[cfg(windows)]
    #[test]
    fn rewrite_preserves_windows_readonly_attribute() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("readonly.opus");
        {
            let file = File::create(&path).unwrap();
            let mut writer = PacketWriter::new(BufWriter::new(file));
            write_stream(&mut writer, 88, -18.0, None);
        }
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions).unwrap();

        rewrite_r128_tags(&path, -16.0, None).unwrap();

        assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
        let mut cleanup_permissions = std::fs::metadata(&path).unwrap().permissions();
        cleanup_permissions.set_readonly(false);
        std::fs::set_permissions(&path, cleanup_permissions).unwrap();
    }

    #[test]
    fn omits_undefined_track_and_album_loudness() {
        let tags = build_opus_tags(f64::NEG_INFINITY, Some(f64::NAN));
        assert_eq!(parse_r128_comments(&tags).unwrap(), (None, None));

        let temporary = tempfile::NamedTempFile::new().unwrap();
        {
            let file = temporary.reopen().unwrap();
            let mut writer = PacketWriter::new(BufWriter::new(file));
            write_stream(&mut writer, 44, -18.0, Some(-20.0));
        }
        rewrite_r128_tags(temporary.path(), f64::NEG_INFINITY, Some(f64::INFINITY)).unwrap();
        assert_eq!(read_r128_tags(temporary.path()).unwrap(), (None, None));
    }
}
