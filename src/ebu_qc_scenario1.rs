//! EBU QC Scenario 1 report output using pinned Catalogue API v2 terms.
//!
//! The generic writer in [`crate::ebu_qc_report`] remains available for API
//! compatibility. This module adds the stricter broadcaster pass/fail profile:
//! every published Item is matched to a pinned catalogue identity, check and
//! report modalities are kept distinct, and Item-specific Inputs and Outputs
//! replace Forge's generic `Event` vocabulary.

use crate::dsp::lufs::LoudnessTimelinePoint;
use crate::ebu_qc_report::{
    BoundedXmlBuffer, EbuQcReportMetadata, EBU_QC_REPORT_NAMESPACE, EBU_QC_TIMING_NAMESPACE,
};
use crate::ebu_qc_validation::{validate_xml, EbuQcValidationProfile};
use crate::normalize::Analysis;
use crate::qc::{QcOptions, QcResult, EBU_QC_CATALOGUE};
use crate::wav::ChannelRole;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::Write;

pub const EBU_QC_SCENARIO1_GUIDANCE: &str =
    "https://github.com/ebu/qc/blob/main/qc-reports/qc-reports-best-practice-guidance-1.md";
pub const EBU_QC_CATALOGUE_API_VERSION: &str = "v2";
pub const EBU_QC_CATALOGUE_API_NAMESPACE: &str = "tag:qc.ebu.ch,2026-01";
pub const EBU_QC_CATALOGUE_PIN_MANIFEST: &str =
    "https://penguin425.github.io/audio-normalizer/schema/ebu-qc-catalogue-v2-pins.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsedAs {
    Check,
    Report,
}

