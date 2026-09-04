//! Schema-valid EBU QC Report XML output.
//!
//! The existing Forge JSON envelope remains the lossless internal evidence
//! format. This module exports the calculated, published EBU QC Items to the
//! generic EBU QC Data Model namespace released in April 2026. Forge-specific
//! checks are intentionally omitted because they do not resolve to definitions
//! in the EBU-hosted catalogue. The generic output is not a claim of complete
//! Scenario 1 Catalogue-vocabulary conformance: item-specific Inputs and
//! Outputs remain available in Forge's JSON evidence instead. Event details
//! carried here use the data model's vendor-specific `-PRIVATE-...` naming
//! convention and never duplicate the model-level `CheckResult` property.

use crate::normalization_diff;
use crate::normalize::Analysis;
use crate::qc::{QcResult, EBU_QC_CATALOGUE};
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const EBU_QC_REPORT_NAMESPACE: &str = "tag:qc.ebu.ch,2026-04";
pub const EBU_QC_REPORT_SCHEMA: &str = "https://ebu.github.io/qc/qc-data-model/qc.xsd";
pub const EBU_QC_TIMING_NAMESPACE: &str = "tag:qc.ebu.ch,2026-04:extensions:timing";

pub(crate) struct BoundedXmlBuffer {
    bytes: Vec<u8>,
}

impl BoundedXmlBuffer {
    pub(crate) fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(64 * 1024),
        }
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for BoundedXmlBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let total = self.bytes.len().checked_add(bytes.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "EBU QC XML size overflow")
        })?;
        if total > crate::ebu_qc_validation::MAX_EBU_QC_XML_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "EBU QC XML exceeds {} bytes",
                    crate::ebu_qc_validation::MAX_EBU_QC_XML_BYTES
                ),
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EbuQcReportMetadata {
    pub content_identifier: String,
    pub last_modified_datetime: String,
    pub sample_rate: u32,
    pub analysis_duration_seconds: f64,
}

impl EbuQcReportMetadata {
    pub fn from_file(path: &Path, analysis: &Analysis) -> Result<Self, String> {
        let evidence = normalization_diff::inspect_file(path)?;
        let modified = fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .map_err(|error| format!("read modification time for {}: {error}", path.display()))?;
        Ok(Self {
            content_identifier: format!("urn:sha256:{}", evidence.sha256),
            last_modified_datetime: format_utc(modified)?,
            sample_rate: analysis.sample_rate,
            analysis_duration_seconds: analysis.duration_secs(),
        })
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.content_identifier.trim().is_empty() {
            return Err("EBU QC content identifier cannot be empty".into());
        }
        if !valid_xs_datetime(&self.last_modified_datetime) {
            return Err("EBU QC last-modified timestamp must be an RFC 3339 UTC date-time".into());
        }
        if self.sample_rate == 0 {
            return Err("EBU QC edit rate requires a non-zero sample rate".into());
        }
        if !self.analysis_duration_seconds.is_finite() || self.analysis_duration_seconds < 0.0 {
            return Err("EBU QC analysis duration must be finite and non-negative".into());
        }
        Ok(())
    }
}

pub fn write_xml<W: Write>(
    mut output: W,
    metadata: &EbuQcReportMetadata,
    results: &[QcResult],
) -> Result<(), String> {
    let mut encoded = BoundedXmlBuffer::new();
    encode_xml(&mut encoded, metadata, results)?;
    let encoded = encoded.into_bytes();
    crate::ebu_qc_validation::validate_xml(
        &encoded,
        crate::ebu_qc_validation::EbuQcValidationProfile::DataModel2026_04,
    )
    .map_err(|error| format!("validate generated EBU QC report: {error}"))?;
    output
        .write_all(&encoded)
        .map_err(|error| format!("write EBU QC XML: {error}"))
}

