//! EBU Tech 3341 integrated-loudness conformance tests.
//!
//! The official test material is not committed to this repository. Run
//! `tools/test-ebu-conformance.sh` to download the EBU v5 test set and execute
//! this ignored test.

use forge_normalizer::normalize;
use forge_normalizer::wav::default_channel_roles;
use std::io::Write;
use std::path::{Path, PathBuf};

const CASES: &[(&str, f64)] = &[
    ("seq-3341-1-16bit.wav", -23.0),
    ("seq-3341-2-16bit.wav", -33.0),
    ("seq-3341-3-16bit-v02.wav", -23.0),
    ("seq-3341-4-16bit-v02.wav", -23.0),
    ("seq-3341-5-16bit-v02.wav", -23.0),
    ("seq-3341-6-5channels-16bit.wav", -23.0),
    ("seq-3341-6-6channels-WAVEEX-16bit.wav", -23.0),
    ("seq-3341-7_seq-3342-5-24bit.wav", -23.0),
    ("seq-3341-2011-8_seq-3342-6-24bit-v02.wav", -23.0),
];

const LRA_CASES: &[(&str, f64)] = &[
    ("seq-3342-1-16bit.wav", 10.0),
    ("seq-3342-2-16bit.wav", 5.0),
    ("seq-3342-3-16bit.wav", 20.0),
    ("seq-3342-4-16bit.wav", 15.0),
    ("seq-3341-7_seq-3342-5-24bit.wav", 5.0),
    ("seq-3341-2011-8_seq-3342-6-24bit-v02.wav", 15.0),
];

const TRUE_PEAK_CASES: &[(&str, f64)] = &[
    ("seq-3341-15-24bit.wav.wav", -6.0),
    ("seq-3341-16-24bit.wav.wav", -6.0),
    ("seq-3341-17-24bit.wav.wav", -6.0),
    ("seq-3341-18-24bit.wav.wav", -6.0),
    ("seq-3341-19-24bit.wav.wav", 3.0),
    ("seq-3341-20-24bit.wav.wav", 0.0),
    ("seq-3341-21-24bit.wav.wav", 0.0),
    ("seq-3341-22-24bit.wav.wav", 0.0),
    ("seq-3341-23-24bit.wav.wav", 0.0),
];

fn fixture(root: &Path, name: &str) -> PathBuf {
    root.join(name)
}

#[test]
#[ignore = "requires the official EBU v5 loudness test set"]
fn tech_3341_integrated_loudness_cases() {
    let root = std::env::var_os("EBU_TEST_SET")
        .map(PathBuf::from)
        .expect("EBU_TEST_SET must point to the extracted official test files");

    for &(name, expected) in CASES {
        let path = fixture(&root, name);
        let repaired = repair_known_ebu_waveex_riff_size(&path, name);
        let analysis_path = repaired.as_ref().map_or(path.as_path(), |file| file.path());
        // The official five-channel fixture predates WAVE_FORMAT_EXTENSIBLE and
        // carries no speaker mask. Supply its documented L/R/C/Ls/Rs order so
        // production decoding can remain fail-closed for ambiguous files.
        let roles = (name == "seq-3341-6-5channels-16bit.wav").then(|| default_channel_roles(5));
        let analysis = normalize::analyze_file_with_roles(analysis_path, roles.as_deref())
            .unwrap_or_else(|error| panic!("failed to analyze {}: {error}", path.display()));
        let error = (analysis.lufs - expected).abs();
        assert!(
            error <= 0.1,
            "{name}: measured {:.3} LUFS, expected {expected:.1} ±0.1 LU",
            analysis.lufs
        );
    }
}