impl UsedAs {
    fn as_str(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Report => "report",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct CatalogueDefinition {
    id: &'static str,
    version: &'static str,
    name: &'static str,
    sha256: &'static str,
    used_as: UsedAs,
}

const DEFINITIONS: &[CatalogueDefinition] = &[
    definition(
        "0078B",
        "3.0",
        "Audio Silence",
        "4b3226020b33fb85e31d4bd2b777cbe958ce06c579d91dbd28d21958e0659ecb",
        UsedAs::Check,
    ),
    definition(
        "0005B",
        "2.0",
        "Audio Digital Clipping",
        "6e339ee5532b9110af0a1266e9a4104bac4bde79f0944a46e187041c2f106dd8",
        UsedAs::Check,
    ),
    definition(
        "0014B",
        "2.0",
        "Audio Test Tones",
        "a479246e9ed1e45b392c52cec821e845573ea53b58a07285efb6b52b5ab0b875",
        UsedAs::Check,
    ),
    definition(
        "0009F",
        "2.0",
        "Audio Duration",
        "e4ab05954b3dddf8e80490cb98f71118fa6a0fba8f295b636acdd5dea3282fb0",
        UsedAs::Check,
    ),
    definition(
        "0010B",
        "2.0",
        "Loudness",
        "684db2e29e69e2c3001ad48f0820fcd8b17d2bc5e781d71d54af91061428905c",
        UsedAs::Report,
    ),
    definition(
        "0084B",
        "1.0",
        "Audio Peaks (TP)",
        "dead2c1efe845f12597ce4f12548d80a8c2080abd6eac685cf2d552d4f56b96c",
        UsedAs::Report,
    ),
    definition(
        "0004F",
        "2.0",
        "Audio Channel Count",
        "ee28e7236377e7fc998ff6dab698269423ce5d0d3add620adec19012e96b106e",
        UsedAs::Check,
    ),
    definition(
        "0008B",
        "2.0",
        "Audio Dropouts",
        "7ef8e6a99480e51a5ccffa1289ab27aeede1f43ce896e02607bbeb73d90598d7",
        UsedAs::Check,
    ),
    definition(
        "0012B",
        "2.0",
        "Audio Phase Reversal",
        "6e7b642fe1987364510f90801e2448f4b3d4d27bf01b6831addb8c6037c8fe0e",
        UsedAs::Check,
    ),
    definition(
        "0057B",
        "1.0",
        "Audio Clicks",
        "e2513825b589ff6c1997946bafc9a680a87816fb641fb54d81170968b0725d25",
        UsedAs::Check,
    ),
    definition(
        "0077B",
        "1.0",
        "Average Minimum Audio Level",
        "abe17b63a4a7ba93020d2f32405b94a0fc0bffcb28302c66d7888b2e3124cd13",
        UsedAs::Check,
    ),
    definition(
        "0088B",
        "1.0",
        "Audio Hum & Buzz",
        "1f46b9b14afec5b9cfea088a2d980dda1ecaad77082b7709b8c7b49d6f4ed956",
        UsedAs::Check,
    ),
    definition(
        "0086B",
        "1.0",
        "Audio Noise",
        "25b63467499c319200e8bec3c7e9bc1a1bd8bf15dc18305e9fd6ce5e9ca4e0c6",
        UsedAs::Check,
    ),
    definition(
        "0170B",
        "1.0",
        "Cross Talk",
        "9a85a5876d970fa4bf5309f5efb8f087c7e7aa9af4ca0f13be0e37ca858cf856",
        UsedAs::Check,
    ),
    definition(
        "0230B",
        "1.0",
        "Audio Channel Panning",
        "e129b0ba2e4c66fe0f250fca0dd572d5984b0d0825dbd0d8c5b5e9783463246a",
        UsedAs::Check,
    ),
    definition(
        "0095B",
        "1.0",
        "LFE/Centre Channel Assignment",
        "f64d58e6f0a93e289edc822bea43cb286ba57fbf47c5dba77a8ff291142a36e3",
        UsedAs::Check,
    ),
    definition(
        "0124B",
        "2.0",
        "Mono Audio",
        "66d079ed3cd6f4bf7b11cf0d234c2152b7034fe42b75bac0faa28fd7e96a36ca",
        UsedAs::Check,
    ),
];

const fn definition(
    id: &'static str,
    version: &'static str,
    name: &'static str,
    sha256: &'static str,
    used_as: UsedAs,
) -> CatalogueDefinition {
    CatalogueDefinition {
        id,
        version,
        name,
        sha256,
        used_as,
    }
}

#[derive(Debug)]
struct ScenarioItem<'a> {
    definition: &'static CatalogueDefinition,
    used_as: UsedAs,
    result: &'a QcResult,
    instance_id: String,
    inputs: Vec<IoValue>,
    outputs: Vec<IoValue>,
}

#[derive(Debug, Clone)]
struct IoValue {
    name: &'static str,
    locator: Option<Locator>,
    track: Option<u16>,
    value: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct Locator {
    start_seconds: f64,
    end_seconds: f64,
}

/// Write an EBU QC Scenario 1 report using pinned Catalogue API v2 terms.
///
/// `timeline_interval_ms` is the actual interval used to produce `timeline`;
/// it is emitted as the loudness report's momentary and short-term sliding
/// interval. The CLI captures a timeline automatically whenever this writer is
/// requested.
pub fn write_xml<W: Write>(
    mut output: W,
    metadata: &EbuQcReportMetadata,
    analysis: &Analysis,
    options: &QcOptions,
    results: &[QcResult],
    timeline: &[LoudnessTimelinePoint],
    timeline_interval_ms: f64,
) -> Result<(), String> {
    let mut encoded = BoundedXmlBuffer::new();
    encode_xml(
        &mut encoded,
        metadata,
        analysis,
        options,
        results,
        timeline,
        timeline_interval_ms,
    )?;
    let encoded = encoded.into_bytes();
    validate_xml(&encoded, EbuQcValidationProfile::Scenario1)
        .map_err(|error| format!("validate generated EBU QC Scenario 1 report: {error}"))?;
    output
        .write_all(&encoded)
        .map_err(|error| format!("write EBU QC Scenario 1 XML: {error}"))
}

fn encode_xml<W: Write>(
    output: W,
    metadata: &EbuQcReportMetadata,
    analysis: &Analysis,
    options: &QcOptions,
    results: &[QcResult],
    timeline: &[LoudnessTimelinePoint],
    timeline_interval_ms: f64,
) -> Result<(), String> {
    metadata.validate()?;
    options.validate()?;
    validate_context(metadata, analysis, timeline, timeline_interval_ms)?;

    let published = results
        .iter()
        .filter(|result| {
            result.calculated
                && result.source_url.starts_with(EBU_QC_CATALOGUE)
                && valid_ebu_qc_id(&result.ebu_qc_id)
                && scenario_item_applicable(result, analysis, timeline)
        })
        .collect::<Vec<_>>();
    if published.is_empty() {
        return Err("EBU QC Scenario 1 requires at least one calculated published Item".into());
    }

    let profile_fingerprint =
        catalogue_fingerprint(&published, analysis, options, timeline_interval_ms)?;
    let profile_id = deterministic_uuid(b"forge-ebu-qc-scenario1-profile", &[&profile_fingerprint]);
    let mut seen = HashSet::with_capacity(published.len());
    let mut items = Vec::with_capacity(published.len());
    for (index, result) in published.into_iter().enumerate() {
        if !seen.insert(result.ebu_qc_id.as_str()) {
            return Err(format!(
                "EBU QC Scenario 1 contains duplicate Item {}",
                result.ebu_qc_id
            ));
        }
        let definition = catalogue_definition(result)?;
        let used_as = item_used_as(definition, options);
        let instance_id = deterministic_uuid(
            b"forge-ebu-qc-scenario1-item",
            &[
                profile_id.as_bytes(),
                definition.id.as_bytes(),
                definition.version.as_bytes(),
                index.to_string().as_bytes(),
            ],
        );
        let inputs = item_inputs(
            definition.id,
            used_as,
            analysis,
            options,
            timeline_interval_ms,
        )?;
        let outputs = item_outputs(definition.id, used_as, result, analysis, options, timeline)?;
        validate_required_io(definition.id, used_as, &inputs, &outputs)?;
        items.push(ScenarioItem {
            definition,
            used_as,
            result,
            instance_id,
            inputs,
            outputs,
        });
    }
    if !items.iter().any(|item| item.used_as == UsedAs::Check) {
        return Err("EBU QC Scenario 1 requires at least one check-mode Item".into());
    }

    let overall = items
        .iter()
        .filter(|item| item.used_as == UsedAs::Check)
        .all(|item| item.result.passed);
    let result_fingerprint = result_fingerprint(&items);
    let report_id = deterministic_uuid(
        b"forge-ebu-qc-scenario1-report",
        &[
            metadata.content_identifier.as_bytes(),
            metadata.last_modified_datetime.as_bytes(),
            profile_id.as_bytes(),
            &result_fingerprint,
        ],
    );

    let mut writer = Writer::new_with_indent(output, b' ', 2);
    xml_event(
        &mut writer,
        Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)),
    )?;
    let mut root = BytesStart::new("Report");
    root.push_attribute(("xmlns", EBU_QC_REPORT_NAMESPACE));
    xml_event(&mut writer, Event::Start(root))?;
    text_element(&mut writer, "ReportId", &report_id)?;
    text_element(&mut writer, "ExecutionStatus", "complete")?;
    text_element(&mut writer, "CheckResult", bool_text(overall))?;
    content_id(&mut writer, &metadata.content_identifier)?;
    text_element(
        &mut writer,
        "LastModifiedDateTime",
        &metadata.last_modified_datetime,
    )?;
    text_element(
        &mut writer,
        "OverallAnalysisDuration",
        &xs_duration(metadata.analysis_duration_seconds),
    )?;
    tool_information(&mut writer)?;

    start(&mut writer, "Profile")?;
    text_element(&mut writer, "ID", &profile_id)?;
    text_element(&mut writer, "Name", "Forge EBU QC Scenario 1 audio profile")?;
    content_id(&mut writer, &metadata.content_identifier)?;
    text_element(&mut writer, "CheckResultRule", "AND")?;
    text_element(
        &mut writer,
        "Description",
        &format!(
            "Scenario 1 profile using pinned EBU QC Catalogue API {} definitions; manifest {}",
            EBU_QC_CATALOGUE_API_VERSION, EBU_QC_CATALOGUE_PIN_MANIFEST
        ),
    )?;
    empty(&mut writer, "Scopes")?;
    start(&mut writer, "Items")?;
    for item in &items {
        start(&mut writer, "Item")?;
        write_identity(&mut writer, item)?;
        text_element(&mut writer, "UsedAs", item.used_as.as_str())?;
        empty(&mut writer, "Scopes")?;
        write_ios(
            &mut writer,
            "Inputs",
            "Input",
            &item.inputs,
            metadata.sample_rate,
        )?;
        end(&mut writer, "Item")?;
    }
    end(&mut writer, "Items")?;
    empty(&mut writer, "ItemDefinitions")?;
    end(&mut writer, "Profile")?;

