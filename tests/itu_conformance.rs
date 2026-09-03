//! ITU-R BS.2217-2 compliance tests for ITU-R BS.1770.
//!
//! The copyrighted official material is downloaded into the external cache by
//! `tools/test-itu-conformance.sh`; it is never committed to the repository.

use forge_normalizer::normalize;
use forge_normalizer::wav::{default_channel_roles, WavReader};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[test]
#[ignore = "requires the official ITU-R BS.2217-2 compliance material"]
fn bs2217_file_meter_cases() {
    let root = std::env::var_os("ITU_BS2217_TEST_SET")
        .map(PathBuf::from)
        .expect("ITU_BS2217_TEST_SET must point to the extracted official files");
    let mut paths: Vec<_> = fs::read_dir(&root)
        .expect("read BS.2217 fixture directory")
        .map(|entry| entry.expect("fixture entry").path())
        .filter(|path| path.extension().is_some_and(|value| value == "wav"))
        .collect();
    paths.sort();
    assert_eq!(paths.len(), 39, "the pinned BS.2217 core set is incomplete");

    for path in paths {
        assert_case(&path);
    }
}

fn assert_case(path: &Path) {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .expect("UTF-8 test filename");
    let repaired = repair_known_bs2217_riff_size(path, name);
    let analysis_path = repaired.as_ref().map_or(path, |file| file.path());
    // The pinned BS.2217 fixtures have documented mono, stereo, or
    // L/R/C/LFE/Ls/Rs order, but several are classic maskless WAVE files and
    // one mono variant uses FL rather than Forge's canonical mono mask. Supply
    // the conformance-set knowledge explicitly so production decoding can
    // remain fail-closed for arbitrary inputs.
    let channels = WavReader::probe_with_layout(analysis_path)
        .unwrap_or_else(|error| panic!("failed to probe {name}: {error}"))
        .0
        .channels;
    let roles = default_channel_roles(channels);
    let analysis = normalize::analyze_file_with_roles(analysis_path, Some(&roles))
        .unwrap_or_else(|error| panic!("failed to analyze {name}: {error}"));

    if name.contains("ChannelCheckLFE") {
        assert!(
            !analysis.lufs.is_finite(),
            "{name}: LFE-only input measured {:.3} LUFS instead of being excluded",
            analysis.lufs
        );
        return;
    }

    let expected = if name.contains("AbsGateTest") {
        -69.5
    } else if name.contains("RelGateTest") {
        -10.0
    } else if name.contains("18LKFS") {
        -18.0
    } else if name.contains("23LKFS") {
        -23.0
    } else if name.contains("24LKFS") {
        -24.0
    } else {
        panic!("no BS.2217 expectation declared for {name}");
    };
    let error = (analysis.lufs - expected).abs();
    assert!(
        error <= 0.1,
        "{name}: measured {:.3} LUFS, expected {expected:.1} ±0.1 LU",
        analysis.lufs
    );
}

/// Two pinned BS.2217-2 programme fixtures omit the 8-byte `data` chunk
/// header from RIFF.ckSize while their PCM extent itself is complete. Keep the
/// production reader strict and repair only that field in temporary copies;
/// the download script still authenticates every original archive.
fn repair_known_bs2217_riff_size(path: &Path, name: &str) -> Option<tempfile::NamedTempFile> {
    if !matches!(
        name,
        "1770-2 Conf Mono Voice+Music-24LKFS.wav" | "1770-2 Conf Stereo VinL+R-24LKFS.wav"
    ) {
        return None;
    }
    let mut bytes = fs::read(path).expect("read official BS.2217 fixture");
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[60..64], b"data");
    let data_size = u32::from_le_bytes(bytes[64..68].try_into().unwrap()) as usize;
    assert_eq!(68_usize.checked_add(data_size), Some(bytes.len()));
    let declared = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let actual = u32::try_from(bytes.len() - 8).expect("fixture fits RIFF");
    assert_eq!(declared.checked_add(8), Some(actual));
    bytes[4..8].copy_from_slice(&actual.to_le_bytes());

    let mut repaired = tempfile::NamedTempFile::new().expect("create repaired fixture");
    repaired
        .write_all(&bytes)
        .expect("write repaired BS.2217 fixture");
    repaired.flush().expect("flush repaired BS.2217 fixture");
    Some(repaired)
}