fn encode_xml<W: Write>(
    output: W,
    metadata: &EbuQcReportMetadata,
    results: &[QcResult],
) -> Result<(), String> {
    metadata.validate()?;
    let items = results
        .iter()
        .filter(|result| {
            result.calculated
                && result.source_url.starts_with(EBU_QC_CATALOGUE)
                && valid_ebu_qc_id(&result.ebu_qc_id)
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        return Err("EBU QC XML requires at least one calculated published EBU QC Item".into());
    }
    let check_items = items
        .iter()
        .filter(|result| !measurement_only(result))
        .collect::<Vec<_>>();
    let overall = check_items.iter().all(|result| result.passed);
    let identity_fingerprint = item_identity_fingerprint(&items);
    let result_fingerprint = results_fingerprint(&items);
    let report_id = deterministic_uuid(
        b"forge-ebu-qc-report",
        &[
            metadata.content_identifier.as_bytes(),
            metadata.last_modified_datetime.as_bytes(),
            result_fingerprint.as_bytes(),
        ],
    );
    let profile_id =
        deterministic_uuid(b"forge-ebu-qc-profile", &[identity_fingerprint.as_bytes()]);

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
    if !check_items.is_empty() {
        text_element(&mut writer, "CheckResult", bool_text(overall))?;
    }
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
    text_element(&mut writer, "Name", "Forge EBU audio QC generic envelope")?;
    content_id(&mut writer, &metadata.content_identifier)?;
    if !check_items.is_empty() {
        text_element(&mut writer, "CheckResultRule", "AND")?;
    }
    text_element(
        &mut writer,
        "Description",
        "Schema-valid generic envelope for calculated published EBU audio QC Items",
    )?;
    empty(&mut writer, "Scopes")?;
    start(&mut writer, "Items")?;
    for (index, result) in items.iter().enumerate() {
        start(&mut writer, "Item")?;
        write_item_identity(&mut writer, result, index, &profile_id)?;
        text_element(
            &mut writer,
            "UsedAs",
            if measurement_only(result) {
                "report"
            } else {
                "check"
            },
        )?;
        empty(&mut writer, "Scopes")?;
        empty(&mut writer, "Inputs")?;
        end(&mut writer, "Item")?;
    }
    end(&mut writer, "Items")?;
    empty(&mut writer, "ItemDefinitions")?;
    end(&mut writer, "Profile")?;

    start(&mut writer, "ItemResults")?;
    for (index, result) in items.iter().enumerate() {
        start(&mut writer, "ItemResult")?;
        write_item_identity(&mut writer, result, index, &profile_id)?;
        text_element(&mut writer, "AnalysisMethodUsed", "measurement")?;
        text_element(&mut writer, "ExecutionStatus", "complete")?;
        if !measurement_only(result) {
            text_element(&mut writer, "CheckResult", bool_text(result.passed))?;
        }
        text_element(&mut writer, "DetectionMethod", "automatic")?;
        start(&mut writer, "Outputs")?;
        if measurement_only(result) && result.events.is_empty() {
            start(&mut writer, "Output")?;
            text_element(
                &mut writer,
                "Name",
                "-PRIVATE-penguin425-Forge-ResultAvailableInJSON",
            )?;
            text_element(&mut writer, "Value", "true")?;
            end(&mut writer, "Output")?;
        }
        for event in &result.events {
            start(&mut writer, "Output")?;
            text_element(&mut writer, "Name", "-PRIVATE-penguin425-Forge-Event")?;
            start(&mut writer, "Locator")?;
            text_element(
                &mut writer,
                "Start",
                &edit_unit(event.start_seconds, metadata.sample_rate).to_string(),
            )?;
            text_element(
                &mut writer,
                "End",
                &edit_unit(event.end_seconds, metadata.sample_rate).to_string(),
            )?;
            end(&mut writer, "Locator")?;
            text_element(&mut writer, "Track", &event.channel.to_string())?;
            if let Some(measured) = event.measured {
                let value = event
                    .unit
                    .as_deref()
                    .map_or_else(|| measured.to_string(), |unit| format!("{measured} {unit}"));
                text_element(&mut writer, "Value", &value)?;
            }
            end(&mut writer, "Output")?;
        }
        end(&mut writer, "Outputs")?;
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

fn write_item_identity<W: Write>(
    writer: &mut Writer<W>,
    result: &QcResult,
    index: usize,
    profile_id: &str,
) -> Result<(), String> {
    text_element(writer, "EBUQCID", &result.ebu_qc_id)?;
    text_element(writer, "EBUQCName", &result.name)?;
    text_element(writer, "EBUQCVersion", &result.version)?;
    text_element(
        writer,
        "InstanceId",
        &deterministic_uuid(
            b"forge-ebu-qc-item",
            &[
                profile_id.as_bytes(),
                result.ebu_qc_id.as_bytes(),
                result.version.as_bytes(),
                index.to_string().as_bytes(),
            ],
        ),
    )
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
        .map_err(|error| format!("write EBU QC XML: {error}"))
}

fn measurement_only(result: &QcResult) -> bool {
    matches!(result.ebu_qc_id.as_str(), "0010B" | "0084B")
}

fn valid_ebu_qc_id(value: &str) -> bool {
    value.len() == 5
        && value.as_bytes()[..4]
            .iter()
            .all(|byte| byte.is_ascii_digit())
        && value.as_bytes()[4].is_ascii_alphabetic()
}

fn item_identity_fingerprint(results: &[&QcResult]) -> String {
    let mut digest = Sha256::new();
    for result in results {
        for value in [
            result.ebu_qc_id.as_bytes(),
            result.version.as_bytes(),
            result.name.as_bytes(),
        ] {
            hash_bytes(&mut digest, value);
        }
    }
    hex(digest.finalize().as_slice())
}

fn results_fingerprint(results: &[&QcResult]) -> String {
    let mut digest = Sha256::new();
    for result in results {
        hash_bytes(&mut digest, result.ebu_qc_id.as_bytes());
        hash_bytes(&mut digest, result.version.as_bytes());
        hash_bytes(&mut digest, result.name.as_bytes());
        digest.update([u8::from(result.passed)]);
        digest.update([u8::from(result.events_truncated)]);
        digest.update((result.events.len() as u64).to_be_bytes());
        for event in &result.events {
            digest.update(event.channel.to_be_bytes());
            digest.update(event.start_seconds.to_bits().to_be_bytes());
            digest.update(event.end_seconds.to_bits().to_be_bytes());
            match event.measured {
                Some(value) => {
                    digest.update([1]);
                    digest.update(value.to_bits().to_be_bytes());
                }
                None => digest.update([0]),
            }
            match event.unit.as_deref() {
                Some(unit) => {
                    digest.update([1]);
                    hash_bytes(&mut digest, unit.as_bytes());
                }
                None => digest.update([0]),
            }
        }
    }
    hex(digest.finalize().as_slice())
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
    // RFC 9562 UUIDv8 is the appropriate version for an application-defined
    // SHA-256 construction; UUIDv5 would require the standardized SHA-1 form.
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

fn valid_xs_datetime(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
        || !bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19) || byte.is_ascii_digit()
        })
    {
        return false;
    }
    let number = |start: usize, end: usize| {
        bytes[start..end]
            .iter()
            .fold(0_u32, |value, byte| value * 10 + u32::from(byte - b'0'))
    };
    let year = number(0, 4);
    let month = number(5, 7);
    let day = number(8, 10);
    let hour = number(11, 13);
    let minute = number(14, 16);
    let second = number(17, 19);
    let leap_year =
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        _ => return false,
    };
    year > 0 && (1..=days_in_month).contains(&day) && hour < 24 && minute < 60 && second < 60
}