    start(&mut writer, "ItemResults")?;
    for item in &items {
        start(&mut writer, "ItemResult")?;
        write_identity(&mut writer, item)?;
        text_element(&mut writer, "AnalysisMethodUsed", "measurement")?;
        text_element(&mut writer, "ExecutionStatus", "complete")?;
        if item.used_as == UsedAs::Check {
            text_element(&mut writer, "CheckResult", bool_text(item.result.passed))?;
        }
        text_element(&mut writer, "DetectionMethod", "automatic")?;
        write_ios(
            &mut writer,
            "Outputs",
            "Output",
            &item.outputs,
            metadata.sample_rate,
        )?;
        end(&mut writer, "ItemResult")?;
    }
    end(&mut writer, "ItemResults")?;

    start(&mut writer, "ExtensionProperties")?;
    let mut timing = BytesStart::new("TimingExtensionMediaPlaybackEditUnits");
    timing.push_attribute(("xmlns", EBU_QC_TIMING_NAMESPACE));
    xml_event(&mut writer, Event::Start(timing))?;
    text_element(
        &mut writer,
        "EditRate",
        &format!("{}/1", metadata.sample_rate),
    )?;
    end(&mut writer, "TimingExtensionMediaPlaybackEditUnits")?;
    end(&mut writer, "ExtensionProperties")?;
    end(&mut writer, "Report")?;
    Ok(())
}

fn validate_context(
    metadata: &EbuQcReportMetadata,
    analysis: &Analysis,
    timeline: &[LoudnessTimelinePoint],
    timeline_interval_ms: f64,
) -> Result<(), String> {
    if analysis.sample_rate != metadata.sample_rate {
        return Err("EBU QC Scenario 1 metadata and analysis sample rates differ".into());
    }
    if !timeline_interval_ms.is_finite() || timeline_interval_ms <= 0.0 {
        return Err("EBU QC Scenario 1 timeline interval must be finite and positive".into());
    }
    if timeline.is_empty() {
        return Err("EBU QC Scenario 1 loudness reports require a measured timeline".into());
    }
    if timeline.iter().any(|point| {
        !point.start_seconds.is_finite()
            || !point.end_seconds.is_finite()
            || point.start_seconds < 0.0
            || point.end_seconds < point.start_seconds
    }) {
        return Err("EBU QC Scenario 1 timeline contains an invalid locator".into());
    }
    Ok(())
}

fn catalogue_definition(result: &QcResult) -> Result<&'static CatalogueDefinition, String> {
    let definition = DEFINITIONS
        .iter()
        .find(|definition| definition.id == result.ebu_qc_id)
        .ok_or_else(|| {
            format!(
                "EBU QC Item {} has no pinned Scenario 1 mapping",
                result.ebu_qc_id
            )
        })?;
    if result.version != definition.version || result.name != definition.name {
        return Err(format!(
            "EBU QC Item {} identity drift: expected {} v{}, got {} v{}",
            definition.id, definition.name, definition.version, result.name, result.version
        ));
    }
    Ok(definition)
}

fn scenario_item_applicable(
    result: &QcResult,
    analysis: &Analysis,
    timeline: &[LoudnessTimelinePoint],
) -> bool {
    match result.ebu_qc_id.as_str() {
        "0010B" => {
            analysis.lufs.is_finite()
                && analysis.max_momentary_lufs.is_finite()
                && analysis.loudness_range_lu.is_finite()
                && timeline
                    .iter()
                    .any(|point| point.momentary_lufs.is_some_and(f64::is_finite))
                && timeline
                    .iter()
                    .any(|point| point.short_term_lufs.is_some_and(f64::is_finite))
        }
        "0084B" => {
            analysis.true_peak_db().is_finite()
                && timeline
                    .iter()
                    .any(|point| point.true_peak_dbtp.is_finite())
        }
        "0095B" => analysis.channel_roles.contains(&ChannelRole::Lfe),
        _ => true,
    }
}

fn item_used_as(definition: &CatalogueDefinition, options: &QcOptions) -> UsedAs {
    if definition.id == "0009F" && options.expected_duration_seconds.is_none() {
        UsedAs::Report
    } else {
        definition.used_as
    }
}

fn catalogue_fingerprint(
    results: &[&QcResult],
    analysis: &Analysis,
    options: &QcOptions,
    timeline_interval_ms: f64,
) -> Result<Vec<u8>, String> {
    let mut digest = Sha256::new();
    for result in results {
        let definition = catalogue_definition(result)?;
        for value in [
            definition.id,
            definition.version,
            definition.name,
            definition.sha256,
            item_used_as(definition, options).as_str(),
        ] {
            hash_bytes(&mut digest, value.as_bytes());
        }
        for input in item_inputs(
            definition.id,
            item_used_as(definition, options),
            analysis,
            options,
            timeline_interval_ms,
        )? {
            hash_bytes(&mut digest, input.name.as_bytes());
            if let Some(value) = input.value.as_deref() {
                hash_bytes(&mut digest, value.as_bytes());
            }
            if let Some(locator) = input.locator {
                digest.update(locator.start_seconds.to_bits().to_be_bytes());
                digest.update(locator.end_seconds.to_bits().to_be_bytes());
            }
            digest.update(input.track.unwrap_or(0).to_be_bytes());
        }
    }
    Ok(digest.finalize().to_vec())
}

