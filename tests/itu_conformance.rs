//! ITU-R BS.2217-2 compliance tests for ITU-R BS.1770.
//!
//! The copyrighted official material is downloaded into the external cache by
//! `tools/test-itu-conformance.sh`; it is never committed to the repository.

use forge_normalizer::normalize;
use std::fs;
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
    let analysis = normalize::analyze_file(path)
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