/// The official v5 six-channel WAVEEX asset omits its 12-byte `fact` chunk
/// from RIFF.ckSize even though the chunk is physically present before
/// `data`. Keep the production reader strict, retain the archive checksum in
/// the download script, and repair only that container length in a temporary
/// test copy so this suite remains a DSP conformance test.
fn repair_known_ebu_waveex_riff_size(path: &Path, name: &str) -> Option<tempfile::NamedTempFile> {
    if name != "seq-3341-6-6channels-WAVEEX-16bit.wav" {
        return None;
    }
    let mut bytes = std::fs::read(path).expect("read official EBU WAVEEX fixture");
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[60..64], b"fact");
    assert_eq!(&bytes[72..76], b"data");
    let declared = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let actual = u32::try_from(bytes.len() - 8).expect("fixture fits RIFF");
    assert_eq!(declared.checked_add(12), Some(actual));
    bytes[4..8].copy_from_slice(&actual.to_le_bytes());

    let mut repaired = tempfile::NamedTempFile::new().expect("create repaired fixture");
    repaired
        .write_all(&bytes)
        .expect("write repaired EBU WAVEEX fixture");
    repaired.flush().expect("flush repaired EBU WAVEEX fixture");
    Some(repaired)
}

#[test]
#[ignore = "requires the official EBU v5 loudness test set"]
fn tech_3342_loudness_range_cases() {
    let root = std::env::var_os("EBU_TEST_SET")
        .map(PathBuf::from)
        .expect("EBU_TEST_SET must point to the extracted official test files");

    for &(name, expected) in LRA_CASES {
        let path = fixture(&root, name);
        let analysis = normalize::analyze_file(&path)
            .unwrap_or_else(|error| panic!("failed to analyze {}: {error}", path.display()));
        let error = (analysis.loudness_range_lu - expected).abs();
        assert!(
            error <= 1.0,
            "{name}: measured {:.3} LU, expected {expected:.1} ±1.0 LU",
            analysis.loudness_range_lu
        );
    }
}

#[test]
#[ignore = "requires the official EBU v5 loudness test set"]
fn tech_3341_file_meter_short_term_cases() {
    let root = fixture_root();
    assert_metric(&root, "seq-3341-9-24bit.wav", -23.0, |analysis| {
        analysis.max_short_term_lufs
    });
    for index in 1..=20 {
        let name = format!("seq-3341-10-{index}-24bit.wav");
        assert_metric(&root, &name, -23.0, |analysis| analysis.max_short_term_lufs);
    }
}

#[test]
#[ignore = "requires the official EBU v5 loudness test set"]
fn tech_3341_file_meter_momentary_cases() {
    let root = fixture_root();
    assert_metric(&root, "seq-3341-12-24bit.wav", -23.0, |analysis| {
        analysis.max_momentary_lufs
    });
    for index in 1..=20 {
        let suffix = if index <= 2 { ".wav" } else { ".wav.wav" };
        let name = format!("seq-3341-13-{index}-24bit{suffix}");
        assert_metric(&root, &name, -23.0, |analysis| analysis.max_momentary_lufs);
    }
}

#[test]
#[ignore = "requires the official EBU v5 loudness test set"]
fn tech_3341_true_peak_cases() {
    let root = fixture_root();
    for &(name, expected) in TRUE_PEAK_CASES {
        let path = fixture(&root, name);
        let analysis = normalize::analyze_file(&path)
            .unwrap_or_else(|error| panic!("failed to analyze {}: {error}", path.display()));
        let error = analysis.true_peak_db() - expected;
        assert!(
            (-0.4..=0.2).contains(&error),
            "{name}: measured {:.3} dBTP, expected {expected:.1} +0.2/-0.4 dB",
            analysis.true_peak_db()
        );
    }
}

fn fixture_root() -> PathBuf {
    std::env::var_os("EBU_TEST_SET")
        .map(PathBuf::from)
        .expect("EBU_TEST_SET must point to the extracted official test files")
}

fn assert_metric(
    root: &Path,
    name: &str,
    expected: f64,
    metric: impl FnOnce(&normalize::Analysis) -> f64,
) {
    let path = fixture(root, name);
    let analysis = normalize::analyze_file(&path)
        .unwrap_or_else(|error| panic!("failed to analyze {}: {error}", path.display()));
    let measured = metric(&analysis);
    assert!(
        (measured - expected).abs() <= 0.1,
        "{name}: measured {measured:.3} LUFS, expected {expected:.1} ±0.1 LU"
    );
}