fn result_fingerprint(items: &[ScenarioItem<'_>]) -> Vec<u8> {
    let mut digest = Sha256::new();
    for item in items {
        hash_bytes(&mut digest, item.definition.id.as_bytes());
        hash_bytes(&mut digest, item.used_as.as_str().as_bytes());
        digest.update([u8::from(item.result.passed)]);
        for output in &item.outputs {
            hash_bytes(&mut digest, output.name.as_bytes());
            if let Some(value) = output.value.as_deref() {
                hash_bytes(&mut digest, value.as_bytes());
            }
            if let Some(locator) = output.locator {
                digest.update(locator.start_seconds.to_bits().to_be_bytes());
                digest.update(locator.end_seconds.to_bits().to_be_bytes());
            }
            digest.update(output.track.unwrap_or(0).to_be_bytes());
        }
    }
    digest.finalize().to_vec()
}

fn item_inputs(
    id: &str,
    used_as: UsedAs,
    analysis: &Analysis,
    options: &QcOptions,
    timeline_interval_ms: f64,
) -> Result<Vec<IoValue>, String> {
    let channels = channel_list(analysis.channels);
    let lfe_channel = analysis
        .channel_roles
        .iter()
        .position(|role| *role == ChannelRole::Lfe)
        .map_or(0, |index| index + 1);
    let milliseconds = |seconds: f64| (seconds * 1_000.0).round() as u64;
    let inputs = match id {
        "0078B" => vec![
            io(
                "SilenceThresholdLevel",
                decimal_1(options.silence_threshold_dbfs),
            ),
            io(
                "MinimumSilenceDuration",
                milliseconds(options.silence_minimum_seconds).to_string(),
            ),
        ],
        "0005B" => vec![io(
            "AudioDigitalClippingMinimumDuration",
            options.clipping_minimum_samples.to_string(),
        )],
        "0014B" => vec![
            io(
                "AudioTestToneMinimumDuration",
                milliseconds(options.tone_minimum_seconds).to_string(),
            ),
            io(
                "AudioTestToneExpectedFrequency",
                options.tone_frequency_hz.round().to_string(),
            ),
        ],
        "0009F" => {
            if used_as == UsedAs::Report {
                Vec::new()
            } else {
                let expected = options.expected_duration_seconds.ok_or_else(|| {
                    "EBU QC 0009F check requires an expected duration".to_string()
                })?;
                vec![
                    io(
                        "AudioDurationExpectedBitstreamValue",
                        edit_unit(expected, analysis.sample_rate).to_string(),
                    ),
                    io(
                        "AudioDurationExpectedBitstreamTolerance",
                        milliseconds(options.duration_tolerance_seconds).to_string(),
                    ),
                ]
            }
        }
        "0010B" => vec![
            io("LoudnessServiceChannelAllocation", channels.clone()),
            io("LoudnessMomentaryMeasureWindow", "400"),
            io(
                "LoudnessMomentaryMeasureSliding",
                timeline_interval_ms.round().to_string(),
            ),
            io("LoudnessShortTermMeasureWindow", "3000"),
            io(
                "LoudnessShortTermMeasureSliding",
                timeline_interval_ms.round().to_string(),
            ),
        ],
        "0084B" => Vec::new(),
        "0004F" => options
            .expected_channel_count
            .map(|expected| {
                vec![io(
                    "AudioChannelNumberExpectedBitstream",
                    expected.to_string(),
                )]
            })
            .unwrap_or_default(),
        "0008B" => vec![
            io(
                "AudioDropoutAverageAudioLevelPreDropout",
                decimal_2(options.dropout_threshold_dbfs),
            ),
            io(
                "AudioDropoutAudioReleaseRatio",
                decimal_1(10.0_f64.powf(options.dropout_threshold_dbfs / 20.0)),
            ),
            io(
                "AudioDropoutMinimumDropoutDuration",
                milliseconds(options.dropout_minimum_seconds).to_string(),
            ),
            io(
                "AudioDropoutMaximumDropoutDuration",
                decimal_3(options.dropout_maximum_seconds),
            ),
        ],
        "0012B" => vec![
            io(
                "AudioPhaseFrequencyWindow",
                (analysis.sample_rate / 2).to_string(),
            ),
            io(
                "AudioPhaseErrorDurationThreshold",
                milliseconds(options.phase_window_seconds).to_string(),
            ),
            io(
                "AudioPhaseFrequencyRange",
                correlation_degrees(options.phase_correlation_threshold).to_string(),
            ),
        ],
        "0057B" => vec![io(
            "AudioClickTriggerThreshold",
            decimal_3(options.click_threshold),
        )],
        "0077B" => vec![
            io(
                "AverageMinimumAudioLevelThresholdExpected",
                options.minimum_average_level_dbfs.round().to_string(),
            ),
            io("AverageMinimumAudioLevelChannelsInService", channels),
        ],
        "0088B" => vec![
            io("AudioHumThreshold", decimal_1(options.hum_threshold_dbfs)),
            io(
                "AudioHumDuration",
                milliseconds(options.hum_minimum_seconds).to_string(),
            ),
        ],
        "0086B" => vec![
            io(
                "AudioNoiseMinimumFrequency",
                options.noise_low_hz.round().to_string(),
            ),
            io(
                "MinimumNoiseSegmentDuration",
                milliseconds(options.noise_minimum_seconds).to_string(),
            ),
        ],
        "0170B" => Vec::new(),
        "0230B" => vec![io(
            "MinimumAudioPanningErrorDuration",
            xs_duration(options.panning_minimum_seconds),
        )],
        "0095B" => vec![
            io(
                "LfeChannelFrequencyHighExpected",
                options.lfe_cutoff_hz.round().to_string(),
            ),
            io("LfeChannelIdExpected", lfe_channel.to_string()),
        ],
        "0124B" => vec![
            io("MonoAudioServiceType", "N/S"),
            io("MonoAudioServiceChannel", "1"),
        ],
        _ => return Err(format!("no EBU QC Scenario 1 input mapping for {id}")),
    };
    Ok(inputs)
}

fn item_outputs(
    id: &str,
    used_as: UsedAs,
    result: &QcResult,
    analysis: &Analysis,
    options: &QcOptions,
    timeline: &[LoudnessTimelinePoint],
) -> Result<Vec<IoValue>, String> {
    let duration = analysis.duration_secs();
    let outputs = match id {
        "0078B" => {
            let mut values = Vec::new();
            for event in &result.events {
                values.push(event_io("Channel", event, event.channel.to_string()));
                if let Some(measured) = event.measured {
                    values.push(event_io("AverageAudioLevel", event, decimal_1(measured)));
                }
            }
            values
        }
        "0005B" => {
            let mut detected =
                event_boolean_outputs("AudioDigitalClippingDetected", result, duration);
            if result.events.is_empty() {
                detected.push(located_io(
                    "AudioDigitalClippingChannel",
                    0.0,
                    duration,
                    None,
                    "0",
                ));
            } else {
                detected.extend(result.events.iter().map(|event| {
                    event_io(
                        "AudioDigitalClippingChannel",
                        event,
                        event.channel.to_string(),
                    )
                }));
            }
            detected
        }
        "0014B" => {
            let mut values =
                event_boolean_outputs("AudioTestTonesDetectedNotExpected", result, duration);
            values.push(io("AudioTestToneTypeValid", "true"));
            values
        }
        "0009F" => {
            let mut values = vec![io(
                "AudioDurationBitstreamMeasured",
                analysis.frames.to_string(),
            )];
            if used_as == UsedAs::Check {
                values.push(io(
                    "AudioDurationMeasuredBitstreamMismatch",
                    bool_text(!result.passed),
                ));
            }
            values
        }
        "0010B" => loudness_outputs(analysis, timeline)?,
        "0084B" => {
            let point = max_timeline_point(timeline, |point| Some(point.true_peak_dbtp));
            vec![located_io(
                "AudioPeaksMeasured",
                point.start_seconds,
                point.end_seconds,
                None,
                point.true_peak_dbtp.round().to_string(),
            )]
        }
        "0004F" => vec![
            io("AudioChannelNumberBitstream", analysis.channels.to_string()),
            io(
                "AudioChannelNumberIndividualServiceBitstream",
                analysis.channels.to_string(),
            ),
            io(
                "AudioChannelNumberBitstreamMismatch",
                bool_text(!result.passed),
            ),
        ],
        "0008B" => {
            let mut values = event_boolean_outputs("AudioDropoutDetected", result, duration);
            for event in &result.events {
                values.push(event_io("AudioDropoutTypeName", event, "short dropout"));
            }
            values
        }
        "0012B" => event_boolean_outputs("AudioPhaseError", result, duration),
        "0057B" => event_boolean_outputs("AudioClicksDetected", result, duration),
        "0077B" => vec![io(
            "AverageMinimumAudioLevelThresholdReported",
            result
                .events
                .iter()
                .filter_map(|event| event.measured)
                .reduce(f64::min)
                .unwrap_or(analysis.rms_db)
                .round()
                .to_string(),
        )],
        "0088B" => event_boolean_outputs("AudioHumDetected", result, duration),
        "0086B" => {
            let mut values = vec![io("AudioNoiseDetected", bool_text(!result.passed))];
            for event in &result.events {
                values.push(event_io(
                    "AudioNoiseReported",
                    event,
                    event.measured.map_or_else(|| "true".into(), decimal_1),
                ));
            }
            values
        }
        "0170B" => {
            let mut values = Vec::new();
            for event in &result.events {
                values.push(event_io(
                    "CrossTalkLocation",
                    event,
                    event.measured.map_or_else(|| "true".into(), decimal_3),
                ));
            }
            values
        }
        "0230B" => {
            let mut values = Vec::new();
            for event in &result.events {
                values.push(event_io("AudioPanningError", event, "true"));
            }
            values
        }
        "0095B" => {
            let lfe_channel = analysis
                .channel_roles
                .iter()
                .position(|role| *role == ChannelRole::Lfe)
                .map(|index| (index + 1) as u16);
            let locator = Locator {
                start_seconds: 0.0,
                end_seconds: duration,
            };
            vec![
                IoValue {
                    name: "LfeAudioChannelAssignment",
                    locator: Some(locator),
                    track: lfe_channel,
                    value: Some(bool_text(lfe_channel.is_some()).into()),
                },
                IoValue {
                    name: "LfeChannelFrequencyHighReported",
                    locator: Some(locator),
                    track: lfe_channel,
                    value: Some(options.lfe_cutoff_hz.round().to_string()),
                },
            ]
        }
        "0124B" => vec![io(
            "MonoAudioRepresentationNotValid",
            bool_text(!result.passed),
        )],
        _ => return Err(format!("no EBU QC Scenario 1 output mapping for {id}")),
    };
    Ok(outputs)
}

fn validate_required_io(
    id: &str,
    used_as: UsedAs,
    inputs: &[IoValue],
    outputs: &[IoValue],
) -> Result<(), String> {
    for required in required_input_names(id, used_as)? {
        if !inputs.iter().any(|input| input.name == *required) {
            return Err(format!(
                "EBU QC {id} {} mode is missing required Input {required}",
                used_as.as_str()
            ));
        }
    }
    for required in required_output_names(id, used_as)? {
        if !outputs.iter().any(|output| output.name == *required) {
            return Err(format!(
                "EBU QC {id} {} mode is missing required Output {required}",
                used_as.as_str()
            ));
        }
    }
    if inputs
        .iter()
        .chain(outputs)
        .any(|value| value.value.as_deref().is_some_and(str::is_empty))
    {
        return Err(format!(
            "EBU QC {id} {} mode contains an empty typed IO value",
            used_as.as_str()
        ));
    }
    Ok(())
}

fn required_input_names(id: &str, used_as: UsedAs) -> Result<&'static [&'static str], String> {
    let names: &[&str] = match (id, used_as) {
        ("0009F", UsedAs::Report) | ("0084B", UsedAs::Report) => &[],
        ("0010B", UsedAs::Report) => &[
            "LoudnessServiceChannelAllocation",
            "LoudnessMomentaryMeasureWindow",
            "LoudnessMomentaryMeasureSliding",
            "LoudnessShortTermMeasureWindow",
            "LoudnessShortTermMeasureSliding",
        ],
        ("0078B", UsedAs::Check) => &["SilenceThresholdLevel", "MinimumSilenceDuration"],
        ("0005B", UsedAs::Check)
        | ("0004F", UsedAs::Check)
        | ("0057B", UsedAs::Check)
        | ("0170B", UsedAs::Check) => &[],
        ("0014B", UsedAs::Check) => &["AudioTestToneMinimumDuration"],
        ("0009F", UsedAs::Check) => &[
            "AudioDurationExpectedBitstreamValue",
            "AudioDurationExpectedBitstreamTolerance",
        ],
        ("0008B", UsedAs::Check) => &[
            "AudioDropoutAverageAudioLevelPreDropout",
            "AudioDropoutAudioReleaseRatio",
            "AudioDropoutMinimumDropoutDuration",
            "AudioDropoutMaximumDropoutDuration",
        ],
        ("0012B", UsedAs::Check) => &[
            "AudioPhaseFrequencyWindow",
            "AudioPhaseErrorDurationThreshold",
            "AudioPhaseFrequencyRange",
        ],
        ("0077B", UsedAs::Check) => &[
            "AverageMinimumAudioLevelThresholdExpected",
            "AverageMinimumAudioLevelChannelsInService",
        ],
        ("0088B", UsedAs::Check) => &["AudioHumThreshold", "AudioHumDuration"],
        ("0086B", UsedAs::Check) => &["AudioNoiseMinimumFrequency", "MinimumNoiseSegmentDuration"],
        ("0230B", UsedAs::Check) => &["MinimumAudioPanningErrorDuration"],
        ("0095B", UsedAs::Check) => &["LfeChannelFrequencyHighExpected", "LfeChannelIdExpected"],
        ("0124B", UsedAs::Check) => &["MonoAudioServiceType", "MonoAudioServiceChannel"],
        _ => {
            return Err(format!(
                "no required EBU QC Input contract for {id} {} mode",
                used_as.as_str()
            ))
        }
    };
    Ok(names)
}