fn format_utc(time: SystemTime) -> Result<String, String> {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "EBU QC cannot represent a pre-1970 file timestamp".to_string())?
        .as_secs();
    let days = i64::try_from(seconds / 86_400)
        .map_err(|_| "EBU QC timestamp is outside the supported range".to_string())?;
    let seconds_in_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_in_day / 3_600;
    let minute = seconds_in_day % 3_600 / 60;
    let second = seconds_in_day % 60;
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

// Howard Hinnant's civil-from-days algorithm, with day zero at 1970-01-01.
fn civil_from_days(days_since_epoch: i64) -> (i64, u64, u64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u64, day as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ebu_qc_validation::{validate_xml, EbuQcValidationProfile};
    use crate::qc::QcOptions;
    use crate::wav::{AudioBuffer, ChannelRole, PcmKind};
    use quick_xml::events::Event;
    use quick_xml::Reader;

    fn fixture() -> (EbuQcReportMetadata, Vec<QcResult>) {
        let audio = AudioBuffer {
            sample_rate: 48_000,
            channels: 2,
            frames: 48_000,
            data: vec![vec![0.1; 48_000], vec![0.1; 48_000]],
            channel_roles: vec![ChannelRole::Main, ChannelRole::Main],
            source_kind: PcmKind::S16,
        };
        let analysis = crate::normalize::analyze(&audio);
        let results = crate::qc::analyze(&audio, &analysis, &QcOptions::default());
        (
            EbuQcReportMetadata {
                content_identifier: format!("urn:sha256:{}", "a".repeat(64)),
                last_modified_datetime: "2026-08-30T12:34:56Z".into(),
                sample_rate: 48_000,
                analysis_duration_seconds: 1.0,
            },
            results,
        )
    }

    #[test]
    fn writes_schema_valid_ebu_2026_04_generic_envelope() {
        let (metadata, results) = fixture();
        let mut xml = Vec::new();
        write_xml(&mut xml, &metadata, &results).unwrap();
        let text = String::from_utf8(xml.clone()).unwrap();
        assert!(text.contains("<Report xmlns=\"tag:qc.ebu.ch,2026-04\">"));
        assert!(text.contains("<TimingExtensionMediaPlaybackEditUnits xmlns=\"tag:qc.ebu.ch,2026-04:extensions:timing\">"));
        assert!(text.contains("<EditRate>48000/1</EditRate>"));
        assert!(!text.contains("FORGE-DC-OFFSET"));
        assert!(!text.contains("<Name>CheckResult</Name>"));
        validate_xml(&xml, EbuQcValidationProfile::DataModel2026_04).unwrap();

        let mut reader = Reader::from_reader(xml.as_slice());
        let mut item_count = 0;
        let mut result_count = 0;
        loop {
            match reader.read_event().unwrap() {
                Event::Start(element) if element.local_name().as_ref() == "Item" => {
                    item_count += 1;
                }
                Event::Start(element) if element.local_name().as_ref() == "ItemResult" => {
                    result_count += 1;
                }
                Event::Eof => break,
                _ => {}
            }
        }
        let expected = results
            .iter()
            .filter(|result| result.calculated && result.source_url.starts_with(EBU_QC_CATALOGUE))
            .count();
        assert_eq!(item_count, expected);
        assert_eq!(result_count, expected);
    }

    #[test]
    fn report_ids_and_xml_are_deterministic() {
        let (metadata, results) = fixture();
        let mut first = Vec::new();
        let mut second = Vec::new();
        write_xml(&mut first, &metadata, &results).unwrap();
        write_xml(&mut second, &metadata, &results).unwrap();
        assert_eq!(first, second);
        let identifier = deterministic_uuid(b"test", &[b"value"]);
        assert!(
            identifier.split('-').nth(2).unwrap().starts_with('8'),
            "custom SHA-256 identifiers use UUIDv8: {identifier}"
        );
    }

    #[test]
    fn buffers_are_bounded_and_invalid_reports_are_not_published() {
        let mut full = BoundedXmlBuffer {
            bytes: vec![0; crate::ebu_qc_validation::MAX_EBU_QC_XML_BYTES],
        };
        assert!(full.write_all(b"x").is_err());

        let (metadata, mut results) = fixture();
        results
            .iter_mut()
            .find(|result| result.calculated && result.source_url.starts_with(EBU_QC_CATALOGUE))
            .unwrap()
            .version = "invalid".into();
        let mut output = Vec::new();
        assert!(write_xml(&mut output, &metadata, &results)
            .unwrap_err()
            .contains("validate generated EBU QC report"));
        assert!(output.is_empty());
    }

    #[test]
    fn profile_identity_does_not_depend_on_check_outcomes() {
        let (_, mut results) = fixture();
        let items = results
            .iter()
            .filter(|result| result.calculated && result.source_url.starts_with(EBU_QC_CATALOGUE))
            .collect::<Vec<_>>();
        let before = item_identity_fingerprint(&items);
        results[0].passed = !results[0].passed;
        results[0].events.clear();
        let items = results
            .iter()
            .filter(|result| result.calculated && result.source_url.starts_with(EBU_QC_CATALOGUE))
            .collect::<Vec<_>>();
        assert_eq!(item_identity_fingerprint(&items), before);
    }

    #[test]
    fn utc_timestamp_conversion_handles_epoch_and_leap_day() {
        assert_eq!(format_utc(UNIX_EPOCH).unwrap(), "1970-01-01T00:00:00Z");
        assert_eq!(
            format_utc(UNIX_EPOCH + std::time::Duration::from_secs(951_827_696)).unwrap(),
            "2000-02-29T12:34:56Z"
        );
        assert!(valid_xs_datetime("2000-02-29T12:34:56Z"));
        assert!(!valid_xs_datetime("2001-02-29T12:34:56Z"));
        assert!(!valid_xs_datetime("2026-08-31T24:00:00Z"));
    }
}
