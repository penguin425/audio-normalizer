use forge_normalizer::wav::{
    default_channel_roles, AudioBuffer, PcmKind, WavContainer, WavWriter, WaveChunk,
};
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

#[test]
fn help_describes_scope_and_bounded_options() {
    let output = command().arg("--help").output().unwrap();
    assert!(output.status.success(), "{output:#?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("ITU-R BS.2168"), "{stdout}");
    assert!(stdout.contains("sections 2-3"), "{stdout}");
    assert!(stdout.contains("not a certification"), "{stdout}");
    for option in [
        "--level",
        "--output",
        "--max-axml-bytes",
        "--max-chna-bytes",
        "--max-xml-nodes",
        "--max-xml-depth",
        "--max-attributes-per-element",
        "--max-xml-text-bytes",
        "--max-report-items",
        "--max-evidence-items",
        "--overwrite",
        "--compact",
    ] {
        assert!(stdout.contains(option), "missing {option} in:\n{stdout}");
    }
}

#[test]
fn rejects_an_invalid_level_with_operational_exit_code() {
    let work = tempfile::tempdir().unwrap();
    let output = command()
        .arg(work.path().join("missing.bw64"))
        .args(["--level", "3", "--output"])
        .arg(work.path().join("report.json"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid value '3'"));
}

#[test]
fn rejects_an_invalid_resource_limit_without_writing_a_report() {
    let work = tempfile::tempdir().unwrap();
    let report = work.path().join("report.json");
    let output = command()
        .arg(work.path().join("missing.bw64"))
        .args(["--level", "1", "--output"])
        .arg(&report)
        .args(["--max-axml-bytes", "0"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("max_axml_bytes must be between 1"));
    assert!(!report.exists());
}

#[test]
fn refuses_to_replace_a_report_before_reading_the_input() {
    let work = tempfile::tempdir().unwrap();
    let report = work.path().join("report.json");
    std::fs::write(&report, b"keep me").unwrap();
    let output = command()
        .arg(work.path().join("missing.bw64"))
        .args(["--level", "1", "--output"])
        .arg(&report)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("pass --overwrite"));
    assert_eq!(std::fs::read(&report).unwrap(), b"keep me");
}

#[test]
fn accepts_a_minimal_mono_level_one_delivery_and_validates_its_schema() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("mono.bw64");
    let report = work.path().join("report.json");
    write_adm(&input, &valid_axml(1), valid_chna());

    let output = run_audit(&input, 1, &report, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let instance = read_report(&report);
    assert_eq!(instance["passed"], true);
    assert_eq!(instance["profile_level"], 1);
    assert_eq!(instance["rendered_audio_verified"], false);
    assert_eq!(instance["counts"]["programmes"], 1);
    assert_eq!(instance["counts"]["track_uids"], 1);
    assert_eq!(instance["input_sha256"].as_str().unwrap().len(), 64);
    assert_eq!(instance["wave_container"], "BW64");
    assert_eq!(instance["axml_chunks"], 1);
    assert_eq!(instance["chna_chunks"], 1);
    assert_eq!(instance["data_bytes"], 1_440);
    assert_eq!(instance["ds64_sample_count"], Value::Null);
    assert_eq!(&std::fs::read(&input).unwrap()[36..44], &[0; 8]);
    validate_schema(&instance);
}

#[test]
fn accepts_metadata_before_data_and_rejects_duplicate_adm_chunks() {
    for duplicate in [*b"axml", *b"chna"] {
        let work = tempfile::tempdir().unwrap();
        let input = work.path().join("duplicate.bw64");
        let report = work.path().join("report.json");
        let axml = valid_axml(1).into_bytes();
        let chna = valid_chna();
        let mut chunks = vec![
            WaveChunk {
                id: *b"axml",
                body: axml.clone(),
            },
            WaveChunk {
                id: *b"chna",
                body: chna.clone(),
            },
        ];
        chunks.push(if duplicate == *b"axml" {
            WaveChunk {
                id: *b"axml",
                body: axml,
            }
        } else {
            WaveChunk {
                id: *b"chna",
                body: chna,
            }
        });
        write_chunks(&input, &chunks);

        let output = run_audit(&input, 1, &report, &[]);
        if duplicate == *b"axml" {
            assert_eq!(output.status.code(), Some(2));
            assert!(String::from_utf8_lossy(&output.stderr).contains("carrier must be unique"));
            assert!(!report.exists());
        } else {
            assert_eq!(output.status.code(), Some(1));
            let instance = read_report(&report);
            assert_failed_rule(&instance, "BS2088-8-9-CHNA-CARRIER");
            validate_schema(&instance);
        }
    }
}

#[test]
fn validates_container_specific_ds64_fields_and_riff_size() {
    let work = tempfile::tempdir().unwrap();
    let valid = work.path().join("valid.bw64");
    write_adm(&valid, &valid_axml(1), valid_chna());

    let reserved_sample_count = work.path().join("reserved-sample-count.bw64");
    let mut bytes = std::fs::read(&valid).unwrap();
    bytes[36..44].copy_from_slice(&481_u64.to_le_bytes());
    std::fs::write(&reserved_sample_count, &bytes).unwrap();
    let reserved_report = work.path().join("reserved-report.json");
    let output = run_audit(&reserved_sample_count, 1, &reserved_report, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let instance = read_report(&reserved_report);
    assert_eq!(instance["wave_container"], "BW64");
    assert_eq!(instance["ds64_sample_count"], Value::Null);
    validate_schema(&instance);

    let bad_sample_count = work.path().join("bad-sample-count.rf64");
    let mut bytes = std::fs::read(&valid).unwrap();
    bytes[0..4].copy_from_slice(b"RF64");
    bytes[36..44].copy_from_slice(&481_u64.to_le_bytes());
    std::fs::write(&bad_sample_count, &bytes).unwrap();
    let sample_report = work.path().join("sample-report.json");
    let output = run_audit(&bad_sample_count, 1, &sample_report, &[]);
    assert_eq!(output.status.code(), Some(1));
    let instance = read_report(&sample_report);
    assert_eq!(instance["wave_container"], "RF64");
    assert_eq!(instance["ds64_sample_count"], 481);
    assert_failed_rule(&instance, "BS2168-2.1.9-TRACK-UID");
    let carrier_rule = instance["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|rule| rule["rule_id"] == "BS2088-8-9-CHNA-CARRIER")
        .unwrap();
    assert_eq!(carrier_rule["authority"], "ITU-R BS.2088-2");
    assert_eq!(carrier_rule["section"], "§§ 8–9");
    assert_eq!(
        carrier_rule["requirement"],
        "a structurally valid chna carrier is present when required"
    );
    validate_schema(&instance);

    let bad_riff_size = work.path().join("bad-riff-size.bw64");
    let mut bytes = std::fs::read(&valid).unwrap();
    bytes[20..28].copy_from_slice(&1_u64.to_le_bytes());
    std::fs::write(&bad_riff_size, &bytes).unwrap();
    let size_report = work.path().join("size-report.json");
    let output = run_audit(&bad_riff_size, 1, &size_report, &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("ds64 riffSize"));
    assert!(!size_report.exists());
}

#[test]
fn rejects_nonzero_riff_padding_and_invalid_fmt_layouts() {
    let cases = [
        ("missing-fmt", "exactly one fmt"),
        ("duplicate-fmt", "exactly one fmt"),
        ("fmt-after-data", "appears after the data"),
        ("bad-block-align", "does not match PCM"),
        ("bad-byte-rate", "does not match PCM"),
        ("nonzero-pad", "non-zero pad byte"),
    ];
    for (name, expected) in cases {
        let work = tempfile::tempdir().unwrap();
        let input = work.path().join(format!("{name}.bw64"));
        let report = work.path().join("report.json");
        if name == "nonzero-pad" {
            write_chunks(
                &input,
                &[
                    WaveChunk {
                        id: *b"JUNK",
                        body: vec![1],
                    },
                    WaveChunk {
                        id: *b"axml",
                        body: valid_axml(1).into_bytes(),
                    },
                    WaveChunk {
                        id: *b"chna",
                        body: valid_chna(),
                    },
                ],
            );
        } else {
            write_adm(&input, &valid_axml(1), valid_chna());
        }
        let mut bytes = std::fs::read(&input).unwrap();
        let fmt = find_chunk(&bytes, b"fmt ").unwrap();
        match name {
            "missing-fmt" => bytes[fmt..fmt + 4].copy_from_slice(b"JUNK"),
            "duplicate-fmt" => {
                let axml = find_chunk(&bytes, b"axml").unwrap();
                bytes[axml..axml + 4].copy_from_slice(b"fmt ");
            }
            "fmt-after-data" => {
                let body = bytes[fmt + 8..fmt + 24].to_vec();
                bytes.extend_from_slice(b"fmt ");
                bytes.extend_from_slice(&16_u32.to_le_bytes());
                bytes.extend_from_slice(&body);
                let riff_size = u64::try_from(bytes.len() - 8).unwrap();
                bytes[20..28].copy_from_slice(&riff_size.to_le_bytes());
            }
            "bad-block-align" => bytes[fmt + 20..fmt + 22].copy_from_slice(&4_u16.to_le_bytes()),
            "bad-byte-rate" => bytes[fmt + 16..fmt + 20].copy_from_slice(&1_u32.to_le_bytes()),
            "nonzero-pad" => {
                bytes.drain(12..48);
                bytes[0..4].copy_from_slice(b"RIFF");
                let riff_size = u32::try_from(bytes.len() - 8).unwrap();
                bytes[4..8].copy_from_slice(&riff_size.to_le_bytes());
                let junk = find_chunk(&bytes, b"JUNK").unwrap();
                bytes[junk + 9] = 1;
            }
            _ => unreachable!(),
        }
        std::fs::write(&input, bytes).unwrap();
        let output = run_audit(&input, 1, &report, &[]);
        assert_eq!(output.status.code(), Some(2), "{name}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!report.exists());
    }
}

#[test]
fn extensible_pcm_requires_consistent_cbsize_valid_bits_and_exact_guid() {
    for (name, valid_bits, mutation, expected_exit) in [
        ("valid-extensible", 20_u16, None, 0),
        ("cbsize-zero", 20, Some((16_usize, 0_u8)), 2),
        ("bogus-guid", 20, Some((26, 1)), 2),
        ("zero-valid-bits", 0, None, 2),
        ("excess-valid-bits", 25, None, 2),
    ] {
        let work = tempfile::tempdir().unwrap();
        let input = work.path().join(format!("{name}.bw64"));
        let report = work.path().join("report.json");
        let axml = valid_axml(1).replace("bitDepth=\"24\"", "bitDepth=\"20\"");
        write_adm(&input, &axml, valid_chna());
        convert_fmt_to_extensible_pcm(&input, valid_bits, mutation);

        let output = run_audit(&input, 1, &report, &[]);
        assert_eq!(
            output.status.code(),
            Some(expected_exit),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if expected_exit == 0 {
            validate_schema(&read_report(&report));
        } else {
            assert!(!report.exists());
        }
    }
}

#[test]
fn legacy_packed_20_bit_pcm_is_a_supported_adm_essence_geometry() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("legacy-20-bit.bw64");
    let report = work.path().join("report.json");
    let axml = valid_axml(1).replace("bitDepth=\"24\"", "bitDepth=\"20\"");
    write_adm(&input, &axml, valid_chna());
    let mut bytes = std::fs::read(&input).unwrap();
    let fmt = find_chunk(&bytes, b"fmt ").unwrap();
    bytes[fmt + 22..fmt + 24].copy_from_slice(&20_u16.to_le_bytes());
    std::fs::write(&input, bytes).unwrap();

    let output = run_audit(&input, 1, &report, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let instance = read_report(&report);
    assert_eq!(instance["passed"], true);
    validate_schema(&instance);
}

#[test]
fn legacy_pcm_fmt_allows_only_size16_or_zero_cbsize18() {
    for (name, extension, expected_exit) in [
        ("zero-cbsize", vec![0, 0], 0),
        ("nonzero-cbsize", vec![1, 0], 2),
        ("trailing-garbage", vec![0; 24], 2),
    ] {
        let work = tempfile::tempdir().unwrap();
        let input = work.path().join(format!("{name}.bw64"));
        let report = work.path().join("report.json");
        write_adm(&input, &valid_axml(1), valid_chna());
        extend_legacy_fmt(&input, &extension);
        let output = run_audit(&input, 1, &report, &[]);
        assert_eq!(
            output.status.code(),
            Some(expected_exit),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if expected_exit == 0 {
            validate_schema(&read_report(&report));
        } else {
            assert!(!report.exists());
        }
    }
}

#[test]
fn reports_a_level_one_count_boundary_failure_with_exit_one() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("too-many-programmes.bw64");
    let report = work.path().join("report.json");
    let mut axml = valid_axml(1);
    let extra = (2..=9)
        .map(|counter| {
            format!(
                r#"<audioProgramme audioProgrammeID="APR_{counter:04X}" audioProgrammeName="Programme {counter}" audioProgrammeLanguage="eng"><audioContentIDRef>ACO_1001</audioContentIDRef><loudnessMetadata><integratedLoudness>-23</integratedLoudness></loudnessMetadata></audioProgramme>"#
            )
        })
        .collect::<String>();
    axml = axml.replace(
        "</audioFormatExtended>",
        &format!("{extra}</audioFormatExtended>"),
    );
    write_adm(&input, &axml, valid_chna());

    let output = run_audit(&input, 1, &report, &[]);
    assert_eq!(output.status.code(), Some(1));
    let instance = read_report(&report);
    assert_eq!(instance["passed"], false);
    assert_eq!(instance["counts"]["programmes"], 9);
    assert_failed_rule(&instance, "BS2168-2.3-LIMITS");
    validate_schema(&instance);
}

#[test]
fn reports_the_programme_alternative_value_set_level_boundary() {
    let definitions = (1..=17)
        .map(|counter| {
            format!(
                r#"<alternativeValueSet alternativeValueSetID="AVS_1001_{counter:04X}"><gain>0</gain></alternativeValueSet>"#
            )
        })
        .collect::<String>();
    let references = (1..=17)
        .map(|counter| {
            format!("<alternativeValueSetIDRef>AVS_1001_{counter:04X}</alternativeValueSetIDRef>")
        })
        .collect::<String>();
    let axml = valid_axml(1)
        .replace("</audioObject>", &format!("{definitions}</audioObject>"))
        .replace(
            "</audioProgramme>",
            &format!("{references}</audioProgramme>"),
        );
    let instance = audit_failure("programme-avs-boundary", &axml, valid_chna());
    assert_failed_rule(&instance, "BS2168-2.3-LIMITS");
    let evidence = failed_rule_evidence(&instance, "BS2168-2.3-LIMITS");
    assert!(evidence.iter().any(|item| item["observed"]
        .as_str()
        .is_some_and(|value| value.contains("alternativeValueSetIDRef"))));
    validate_schema(&instance);
}

#[test]
fn reports_a_requested_profile_level_mismatch() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("level-mismatch.bw64");
    let report = work.path().join("report.json");
    write_adm(&input, &valid_axml(1), valid_chna());

    let output = run_audit(&input, 2, &report, &[]);
    assert_eq!(output.status.code(), Some(1));
    let instance = read_report(&report);
    assert_eq!(instance["profile_level"], 2);
    assert_failed_rule(&instance, "BS2168-2.1.10-PROFILE");
    validate_schema(&instance);
}

#[test]
fn reports_chna_and_pcm_mapping_mismatch() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("chna-mismatch.bw64");
    let report = work.path().join("report.json");
    let mismatched = mono_chna("ATU_00000002", "", "AP_00031001");
    write_adm(&input, &valid_axml(1), mismatched);

    let output = run_audit(&input, 1, &report, &[]);
    assert_eq!(output.status.code(), Some(1));
    let instance = read_report(&report);
    assert_failed_rule(&instance, "BS2168-2.1.9-TRACK-UID");
    validate_schema(&instance);
}

#[test]
fn reports_a_truncated_chna_and_keeps_the_failure_report_schema_valid() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("empty-chna.bw64");
    let report = work.path().join("report.json");
    write_adm(&input, &valid_axml(1), Vec::new());

    let output = run_audit(&input, 1, &report, &[]);
    assert_eq!(output.status.code(), Some(1));
    let instance = read_report(&report);
    assert_eq!(instance["chna_bytes"], 0);
    assert_failed_rule(&instance, "BS2088-8-9-CHNA-CARRIER");
    validate_schema(&instance);
}

#[test]
fn missing_adm_chunks_produce_schema_valid_conformance_reports() {
    for missing in [*b"axml", *b"chna"] {
        let work = tempfile::tempdir().unwrap();
        let input = work.path().join(format!(
            "missing-{}.bw64",
            String::from_utf8_lossy(&missing)
        ));
        let report = work.path().join("report.json");
        let axml = valid_axml(1).into_bytes();
        let chna = valid_chna();
        let chunks = [
            WaveChunk {
                id: *b"axml",
                body: axml,
            },
            WaveChunk {
                id: *b"chna",
                body: chna,
            },
        ];
        let retained = chunks
            .into_iter()
            .filter(|chunk| chunk.id != missing)
            .collect::<Vec<_>>();
        write_chunks(&input, &retained);

        let output = run_audit(&input, 1, &report, &[]);
        assert_eq!(output.status.code(), Some(1));
        let instance = read_report(&report);
        assert_eq!(instance["passed"], false);
        assert_eq!(instance["axml_bytes"] == 0, missing == *b"axml");
        assert_eq!(instance["chna_bytes"] == 0, missing == *b"chna");
        validate_schema(&instance);
    }
}

#[test]
fn bxml_is_an_unsupported_carrier_not_a_bs2168_failure_report() {
    for include_axml in [false, true] {
        let work = tempfile::tempdir().unwrap();
        let input = work.path().join("bxml.bw64");
        let report = work.path().join("report.json");
        let mut chunks = vec![WaveChunk {
            id: *b"bxml",
            body: vec![0; 16],
        }];
        if include_axml {
            chunks.push(WaveChunk {
                id: *b"axml",
                body: valid_axml(1).into_bytes(),
            });
        }
        chunks.push(WaveChunk {
            id: *b"chna",
            body: valid_chna(),
        });
        write_chunks(&input, &chunks);
        let output = run_audit(&input, 1, &report, &[]);
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported"));
        assert!(!report.exists());
    }
}

#[test]
fn rejects_noncanonical_or_malformed_pcm_chna_fields() {
    let mut non_ascii = valid_chna();
    non_ascii[18] = 0xff;
    let mut embedded_nul_garbage = valid_chna();
    embedded_nul_garbage[18..32].fill(0);
    embedded_nul_garbage[18..25].copy_from_slice(b"AC_0003");
    embedded_nul_garbage[26..32].copy_from_slice(b"garbag");
    let mut nonzero_pad = valid_chna();
    nonzero_pad[43] = 1;

    for (name, chna) in [
        (
            "empty-channel-ref",
            mono_chna("ATU_00000001", "", "AP_00031001"),
        ),
        (
            "non-pcm-channel-ref",
            mono_chna("ATU_00000001", "AC_00031001_01", "AP_00031001"),
        ),
        ("non-ascii-channel-ref", non_ascii),
        ("embedded-nul-garbage", embedded_nul_garbage),
        ("nonzero-pad", nonzero_pad),
    ] {
        let instance = audit_failure(name, &valid_axml(1), chna);
        assert!(
            rule_failed(&instance, "BS2088-8-9-CHNA-CARRIER")
                || rule_failed(&instance, "BS2168-2.1.9-TRACK-UID"),
            "{name}: expected a CHNA carrier or TrackUID reconciliation failure in {instance:#}"
        );
        validate_schema(&instance);
    }
}

#[test]
fn accepts_lowercase_hex_digits_but_rejects_lowercase_fixed_prefixes() {
    let lowercase_hex = valid_axml(1)
        .replace("ACO_1001", "ACO_10a1")
        .replace("AO_1001", "AO_10a1");
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("lowercase-hex.bw64");
    let report = work.path().join("report.json");
    write_adm(&input, &lowercase_hex, valid_chna());
    let output = run_audit(&input, 1, &report, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    validate_schema(&read_report(&report));

    let lowercase_prefix = valid_axml(1).replace("AO_1001", "ao_1001");
    let instance = audit_failure("lowercase-prefix", &lowercase_prefix, valid_chna());
    assert_failed_rule(&instance, "BS2168-2.2-IDS");
}

#[test]
fn rejects_invalid_structure_names_mixed_text_profile_children_and_language_codes() {
    let cases = [
        (
            "block-prefix",
            valid_axml(1).replace("AB_00031001_00000001", "ZZ_00031001_00000001"),
            "BS2168-2.2-IDS",
        ),
        (
            "mixed-container-text",
            valid_axml(1).replace("<profileList>", "unexpected text<profileList>"),
            "BS2168-2.1.1-STRUCTURE",
        ),
        (
            "nested-profile-child",
            valid_axml(1).replace(
                "ITU-R BS.2168</profile>",
                "ITU-R BS.2168<unexpected/></profile>",
            ),
            "BS2168-2.1.1-STRUCTURE",
        ),
        (
            "unregistered-language",
            valid_axml(1).replace(
                "audioProgrammeLanguage=\"eng\"",
                "audioProgrammeLanguage=\"zzz\"",
            ),
            "BS2168-2.1.1-STRUCTURE",
        ),
    ];
    for (name, axml, rule) in cases {
        let instance = audit_failure(name, &axml, valid_chna());
        assert_failed_rule(&instance, rule);
        validate_schema(&instance);
    }
}

#[test]
fn rejects_noncanonical_numbers_nonfinite_values_and_invalid_booleans() {
    for (name, value) in [("leading-zero", "01"), ("exponent", "1e0"), ("nan", "NaN")] {
        let axml = valid_axml(1).replace(
            "</audioObject>",
            &format!("<gain>{value}</gain></audioObject>"),
        );
        let instance = audit_failure(name, &axml, valid_chna());
        assert_failed_rule(&instance, "BS2168-2.1.5-INTERACTIVITY");
        validate_schema(&instance);
    }

    let invalid_boolean = valid_axml(1)
        .replace("interact=\"0\"", "interact=\"1\"")
        .replace(
            "</audioObject>",
            "<audioObjectInteraction onOffInteract=\"false\"/></audioObject>",
        );
    let instance = audit_failure("invalid-boolean", &invalid_boolean, valid_chna());
    assert_failed_rule(&instance, "BS2168-2.1.5-INTERACTIVITY");
    validate_schema(&instance);
}

#[test]
fn rejects_gain_position_offset_ranges_and_ineligible_coordinate_systems() {
    let cases = [
        ("gain-over-maximum", "<gain gainUnit=\"dB\">22</gain>"),
        (
            "offset-over-maximum",
            "<positionOffset coordinate=\"azimuth\">31</positionOffset>",
        ),
        (
            "cartesian-offset-on-spherical-object",
            "<positionOffset coordinate=\"X\">0.5</positionOffset>",
        ),
    ];
    for (name, metadata) in cases {
        let axml = valid_axml(1).replace("</audioObject>", &format!("{metadata}</audioObject>"));
        let instance = audit_failure(name, &axml, valid_chna());
        assert_failed_rule(&instance, "BS2168-2.1.5-INTERACTIVITY");
        validate_schema(&instance);
    }
}

#[test]
fn rejects_invented_or_mismatched_common_direct_speaker_references() {
    for (name, pack, channel) in [
        ("invented-common-channel", "AP_00010001", "AC_00019999"),
        ("mismatched-common-layout", "AP_00010002", "AC_00010001"),
    ] {
        let axml = common_direct_speakers_axml(pack, channel);
        let chna_channel = format!("{channel}_00");
        let instance = audit_failure(name, &axml, mono_chna("ATU_00000001", &chna_channel, pack));
        assert_eq!(instance["passed"], false);
        validate_schema(&instance);
    }
}

#[test]
fn rejects_locally_defined_matrix_as_an_object_and_track_source() {
    let axml = local_matrix_axml("AP_00010001", "AC_00010001", "AC_00010001");
    let instance = audit_failure(
        "matrix-object-source",
        &axml,
        mono_chna("ATU_00000001", "AC_00021001_00", "AP_00021001"),
    );
    assert_failed_rule(&instance, "BS2168-2.1.6-PACKS");
    validate_schema(&instance);
}

#[test]
fn rejects_matrix_coefficient_output_mismatch_and_unreferenced_input_layout() {
    for (name, input_pack, coefficient, output_channel) in [
        (
            "matrix-coefficient-output-mismatch",
            "AP_00010001",
            "AC_00010002",
            "AC_00010001",
        ),
        (
            "matrix-input-not-used",
            "AP_00010002",
            "AC_00010001",
            "AC_00010001",
        ),
    ] {
        let axml = local_matrix_axml(input_pack, coefficient, output_channel);
        let instance = audit_failure(
            name,
            &axml,
            mono_chna("ATU_00000001", "AC_00021001_00", "AP_00021001"),
        );
        assert_eq!(instance["passed"], false);
        assert!(
            rule_failed(&instance, "BS2168-2.1.6-PACKS")
                || rule_failed(&instance, "BS2168-2.1.8-BLOCKS")
        );
        validate_schema(&instance);
    }
}

#[test]
fn complementary_group_programme_avs_coverage_and_identity_are_enforced() {
    let partial = complementary_axml(
        "<alternativeValueSetIDRef>AVS_1001_0001</alternativeValueSetIDRef>",
        "0",
    );
    let partial_report = audit_failure("partial-complementary-avs", &partial, valid_chna());
    assert_failed_rule(&partial_report, "BS2168-2.1.5-GRAPH");

    let all_refs = "<alternativeValueSetIDRef>AVS_1001_0001</alternativeValueSetIDRef><alternativeValueSetIDRef>AVS_1002_0001</alternativeValueSetIDRef>";
    let matching = complementary_axml(all_refs, "0");
    let matching_report = audit_failure("matching-complementary-avs", &matching, valid_chna());
    assert!(
        !rule_failed(&matching_report, "BS2168-2.1.5-GRAPH"),
        "matching all-member AVS metadata should satisfy the graph rule: {matching_report:#}"
    );

    let mismatched = complementary_axml(all_refs, "1");
    let mismatch_report = audit_failure("mismatched-complementary-avs", &mismatched, valid_chna());
    assert_failed_rule(&mismatch_report, "BS2168-2.1.5-GRAPH");
    for report in [partial_report, matching_report, mismatch_report] {
        validate_schema(&report);
    }
}

#[test]
fn reports_a_block_timing_gap() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("block-gap.bw64");
    let report = work.path().join("report.json");
    let axml = valid_axml(1).replace(
        r#"<audioBlockFormat audioBlockFormatID="AB_00031001_00000001" rtime="00:00:00.00000" duration="00:00:00.01000"><cartesian>0</cartesian><position coordinate="azimuth">0</position><position coordinate="elevation">0</position><position coordinate="distance">1</position></audioBlockFormat>"#,
        r#"<audioBlockFormat audioBlockFormatID="AB_00031001_00000001" rtime="00:00:00.00000" duration="00:00:00.00400"><cartesian>0</cartesian><position coordinate="azimuth">0</position><position coordinate="elevation">0</position><position coordinate="distance">1</position></audioBlockFormat><audioBlockFormat audioBlockFormatID="AB_00031001_00000002" rtime="00:00:00.00500" duration="00:00:00.00500"><cartesian>0</cartesian><position coordinate="azimuth">0</position><position coordinate="elevation">0</position><position coordinate="distance">1</position></audioBlockFormat>"#,
    );
    write_adm(&input, &axml, valid_chna());

    let output = run_audit(&input, 1, &report, &[]);
    assert_eq!(output.status.code(), Some(1));
    let instance = read_report(&report);
    assert_failed_rule(&instance, "BS2168-2.1.8-BLOCKS");
    validate_schema(&instance);
}

#[test]
fn reports_unknown_elements_and_attributes() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("unknown-metadata.bw64");
    let report = work.path().join("report.json");
    let axml = valid_axml(1)
        .replace("interact=\"0\"", "interact=\"0\" vendorFlag=\"1\"")
        .replace("</audioObject>", "<vendorThing/></audioObject>");
    write_adm(&input, &axml, valid_chna());

    let output = run_audit(&input, 1, &report, &[]);
    assert_eq!(output.status.code(), Some(1));
    let instance = read_report(&report);
    assert_failed_rule(&instance, "BS2168-2.1.1-STRUCTURE");
    let evidence = instance["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|rule| rule["rule_id"] == "BS2168-2.1.1-STRUCTURE")
        .unwrap()["evidence"]
        .as_array()
        .unwrap();
    assert!(evidence.iter().any(|item| item["observed"]
        .as_str()
        .is_some_and(|value| value.contains("vendorFlag"))));
    assert!(evidence.iter().any(|item| item["observed"]
        .as_str()
        .is_some_and(|value| value.contains("vendorThing"))));
    validate_schema(&instance);
}

#[test]
fn overwrite_replaces_an_existing_report_atomically() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("overwrite.bw64");
    let report = work.path().join("report.json");
    write_adm(&input, &valid_axml(1), valid_chna());
    std::fs::write(&report, b"old report").unwrap();

    let output = run_audit(&input, 1, &report, &["--overwrite", "--compact"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let instance = read_report(&report);
    assert_eq!(instance["passed"], true);
    validate_schema(&instance);
}

#[test]
fn rejects_an_oversized_axml_without_writing_a_report() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("bounded.bw64");
    let report = work.path().join("report.json");
    write_adm(&input, &valid_axml(1), valid_chna());

    let output = run_audit(&input, 1, &report, &["--max-axml-bytes", "1"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("configured limit 1"));
    assert!(!report.exists());
}

fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge-adm-emission-qc"))
}

fn run_audit(input: &Path, level: u8, report: &Path, extra: &[&str]) -> Output {
    let mut command = command();
    command
        .arg(input)
        .args(["--level", &level.to_string(), "--output"])
        .arg(report)
        .args(extra)
        .output()
        .unwrap()
}

fn write_adm(input: &Path, axml: &str, chna: Vec<u8>) {
    write_chunks(
        input,
        &[
            WaveChunk {
                id: *b"axml",
                body: axml.as_bytes().to_vec(),
            },
            WaveChunk {
                id: *b"chna",
                body: chna,
            },
        ],
    );
}

fn write_chunks(input: &Path, chunks: &[WaveChunk]) {
    let audio = AudioBuffer {
        sample_rate: 48_000,
        channels: 1,
        frames: 480,
        data: vec![vec![0.0; 480]],
        channel_roles: default_channel_roles(1),
        source_kind: PcmKind::S24,
    };
    WavWriter::write_with_metadata(
        input,
        &audio,
        PcmKind::S24,
        false,
        WavContainer::Bw64,
        chunks,
    )
    .unwrap();
}

fn find_chunk(bytes: &[u8], id: &[u8; 4]) -> Option<usize> {
    bytes.windows(4).position(|window| window == id)
}

fn convert_fmt_to_extensible_pcm(input: &Path, valid_bits: u16, mutation: Option<(usize, u8)>) {
    let mut bytes = std::fs::read(input).unwrap();
    let fmt = find_chunk(&bytes, b"fmt ").unwrap();
    assert_eq!(
        u32::from_le_bytes(bytes[fmt + 4..fmt + 8].try_into().unwrap()),
        16
    );
    bytes[fmt + 4..fmt + 8].copy_from_slice(&40_u32.to_le_bytes());
    bytes[fmt + 8..fmt + 10].copy_from_slice(&0xfffe_u16.to_le_bytes());
    let mut extension = Vec::with_capacity(24);
    extension.extend_from_slice(&22_u16.to_le_bytes());
    extension.extend_from_slice(&valid_bits.to_le_bytes());
    extension.extend_from_slice(&0_u32.to_le_bytes());
    extension.extend_from_slice(&[
        1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71,
    ]);
    bytes.splice(fmt + 24..fmt + 24, extension);
    if let Some((body_offset, value)) = mutation {
        bytes[fmt + 8 + body_offset] = value;
    }
    let riff_size = u64::try_from(bytes.len() - 8).unwrap();
    bytes[20..28].copy_from_slice(&riff_size.to_le_bytes());
    std::fs::write(input, bytes).unwrap();
}

fn extend_legacy_fmt(input: &Path, extension: &[u8]) {
    let mut bytes = std::fs::read(input).unwrap();
    let fmt = find_chunk(&bytes, b"fmt ").unwrap();
    let size = 16_u32 + u32::try_from(extension.len()).unwrap();
    bytes[fmt + 4..fmt + 8].copy_from_slice(&size.to_le_bytes());
    bytes.splice(fmt + 24..fmt + 24, extension.iter().copied());
    let riff_size = u64::try_from(bytes.len() - 8).unwrap();
    bytes[20..28].copy_from_slice(&riff_size.to_le_bytes());
    std::fs::write(input, bytes).unwrap();
}

fn mono_chna(uid: &str, track_format: &str, pack_format: &str) -> Vec<u8> {
    assert!(uid.len() <= 12);
    assert!(track_format.len() <= 14);
    assert!(pack_format.len() <= 11);
    let mut body = Vec::with_capacity(44);
    body.extend_from_slice(&1_u16.to_le_bytes());
    body.extend_from_slice(&1_u16.to_le_bytes());
    body.extend_from_slice(&1_u16.to_le_bytes());
    extend_fixed(&mut body, uid, 12);
    extend_fixed(&mut body, track_format, 14);
    extend_fixed(&mut body, pack_format, 11);
    body.push(0);
    body
}

fn valid_chna() -> Vec<u8> {
    mono_chna("ATU_00000001", "AC_00031001_00", "AP_00031001")
}

fn valid_axml(level: u8) -> String {
    r#"<audioFormatExtended version="ITU-R_BS.2076-3">
  <profileList><profile profileName="Advanced sound system: ADM and S-ADM profile for emission" profileVersion="1" profileLevel="LEVEL">ITU-R BS.2168</profile></profileList>
  <audioProgramme audioProgrammeID="APR_1001" audioProgrammeName="Programme" audioProgrammeLanguage="eng">
    <audioContentIDRef>ACO_1001</audioContentIDRef>
    <loudnessMetadata><integratedLoudness>-23</integratedLoudness></loudnessMetadata>
  </audioProgramme>
  <audioContent audioContentID="ACO_1001" audioContentName="Content" audioContentLanguage="eng">
    <audioObjectIDRef>AO_1001</audioObjectIDRef>
    <loudnessMetadata><integratedLoudness>-23</integratedLoudness></loudnessMetadata>
    <dialogue dialogueContentKind="1">1</dialogue>
  </audioContent>
  <audioObject audioObjectID="AO_1001" audioObjectName="Object" interact="0">
    <audioPackFormatIDRef>AP_00031001</audioPackFormatIDRef>
    <audioTrackUIDRef>ATU_00000001</audioTrackUIDRef>
  </audioObject>
  <audioPackFormat audioPackFormatID="AP_00031001" audioPackFormatName="Object pack" typeLabel="0003" typeDefinition="Objects">
    <audioChannelFormatIDRef>AC_00031001</audioChannelFormatIDRef>
  </audioPackFormat>
  <audioChannelFormat audioChannelFormatID="AC_00031001" audioChannelFormatName="Object channel" typeLabel="0003" typeDefinition="Objects">
    <audioBlockFormat audioBlockFormatID="AB_00031001_00000001" rtime="00:00:00.00000" duration="00:00:00.01000"><cartesian>0</cartesian><position coordinate="azimuth">0</position><position coordinate="elevation">0</position><position coordinate="distance">1</position></audioBlockFormat>
  </audioChannelFormat>
  <audioTrackUID UID="ATU_00000001" sampleRate="48000" bitDepth="24">
    <audioPackFormatIDRef>AP_00031001</audioPackFormatIDRef>
    <audioChannelFormatIDRef>AC_00031001</audioChannelFormatIDRef>
  </audioTrackUID>
</audioFormatExtended>"#
        .replace("LEVEL", &level.to_string())
}

fn common_direct_speakers_axml(pack: &str, channel: &str) -> String {
    format!(
        r#"<audioFormatExtended version="ITU-R_BS.2076-3">
  <profileList><profile profileName="Advanced sound system: ADM and S-ADM profile for emission" profileVersion="1" profileLevel="1">ITU-R BS.2168</profile></profileList>
  <audioProgramme audioProgrammeID="APR_1001" audioProgrammeName="P" audioProgrammeLanguage="eng"><audioContentIDRef>ACO_1001</audioContentIDRef><loudnessMetadata><integratedLoudness>-23</integratedLoudness></loudnessMetadata></audioProgramme>
  <audioContent audioContentID="ACO_1001" audioContentName="C" audioContentLanguage="eng"><audioObjectIDRef>AO_1001</audioObjectIDRef><loudnessMetadata><integratedLoudness>-23</integratedLoudness></loudnessMetadata><dialogue dialogueContentKind="1">1</dialogue></audioContent>
  <audioObject audioObjectID="AO_1001" audioObjectName="O" interact="0"><audioPackFormatIDRef>{pack}</audioPackFormatIDRef><audioTrackUIDRef>ATU_00000001</audioTrackUIDRef></audioObject>
  <audioTrackUID UID="ATU_00000001" sampleRate="48000" bitDepth="24"><audioPackFormatIDRef>{pack}</audioPackFormatIDRef><audioChannelFormatIDRef>{channel}</audioChannelFormatIDRef></audioTrackUID>
</audioFormatExtended>"#
    )
}

fn local_matrix_axml(input_pack: &str, coefficient: &str, output_channel: &str) -> String {
    format!(
        r#"<audioFormatExtended version="ITU-R_BS.2076-3">
  <profileList><profile profileName="Advanced sound system: ADM and S-ADM profile for emission" profileVersion="1" profileLevel="1">ITU-R BS.2168</profile></profileList>
  <audioProgramme audioProgrammeID="APR_1001" audioProgrammeName="P" audioProgrammeLanguage="eng"><audioContentIDRef>ACO_1001</audioContentIDRef><loudnessMetadata><integratedLoudness>-23</integratedLoudness></loudnessMetadata></audioProgramme>
  <audioContent audioContentID="ACO_1001" audioContentName="C" audioContentLanguage="eng"><audioObjectIDRef>AO_1001</audioObjectIDRef><loudnessMetadata><integratedLoudness>-23</integratedLoudness></loudnessMetadata><dialogue dialogueContentKind="1">1</dialogue></audioContent>
  <audioObject audioObjectID="AO_1001" audioObjectName="O" interact="0"><audioPackFormatIDRef>AP_00021001</audioPackFormatIDRef><audioTrackUIDRef>ATU_00000001</audioTrackUIDRef></audioObject>
  <audioPackFormat audioPackFormatID="AP_00021001" audioPackFormatName="Matrix pack" typeLabel="0002" typeDefinition="Matrix"><audioChannelFormatIDRef>AC_00021001</audioChannelFormatIDRef><inputPackFormatIDRef>{input_pack}</inputPackFormatIDRef><outputPackFormatIDRef>AP_00010002</outputPackFormatIDRef></audioPackFormat>
  <audioChannelFormat audioChannelFormatID="AC_00021001" audioChannelFormatName="Matrix channel" typeLabel="0002" typeDefinition="Matrix"><audioBlockFormat audioBlockFormatID="AB_00021001_00000001"><outputChannelFormatIDRef>{output_channel}</outputChannelFormatIDRef><matrix><coefficient>{coefficient}</coefficient></matrix></audioBlockFormat></audioChannelFormat>
  <audioTrackUID UID="ATU_00000001" sampleRate="48000" bitDepth="24"><audioPackFormatIDRef>AP_00021001</audioPackFormatIDRef><audioChannelFormatIDRef>AC_00021001</audioChannelFormatIDRef></audioTrackUID>
</audioFormatExtended>"#
    )
}

fn complementary_axml(programme_avs: &str, second_gain: &str) -> String {
    format!(
        r#"<audioFormatExtended version="ITU-R_BS.2076-3">
  <profileList><profile profileName="Advanced sound system: ADM and S-ADM profile for emission" profileVersion="1" profileLevel="1">ITU-R BS.2168</profile></profileList>
  <audioProgramme audioProgrammeID="APR_1001" audioProgrammeName="P" audioProgrammeLanguage="eng"><audioContentIDRef>ACO_1001</audioContentIDRef><audioContentIDRef>ACO_1002</audioContentIDRef><loudnessMetadata><integratedLoudness>-23</integratedLoudness></loudnessMetadata>{programme_avs}</audioProgramme>
  <audioContent audioContentID="ACO_1001" audioContentName="C1" audioContentLanguage="eng"><audioObjectIDRef>AO_1001</audioObjectIDRef><loudnessMetadata><integratedLoudness>-23</integratedLoudness></loudnessMetadata><dialogue dialogueContentKind="1">1</dialogue></audioContent>
  <audioContent audioContentID="ACO_1002" audioContentName="C2" audioContentLanguage="eng"><audioObjectIDRef>AO_1002</audioObjectIDRef><loudnessMetadata><integratedLoudness>-23</integratedLoudness></loudnessMetadata><dialogue dialogueContentKind="1">1</dialogue></audioContent>
  <audioObject audioObjectID="AO_1001" audioObjectName="O1" interact="0"><audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef><audioTrackUIDRef>ATU_00000001</audioTrackUIDRef><audioComplementaryObjectIDRef>AO_1002</audioComplementaryObjectIDRef><alternativeValueSet alternativeValueSetID="AVS_1001_0001"><gain>0</gain></alternativeValueSet></audioObject>
  <audioObject audioObjectID="AO_1002" audioObjectName="O2" interact="0"><audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef><audioTrackUIDRef>ATU_00000002</audioTrackUIDRef><alternativeValueSet alternativeValueSetID="AVS_1002_0001"><gain>{second_gain}</gain></alternativeValueSet></audioObject>
  <audioTrackUID UID="ATU_00000001" sampleRate="48000" bitDepth="24"><audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef><audioChannelFormatIDRef>AC_00010001</audioChannelFormatIDRef></audioTrackUID>
  <audioTrackUID UID="ATU_00000002" sampleRate="48000" bitDepth="24"><audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef><audioChannelFormatIDRef>AC_00010001</audioChannelFormatIDRef></audioTrackUID>
</audioFormatExtended>"#
    )
}

fn extend_fixed(output: &mut Vec<u8>, value: &str, width: usize) {
    output.extend_from_slice(value.as_bytes());
    output.resize(output.len() + width - value.len(), 0);
}

fn read_report(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn audit_failure(name: &str, axml: &str, chna: Vec<u8>) -> Value {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join(format!("{name}.bw64"));
    let report = work.path().join(format!("{name}.json"));
    write_adm(&input, axml, chna);
    let output = run_audit(&input, 1, &report, &[]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{name}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    read_report(&report)
}

fn assert_failed_rule(report: &Value, rule_id: &str) {
    assert!(
        report["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|rule| { rule["rule_id"] == rule_id && rule["passed"] == false }),
        "missing failed {rule_id} in {report:#}"
    );
}

fn rule_failed(report: &Value, rule_id: &str) -> bool {
    report["rules"]
        .as_array()
        .unwrap()
        .iter()
        .any(|rule| rule["rule_id"] == rule_id && rule["passed"] == false)
}

fn failed_rule_evidence<'a>(report: &'a Value, rule_id: &str) -> &'a [Value] {
    report["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|rule| rule["rule_id"] == rule_id && rule["passed"] == false)
        .unwrap()["evidence"]
        .as_array()
        .unwrap()
}

fn validate_schema(instance: &Value) {
    let schema: Value =
        serde_json::from_str(include_str!("../schema/adm-emission-report-v1.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors: Vec<_> = validator
        .iter_errors(instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}