fn required_output_names(id: &str, used_as: UsedAs) -> Result<&'static [&'static str], String> {
    let names: &[&str] = match (id, used_as) {
        ("0009F", UsedAs::Report) => &["AudioDurationBitstreamMeasured"],
        ("0010B", UsedAs::Report) => &[
            "LoudnessTargetLevelIntegrated",
            "LoudnessMaximumTruePeak",
            "LoudnessRange",
            "LoudnessPermittedRangeExceededSegment",
            "LoudnessMaximumMomentarySegment",
            "LoudnessMaximumShortTermSegment",
            "LoudnessMomentaryOverTime",
            "LoudnessShortTermOverTime",
        ],
        ("0084B", UsedAs::Report) => &["AudioPeaksMeasured"],
        ("0078B", UsedAs::Check) => &[],
        ("0005B", UsedAs::Check) => &[
            "AudioDigitalClippingDetected",
            "AudioDigitalClippingChannel",
        ],
        ("0014B", UsedAs::Check) => &[
            "AudioTestTonesDetectedNotExpected",
            "AudioTestToneTypeValid",
        ],
        ("0009F", UsedAs::Check) => &["AudioDurationMeasuredBitstreamMismatch"],
        ("0004F", UsedAs::Check) => &[
            "AudioChannelNumberBitstream",
            "AudioChannelNumberIndividualServiceBitstream",
        ],
        ("0008B", UsedAs::Check) => &["AudioDropoutDetected"],
        ("0012B", UsedAs::Check) => &["AudioPhaseError"],
        ("0057B", UsedAs::Check) => &["AudioClicksDetected"],
        ("0077B", UsedAs::Check) => &["AverageMinimumAudioLevelThresholdReported"],
        ("0088B", UsedAs::Check) => &["AudioHumDetected"],
        ("0086B", UsedAs::Check) => &["AudioNoiseDetected"],
        ("0170B", UsedAs::Check) | ("0230B", UsedAs::Check) => &[],
        ("0095B", UsedAs::Check) => &[
            "LfeAudioChannelAssignment",
            "LfeChannelFrequencyHighReported",
        ],
        ("0124B", UsedAs::Check) => &["MonoAudioRepresentationNotValid"],
        _ => {
            return Err(format!(
                "no required EBU QC Output contract for {id} {} mode",
                used_as.as_str()
            ))
        }
    };
    Ok(names)
}

