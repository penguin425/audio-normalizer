//! EBU Tech 3341 integrated-loudness conformance tests.
//!
//! The official test material is not committed to this repository. Run
//! `tools/test-ebu-conformance.sh` to download the EBU v5 test set and execute
//! this ignored test.

use forge_normalizer::normalize;
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
        let analysis = normalize::analyze_file(&path)
            .unwrap_or_else(|error| panic!("failed to analyze {}: {error}", path.display()));
        let error = (analysis.lufs - expected).abs();
        assert!(
            error <= 0.1,
            "{name}: measured {:.3} LUFS, expected {expected:.1} ±0.1 LU",
            analysis.lufs
        );
    }
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