fn loudness_outputs(
    analysis: &Analysis,
    timeline: &[LoudnessTimelinePoint],
) -> Result<Vec<IoValue>, String> {
    if timeline.is_empty() {
        return Err("EBU QC 0010B report requires loudness timeline points".into());
    }
    let true_peak = max_timeline_point(timeline, |point| Some(point.true_peak_dbtp));
    let momentary = max_timeline_point(timeline, |point| point.momentary_lufs);
    let short_term = max_timeline_point(timeline, |point| point.short_term_lufs);
    let momentary_values = timeline
        .iter()
        .filter_map(|point| point.momentary_lufs)
        .map(decimal_1)
        .collect::<Vec<_>>()
        .join(" ");
    let short_term_values = timeline
        .iter()
        .filter_map(|point| point.short_term_lufs)
        .map(decimal_1)
        .collect::<Vec<_>>()
        .join(" ");
    debug_assert!(!momentary_values.is_empty());
    debug_assert!(!short_term_values.is_empty());
    let duration = analysis.duration_secs();
    Ok(vec![
        io("LoudnessTargetLevelIntegrated", decimal_1(analysis.lufs)),
        located_io(
            "LoudnessMaximumTruePeak",
            true_peak.start_seconds,
            true_peak.end_seconds,
            None,
            decimal_1(analysis.true_peak_db()),
        ),
        io("LoudnessRange", decimal_1(analysis.loudness_range_lu)),
        located_io(
            "LoudnessPermittedRangeExceededSegment",
            0.0,
            duration,
            None,
            decimal_1(analysis.loudness_range_lu),
        ),
        located_io(
            "LoudnessMaximumMomentarySegment",
            momentary.start_seconds,
            momentary.end_seconds,
            None,
            decimal_1(
                momentary
                    .momentary_lufs
                    .expect("applicability requires a momentary value"),
            ),
        ),
        located_io(
            "LoudnessMaximumShortTermSegment",
            short_term.start_seconds,
            short_term.end_seconds,
            None,
            decimal_1(
                short_term
                    .short_term_lufs
                    .expect("applicability requires a short-term value"),
            ),
        ),
        io("LoudnessMomentaryOverTime", momentary_values),
        io("LoudnessShortTermOverTime", short_term_values),
    ])
}

fn max_timeline_point<F>(timeline: &[LoudnessTimelinePoint], value: F) -> &LoudnessTimelinePoint
where
    F: Fn(&LoudnessTimelinePoint) -> Option<f64>,
{
    timeline
        .iter()
        .filter_map(|point| {
            value(point)
                .filter(|value| value.is_finite())
                .map(|value| (point, value))
        })
        .max_by(|(_, left), (_, right)| left.total_cmp(right))
        .map_or(&timeline[0], |(point, _)| point)
}

fn event_boolean_outputs(name: &'static str, result: &QcResult, duration: f64) -> Vec<IoValue> {
    if result.events.is_empty() {
        return vec![located_io(name, 0.0, duration, None, "false")];
    }
    result
        .events
        .iter()
        .map(|event| event_io(name, event, "true"))
        .collect()
}

fn event_io(name: &'static str, event: &crate::qc::QcEvent, value: impl Into<String>) -> IoValue {
    located_io(
        name,
        event.start_seconds,
        event.end_seconds,
        (event.channel > 0).then_some(event.channel),
        value,
    )
}

fn located_io(
    name: &'static str,
    start_seconds: f64,
    end_seconds: f64,
    track: Option<u16>,
    value: impl Into<String>,
) -> IoValue {
    IoValue {
        name,
        locator: Some(Locator {
            start_seconds,
            end_seconds,
        }),
        track,
        value: Some(value.into()),
    }
}

fn io(name: &'static str, value: impl Into<String>) -> IoValue {
    IoValue {
        name,
        locator: None,
        track: None,
        value: Some(value.into()),
    }
}

fn channel_list(channels: u16) -> String {
    (1..=channels)
        .map(|channel| channel.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

fn correlation_degrees(correlation: f64) -> u16 {
    correlation
        .clamp(-1.0, 1.0)
        .acos()
        .to_degrees()
        .round()
        .clamp(0.0, 180.0) as u16
}

fn decimal_1(value: f64) -> String {
    finite_decimal(value, 1)
}

fn decimal_2(value: f64) -> String {
    finite_decimal(value, 2)
}

fn decimal_3(value: f64) -> String {
    finite_decimal(value, 3)
}

fn finite_decimal(value: f64, precision: usize) -> String {
    let value = if value.is_finite() {
        value
    } else if value.is_sign_negative() {
        -200.0
    } else {
        200.0
    };
    format!("{value:.precision$}")
}

fn write_identity<W: Write>(writer: &mut Writer<W>, item: &ScenarioItem<'_>) -> Result<(), String> {
    text_element(writer, "EBUQCID", item.definition.id)?;
    text_element(writer, "EBUQCName", item.definition.name)?;
    text_element(writer, "EBUQCVersion", item.definition.version)?;
    text_element(writer, "InstanceId", &item.instance_id)
}

fn write_ios<W: Write>(
    writer: &mut Writer<W>,
    collection: &str,
    element: &str,
    values: &[IoValue],
    sample_rate: u32,
) -> Result<(), String> {
    if values.is_empty() {
        return empty(writer, collection);
    }
    start(writer, collection)?;
    for value in values {
        start(writer, element)?;
        text_element(writer, "Name", value.name)?;
        if let Some(locator) = value.locator {
            start(writer, "Locator")?;
            text_element(
                writer,
                "Start",
                &edit_unit(locator.start_seconds, sample_rate).to_string(),
            )?;
            text_element(
                writer,
                "End",
                &edit_unit(locator.end_seconds, sample_rate).to_string(),
            )?;
            end(writer, "Locator")?;
        }
        if let Some(track) = value.track {
            text_element(writer, "Track", &track.to_string())?;
        }
        if let Some(text) = value.value.as_deref() {
            text_element(writer, "Value", text)?;
        }
        end(writer, element)?;
    }
    end(writer, collection)
}

fn content_id<W: Write>(writer: &mut Writer<W>, identifier: &str) -> Result<(), String> {
    start(writer, "ContentId")?;
    start(writer, "ContentIdentifier")?;
    text_element(writer, "ID", identifier)?;
    end(writer, "ContentIdentifier")?;
    end(writer, "ContentId")
}

fn tool_information<W: Write>(writer: &mut Writer<W>) -> Result<(), String> {
    start(writer, "ToolInformation")?;
    text_element(
        writer,
        "ToolID",
        "https://github.com/penguin425/audio-normalizer",
    )?;
    text_element(writer, "ToolName", "Forge")?;
    text_element(writer, "Vendor", "penguin425")?;
    text_element(writer, "URL", env!("CARGO_PKG_REPOSITORY"))?;
    text_element(writer, "Version", env!("CARGO_PKG_VERSION"))?;
    end(writer, "ToolInformation")
}

fn start<W: Write>(writer: &mut Writer<W>, name: &str) -> Result<(), String> {
    xml_event(writer, Event::Start(BytesStart::new(name)))
}

fn empty<W: Write>(writer: &mut Writer<W>, name: &str) -> Result<(), String> {
    xml_event(writer, Event::Empty(BytesStart::new(name)))
}

fn end<W: Write>(writer: &mut Writer<W>, name: &str) -> Result<(), String> {
    xml_event(writer, Event::End(BytesEnd::new(name)))
}

fn text_element<W: Write>(writer: &mut Writer<W>, name: &str, value: &str) -> Result<(), String> {
    start(writer, name)?;
    xml_event(writer, Event::Text(BytesText::new(value)))?;
    end(writer, name)
}

fn xml_event<W: Write>(writer: &mut Writer<W>, event: Event<'_>) -> Result<(), String> {
    writer
        .write_event(event)
        .map_err(|error| format!("write EBU QC Scenario 1 XML: {error}"))
}

fn valid_ebu_qc_id(value: &str) -> bool {
    value.len() == 5
        && value.as_bytes()[..4]
            .iter()
            .all(|byte| byte.is_ascii_digit())
        && value.as_bytes()[4].is_ascii_alphabetic()
}

fn hash_bytes(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn deterministic_uuid(namespace: &[u8], values: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    hash_bytes(&mut digest, namespace);
    for value in values {
        hash_bytes(&mut digest, value);
    }
    let digest = digest.finalize();
    let mut bytes: [u8; 16] = digest[..16].try_into().expect("SHA-256 prefix length");
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        hex(&bytes[0..4]),
        hex(&bytes[4..6]),
        hex(&bytes[6..8]),
        hex(&bytes[8..10]),
        hex(&bytes[10..16])
    )
}

fn hex(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
    }
    value
}

fn edit_unit(seconds: f64, sample_rate: u32) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        0
    } else {
        (seconds * f64::from(sample_rate))
            .round()
            .clamp(0.0, u64::MAX as f64) as u64
    }
}

fn xs_duration(seconds: f64) -> String {
    let mut value = format!("{seconds:.9}");
    while value.ends_with('0') {
        value.pop();
    }
    if value.ends_with('.') {
        value.push('0');
    }
    format!("PT{value}S")
}

fn bool_text(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{AudioBuffer, PcmKind};

    fn fixture() -> (
        EbuQcReportMetadata,
        Analysis,
        QcOptions,
        Vec<QcResult>,
        Vec<LoudnessTimelinePoint>,
    ) {
        let audio = AudioBuffer {
            sample_rate: 48_000,
            channels: 2,
            frames: 48_000,
            data: vec![vec![0.1; 48_000], vec![0.1; 48_000]],
            channel_roles: vec![ChannelRole::Main, ChannelRole::Main],
            source_kind: PcmKind::S16,
        };
        let analysis = crate::normalize::analyze(&audio);
        let options = QcOptions::default();
        let results = crate::qc::analyze(&audio, &analysis, &options);
        let timeline = vec![
            LoudnessTimelinePoint {
                start_seconds: 0.0,
                end_seconds: 0.5,
                momentary_lufs: Some(-20.2),
                short_term_lufs: None,
                sample_peak_dbfs: -20.0,
                true_peak_dbtp: -19.9,
            },
            LoudnessTimelinePoint {
                start_seconds: 0.5,
                end_seconds: 1.0,
                momentary_lufs: Some(-19.9),
                short_term_lufs: Some(-20.0),
                sample_peak_dbfs: -19.8,
                true_peak_dbtp: -19.7,
            },
        ];
        (
            EbuQcReportMetadata {
                content_identifier: format!("urn:sha256:{}", "a".repeat(64)),
                last_modified_datetime: "2026-09-02T12:34:56Z".into(),
                sample_rate: 48_000,
                analysis_duration_seconds: 1.0,
            },
            analysis,
            options,
            results,
            timeline,
        )
    }

    #[test]
    fn writes_catalogue_specific_scenario1_vocabulary() {
        let (metadata, analysis, options, results, timeline) = fixture();
        let mut xml = Vec::new();
        write_xml(
            &mut xml, &metadata, &analysis, &options, &results, &timeline, 40.0,
        )
        .unwrap();
        let text = String::from_utf8(xml).unwrap();
        assert!(text.contains("<Report xmlns=\"tag:qc.ebu.ch,2026-04\">"));
        assert!(text.contains("<Name>SilenceThresholdLevel</Name>"));
        assert!(text.contains("<Name>MinimumSilenceDuration</Name>"));
        assert!(text.contains("<Name>LoudnessTargetLevelIntegrated</Name>"));
        assert!(text.contains("<Name>LoudnessMomentaryOverTime</Name>"));
        assert!(text.contains("<Name>AudioPeaksMeasured</Name>"));
        assert!(!text.contains("<Name>Event</Name>"));
        assert!(!text.contains("FORGE-DC-OFFSET"));
        assert_eq!(
            text.matches("<Item>").count(),
            text.matches("<ItemResult>").count()
        );
    }

    #[test]
    fn omits_check_result_for_report_mode_items() {
        let (metadata, analysis, options, results, timeline) = fixture();
        let mut xml = Vec::new();
        write_xml(
            &mut xml, &metadata, &analysis, &options, &results, &timeline, 40.0,
        )
        .unwrap();
        let text = String::from_utf8(xml).unwrap();
        let results_start = text.find("<ItemResults>").unwrap();
        let results = &text[results_start..];
        for id in ["0010B", "0084B"] {
            let marker = format!("<EBUQCID>{id}</EBUQCID>");
            let start = results.find(&marker).unwrap();
            let end = results[start..].find("</ItemResult>").unwrap() + start;
            assert!(!results[start..end].contains("<CheckResult>"), "{id}");
        }
        let silence = results.find("<EBUQCID>0078B</EBUQCID>").unwrap();
        let silence_end = results[silence..].find("</ItemResult>").unwrap() + silence;
        assert!(results[silence..silence_end].contains("<CheckResult>"));
    }

    #[test]
    fn checked_in_catalogue_pins_match_compiled_identities() {
        let manifest: serde_json::Value =
            serde_json::from_str(include_str!("../schema/ebu-qc-catalogue-v2-pins.json")).unwrap();
        assert_eq!(
            manifest["catalogue_api_namespace"],
            EBU_QC_CATALOGUE_API_NAMESPACE
        );
        let entries = manifest["definitions"].as_array().unwrap();
        assert_eq!(entries.len(), DEFINITIONS.len());
        for definition in DEFINITIONS {
            let entry = entries
                .iter()
                .find(|entry| entry["id"] == definition.id)
                .unwrap();
            assert_eq!(entry["version"], definition.version);
            assert_eq!(entry["name"], definition.name);
            assert_eq!(entry["sha256"], definition.sha256);
        }
    }

    #[test]
    fn rejects_catalogue_identity_drift_and_missing_timeline() {
        let (metadata, analysis, options, mut results, timeline) = fixture();
        results
            .iter_mut()
            .find(|result| result.ebu_qc_id == "0078B")
            .unwrap()
            .version = "4.0".into();
        let error = write_xml(
            Vec::new(),
            &metadata,
            &analysis,
            &options,
            &results,
            &timeline,
            40.0,
        )
        .unwrap_err();
        assert!(error.contains("identity drift"));

        let (_, _, _, results, _) = fixture();
        let error = write_xml(
            Vec::new(),
            &metadata,
            &analysis,
            &options,
            &results,
            &[],
            40.0,
        )
        .unwrap_err();
        assert!(error.contains("require a measured timeline"));
    }

    #[test]
    fn duration_mode_and_profile_identity_follow_inputs() {
        let (metadata, analysis, mut options, results, timeline) = fixture();
        let mut report_mode = Vec::new();
        write_xml(
            &mut report_mode,
            &metadata,
            &analysis,
            &options,
            &results,
            &timeline,
            40.0,
        )
        .unwrap();

        options.expected_duration_seconds = Some(1.0);
        let mut check_mode = Vec::new();
        write_xml(
            &mut check_mode,
            &metadata,
            &analysis,
            &options,
            &results,
            &timeline,
            40.0,
        )
        .unwrap();
        let check_text = String::from_utf8(check_mode).unwrap();
        let duration = check_text.find("<EBUQCID>0009F</EBUQCID>").unwrap();
        let duration_end = check_text[duration..].find("</Item>").unwrap() + duration;
        let duration_item = &check_text[duration..duration_end];
        assert!(duration_item.contains("<UsedAs>check</UsedAs>"));
        assert!(duration_item.contains("<Name>AudioDurationExpectedBitstreamValue</Name>"));

        let profile_id = |xml: &[u8]| {
            let text = std::str::from_utf8(xml).unwrap();
            let profile = text.find("<Profile>").unwrap();
            let id = text[profile..].find("<ID>").unwrap() + profile + 4;
            let end = text[id..].find("</ID>").unwrap() + id;
            text[id..end].to_string()
        };
        assert_ne!(profile_id(&report_mode), profile_id(check_text.as_bytes()));
    }
}
