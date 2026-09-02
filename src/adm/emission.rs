//! Native, bounded ITU-R BS.2168-0 file-based ADM emission-profile audit.
//!
//! This module validates the machine-checkable requirements in Annex 1
//! sections 2 and 3.  It intentionally does not claim rendered-audio
//! verification or validate the S-ADM frame rules in section 4.

use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::{NsReader, XmlVersion};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

pub const SCHEMA_VERSION: u32 = 1;
pub const REPORT_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/adm-emission-report-v1";
pub const VALIDATOR: &str = "forge-bs2168-0-file-1";
pub const STANDARD: &str = "ITU-R BS.2168-0";
pub const PROFILE_TEXT: &str = "ITU-R BS.2168";
pub const PROFILE_NAME: &str = "Advanced sound system: ADM and S-ADM profile for emission";
pub const PROFILE_VERSION: &str = "1";

pub const DEFAULT_MAX_AXML_BYTES: usize = 16 * 1024 * 1024;
pub const HARD_MAX_AXML_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_MAX_CHNA_BYTES: usize = 4 * 1024 * 1024;
pub const HARD_MAX_CHNA_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_XML_NODES: usize = 250_000;
pub const HARD_MAX_XML_NODES: usize = 1_000_000;
pub const DEFAULT_MAX_XML_DEPTH: usize = 64;
pub const HARD_MAX_XML_DEPTH: usize = 256;
pub const DEFAULT_MAX_ATTRIBUTES_PER_ELEMENT: usize = 256;
pub const HARD_MAX_ATTRIBUTES_PER_ELEMENT: usize = 4_096;
pub const DEFAULT_MAX_XML_TEXT_BYTES: usize = 16 * 1024 * 1024;
pub const HARD_MAX_XML_TEXT_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_MAX_REPORT_ITEMS: usize = 32_768;
pub const HARD_MAX_REPORT_ITEMS: usize = 250_000;
pub const DEFAULT_MAX_EVIDENCE_ITEMS: usize = 64;
pub const HARD_MAX_EVIDENCE_ITEMS: usize = 4_096;

const MAX_NAMESPACE_COUNT: usize = 1_024;
const MAX_NAMESPACE_BYTES: usize = 1024 * 1024;
const MAX_NAMESPACE_URI_BYTES: usize = 4 * 1024;
const MAX_EXPANDED_NAME_BYTES: usize = 64 * 1024 * 1024;
const MAX_EVIDENCE_PATH_BYTES: usize = 4 * 1024;
const MAX_EVIDENCE_OBSERVED_BYTES: usize = 4 * 1024;
// `count` in the version-1 JSON schema is bounded to this value.  Derived
// topology counts are capped here so even deliberately non-conformant DAGs
// cannot produce an out-of-schema report value.
const MAX_SERIALIZED_COUNT: usize = 1_000_000;
const REQUIRED_RULE_COUNT: usize = 12;
const XML_NAMESPACE_URI: &str = "http://www.w3.org/XML/1998/namespace";
const SCOPE_NOTE: &str = "File-based ADM metadata and PCM/chna reconciliation only; this report does not render audio or validate S-ADM frame rules.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Level0,
    Level1,
    Level2,
}

impl Level {
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Level0 => 0,
            Self::Level1 => 1,
            Self::Level2 => 2,
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse()
    }
}

impl FromStr for Level {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "0" => Ok(Self::Level0),
            "1" => Ok(Self::Level1),
            "2" => Ok(Self::Level2),
            _ => Err("BS.2168 emission-profile level must be 0, 1, or 2".into()),
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_u8().fmt(formatter)
    }
}

impl Serialize for Level {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u8(self.as_u8())
    }
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Options {
    pub input: PathBuf,
    pub level: Level,
    pub max_axml_bytes: usize,
    pub max_chna_bytes: usize,
    pub max_xml_nodes: usize,
    pub max_xml_depth: usize,
    pub max_attributes_per_element: usize,
    pub max_xml_text_bytes: usize,
    pub max_report_items: usize,
    pub max_evidence_items: usize,
}

impl Options {
    pub fn new(input: impl Into<PathBuf>, level: Level) -> Self {
        Self {
            input: input.into(),
            level,
            max_axml_bytes: DEFAULT_MAX_AXML_BYTES,
            max_chna_bytes: DEFAULT_MAX_CHNA_BYTES,
            max_xml_nodes: DEFAULT_MAX_XML_NODES,
            max_xml_depth: DEFAULT_MAX_XML_DEPTH,
            max_attributes_per_element: DEFAULT_MAX_ATTRIBUTES_PER_ELEMENT,
            max_xml_text_bytes: DEFAULT_MAX_XML_TEXT_BYTES,
            max_report_items: DEFAULT_MAX_REPORT_ITEMS,
            max_evidence_items: DEFAULT_MAX_EVIDENCE_ITEMS,
        }
    }
}

#[derive(Debug, Serialize)]
#[non_exhaustive]
pub struct Limits {
    pub max_axml_bytes: usize,
    pub max_chna_bytes: usize,
    pub max_xml_nodes: usize,
    pub max_xml_depth: usize,
    pub max_attributes_per_element: usize,
    pub max_xml_text_bytes: usize,
    pub max_report_items: usize,
    pub max_evidence_items: usize,
    pub max_programmes: Option<usize>,
    pub max_contents: Option<usize>,
    pub max_objects: Option<usize>,
    pub max_pack_formats: Option<usize>,
    pub max_channel_formats: Option<usize>,
    pub max_track_uids: Option<usize>,
    pub max_non_complementary_tracks: Option<usize>,
    pub max_complementary_groups: Option<usize>,
    pub max_independent_groups: Option<usize>,
    pub max_channels_per_layout: Option<usize>,
    pub max_programme_content_refs: Option<usize>,
    pub max_programme_alternative_value_set_refs: Option<usize>,
    pub max_programme_labels: Option<usize>,
    pub max_content_labels: Option<usize>,
    pub max_object_child_refs: Option<usize>,
    pub max_object_complementary_refs: Option<usize>,
    pub max_object_alternative_value_sets: Option<usize>,
    pub max_object_group_labels: Option<usize>,
}

#[derive(Debug, Default, Serialize)]
#[non_exhaustive]
pub struct Counts {
    pub xml_nodes: usize,
    pub programmes: usize,
    pub contents: usize,
    pub objects: usize,
    pub pack_formats: usize,
    pub matrix_pack_formats: usize,
    pub channel_formats: usize,
    pub matrix_channel_formats: usize,
    pub block_formats: usize,
    pub track_uids: usize,
    pub alternative_value_sets: usize,
    pub complementary_groups: usize,
    pub non_complementary_tracks: usize,
    pub independent_groups: usize,
    pub report_items: usize,
}

#[derive(Debug, Serialize)]
#[non_exhaustive]
pub struct Evidence {
    pub path: String,
    pub observed: String,
}

struct Violations {
    total: usize,
    retained: Vec<Evidence>,
    limit: usize,
}

impl Violations {
    fn new(limit: usize) -> Self {
        Self {
            total: 0,
            retained: Vec::with_capacity(limit.min(256)),
            limit,
        }
    }

    fn push(&mut self, evidence: Evidence) {
        self.total = self.total.saturating_add(1);
        if self.retained.len() < self.limit {
            self.retained.push(evidence);
        }
    }

    fn extend(&mut self, other: Self) {
        self.total = self.total.saturating_add(other.total);
        for evidence in other.retained {
            if self.retained.len() >= self.limit {
                break;
            }
            self.retained.push(evidence);
        }
    }
}

#[derive(Debug, Serialize)]
#[non_exhaustive]
pub struct Rule {
    pub rule_id: &'static str,
    pub authority: &'static str,
    pub section: &'static str,
    pub subject: String,
    pub requirement: String,
    pub observed: String,
    pub passed: bool,
    pub evidence_truncated: bool,
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Serialize)]
#[non_exhaustive]
pub struct Report {
    pub schema: &'static str,
    pub schema_version: u32,
    pub validator: &'static str,
    pub standard: &'static str,
    pub profile_name: &'static str,
    pub profile_version: &'static str,
    pub profile_level: u8,
    pub input_path: String,
    pub input_bytes: u64,
    pub input_sha256: String,
    pub axml_bytes: usize,
    pub chna_bytes: usize,
    pub wave_container: &'static str,
    pub axml_chunks: usize,
    pub chna_chunks: usize,
    pub data_bytes: u64,
    pub ds64_sample_count: Option<u64>,
    pub limits: Limits,
    pub counts: Counts,
    pub passed: bool,
    pub rendered_audio_verified: bool,
    pub scope_note: &'static str,
    pub rules: Vec<Rule>,
}

#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct XmlName {
    namespace: Option<Arc<str>>,
    local: String,
}

#[derive(Clone, Debug)]
struct XmlAttribute {
    name: XmlName,
    value: String,
}

#[derive(Debug, Default)]
struct Node {
    name: XmlName,
    parent: Option<usize>,
    children: Vec<usize>,
    attributes: Vec<XmlAttribute>,
    text: String,
}

#[derive(Debug, Default)]
struct XmlNamePool {
    namespaces: HashSet<Arc<str>>,
    aliases: HashMap<String, Arc<str>>,
    bytes: usize,
    expanded_name_bytes: usize,
}

#[derive(Debug)]
struct ParsedDocument {
    nodes: Vec<Node>,
    afe: Option<usize>,
    afe_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExactTime {
    numerator: u128,
    denominator: u128,
}

impl ExactTime {
    fn new(numerator: u128, denominator: u128) -> Option<Self> {
        if denominator == 0 {
            return None;
        }
        let divisor = gcd(numerator, denominator);
        Some(Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }
}

impl fmt::Display for ExactTime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{} s", self.numerator, self.denominator)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TimeValue {
    Exact(ExactTime),
    Decimal { coefficient: BigNat, scale: usize },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedTime {
    value: TimeValue,
}

impl ParsedTime {
    fn exact(value: ExactTime) -> Self {
        Self {
            value: TimeValue::Exact(value),
        }
    }
}

const BIG_NAT_BASE: u64 = 1_000_000_000;

#[derive(Clone, Debug, Eq, PartialEq)]
struct BigNat(Vec<u32>);

impl BigNat {
    fn zero() -> Self {
        Self(Vec::new())
    }

    fn one() -> Self {
        Self(vec![1])
    }

    fn from_u128(mut value: u128) -> Self {
        let mut digits = Vec::new();
        while value != 0 {
            digits.push((value % u128::from(BIG_NAT_BASE)) as u32);
            value /= u128::from(BIG_NAT_BASE);
        }
        Self(digits)
    }

    fn from_decimal(value: &str) -> Option<Self> {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let mut digits = Vec::with_capacity(value.len().div_ceil(9));
        let mut end = value.len();
        while end != 0 {
            let start = end.saturating_sub(9);
            digits.push(value[start..end].parse().ok()?);
            end = start;
        }
        let mut result = Self(digits);
        result.normalize();
        Some(result)
    }

    fn normalize(&mut self) {
        while self.0.last() == Some(&0) {
            self.0.pop();
        }
    }

    fn add_assign(&mut self, other: &Self) {
        let width = self.0.len().max(other.0.len());
        self.0.resize(width, 0);
        let mut carry = 0_u64;
        for index in 0..width {
            let sum = u64::from(self.0[index])
                + other.0.get(index).copied().map(u64::from).unwrap_or(0)
                + carry;
            self.0[index] = (sum % BIG_NAT_BASE) as u32;
            carry = sum / BIG_NAT_BASE;
        }
        if carry != 0 {
            self.0.push(carry as u32);
        }
    }

    fn mul_assign(&mut self, other: &Self) {
        if self.0.is_empty() || other.0.is_empty() {
            self.0.clear();
            return;
        }
        let mut product = vec![0_u32; self.0.len() + other.0.len()];
        for (left_index, left) in self.0.iter().copied().enumerate() {
            let mut carry = 0_u64;
            for (right_index, right) in other.0.iter().copied().enumerate() {
                let index = left_index + right_index;
                let value = u64::from(product[index]) + u64::from(left) * u64::from(right) + carry;
                product[index] = (value % BIG_NAT_BASE) as u32;
                carry = value / BIG_NAT_BASE;
            }
            product[left_index + other.0.len()] = carry as u32;
        }
        self.0 = product;
        self.normalize();
    }

    fn mul_pow10(&mut self, exponent: usize) {
        if self.0.is_empty() || exponent == 0 {
            return;
        }
        let groups = exponent / 9;
        let remainder = exponent % 9;
        if groups != 0 {
            self.0.splice(0..0, std::iter::repeat_n(0, groups));
        }
        if remainder != 0 {
            self.mul_assign(&Self::from_u128(10_u128.pow(remainder as u32)));
        }
    }

    fn cmp_value(&self, other: &Self) -> Ordering {
        self.0
            .len()
            .cmp(&other.0.len())
            .then_with(|| self.0.iter().rev().cmp(other.0.iter().rev()))
    }

    fn abs_diff(&self, other: &Self) -> Self {
        let (larger, smaller) = if self.cmp_value(other) == Ordering::Less {
            (other, self)
        } else {
            (self, other)
        };
        let mut result = Vec::with_capacity(larger.0.len());
        let mut borrow = 0_i64;
        for index in 0..larger.0.len() {
            let mut value = i64::from(larger.0[index])
                - smaller.0.get(index).copied().map(i64::from).unwrap_or(0)
                - borrow;
            if value < 0 {
                value += BIG_NAT_BASE as i64;
                borrow = 1;
            } else {
                borrow = 0;
            }
            result.push(value as u32);
        }
        let mut result = Self(result);
        result.normalize();
        result
    }
}

#[derive(Clone, Copy)]
struct ProfileLimits {
    programme: Option<usize>,
    content: Option<usize>,
    object: Option<usize>,
    pack: Option<usize>,
    channel: Option<usize>,
    track_uid: Option<usize>,
    non_comp_tracks: Option<usize>,
    comp_groups: Option<usize>,
    independent_groups: Option<usize>,
    channels_layout: Option<usize>,
    apr_aco: Option<usize>,
    apr_label: Option<usize>,
    aco_label: Option<usize>,
    ao_object: Option<usize>,
    ao_comp: Option<usize>,
    ao_avs: Option<usize>,
    ao_label: Option<usize>,
}

impl ProfileLimits {
    fn for_level(level: Level) -> Self {
        let values = match level {
            Level::Level0 => return Self::unlimited(),
            Level::Level1 => [8, 16, 48, 32, 32, 32, 16, 8, 16, 12, 16, 4, 4, 16, 15, 8, 4],
            Level::Level2 => [
                16, 28, 84, 56, 56, 56, 28, 14, 16, 24, 28, 8, 8, 28, 27, 16, 8,
            ],
        };
        Self {
            programme: Some(values[0]),
            content: Some(values[1]),
            object: Some(values[2]),
            pack: Some(values[3]),
            channel: Some(values[4]),
            track_uid: Some(values[5]),
            non_comp_tracks: Some(values[6]),
            comp_groups: Some(values[7]),
            independent_groups: Some(values[8]),
            channels_layout: Some(values[9]),
            apr_aco: Some(values[10]),
            apr_label: Some(values[11]),
            aco_label: Some(values[12]),
            ao_object: Some(values[13]),
            ao_comp: Some(values[14]),
            ao_avs: Some(values[15]),
            ao_label: Some(values[16]),
        }
    }

    const fn unlimited() -> Self {
        Self {
            programme: None,
            content: None,
            object: None,
            pack: None,
            channel: None,
            track_uid: None,
            non_comp_tracks: None,
            comp_groups: None,
            independent_groups: None,
            channels_layout: None,
            apr_aco: None,
            apr_label: None,
            aco_label: None,
            ao_object: None,
            ao_comp: None,
            ao_avs: None,
            ao_label: None,
        }
    }
}

pub fn validate(options: &Options) -> Result<Report, String> {
    validate_options(options)?;
    let input = fs::canonicalize(&options.input)
        .map_err(|error| format!("resolve ADM input {}: {error}", options.input.display()))?;
    let report_input_path = input
        .to_str()
        .ok_or_else(|| "ADM input path is not valid UTF-8 for report serialization".to_string())?
        .to_owned();
    let mut input_file = File::open(&input)
        .map_err(|error| format!("open ADM input {}: {error}", input.display()))?;
    ensure_regular_file(&input_file, &input)?;
    let (input_sha256, input_bytes) = sha256_file(&mut input_file, &input)?;
    let wave = super::wave_input::read_from(
        &mut input_file,
        &input,
        options.max_axml_bytes,
        options.max_chna_bytes,
    )?;
    if wave.file_bytes != input_bytes {
        return Err("ADM input changed size between hashing and WAVE scanning".into());
    }
    let parsed = match wave.axml.as_deref() {
        Some(xml) => parse_xml(xml, options)?,
        None => ParsedDocument {
            nodes: Vec::new(),
            afe: None,
            afe_count: 0,
        },
    };
    let essence = essence_info(wave.pcm, wave.data_size, wave.ds64_sample_count)?;
    let profile_limits = ProfileLimits::for_level(options.level);
    let mut audit = Audit::new(
        &parsed,
        options.max_evidence_items,
        options.max_report_items,
    );
    audit.run(
        options.level,
        profile_limits,
        wave.chna.as_deref().unwrap_or_default(),
        essence,
        wave.chna_count,
    );
    ensure_unchanged(&mut input_file, &input, &input_sha256, input_bytes)?;
    if let Some(error) = audit.operational_error.take() {
        return Err(error);
    }
    let counts = audit.counts;
    let passed = audit.rules.iter().all(|rule| rule.passed);
    Ok(Report {
        schema: REPORT_SCHEMA,
        schema_version: SCHEMA_VERSION,
        validator: VALIDATOR,
        standard: STANDARD,
        profile_name: PROFILE_NAME,
        profile_version: PROFILE_VERSION,
        profile_level: options.level.as_u8(),
        input_path: report_input_path,
        input_bytes,
        input_sha256,
        axml_bytes: wave.axml.as_ref().map_or(0, Vec::len),
        chna_bytes: wave.chna.as_ref().map_or(0, Vec::len),
        wave_container: wave.container.as_str(),
        axml_chunks: wave.axml_count,
        chna_chunks: wave.chna_count,
        data_bytes: wave.data_size,
        ds64_sample_count: wave.ds64_sample_count,
        limits: report_limits(options, profile_limits),
        counts,
        passed,
        rendered_audio_verified: false,
        scope_note: SCOPE_NOTE,
        rules: audit.rules,
    })
}

pub fn write_report(
    path: &Path,
    report: &Report,
    compact: bool,
    overwrite: bool,
) -> Result<(), String> {
    if paths_identify_same_existing_file(Path::new(&report.input_path), path)? {
        return Err(format!(
            "refusing to replace audited ADM input {} with its report",
            report.input_path
        ));
    }
    if path.exists() && !overwrite {
        return Err(format!(
            "refusing to replace existing ADM emission report {}; pass --overwrite",
            path.display()
        ));
    }
    let mut bytes = if compact {
        serde_json::to_vec(report)
    } else {
        serde_json::to_vec_pretty(report)
    }
    .map_err(|error| format!("serialize ADM emission report: {error}"))?;
    bytes.push(b'\n');
    let mut output = crate::atomic::AtomicOutput::new(path)?;
    output.write_all(&bytes)?;
    if overwrite {
        output.commit()
    } else {
        output.commit_noclobber()
    }
}

fn paths_identify_same_existing_file(input: &Path, output: &Path) -> Result<bool, String> {
    let canonical_output = match fs::canonicalize(output) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "resolve report output {}: {error}",
                output.display()
            ))
        }
    };
    let canonical_input = fs::canonicalize(input)
        .map_err(|error| format!("resolve audited ADM input {}: {error}", input.display()))?;
    if canonical_input == canonical_output {
        return Ok(true);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let input_metadata = fs::metadata(&canonical_input).map_err(|error| {
            format!(
                "stat audited ADM input {}: {error}",
                canonical_input.display()
            )
        })?;
        let output_metadata = fs::metadata(&canonical_output).map_err(|error| {
            format!("stat report output {}: {error}", canonical_output.display())
        })?;
        if input_metadata.dev() == output_metadata.dev()
            && input_metadata.ino() == output_metadata.ino()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn report_limits(options: &Options, limits: ProfileLimits) -> Limits {
    Limits {
        max_axml_bytes: options.max_axml_bytes,
        max_chna_bytes: options.max_chna_bytes,
        max_xml_nodes: options.max_xml_nodes,
        max_xml_depth: options.max_xml_depth,
        max_attributes_per_element: options.max_attributes_per_element,
        max_xml_text_bytes: options.max_xml_text_bytes,
        max_report_items: options.max_report_items,
        max_evidence_items: options.max_evidence_items,
        max_programmes: limits.programme,
        max_contents: limits.content,
        max_objects: limits.object,
        max_pack_formats: limits.pack,
        max_channel_formats: limits.channel,
        max_track_uids: limits.track_uid,
        max_non_complementary_tracks: limits.non_comp_tracks,
        max_complementary_groups: limits.comp_groups,
        max_independent_groups: limits.independent_groups,
        max_channels_per_layout: limits.channels_layout,
        max_programme_content_refs: limits.apr_aco,
        max_programme_alternative_value_set_refs: limits.apr_aco,
        max_programme_labels: limits.apr_label,
        max_content_labels: limits.aco_label,
        max_object_child_refs: limits.ao_object,
        max_object_complementary_refs: limits.ao_comp,
        max_object_alternative_value_sets: limits.ao_avs,
        max_object_group_labels: limits.ao_label,
    }
}

fn validate_options(options: &Options) -> Result<(), String> {
    for (name, value, hard) in [
        (
            "max_axml_bytes",
            options.max_axml_bytes,
            HARD_MAX_AXML_BYTES,
        ),
        (
            "max_chna_bytes",
            options.max_chna_bytes,
            HARD_MAX_CHNA_BYTES,
        ),
        ("max_xml_nodes", options.max_xml_nodes, HARD_MAX_XML_NODES),
        ("max_xml_depth", options.max_xml_depth, HARD_MAX_XML_DEPTH),
        (
            "max_attributes_per_element",
            options.max_attributes_per_element,
            HARD_MAX_ATTRIBUTES_PER_ELEMENT,
        ),
        (
            "max_xml_text_bytes",
            options.max_xml_text_bytes,
            HARD_MAX_XML_TEXT_BYTES,
        ),
        (
            "max_report_items",
            options.max_report_items,
            HARD_MAX_REPORT_ITEMS,
        ),
        (
            "max_evidence_items",
            options.max_evidence_items,
            HARD_MAX_EVIDENCE_ITEMS,
        ),
    ] {
        let minimum = if name == "max_report_items" {
            REQUIRED_RULE_COUNT
        } else {
            1
        };
        if value < minimum || value > hard {
            return Err(format!("{name} must be between {minimum} and {hard}"));
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct EssenceInfo {
    channels: u16,
    sample_rate: u32,
    container_bit_depth: u16,
    valid_bit_depth: u16,
    duration: ExactTime,
    aligned: bool,
    integer_pcm: bool,
    probe_data_size_matches: bool,
    ds64_sample_count_matches: bool,
}

fn essence_info(
    pcm: super::wave_input::PcmGeometry,
    scanned_data_size: u64,
    ds64_sample_count: Option<u64>,
) -> Result<EssenceInfo, String> {
    let frame_bytes = u64::from(pcm.block_align);
    if frame_bytes == 0 || pcm.sample_rate == 0 {
        return Err("PCM essence has zero frame size or sample rate".into());
    }
    let frames = scanned_data_size / frame_bytes;
    Ok(EssenceInfo {
        channels: pcm.channels,
        sample_rate: pcm.sample_rate,
        container_bit_depth: pcm.container_bits_per_sample,
        valid_bit_depth: pcm.valid_bits_per_sample,
        duration: ExactTime::new(u128::from(frames), u128::from(pcm.sample_rate)).unwrap(),
        aligned: scanned_data_size.is_multiple_of(frame_bytes),
        integer_pcm: true,
        probe_data_size_matches: true,
        ds64_sample_count_matches: ds64_sample_count.is_none_or(|count| count == frames),
    })
}

fn parse_xml(xml: &[u8], options: &Options) -> Result<ParsedDocument, String> {
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().trim_text(false);
    reader.config_mut().check_comments = true;
    let mut nodes = Vec::<Node>::new();
    let mut stack = Vec::<usize>::new();
    let mut roots = 0_usize;
    let mut declaration_seen = false;
    let mut content_seen = false;
    let mut text_bytes = 0_usize;
    let mut pool = XmlNamePool::default();
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                if stack.len() >= options.max_xml_depth {
                    return Err(format!(
                        "XML nesting depth exceeds {}",
                        options.max_xml_depth
                    ));
                }
                if stack.is_empty() {
                    roots = roots
                        .checked_add(1)
                        .ok_or_else(|| "XML root count overflow".to_string())?;
                    if roots > 1 {
                        return Err("XML document contains more than one root element".into());
                    }
                }
                content_seen = true;
                let index = push_node(
                    &reader,
                    &element,
                    stack.last().copied(),
                    &mut nodes,
                    &mut pool,
                    options,
                )?;
                stack.push(index);
            }
            Ok(Event::Empty(element)) => {
                if stack.len() >= options.max_xml_depth {
                    return Err(format!(
                        "XML nesting depth exceeds {}",
                        options.max_xml_depth
                    ));
                }
                if stack.is_empty() {
                    roots = roots
                        .checked_add(1)
                        .ok_or_else(|| "XML root count overflow".to_string())?;
                    if roots > 1 {
                        return Err("XML document contains more than one root element".into());
                    }
                }
                content_seen = true;
                push_node(
                    &reader,
                    &element,
                    stack.last().copied(),
                    &mut nodes,
                    &mut pool,
                    options,
                )?;
            }
            Ok(Event::Text(text)) => {
                content_seen = true;
                if text.as_ref().contains("]]>") {
                    return Err(
                        "XML character data shall not contain the literal ]]> sequence".into(),
                    );
                }
                let encoded = text.xml_content(XmlVersion::Implicit1_0);
                let value = quick_xml::escape::unescape(&encoded)
                    .map_err(|error| format!("XML entity: {error}"))?;
                validate_xml_chars(&value, "text")?;
                if let Some(parent) = stack.last().copied() {
                    add_text(&mut nodes[parent].text, &value, &mut text_bytes, options)?;
                } else if !value.chars().all(is_xml_space) {
                    return Err("XML document contains non-whitespace text outside its root".into());
                }
            }
            Ok(Event::CData(text)) => {
                content_seen = true;
                let parent = stack
                    .last()
                    .copied()
                    .ok_or_else(|| "XML document contains CDATA outside its root".to_string())?;
                let value = text.xml_content(XmlVersion::Implicit1_0);
                validate_xml_chars(&value, "CDATA")?;
                add_text(&mut nodes[parent].text, &value, &mut text_bytes, options)?;
            }
            Ok(Event::GeneralRef(reference)) => {
                content_seen = true;
                let parent = stack.last().copied().ok_or_else(|| {
                    "XML document contains an entity reference outside its root".to_string()
                })?;
                let encoded = format!("&{};", reference.xml_content(XmlVersion::Implicit1_0));
                let value = quick_xml::escape::unescape(&encoded)
                    .map_err(|error| format!("XML entity: {error}"))?;
                validate_xml_chars(&value, "entity reference")?;
                add_text(&mut nodes[parent].text, &value, &mut text_bytes, options)?;
            }
            Ok(Event::Decl(declaration)) => {
                if declaration_seen || content_seen || !stack.is_empty() {
                    return Err(
                        "XML declaration shall occur once, at the start of the document".into(),
                    );
                }
                validate_declaration(&declaration)?;
                declaration_seen = true;
                content_seen = true;
            }
            Ok(Event::DocType(_)) => return Err("XML document types are not accepted".into()),
            Ok(Event::End(element)) => {
                content_seen = true;
                let index = stack
                    .pop()
                    .ok_or_else(|| "closing element without an open element".to_string())?;
                validate_xml_qname(element.name().as_ref(), "closing element")?;
                let actual = resolve_element_name(&reader, element.name(), &mut pool)?;
                if nodes[index].name != actual {
                    return Err(format!(
                        "closing element {} does not match {}",
                        actual.local, nodes[index].name.local
                    ));
                }
            }
            Ok(Event::Comment(comment)) => {
                content_seen = true;
                let value = comment.xml_content(XmlVersion::Implicit1_0);
                validate_xml_chars(&value, "comment")?;
                add_text_bytes(&mut text_bytes, value.len(), options)?;
            }
            Ok(Event::PI(instruction)) => {
                content_seen = true;
                validate_xml_name(instruction.target(), "processing-instruction target")?;
                if instruction.target().eq_ignore_ascii_case("xml") {
                    return Err("processing-instruction target shall not be XML".into());
                }
                validate_xml_chars(instruction.content(), "processing instruction")?;
                add_text_bytes(&mut text_bytes, instruction.content().len(), options)?;
            }
            Ok(Event::Eof) => break,
            Err(error) => return Err(format!("XML at byte {}: {error}", reader.error_position())),
        }
    }
    if !stack.is_empty() {
        return Err("XML ended with unclosed elements".into());
    }
    if roots != 1 {
        return Err(format!(
            "XML document shall contain exactly one root element, found {roots}"
        ));
    }
    let candidates = nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| node.name.local == "audioFormatExtended")
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let identified = candidates
        .iter()
        .copied()
        .filter(|index| is_adm_afe_candidate(&nodes, *index))
        .collect::<Vec<_>>();
    // Wrapper formats may contain unrelated vocabularies. Once an ADM identity
    // selects a namespace, retain every AFE in that expanded namespace: a
    // malformed duplicate must not be hidden by its valid sibling. Foreign
    // same-local-name elements remain outside the ADM candidate population.
    let resolved = if identified.is_empty() {
        candidates
    } else {
        let namespaces = identified
            .iter()
            .map(|index| nodes[*index].name.namespace.clone())
            .collect::<HashSet<_>>();
        candidates
            .into_iter()
            .filter(|index| namespaces.contains(&nodes[*index].name.namespace))
            .collect()
    };
    Ok(ParsedDocument {
        afe: (resolved.len() == 1).then(|| resolved[0]),
        afe_count: resolved.len(),
        nodes,
    })
}

fn is_adm_afe_candidate(nodes: &[Node], index: usize) -> bool {
    let afe = &nodes[index];
    if attr(afe, "version") == Some("ITU-R_BS.2076-3") {
        return true;
    }
    afe.children.iter().copied().any(|profile_list| {
        let profile_list = &nodes[profile_list];
        profile_list.name.namespace == afe.name.namespace
            && profile_list.name.local == "profileList"
            && profile_list.children.iter().copied().any(|profile| {
                let profile = &nodes[profile];
                profile.name.namespace == afe.name.namespace
                    && profile.name.local == "profile"
                    && attr(profile, "profileName") == Some(PROFILE_NAME)
                    && attr(profile, "profileVersion") == Some(PROFILE_VERSION)
                    && trim_xml(&profile.text) == PROFILE_TEXT
            })
    })
}

fn push_node(
    reader: &NsReader<&[u8]>,
    element: &quick_xml::events::BytesStart<'_>,
    parent: Option<usize>,
    nodes: &mut Vec<Node>,
    pool: &mut XmlNamePool,
    options: &Options,
) -> Result<usize, String> {
    if nodes.len() >= options.max_xml_nodes {
        return Err(format!(
            "XML element count exceeds {}",
            options.max_xml_nodes
        ));
    }
    validate_xml_qname(element.name().as_ref(), "element")?;
    register_namespaces(element, pool, options)?;
    let name = resolve_element_name(reader, element.name(), pool)?;
    let mut attributes = Vec::new();
    let mut expanded = HashSet::new();
    for (offset, attribute) in element.attributes().enumerate() {
        if offset >= options.max_attributes_per_element {
            return Err(format!(
                "XML element {} contains too many attributes",
                name.local
            ));
        }
        let attribute = attribute.map_err(|error| format!("XML attribute: {error}"))?;
        let raw_name = attribute.key.as_ref();
        validate_xml_qname(raw_name, "attribute")?;
        if raw_name == "xmlns" || raw_name.starts_with("xmlns:") {
            continue;
        }
        validate_xml_attribute_lexical_value(attribute.value.as_ref(), "attribute value")?;
        let attr_name = resolve_attribute_name(reader, attribute.key, pool)?;
        if !expanded.insert(attr_name.clone()) {
            return Err(format!(
                "XML repeats expanded attribute {}",
                attr_name.local
            ));
        }
        let value = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|error| format!("XML attribute {}: {error}", attr_name.local))?
            .into_owned();
        validate_xml_chars(&value, "attribute value")?;
        attributes.push(XmlAttribute {
            name: attr_name,
            value,
        });
    }
    let index = nodes.len();
    nodes.push(Node {
        name,
        parent,
        children: Vec::new(),
        attributes,
        text: String::new(),
    });
    if let Some(parent) = parent {
        nodes[parent].children.push(index);
    }
    Ok(index)
}

fn register_namespaces(
    element: &quick_xml::events::BytesStart<'_>,
    pool: &mut XmlNamePool,
    options: &Options,
) -> Result<(), String> {
    for (offset, attribute) in element.attributes().enumerate() {
        if offset >= options.max_attributes_per_element {
            return Err("XML element contains too many attributes".into());
        }
        let attribute = attribute.map_err(|error| format!("XML attribute: {error}"))?;
        let raw_name = attribute.key.as_ref();
        validate_xml_qname(raw_name, "attribute")?;
        let prefix = if raw_name == "xmlns" {
            Some(None)
        } else {
            raw_name.strip_prefix("xmlns:").map(Some)
        };
        let Some(prefix) = prefix else { continue };
        validate_xml_attribute_lexical_value(attribute.value.as_ref(), "namespace declaration")?;
        let normalized = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|error| format!("XML namespace declaration: {error}"))?;
        validate_xml_chars(&normalized, "namespace URI")?;
        validate_namespace_declaration(prefix, &normalized)?;
        pool.register(attribute.value.as_ref(), &normalized)?;
    }
    Ok(())
}

impl XmlNamePool {
    fn intern(&mut self, uri: &str) -> Result<Option<Arc<str>>, String> {
        if uri.is_empty() {
            return Ok(None);
        }
        if uri.len() > MAX_NAMESPACE_URI_BYTES {
            return Err("XML namespace URI exceeds safety limit".into());
        }
        if let Some(value) = self.namespaces.get(uri) {
            return Ok(Some(value.clone()));
        }
        if self.namespaces.len() >= MAX_NAMESPACE_COUNT {
            return Err("XML namespace count exceeds safety limit".into());
        }
        self.bytes = self
            .bytes
            .checked_add(uri.len())
            .ok_or_else(|| "namespace byte count overflow".to_string())?;
        if self.bytes > MAX_NAMESPACE_BYTES {
            return Err("XML namespace data exceeds safety limit".into());
        }
        let value = Arc::<str>::from(uri);
        self.namespaces.insert(value.clone());
        Ok(Some(value))
    }

    fn register(&mut self, raw: &str, normalized: &str) -> Result<(), String> {
        let Some(value) = self.intern(normalized)? else {
            return Ok(());
        };
        if raw != normalized && !self.aliases.contains_key(raw) {
            if self.aliases.len() >= MAX_NAMESPACE_COUNT {
                return Err("XML namespace alias count exceeds safety limit".into());
            }
            self.bytes = self
                .bytes
                .checked_add(raw.len())
                .ok_or_else(|| "namespace byte count overflow".to_string())?;
            if self.bytes > MAX_NAMESPACE_BYTES {
                return Err("XML namespace data exceeds safety limit".into());
            }
            self.aliases.insert(raw.to_owned(), value);
        }
        Ok(())
    }

    fn resolve(&mut self, raw: &str) -> Result<Option<Arc<str>>, String> {
        if let Some(value) = self.aliases.get(raw) {
            return Ok(Some(value.clone()));
        }
        self.intern(raw)
    }

    fn expanded_name(
        &mut self,
        namespace: Option<Arc<str>>,
        local: &str,
    ) -> Result<XmlName, String> {
        let bytes = local
            .len()
            .checked_add(namespace.as_ref().map_or(0, |uri| uri.len()))
            .ok_or_else(|| "XML expanded-name byte count overflow".to_string())?;
        self.expanded_name_bytes = self
            .expanded_name_bytes
            .checked_add(bytes)
            .ok_or_else(|| "XML expanded-name byte count overflow".to_string())?;
        if self.expanded_name_bytes > MAX_EXPANDED_NAME_BYTES {
            return Err("XML expanded-name data exceeds safety limit".into());
        }
        Ok(XmlName {
            namespace,
            local: local.to_owned(),
        })
    }
}

fn resolve_element_name(
    reader: &NsReader<&[u8]>,
    name: quick_xml::name::QName<'_>,
    pool: &mut XmlNamePool,
) -> Result<XmlName, String> {
    let (namespace, local) = reader.resolver().resolve_element(name);
    resolved_name(namespace, local.as_ref(), pool)
}

fn resolve_attribute_name(
    reader: &NsReader<&[u8]>,
    name: quick_xml::name::QName<'_>,
    pool: &mut XmlNamePool,
) -> Result<XmlName, String> {
    let (namespace, local) = reader.resolver().resolve_attribute(name);
    resolved_name(namespace, local.as_ref(), pool)
}

fn resolved_name(
    namespace: ResolveResult<'_>,
    local: &str,
    pool: &mut XmlNamePool,
) -> Result<XmlName, String> {
    let namespace = match namespace {
        ResolveResult::Unbound => None,
        ResolveResult::Bound(value) => pool.resolve(value.as_ref())?,
        ResolveResult::Unknown(prefix) => {
            return Err(format!("XML uses undeclared namespace prefix {prefix}"))
        }
    };
    pool.expanded_name(namespace, local)
}

fn add_text(
    target: &mut String,
    value: &str,
    total: &mut usize,
    options: &Options,
) -> Result<(), String> {
    add_text_bytes(total, value.len(), options)?;
    target.push_str(value);
    Ok(())
}

fn add_text_bytes(total: &mut usize, bytes: usize, options: &Options) -> Result<(), String> {
    *total = total
        .checked_add(bytes)
        .ok_or_else(|| "XML text byte count overflow".to_string())?;
    if *total > options.max_xml_text_bytes {
        return Err(format!(
            "XML text data exceeds {} bytes",
            options.max_xml_text_bytes
        ));
    }
    Ok(())
}

fn validate_declaration(declaration: &quick_xml::events::BytesDecl<'_>) -> Result<(), String> {
    let mut stage = 0_u8;
    let pseudo = quick_xml::events::BytesStart::from_content(declaration.as_ref(), "xml".len());
    for attribute in pseudo.attributes() {
        let attribute = attribute.map_err(|error| format!("XML declaration: {error}"))?;
        let name = attribute.key.as_ref();
        validate_xml_attribute_lexical_value(attribute.value.as_ref(), "XML declaration")?;
        if attribute.value.contains('&') {
            return Err(format!(
                "XML declaration {name} shall not contain an entity reference"
            ));
        }
        match (stage, name, attribute.value.as_ref()) {
            (0, "version", "1.0") => stage = 1,
            (0, "version", _) => return Err("XML declaration version shall be 1.0".into()),
            (1, "encoding", value) if value.eq_ignore_ascii_case("UTF-8") => stage = 2,
            (1 | 2, "standalone", "yes" | "no") => stage = 3,
            _ => {
                return Err(format!(
                    "XML declaration contains unknown, repeated, or out-of-order attribute {name}"
                ))
            }
        }
    }
    if stage == 0 {
        return Err("XML declaration shall begin with version=\"1.0\"".into());
    }
    Ok(())
}

fn validate_xml_chars(value: &str, context: &str) -> Result<(), String> {
    if let Some(ch) = value.chars().find(|ch| !is_xml_10_char(*ch)) {
        return Err(format!(
            "{context} contains invalid XML 1.0 character U+{:04X}",
            u32::from(ch)
        ));
    }
    Ok(())
}

fn validate_xml_attribute_lexical_value(value: &str, context: &str) -> Result<(), String> {
    validate_xml_chars(value, context)?;
    if value.contains('<') {
        return Err(format!("{context} contains a literal < character"));
    }
    Ok(())
}

fn is_xml_10_char(value: char) -> bool {
    matches!(value, '\u{9}' | '\u{A}' | '\u{D}')
        || ('\u{20}'..='\u{D7FF}').contains(&value)
        || ('\u{E000}'..='\u{FFFD}').contains(&value)
        || ('\u{10000}'..='\u{10FFFF}').contains(&value)
}

fn is_xml_space(value: char) -> bool {
    matches!(value, ' ' | '\t' | '\r' | '\n')
}

fn validate_xml_qname(value: &str, context: &str) -> Result<(), String> {
    let mut parts = value.split(':');
    let first = parts.next().unwrap_or_default();
    let second = parts.next();
    if parts.next().is_some()
        || !is_xml_ncname(first)
        || second.is_some_and(|part| !is_xml_ncname(part))
    {
        return Err(format!("{context} name {value:?} is not a valid XML QName"));
    }
    Ok(())
}

fn validate_xml_name(value: &str, context: &str) -> Result<(), String> {
    let mut chars = value.chars();
    if !chars.next().is_some_and(is_xml_name_start) || !chars.all(is_xml_name_char) {
        return Err(format!("{context} {value:?} is not a valid XML Name"));
    }
    Ok(())
}

fn is_xml_ncname(value: &str) -> bool {
    let mut chars = value.chars();
    chars.next().is_some_and(is_xml_ncname_start) && chars.all(is_xml_ncname_char)
}

fn is_xml_name_start(ch: char) -> bool {
    ch == ':' || is_xml_ncname_start(ch)
}
fn is_xml_name_char(ch: char) -> bool {
    ch == ':' || is_xml_ncname_char(ch)
}
fn is_xml_ncname_start(ch: char) -> bool {
    ch == '_'
        || ch.is_ascii_alphabetic()
        || ('\u{C0}'..='\u{D6}').contains(&ch)
        || ('\u{D8}'..='\u{F6}').contains(&ch)
        || ('\u{F8}'..='\u{2FF}').contains(&ch)
        || ('\u{370}'..='\u{37D}').contains(&ch)
        || ('\u{37F}'..='\u{1FFF}').contains(&ch)
        || ('\u{200C}'..='\u{200D}').contains(&ch)
        || ('\u{2070}'..='\u{218F}').contains(&ch)
        || ('\u{2C00}'..='\u{2FEF}').contains(&ch)
        || ('\u{3001}'..='\u{D7FF}').contains(&ch)
        || ('\u{F900}'..='\u{FDCF}').contains(&ch)
        || ('\u{FDF0}'..='\u{FFFD}').contains(&ch)
        || ('\u{10000}'..='\u{EFFFF}').contains(&ch)
}
fn is_xml_ncname_char(ch: char) -> bool {
    is_xml_ncname_start(ch)
        || matches!(ch, '-' | '.' | '\u{B7}')
        || ch.is_ascii_digit()
        || ('\u{300}'..='\u{36F}').contains(&ch)
        || ('\u{203F}'..='\u{2040}').contains(&ch)
}

fn validate_namespace_declaration(prefix: Option<&str>, uri: &str) -> Result<(), String> {
    const XMLNS: &str = "http://www.w3.org/2000/xmlns/";
    if uri == XMLNS {
        return Err("the xmlns namespace URI shall not be declared".into());
    }
    match prefix {
        Some("xmlns") => return Err("the xmlns prefix shall not be declared".into()),
        Some("xml") if uri != XML_NAMESPACE_URI => {
            return Err("the xml prefix shall bind only the XML namespace URI".into())
        }
        Some("xml") => {}
        Some(_) if uri.is_empty() => {
            return Err("a namespace prefix shall not bind an empty URI".into())
        }
        _ if uri == XML_NAMESPACE_URI => {
            return Err("the XML namespace URI shall bind only the xml prefix".into())
        }
        _ => {}
    }
    Ok(())
}

fn ensure_regular_file(file: &File, path: &Path) -> Result<(), String> {
    let meta = file
        .metadata()
        .map_err(|error| format!("stat ADM input {}: {error}", path.display()))?;
    if !meta.file_type().is_file() {
        return Err("ADM input must be a regular file".into());
    }
    Ok(())
}

fn sha256_file(file: &mut File, path: &Path) -> Result<(String, u64), String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut bytes = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| "file length overflow".to_string())?;
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok((hex, bytes))
}

fn ensure_unchanged(
    file: &mut File,
    path: &Path,
    expected_hash: &str,
    expected_bytes: u64,
) -> Result<(), String> {
    let (hash, bytes) = sha256_file(file, path)?;
    if hash != expected_hash || bytes != expected_bytes {
        return Err("ADM input changed while it was being audited".into());
    }
    Ok(())
}

fn gcd(mut left: u128, mut right: u128) -> u128 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

struct Audit<'a> {
    document: &'a ParsedDocument,
    afe_namespace: Option<Arc<str>>,
    path_ordinals: Vec<usize>,
    in_afe: Vec<bool>,
    max_evidence: usize,
    max_report_items: usize,
    emitted_items: usize,
    operational_error: Option<String>,
    rules: Vec<Rule>,
    counts: Counts,
}

impl<'a> Audit<'a> {
    fn new(document: &'a ParsedDocument, max_evidence: usize, max_report_items: usize) -> Self {
        let path_ordinals = build_path_ordinals(document);
        let mut in_afe = vec![false; document.nodes.len()];
        if let Some(afe) = document.afe {
            for index in 0..document.nodes.len() {
                in_afe[index] = index == afe
                    || document.nodes[index]
                        .parent
                        .is_some_and(|parent| in_afe[parent]);
            }
        }
        Self {
            document,
            afe_namespace: document
                .afe
                .and_then(|index| document.nodes[index].name.namespace.clone()),
            path_ordinals,
            in_afe,
            max_evidence,
            max_report_items,
            emitted_items: 0,
            operational_error: None,
            rules: Vec::new(),
            counts: Counts {
                xml_nodes: document.nodes.len(),
                ..Counts::default()
            },
        }
    }

    fn run(
        &mut self,
        level: Level,
        limits: ProfileLimits,
        chna: &[u8],
        essence: EssenceInfo,
        chna_count: usize,
    ) {
        let mut location_errors = Violations::new(self.max_evidence);
        if self.document.afe_count != 1 {
            location_errors.push(Evidence {
                path: "/".into(),
                observed: format!(
                    "found {} audioFormatExtended element(s)",
                    self.document.afe_count
                ),
            });
        }
        self.push_rule(
            "BS2168-3-AFE-LOCATION", "§ 3", "/",
            "the XML document contains exactly one emission-profile audioFormatExtended element at any wrapper depth",
            location_errors,
        );
        let Some(afe) = self.document.afe else {
            self.push_rule_with_authority(
                "BS2088-8-9-CHNA-CARRIER",
                "ITU-R BS.2088-2",
                "§§ 8–9",
                "/chna",
                "a structurally valid chna carrier is present when required",
                self.chna_carrier_errors(chna, essence, chna_count),
            );
            self.finish_counts();
            return;
        };

        self.collect_counts(afe);
        self.audit_structure(afe);
        self.audit_profile(afe, level);
        let definitions = Definitions::build(
            self.document,
            afe,
            self.afe_namespace.as_ref(),
            self.max_evidence,
        );
        self.audit_ids(&definitions);
        self.audit_graph(&definitions);
        self.audit_limits(afe, limits, &definitions);
        self.audit_interactivity(&definitions);
        self.audit_packs_channels(&definitions);
        self.audit_blocks(&definitions, essence);
        self.audit_tracks_chna(&definitions, chna, essence, chna_count);
        self.finish_counts();
    }

    fn node(&self, index: usize) -> &Node {
        &self.document.nodes[index]
    }

    fn is_adm_node(&self, index: usize) -> bool {
        self.node(index).name.namespace.as_ref() == self.afe_namespace.as_ref()
    }

    fn children(&self, parent: usize, name: &str) -> Vec<usize> {
        self.node(parent)
            .children
            .iter()
            .copied()
            .filter(|index| {
                self.is_adm_node(*index) && canonical_name(&self.node(*index).name.local) == name
            })
            .collect()
    }

    fn descendants(&self, root: usize) -> impl Iterator<Item = usize> + '_ {
        debug_assert_eq!(self.document.afe, Some(root));
        self.in_afe
            .iter()
            .enumerate()
            .filter(|(_, included)| **included)
            .map(|(index, _)| index)
    }

    fn path(&self, index: usize) -> String {
        bounded_node_path(self.document, &self.path_ordinals, index)
    }

    fn push_rule(
        &mut self,
        rule_id: &'static str,
        section: &'static str,
        subject: impl Into<String>,
        requirement: impl Into<String>,
        errors: Violations,
    ) {
        self.push_rule_with_authority(rule_id, STANDARD, section, subject, requirement, errors);
    }

    fn push_rule_with_authority(
        &mut self,
        rule_id: &'static str,
        authority: &'static str,
        section: &'static str,
        subject: impl Into<String>,
        requirement: impl Into<String>,
        mut errors: Violations,
    ) {
        if self.emitted_items >= self.max_report_items {
            return;
        }
        errors.retained.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then(left.observed.cmp(&right.observed))
        });
        errors
            .retained
            .dedup_by(|left, right| left.path == right.path && left.observed == right.observed);
        let total = errors.total;
        let remaining_evidence = self.max_report_items.saturating_sub(
            self.emitted_items
                .saturating_add(1)
                .saturating_add(REQUIRED_RULE_COUNT.saturating_sub(self.rules.len() + 1)),
        );
        errors
            .retained
            .truncate(self.max_evidence.min(remaining_evidence));
        self.emitted_items = self.emitted_items.saturating_add(1 + errors.retained.len());
        self.rules.push(Rule {
            rule_id,
            authority,
            section,
            subject: subject.into(),
            requirement: requirement.into(),
            observed: if total == 0 {
                "conformant".into()
            } else {
                format!("{total} violation(s)")
            },
            passed: total == 0,
            evidence_truncated: total > errors.retained.len(),
            evidence: errors.retained,
        });
    }

    fn collect_counts(&mut self, afe: usize) {
        self.counts.programmes = self.children(afe, "audioProgramme").len();
        self.counts.contents = self.children(afe, "audioContent").len();
        self.counts.objects = self.children(afe, "audioObject").len();
        let packs = self.children(afe, "audioPackFormat");
        self.counts.matrix_pack_formats = packs
            .iter()
            .filter(|index| node_type(self.node(**index)) == Some("0002"))
            .count();
        self.counts.pack_formats = packs.len().saturating_sub(self.counts.matrix_pack_formats);
        let channels = self.children(afe, "audioChannelFormat");
        self.counts.matrix_channel_formats = channels
            .iter()
            .filter(|index| node_type(self.node(**index)) == Some("0002"))
            .count();
        self.counts.channel_formats = channels
            .len()
            .saturating_sub(self.counts.matrix_channel_formats);
        self.counts.track_uids = self.children(afe, "audioTrackUID").len();
        self.counts.alternative_value_sets = self
            .descendants(afe)
            .filter(|index| canonical_name(&self.node(*index).name.local) == "alternativeValueSet")
            .count();
        self.counts.block_formats = self
            .descendants(afe)
            .filter(|index| canonical_name(&self.node(*index).name.local) == "audioBlockFormat")
            .count();
    }

    fn finish_counts(&mut self) {
        self.counts.report_items = self.rules.len()
            + self
                .rules
                .iter()
                .map(|rule| rule.evidence.len())
                .sum::<usize>();
    }

    fn audit_structure(&mut self, afe: usize) {
        let mut errors = Violations::new(self.max_evidence);
        for index in self.descendants(afe) {
            let node = self.node(index);
            if !self.is_adm_node(index) {
                errors.push(self.evidence(
                    index,
                    "foreign-namespace element inside audioFormatExtended",
                ));
                continue;
            }
            let name = canonical_name(&node.name.local);
            if is_container_name(name) && !trim_xml(&node.text).is_empty() {
                errors.push(self.evidence(
                    index,
                    "container element contains non-whitespace mixed character data",
                ));
            }
            if index == afe {
                self.check_attributes(index, &["version"], &["version"], &mut errors);
                self.check_children(
                    index,
                    &[
                        ("audioProgramme", 1, None),
                        ("audioContent", 1, None),
                        ("audioObject", 1, None),
                        ("audioPackFormat", 0, None),
                        ("audioChannelFormat", 0, None),
                        ("audioTrackUID", 1, None),
                        ("audioTrackFormat", 0, Some(0)),
                        ("audioStreamFormat", 0, Some(0)),
                        ("profileList", 1, Some(1)),
                    ],
                    &mut errors,
                );
                if attr(node, "version") != Some("ITU-R_BS.2076-3") {
                    errors.push(self.evidence(index, "version shall equal ITU-R_BS.2076-3"));
                }
                continue;
            }
            match name {
                "audioProgramme" => {
                    self.check_attributes(
                        index,
                        &[
                            "audioProgrammeID",
                            "audioProgrammeName",
                            "audioProgrammeLanguage",
                        ],
                        &[
                            "audioProgrammeID",
                            "audioProgrammeName",
                            "audioProgrammeLanguage",
                        ],
                        &mut errors,
                    );
                    self.check_children(
                        index,
                        &[
                            ("audioContentIDRef", 1, None),
                            ("audioProgrammeLabel", 0, None),
                            ("loudnessMetadata", 1, Some(1)),
                            ("alternativeValueSetIDRef", 0, None),
                        ],
                        &mut errors,
                    );
                    self.check_name_language(
                        index,
                        "audioProgrammeName",
                        Some("audioProgrammeLanguage"),
                        &mut errors,
                    );
                    self.check_unique_label_languages(index, "audioProgrammeLabel", &mut errors);
                }
                "audioContent" => {
                    self.check_attributes(
                        index,
                        &["audioContentID", "audioContentName", "audioContentLanguage"],
                        &["audioContentID", "audioContentName", "audioContentLanguage"],
                        &mut errors,
                    );
                    self.check_children(
                        index,
                        &[
                            ("audioObjectIDRef", 1, Some(1)),
                            ("audioContentLabel", 0, None),
                            ("loudnessMetadata", 1, Some(1)),
                            ("dialogue", 1, Some(1)),
                        ],
                        &mut errors,
                    );
                    self.check_name_language(
                        index,
                        "audioContentName",
                        Some("audioContentLanguage"),
                        &mut errors,
                    );
                    self.check_unique_label_languages(index, "audioContentLabel", &mut errors);
                }
                "audioObject" => {
                    self.check_attributes(
                        index,
                        &["audioObjectID", "audioObjectName", "interact"],
                        &["audioObjectID", "audioObjectName", "interact"],
                        &mut errors,
                    );
                    self.check_children(
                        index,
                        &[
                            ("audioPackFormatIDRef", 0, Some(1)),
                            ("audioObjectIDRef", 0, None),
                            ("audioTrackUIDRef", 0, None),
                            ("audioComplementaryObjectGroupLabel", 0, None),
                            ("audioComplementaryObjectIDRef", 0, None),
                            ("audioObjectInteraction", 0, Some(1)),
                            ("gain", 0, Some(1)),
                            ("positionOffset", 0, Some(1)),
                            ("alternativeValueSet", 0, None),
                        ],
                        &mut errors,
                    );
                    self.check_name_language(index, "audioObjectName", None, &mut errors);
                    self.check_unique_label_languages(
                        index,
                        "audioComplementaryObjectGroupLabel",
                        &mut errors,
                    );
                    if !matches!(attr(node, "interact"), Some("0" | "1")) {
                        errors.push(self.evidence(index, "interact shall be decimal 0 or 1"));
                    }
                }
                "audioPackFormat" => {
                    self.check_attributes(
                        index,
                        &[
                            "audioPackFormatID",
                            "audioPackFormatName",
                            "typeLabel",
                            "typeDefinition",
                        ],
                        &[
                            "audioPackFormatID",
                            "audioPackFormatName",
                            "typeLabel",
                            "typeDefinition",
                        ],
                        &mut errors,
                    );
                    let children = if node_type(node) == Some("0002") {
                        vec![
                            ("audioChannelFormatIDRef", 1, Some(24)),
                            ("inputPackFormatIDRef", 1, Some(1)),
                            ("outputPackFormatIDRef", 1, Some(1)),
                        ]
                    } else {
                        vec![("audioChannelFormatIDRef", 1, Some(1))]
                    };
                    self.check_children(index, &children, &mut errors);
                    self.check_name_language(index, "audioPackFormatName", None, &mut errors);
                }
                "audioChannelFormat" => {
                    self.check_attributes(
                        index,
                        &[
                            "audioChannelFormatID",
                            "audioChannelFormatName",
                            "typeLabel",
                            "typeDefinition",
                        ],
                        &[
                            "audioChannelFormatID",
                            "audioChannelFormatName",
                            "typeLabel",
                            "typeDefinition",
                        ],
                        &mut errors,
                    );
                    self.check_children(
                        index,
                        &[(
                            "audioBlockFormat",
                            1,
                            if node_type(node) == Some("0002") {
                                Some(1)
                            } else {
                                None
                            },
                        )],
                        &mut errors,
                    );
                    self.check_name_language(index, "audioChannelFormatName", None, &mut errors);
                }
                "audioBlockFormat" => {
                    if parent_type(self.document, index) == Some("0002") {
                        self.check_attributes(
                            index,
                            &["audioBlockFormatID"],
                            &["audioBlockFormatID"],
                            &mut errors,
                        );
                        self.check_children(
                            index,
                            &[
                                ("outputChannelFormatIDRef", 1, Some(1)),
                                ("matrix", 1, Some(1)),
                            ],
                            &mut errors,
                        );
                    } else {
                        self.check_attributes(
                            index,
                            &["audioBlockFormatID", "rtime", "duration"],
                            &["audioBlockFormatID", "rtime", "duration"],
                            &mut errors,
                        );
                        self.check_children(
                            index,
                            &[
                                ("cartesian", 0, Some(1)),
                                ("position", 3, Some(3)),
                                ("objectDivergence", 0, Some(1)),
                                ("gain", 0, Some(1)),
                                ("jumpPosition", 0, Some(1)),
                            ],
                            &mut errors,
                        );
                    }
                }
                "audioTrackUID" => {
                    self.check_attributes(
                        index,
                        &["UID", "sampleRate", "bitDepth"],
                        &["UID"],
                        &mut errors,
                    );
                    self.check_children(
                        index,
                        &[
                            ("audioPackFormatIDRef", 1, Some(1)),
                            ("audioChannelFormatIDRef", 1, Some(1)),
                        ],
                        &mut errors,
                    );
                }
                "profileList" => {
                    self.check_attributes(index, &[], &[], &mut errors);
                    self.check_children(index, &[("profile", 1, None)], &mut errors);
                }
                "profile" => {
                    self.check_attributes(
                        index,
                        &["profileName", "profileVersion", "profileLevel"],
                        &["profileName", "profileVersion", "profileLevel"],
                        &mut errors,
                    );
                    self.check_leaf(index, &mut errors);
                }
                "loudnessMetadata" => {
                    self.check_attributes(index, &[], &[], &mut errors);
                    self.check_children(
                        index,
                        &[
                            ("integratedLoudness", 0, Some(1)),
                            ("dialogueLoudness", 0, Some(1)),
                        ],
                        &mut errors,
                    );
                    if self.children(index, "integratedLoudness").is_empty()
                        && self.children(index, "dialogueLoudness").is_empty()
                    {
                        errors.push(self.evidence(
                            index,
                            "integratedLoudness or dialogueLoudness shall be present",
                        ));
                    }
                }
                "alternativeValueSet" => {
                    self.check_attributes(
                        index,
                        &["alternativeValueSetID"],
                        &["alternativeValueSetID"],
                        &mut errors,
                    );
                    self.check_children(
                        index,
                        &[
                            ("gain", 0, Some(1)),
                            ("audioObjectInteraction", 0, Some(1)),
                            ("positionOffset", 0, Some(1)),
                        ],
                        &mut errors,
                    );
                }
                "audioObjectInteraction" => {
                    self.check_attributes(
                        index,
                        &["onOffInteract", "gainInteract", "positionInteract"],
                        &["onOffInteract"],
                        &mut errors,
                    );
                    self.check_children(
                        index,
                        &[
                            ("gainInteractionRange", 0, Some(2)),
                            ("positionInteractionRange", 0, Some(2)),
                        ],
                        &mut errors,
                    );
                }
                "audioProgrammeLabel"
                | "audioContentLabel"
                | "audioComplementaryObjectGroupLabel" => {
                    self.check_attributes(index, &["language"], &["language"], &mut errors);
                    self.check_leaf(index, &mut errors);
                    self.check_text_len(index, &mut errors);
                }
                "gain" => {
                    self.check_attributes(index, &["gainUnit"], &[], &mut errors);
                    self.check_leaf(index, &mut errors);
                }
                "positionOffset" => {
                    self.check_attributes(index, &["coordinate"], &["coordinate"], &mut errors);
                    self.check_leaf(index, &mut errors);
                }
                "gainInteractionRange" => {
                    self.check_attributes(index, &["bound", "gainUnit"], &["bound"], &mut errors);
                    self.check_leaf(index, &mut errors);
                }
                "positionInteractionRange" => {
                    self.check_attributes(
                        index,
                        &["coordinate", "bound"],
                        &["coordinate", "bound"],
                        &mut errors,
                    );
                    self.check_leaf(index, &mut errors);
                }
                "position" => {
                    self.check_attributes(index, &["coordinate"], &["coordinate"], &mut errors);
                    self.check_leaf(index, &mut errors);
                }
                "objectDivergence" => {
                    self.check_attributes(
                        index,
                        &["azimuthRange", "positionRange"],
                        &[],
                        &mut errors,
                    );
                    self.check_leaf(index, &mut errors);
                }
                "jumpPosition" | "cartesian" => {
                    self.check_attributes(index, &[], &[], &mut errors);
                    self.check_leaf(index, &mut errors);
                    if parse_bool_text(node).is_none() {
                        errors
                            .push(self.evidence(index, format!("{name} shall be decimal 0 or 1")));
                    }
                }
                "integratedLoudness" | "dialogueLoudness" => {
                    self.check_attributes(index, &[], &[], &mut errors);
                    self.check_leaf(index, &mut errors);
                    if !valid_decimal_lexical(trim_xml(&node.text)) {
                        errors.push(
                            self.evidence(index, format!("{name} shall be a finite decimal value")),
                        );
                    }
                }
                "dialogue" => {
                    self.check_attributes(
                        index,
                        &[
                            "nonDialogueContentKind",
                            "dialogueContentKind",
                            "mixedContentKind",
                        ],
                        &[],
                        &mut errors,
                    );
                    self.check_leaf(index, &mut errors);
                    if !valid_dialogue(node) {
                        errors.push(self.evidence(index, "dialogue value and content-kind attribute do not match BS.2076-3 Tables A1-35/A1-36"));
                    }
                }
                "matrix" => {
                    self.check_attributes(index, &[], &[], &mut errors);
                    self.check_children(index, &[("coefficient", 0, None)], &mut errors);
                }
                "coefficient" => {
                    self.check_attributes(index, &["gain", "gainUnit"], &[], &mut errors);
                    self.check_leaf(index, &mut errors);
                }
                "audioContentIDRef"
                | "alternativeValueSetIDRef"
                | "audioObjectIDRef"
                | "audioPackFormatIDRef"
                | "audioTrackUIDRef"
                | "audioComplementaryObjectIDRef"
                | "audioChannelFormatIDRef"
                | "inputPackFormatIDRef"
                | "outputPackFormatIDRef"
                | "outputChannelFormatIDRef" => {
                    self.check_attributes(index, &[], &[], &mut errors);
                    self.check_leaf(index, &mut errors);
                }
                _ => errors.push(self.evidence(
                    index,
                    format!(
                        "unknown element {} inside audioFormatExtended",
                        node.name.local
                    ),
                )),
            }
        }
        self.push_rule(
            "BS2168-2.1.1-STRUCTURE",
            "§§ 2.1.1–2.1.10, Tables 1–29",
            self.path(afe),
            "only listed ADM elements, attributes and cardinalities are present",
            errors,
        );
    }

    fn check_attributes(
        &self,
        index: usize,
        allowed: &[&str],
        required: &[&str],
        errors: &mut Violations,
    ) {
        let node = self.node(index);
        for attribute in &node.attributes {
            if attribute.name.namespace.is_some()
                || !allowed.contains(&attribute.name.local.as_str())
            {
                errors.push(
                    self.evidence(index, format!("unknown attribute {}", attribute.name.local)),
                );
            }
        }
        for required in required {
            if attr(node, required).is_none() {
                errors.push(self.evidence(index, format!("missing required attribute {required}")));
            }
        }
    }

    fn check_children(
        &self,
        index: usize,
        specifications: &[(&str, usize, Option<usize>)],
        errors: &mut Violations,
    ) {
        let allowed = specifications
            .iter()
            .map(|spec| spec.0)
            .collect::<HashSet<_>>();
        for child in &self.node(index).children {
            if !self.is_adm_node(*child)
                || !allowed.contains(canonical_name(&self.node(*child).name.local))
            {
                errors.push(self.evidence(
                    *child,
                    format!(
                        "element is not allowed under {}",
                        canonical_name(&self.node(index).name.local)
                    ),
                ));
            }
        }
        for (name, minimum, maximum) in specifications {
            let count = self.children(index, name).len();
            if count < *minimum || maximum.is_some_and(|maximum| count > maximum) {
                errors.push(self.evidence(
                    index,
                    format!(
                        "{name} occurrence count {count} is outside {minimum}..={}",
                        maximum.map_or("unlimited".into(), |value| value.to_string())
                    ),
                ));
            }
        }
    }

    fn check_leaf(&self, index: usize, errors: &mut Violations) {
        if !self.node(index).children.is_empty() {
            errors.push(self.evidence(index, "leaf element contains child elements"));
        }
    }

    fn check_text_len(&self, index: usize, errors: &mut Violations) {
        let length = trim_xml(&self.node(index).text).chars().count();
        if !(1..=64).contains(&length) {
            errors.push(self.evidence(
                index,
                format!("text length is {length}, expected 1..=64 characters"),
            ));
        }
    }

    fn check_name_language(
        &self,
        index: usize,
        name_attribute: &str,
        language_attribute: Option<&str>,
        errors: &mut Violations,
    ) {
        let node = self.node(index);
        let length = attr(node, name_attribute)
            .map(str::chars)
            .map(Iterator::count)
            .unwrap_or(0);
        if !(1..=64).contains(&length) {
            errors.push(self.evidence(
                index,
                format!("{name_attribute} length is {length}, expected 1..=64 characters"),
            ));
        }
        if let Some(language) = language_attribute.and_then(|name| attr(node, name)) {
            if !valid_language(language) {
                errors.push(self.evidence(
                    index,
                    format!("language {language:?} is not a three-letter ISO 639-2 code"),
                ));
            }
        }
    }

    fn check_unique_label_languages(&self, index: usize, label: &str, errors: &mut Violations) {
        let mut seen = HashSet::new();
        for child in self.children(index, label) {
            let language = attr(self.node(child), "language").unwrap_or_default();
            if !valid_language(language) || !seen.insert(language.to_ascii_lowercase()) {
                errors.push(self.evidence(
                    child,
                    format!("label language {language:?} is invalid or repeated"),
                ));
            }
        }
    }

    fn evidence(&self, index: usize, observed: impl Into<String>) -> Evidence {
        Evidence {
            path: self.path(index),
            observed: bounded_utf8(&observed.into(), MAX_EVIDENCE_OBSERVED_BYTES),
        }
    }
}

#[derive(Default)]
struct Definitions {
    programmes: BTreeMap<String, usize>,
    contents: BTreeMap<String, usize>,
    objects: BTreeMap<String, usize>,
    packs: BTreeMap<String, usize>,
    channels: BTreeMap<String, usize>,
    tracks: BTreeMap<String, usize>,
    avs: BTreeMap<String, usize>,
    duplicate_ids: Vec<(String, usize)>,
    duplicate_id_count: usize,
}

impl Definitions {
    fn build(
        document: &ParsedDocument,
        afe: usize,
        namespace: Option<&Arc<str>>,
        max_duplicate_evidence: usize,
    ) -> Self {
        let mut result = Self::default();
        for index in document.nodes[afe].children.iter().copied() {
            let node = &document.nodes[index];
            if node.name.namespace.as_ref() != namespace {
                continue;
            }
            let target = match canonical_name(&node.name.local) {
                "audioProgramme" => Some((&mut result.programmes, "audioProgrammeID")),
                "audioContent" => Some((&mut result.contents, "audioContentID")),
                "audioObject" => Some((&mut result.objects, "audioObjectID")),
                "audioPackFormat" => Some((&mut result.packs, "audioPackFormatID")),
                "audioChannelFormat" => Some((&mut result.channels, "audioChannelFormatID")),
                "audioTrackUID" => Some((&mut result.tracks, "UID")),
                _ => None,
            };
            if let Some((target, attribute)) = target {
                if let Some(id) = attr(node, attribute) {
                    insert_definition(
                        target,
                        id,
                        index,
                        &mut result.duplicate_ids,
                        &mut result.duplicate_id_count,
                        max_duplicate_evidence,
                    );
                }
            }
            if canonical_name(&node.name.local) == "audioObject" {
                for child in &node.children {
                    let child_node = &document.nodes[*child];
                    if child_node.name.namespace.as_ref() == namespace
                        && canonical_name(&child_node.name.local) == "alternativeValueSet"
                    {
                        if let Some(id) = attr(child_node, "alternativeValueSetID") {
                            insert_definition(
                                &mut result.avs,
                                id,
                                *child,
                                &mut result.duplicate_ids,
                                &mut result.duplicate_id_count,
                                max_duplicate_evidence,
                            );
                        }
                    }
                }
            }
        }
        result
    }
}

fn insert_definition(
    target: &mut BTreeMap<String, usize>,
    id: &str,
    index: usize,
    duplicates: &mut Vec<(String, usize)>,
    duplicate_count: &mut usize,
    max_duplicate_evidence: usize,
) {
    let key = canonical_id(id);
    if target.insert(key.clone(), index).is_some() {
        *duplicate_count = duplicate_count.saturating_add(1);
        if duplicates.len() < max_duplicate_evidence {
            duplicates.push((key, index));
        }
    }
}

impl Audit<'_> {
    fn audit_profile(&mut self, afe: usize, level: Level) {
        let mut errors = Violations::new(self.max_evidence);
        let lists = self.children(afe, "profileList");
        if lists.len() != 1 {
            errors
                .push(self.evidence(afe, format!("found {} profileList element(s)", lists.len())));
        }
        let mut requested = 0;
        let mut identities = HashSet::new();
        for list in lists {
            for profile in self.children(list, "profile") {
                let node = self.node(profile);
                let identity = (
                    trim_xml(&node.text).to_owned(),
                    attr(node, "profileName").unwrap_or_default().to_owned(),
                    attr(node, "profileVersion").unwrap_or_default().to_owned(),
                    attr(node, "profileLevel").unwrap_or_default().to_owned(),
                );
                if !identities.insert(identity.clone()) {
                    errors.push(self.evidence(profile, "duplicate profile declaration"));
                }
                if identity.0 == PROFILE_TEXT {
                    let declared_level = Level::parse(&identity.3).ok();
                    if identity.1 != PROFILE_NAME
                        || identity.2 != PROFILE_VERSION
                        || declared_level.is_none()
                    {
                        errors.push(self.evidence(
                            profile,
                            format!(
                                "invalid emission declaration {:?}/{:?}/level {:?}",
                                identity.1, identity.2, identity.3
                            ),
                        ));
                    } else if declared_level == Some(level) {
                        requested += 1;
                    }
                }
            }
        }
        if requested == 0 {
            errors.push(self.evidence(
                afe,
                format!("no exact declaration for requested level {level}"),
            ));
        }
        self.push_rule("BS2168-2.1.10-PROFILE", "§ 2.1.10, Tables 27–29", self.path(afe), "one or more valid ITU-R BS.2168 profile declarations are permitted, the requested level is declared, and declarations are unique", errors);
    }

    fn audit_ids(&mut self, definitions: &Definitions) {
        let mut errors = Violations::new(self.max_evidence);
        for (id, index) in &definitions.duplicate_ids {
            errors.push(self.evidence(*index, format!("duplicate ID {id}")));
        }
        errors.total = errors
            .total
            .saturating_add(definitions.duplicate_id_count.saturating_sub(errors.total));
        for (id, index) in &definitions.programmes {
            if !valid_short_id(id, "APR_") {
                errors.push(self.evidence(*index, format!("invalid audioProgrammeID {id}")));
            }
        }
        for (id, index) in &definitions.objects {
            if !valid_short_id(id, "AO_") {
                errors.push(self.evidence(*index, format!("invalid audioObjectID {id}")));
            }
        }
        for (id, index) in &definitions.contents {
            if !valid_short_id(id, "ACO_") {
                errors.push(self.evidence(*index, format!("invalid audioContentID {id}")));
            }
        }
        for (id, index) in &definitions.packs {
            if !valid_format_id(id, "AP_") {
                errors.push(self.evidence(*index, format!("invalid audioPackFormatID {id}")));
            }
        }
        for (id, index) in &definitions.channels {
            if !valid_format_id(id, "AC_") {
                errors.push(self.evidence(*index, format!("invalid audioChannelFormatID {id}")));
            }
        }
        for (id, index) in &definitions.tracks {
            if !valid_track_id(id) {
                errors.push(self.evidence(*index, format!("invalid audioTrackUID {id}")));
            }
        }

        let afe = self.document.afe.unwrap();
        for (offset, index) in self.children(afe, "audioTrackUID").into_iter().enumerate() {
            let expected = format!("ATU_{:08X}", offset + 1);
            if attr(self.node(index), "UID").map(canonical_id).as_deref() != Some(expected.as_str())
            {
                errors.push(
                    self.evidence(index, format!("audioTrackUID counter shall be {expected}")),
                );
            }
        }
        for object_index in self.children(afe, "audioObject") {
            let object_id = attr(self.node(object_index), "audioObjectID").map(canonical_id);
            let object_word = object_id.as_deref().and_then(|id| id.strip_prefix("AO_"));
            for (offset, avs) in self
                .children(object_index, "alternativeValueSet")
                .into_iter()
                .enumerate()
            {
                let expected = object_word.map(|word| format!("AVS_{word}_{:04X}", offset + 1));
                if attr(self.node(avs), "alternativeValueSetID").map(canonical_id) != expected {
                    errors.push(self.evidence(
                        avs,
                        format!(
                            "alternativeValueSet counter/owner mismatch; expected {}",
                            expected.as_deref().unwrap_or("valid parent ID")
                        ),
                    ));
                }
            }
        }
        for (id, index) in &definitions.contents {
            let reference = child_texts(
                self.document,
                *index,
                self.afe_namespace.as_ref(),
                "audioObjectIDRef",
            )
            .first()
            .cloned();
            let expected = reference
                .and_then(|value| canonical_id(value).strip_prefix("AO_").map(str::to_owned));
            if id.strip_prefix("ACO_") != expected.as_deref() {
                errors.push(self.evidence(
                    *index,
                    format!("audioContentID {id} does not match referenced audioObject"),
                ));
            }
        }
        for (id, index) in definitions.packs.iter().chain(definitions.channels.iter()) {
            let node = self.node(*index);
            let type_label = attr(node, "typeLabel");
            let type_definition = attr(node, "typeDefinition");
            let id_type = id.get(3..7);
            let expected_definition = match type_label {
                Some("0002") => Some("Matrix"),
                Some("0003") => Some("Objects"),
                _ => None,
            };
            if type_label != id_type || type_definition != expected_definition {
                errors.push(self.evidence(
                    *index,
                    format!("ID type, typeLabel and typeDefinition disagree for {id}"),
                ));
            }
        }
        for (channel_id, channel_index) in &definitions.channels {
            let suffix = channel_id.strip_prefix("AC_").unwrap_or_default();
            let matrix = node_type(self.node(*channel_index)) == Some("0002");
            for (offset, block) in self
                .children(*channel_index, "audioBlockFormat")
                .into_iter()
                .enumerate()
            {
                let expected_counter = if matrix { 1 } else { offset + 1 };
                let expected = format!("AB_{suffix}_{expected_counter:08X}");
                if attr(self.node(block), "audioBlockFormatID")
                    .map(canonical_id)
                    .as_deref()
                    != Some(expected.as_str())
                {
                    errors.push(
                        self.evidence(block, format!("audioBlockFormatID shall be {expected}")),
                    );
                }
            }
        }
        self.push_rule("BS2168-2.2-IDS", "§ 2.2, Tables 30–32", "/audioFormatExtended", "ADM IDs use the required hexadecimal formats, owner fields, type fields and monotonically increasing counters", errors);
    }

    fn audit_limits(&mut self, afe: usize, limits: ProfileLimits, _definitions: &Definitions) {
        let mut errors = Violations::new(self.max_evidence);
        check_limit(
            &mut errors,
            self.path(afe),
            "MAX_PROGRAMME",
            self.counts.programmes,
            limits.programme,
        );
        check_limit(
            &mut errors,
            self.path(afe),
            "MAX_CONTENT",
            self.counts.contents,
            limits.content,
        );
        check_limit(
            &mut errors,
            self.path(afe),
            "MAX_OBJECT",
            self.counts.objects,
            limits.object,
        );
        check_limit(
            &mut errors,
            self.path(afe),
            "MAX_PACK_FORMAT (Matrix excluded)",
            self.counts.pack_formats,
            limits.pack,
        );
        check_limit(
            &mut errors,
            self.path(afe),
            "MAX_CHANNEL_FORMAT (Matrix excluded)",
            self.counts.channel_formats,
            limits.channel,
        );
        check_limit(
            &mut errors,
            self.path(afe),
            "MAX_TRACK_UID",
            self.counts.track_uids,
            limits.track_uid,
        );
        for index in self.children(afe, "audioProgramme") {
            check_limit(
                &mut errors,
                self.path(index),
                "MAX_APR_ACO",
                self.children(index, "audioContentIDRef").len(),
                limits.apr_aco,
            );
            check_limit(
                &mut errors,
                self.path(index),
                "MAX_APR_ACO (alternativeValueSetIDRef)",
                self.children(index, "alternativeValueSetIDRef").len(),
                limits.apr_aco,
            );
            check_limit(
                &mut errors,
                self.path(index),
                "MAX_APR_PL",
                self.children(index, "audioProgrammeLabel").len(),
                limits.apr_label,
            );
        }
        for index in self.children(afe, "audioContent") {
            check_limit(
                &mut errors,
                self.path(index),
                "MAX_ACO_CL",
                self.children(index, "audioContentLabel").len(),
                limits.aco_label,
            );
        }
        for index in self.children(afe, "audioObject") {
            check_limit(
                &mut errors,
                self.path(index),
                "MAX_AO_AO",
                self.children(index, "audioObjectIDRef").len(),
                limits.ao_object,
            );
            check_limit(
                &mut errors,
                self.path(index),
                "MAX_AO_CO",
                self.children(index, "audioComplementaryObjectIDRef").len(),
                limits.ao_comp,
            );
            check_limit(
                &mut errors,
                self.path(index),
                "MAX_AO_AVS",
                self.children(index, "alternativeValueSet").len(),
                limits.ao_avs,
            );
            check_limit(
                &mut errors,
                self.path(index),
                "MAX_AO_CL",
                self.children(index, "audioComplementaryObjectGroupLabel")
                    .len(),
                limits.ao_label,
            );
            check_limit(
                &mut errors,
                self.path(index),
                "MAX_CHANNELS_LAYOUT",
                self.children(index, "audioTrackUIDRef").len(),
                limits.channels_layout,
            );
        }
        check_range_limit(
            &mut errors,
            self.path(afe),
            "MAX_TRACK_NON_COMP",
            self.counts.non_complementary_tracks,
            1,
            limits.non_comp_tracks,
        );
        check_limit(
            &mut errors,
            self.path(afe),
            "MAX_GROUP_COMP",
            self.counts.complementary_groups,
            limits.comp_groups,
        );
        check_range_limit(
            &mut errors,
            self.path(afe),
            "MAX_GROUP_INDEP",
            self.counts.independent_groups,
            1,
            limits.independent_groups,
        );
        self.push_rule("BS2168-2.3-LIMITS", "§ 2.3, Tables 33–38", self.path(afe), "element, sub-element and derived topology counts stay within the requested profile level", errors);
    }

    fn audit_graph(&mut self, definitions: &Definitions) {
        let mut errors = Violations::new(self.max_evidence);
        let mut object_owners = HashMap::<String, usize>::new();
        let mut content_owners = HashMap::<String, usize>::new();
        for programme in definitions.programmes.values() {
            for reference in child_texts(
                self.document,
                *programme,
                self.afe_namespace.as_ref(),
                "audioContentIDRef",
            ) {
                let id = canonical_id(reference);
                if !definitions.contents.contains_key(&id) {
                    errors.push(self.evidence(
                        *programme,
                        format!("unresolved audioContentIDRef {reference}"),
                    ));
                } else {
                    *content_owners.entry(id).or_default() += 1;
                }
            }
        }
        for id in definitions.contents.keys() {
            if content_owners.get(id).copied().unwrap_or(0) == 0 {
                errors.push(self.evidence(
                    definitions.contents[id],
                    format!("audioContent {id} is not referenced by any audioProgramme"),
                ));
            }
        }
        let mut top_objects = BTreeSet::new();
        for content in definitions.contents.values() {
            let references = child_texts(
                self.document,
                *content,
                self.afe_namespace.as_ref(),
                "audioObjectIDRef",
            );
            if let Some(reference) = references.first() {
                let id = canonical_id(reference);
                if definitions.objects.contains_key(&id) {
                    top_objects.insert(id.clone());
                    *object_owners.entry(id).or_default() += 1;
                } else {
                    errors.push(
                        self.evidence(*content, format!("unresolved audioObjectIDRef {reference}")),
                    );
                }
            }
        }
        let mut edges = BTreeMap::<String, Vec<String>>::new();
        for (id, object) in &definitions.objects {
            let references = child_texts(
                self.document,
                *object,
                self.afe_namespace.as_ref(),
                "audioObjectIDRef",
            )
            .into_iter()
            .map(canonical_id)
            .collect::<Vec<_>>();
            for reference in &references {
                if !definitions.objects.contains_key(reference) {
                    errors.push(
                        self.evidence(*object, format!("unresolved audioObjectIDRef {reference}")),
                    );
                } else {
                    *object_owners.entry(reference.clone()).or_default() += 1;
                    let child = definitions.objects[reference];
                    let pack_references = child_texts(
                        self.document,
                        child,
                        self.afe_namespace.as_ref(),
                        "audioPackFormatIDRef",
                    );
                    if pack_references.len() != 1 {
                        errors.push(self.evidence(
                            child,
                            "nested audioObject shall directly reference one audioPackFormat",
                        ));
                    } else {
                        let pack_id = canonical_id(pack_references[0]);
                        if !definitions
                            .packs
                            .get(&pack_id)
                            .is_some_and(|pack| node_type(self.node(*pack)) == Some("0003"))
                        {
                            errors.push(self.evidence(
                                child,
                                format!("nested audioObject directly references {pack_id}; expected a present local Objects audioPackFormat of typeLabel 0003"),
                            ));
                        }
                    }
                }
            }
            edges.insert(id.clone(), references);
        }
        for (id, object) in &definitions.objects {
            let owners = object_owners.get(id).copied().unwrap_or(0);
            if owners != 1 {
                errors.push(self.evidence(
                    *object,
                    format!(
                        "audioObject {id} has {owners} content/object owners; expected exactly one"
                    ),
                ));
            }
            let packs = self.children(*object, "audioPackFormatIDRef");
            let children = self.children(*object, "audioObjectIDRef");
            if (packs.len() == 1) != children.is_empty() {
                errors.push(self.evidence(*object, "audioPackFormatIDRef shall be present if and only if audioObjectIDRef is absent"));
            }
            let tracks = self.children(*object, "audioTrackUIDRef");
            if packs.is_empty() != tracks.is_empty() {
                errors.push(self.evidence(
                    *object,
                    "audioTrackUIDRef presence shall match audioPackFormatIDRef presence",
                ));
            }
        }
        let graph = analyze_object_graph(
            &edges,
            definitions,
            self.document,
            self.afe_namespace.as_ref(),
        );
        for id in &graph.cyclic {
            errors.push(self.evidence(
                definitions.objects[id],
                format!("audioObject {id} is in or depends on a reference cycle"),
            ));
        }
        for (id, depth) in &graph.depths {
            if *depth > 2 {
                errors.push(self.evidence(
                    definitions.objects[id],
                    format!("audioObject nesting depth is {depth}, maximum is 2"),
                ));
            }
        }

        for (id, object) in &definitions.objects {
            if !self
                .children(*object, "audioComplementaryObjectGroupLabel")
                .is_empty()
                && self
                    .children(*object, "audioComplementaryObjectIDRef")
                    .is_empty()
            {
                errors.push(self.evidence(
                    *object,
                    format!("audioObject {id} has a complementary-group label but is not the leader of that group"),
                ));
            }
            if !top_objects.contains(id)
                && !self
                    .children(*object, "audioComplementaryObjectIDRef")
                    .is_empty()
            {
                errors.push(self.evidence(
                    *object,
                    "a non-top-level audioObject shall not lead a complementary group",
                ));
            }
        }
        let exact_subtree_digests = subtree_digest_cache(self.document, &[]);
        let object_signatures = definitions
            .objects
            .iter()
            .map(|(id, object)| {
                (
                    id.clone(),
                    object_configuration_signature(
                        self.document,
                        *object,
                        self.afe_namespace.as_ref(),
                        &exact_subtree_digests,
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let object_types = definitions
            .objects
            .iter()
            .map(|(id, object)| {
                (
                    id.clone(),
                    object_pack_type(
                        self.document,
                        *object,
                        self.afe_namespace.as_ref(),
                        definitions,
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let avs_subtree_digests = subtree_digest_cache(self.document, &["alternativeValueSetID"]);
        let avs_signatures = definitions
            .avs
            .values()
            .map(|avs| (*avs, avs_subtree_digests[*avs]))
            .collect::<BTreeMap<_, _>>();
        let mut complementary_target_owner = BTreeMap::<String, usize>::new();
        let mut groups = Vec::<BTreeSet<String>>::new();
        for leader in &top_objects {
            let leader_index = definitions.objects[leader];
            let refs = child_texts(
                self.document,
                leader_index,
                self.afe_namespace.as_ref(),
                "audioComplementaryObjectIDRef",
            )
            .into_iter()
            .map(canonical_id)
            .collect::<Vec<_>>();
            if refs.is_empty() {
                continue;
            }
            let mut group = BTreeSet::from([leader.clone()]);
            for reference in refs {
                if !top_objects.contains(&reference) {
                    errors.push(self.evidence(leader_index, format!("complementary reference {reference} is not a present top-level audioObject")));
                    continue;
                }
                *complementary_target_owner
                    .entry(reference.clone())
                    .or_default() += 1;
                group.insert(reference.clone());
                let target = definitions.objects[&reference];
                if !self
                    .children(target, "audioComplementaryObjectIDRef")
                    .is_empty()
                {
                    errors.push(self.evidence(
                        target,
                        "a complementary target shall not itself lead a complementary group",
                    ));
                }
                if object_signatures.get(leader) != object_signatures.get(&reference)
                    || object_types.get(leader) != object_types.get(&reference)
                {
                    errors.push(self.evidence(target, format!("complementary member {reference} has different interact/gain/position/type configuration")));
                }
            }
            groups.push(group);
        }
        for (target, owners) in complementary_target_owner {
            if owners > 1 {
                errors.push(self.evidence(
                    definitions.objects[&target],
                    format!("audioObject {target} belongs to {owners} complementary leaders"),
                ));
            }
        }
        let groups_by_member = complementary_group_membership_index(&groups);
        for programme in definitions.programmes.values() {
            let mut avs_by_owner = BTreeMap::<String, Vec<usize>>::new();
            let included = child_texts(
                self.document,
                *programme,
                self.afe_namespace.as_ref(),
                "audioContentIDRef",
            )
            .into_iter()
            .filter_map(|reference| definitions.contents.get(&canonical_id(reference)))
            .flat_map(|content| {
                child_texts(
                    self.document,
                    *content,
                    self.afe_namespace.as_ref(),
                    "audioObjectIDRef",
                )
            })
            .map(canonical_id)
            .collect::<BTreeSet<_>>();
            let (included_group_counts, _) =
                count_included_complementary_groups(&included, &groups_by_member);
            for (group_index, count) in &included_group_counts {
                let group = &groups[*group_index];
                if *count != 0 && *count != 1 && *count != group.len() {
                    errors.push(self.evidence(
                        *programme,
                        format!(
                            "programme includes {count}/{} members of a complementary group",
                            group.len()
                        ),
                    ));
                }
            }
            for reference in child_texts(
                self.document,
                *programme,
                self.afe_namespace.as_ref(),
                "alternativeValueSetIDRef",
            ) {
                let id = canonical_id(reference);
                let Some(avs) = definitions.avs.get(&id) else {
                    errors.push(self.evidence(
                        *programme,
                        format!("unresolved alternativeValueSetIDRef {reference}"),
                    ));
                    continue;
                };
                let owner = self
                    .node(*avs)
                    .parent
                    .and_then(|parent| attr(self.node(parent), "audioObjectID"))
                    .map(canonical_id);
                if owner.as_ref().is_none_or(|owner| !included.contains(owner)) {
                    errors.push(self.evidence(*programme, format!("alternativeValueSet {id} is not owned by an object included in this programme")));
                } else if let Some(owner) = owner {
                    let owner_sets = avs_by_owner.entry(owner).or_default();
                    if !owner_sets.is_empty() {
                        errors.push(self.evidence(
                            *programme,
                            "an audioProgramme shall reference at most one alternativeValueSet from the same audioObject",
                        ));
                    }
                    owner_sets.push(*avs);
                }
            }
            for (group_index, count) in &included_group_counts {
                let group = &groups[*group_index];
                if *count != group.len() {
                    continue;
                }
                let referenced_members = group
                    .iter()
                    .filter(|member| avs_by_owner.contains_key(*member))
                    .count();
                if referenced_members == 0 {
                    continue;
                }
                if referenced_members != group.len() {
                    errors.push(self.evidence(
                        *programme,
                        format!("alternativeValueSet references cover {referenced_members}/{} included complementary members", group.len()),
                    ));
                    continue;
                }
                let signatures = group
                    .iter()
                    .flat_map(|member| avs_by_owner[member].iter().copied())
                    .filter_map(|avs| avs_signatures.get(&avs))
                    .collect::<BTreeSet<_>>();
                if signatures.len() != 1 {
                    errors.push(self.evidence(
                        *programme,
                        "referenced complementary-group alternativeValueSet elements differ beyond alternativeValueSetID",
                    ));
                }
            }
        }
        let group_members = groups
            .iter()
            .flat_map(|group| group.iter().cloned())
            .collect::<HashSet<_>>();
        let independent = top_objects
            .iter()
            .filter(|id| !group_members.contains(*id))
            .count();
        let mut non_complementary_tracks = 0_usize;
        let mut derived_tracks_truncated = false;
        for group in &groups {
            let group_tracks = group
                .iter()
                .map(|id| graph.track_counts.get(id).copied().unwrap_or(0))
                .max()
                .unwrap_or(0);
            derived_tracks_truncated |= group
                .iter()
                .any(|id| graph.track_counts_truncated.contains(id));
            let (total, truncated) = bounded_count_add(non_complementary_tracks, group_tracks);
            non_complementary_tracks = total;
            derived_tracks_truncated |= truncated;
        }
        for id in top_objects.iter().filter(|id| !group_members.contains(*id)) {
            derived_tracks_truncated |= graph.track_counts_truncated.contains(id);
            let (total, truncated) = bounded_count_add(
                non_complementary_tracks,
                graph.track_counts.get(id).copied().unwrap_or(0),
            );
            non_complementary_tracks = total;
            derived_tracks_truncated |= truncated;
        }
        if derived_tracks_truncated {
            errors.push(Evidence {
                path: "/audioFormatExtended".into(),
                observed: format!("derived audioTrackUID count exceeded the report-schema maximum {MAX_SERIALIZED_COUNT}; reported count is explicitly truncated"),
            });
        }
        self.counts.complementary_groups = groups.len();
        self.counts.independent_groups = groups.len().saturating_add(independent);
        self.counts.non_complementary_tracks = non_complementary_tracks;
        self.push_rule("BS2168-2.1.5-GRAPH", "§§ 2.1.3.1–2.1.5", "/audioFormatExtended", "programme/content/object ownership is resolvable, acyclic, at most two levels deep, and complementary groups are complete and consistent", errors);
    }

    fn audit_interactivity(&mut self, definitions: &Definitions) {
        let mut errors = Violations::new(self.max_evidence);
        let interaction_subtree_digests =
            subtree_digest_cache(self.document, &["gainInteract", "positionInteract"]);
        let (position_pack_index, _) =
            build_position_pack_index(self.document, self.afe_namespace.as_ref(), definitions);
        let top = definitions
            .contents
            .values()
            .flat_map(|content| {
                child_texts(
                    self.document,
                    *content,
                    self.afe_namespace.as_ref(),
                    "audioObjectIDRef",
                )
            })
            .map(canonical_id)
            .collect::<HashSet<_>>();
        for (id, object) in &definitions.objects {
            let node = self.node(*object);
            let interact = attr(node, "interact") == Some("1");
            let interactions = self.children(*object, "audioObjectInteraction");
            if (interactions.len() == 1) != interact {
                errors.push(self.evidence(
                    *object,
                    "audioObjectInteraction shall be present iff interact=1",
                ));
            }
            if !top.contains(id)
                && (!interactions.is_empty()
                    || !self.children(*object, "gain").is_empty()
                    || !self.children(*object, "positionOffset").is_empty()
                    || !self.children(*object, "alternativeValueSet").is_empty())
            {
                errors.push(self.evidence(*object, "interactivity, gain, positionOffset and alternativeValueSet are restricted to top-level audioObject elements"));
            }
            if let Some(interaction) = interactions.first() {
                self.check_interaction(*interaction, &mut errors);
            }
            for gain in self.children(*object, "gain") {
                if !gain_at_most(self.node(gain), 21) {
                    if gain_limit_is_indeterminate(self.node(gain), 21) {
                        self.operational_error.get_or_insert_with(|| {
                            "indeterminate linear gain at the certified +21 dB interval".into()
                        });
                    } else {
                        errors.push(
                            self.evidence(gain, "audioObject gain exceeds +21 dB or is invalid"),
                        );
                    }
                }
            }
            for offset in self.children(*object, "positionOffset") {
                if !valid_offset(self.node(offset)) {
                    errors.push(self.evidence(
                        offset,
                        "positionOffset shall use azimuth -30..30 or X -1..1",
                    ));
                }
            }
            for avs in self.children(*object, "alternativeValueSet") {
                for gain in self.children(avs, "gain") {
                    if !gain_at_most(self.node(gain), 21) {
                        if gain_limit_is_indeterminate(self.node(gain), 21) {
                            self.operational_error.get_or_insert_with(|| {
                                "indeterminate linear gain at the certified +21 dB interval".into()
                            });
                        } else {
                            errors.push(self.evidence(
                                gain,
                                "alternativeValueSet gain exceeds +21 dB or is invalid",
                            ));
                        }
                    }
                }
                for offset in self.children(avs, "positionOffset") {
                    if !valid_offset(self.node(offset)) {
                        errors.push(
                            self.evidence(offset, "alternativeValueSet positionOffset is invalid"),
                        );
                    }
                }
                for interaction in self.children(avs, "audioObjectInteraction") {
                    if interactions.is_empty() {
                        errors.push(self.evidence(
                            interaction,
                            "alternativeValueSet interaction requires parent interaction",
                        ));
                    }
                    self.check_interaction(interaction, &mut errors);
                    if let Some(parent_interaction) = interactions.first() {
                        let parent_signature = interaction_subtree_digests[*parent_interaction];
                        let avs_signature = interaction_subtree_digests[interaction];
                        if parent_signature != avs_signature {
                            errors.push(self.evidence(
                                interaction,
                                "alternativeValueSet interaction differs from parent beyond gainInteract/positionInteract attributes",
                            ));
                        }
                    }
                }
            }

            let mut indeterminate_gain_comparison = false;
            if let Some(parent_interaction) = interactions.first().copied() {
                let gain_bounds = interaction_gain_bounds(
                    self.document,
                    parent_interaction,
                    self.afe_namespace.as_ref(),
                );
                let position_bounds = interaction_position_bounds(
                    self.document,
                    parent_interaction,
                    self.afe_namespace.as_ref(),
                );
                let parent_gain_node = self.children(*object, "gain").first().copied();
                let parent_gain = parent_gain_node
                    .and_then(|gain| gain_value(self.node(gain)))
                    .unwrap_or(GainValue::Linear("1"));
                let parent_offset_node = self.children(*object, "positionOffset").first().copied();
                let parent_offset = parent_offset_node
                    .map(|offset| trim_xml(&self.node(offset).text))
                    .unwrap_or("0");
                if let Some(bounds) = gain_bounds {
                    match gain_value_is_within(parent_gain, bounds) {
                        Some(true) => {}
                        Some(false) => errors.push(
                            self.evidence(*object, "default gain is outside the interaction range"),
                        ),
                        None => {
                            indeterminate_gain_comparison = true;
                        }
                    };
                }
                if position_bounds.as_ref().is_some_and(|bounds| {
                    !decimal_is_within(parent_offset, bounds.minimum, bounds.maximum)
                        || parent_offset_node.is_some_and(|node| {
                            attr(self.node(node), "coordinate") != Some(bounds.coordinate)
                        })
                }) {
                    errors.push(self.evidence(
                        *object,
                        "default positionOffset is outside or mismatched with the interaction range",
                    ));
                }
                for avs in self.children(*object, "alternativeValueSet") {
                    let gain = self
                        .children(avs, "gain")
                        .first()
                        .and_then(|gain| gain_value(self.node(*gain)))
                        .unwrap_or(parent_gain);
                    if let Some(bounds) = gain_bounds {
                        match gain_value_is_within(gain, bounds) {
                            Some(true) => {}
                            Some(false) => errors.push(self.evidence(avs, "alternativeValueSet effective gain is outside the parent interaction range")),
                            None => {
                                indeterminate_gain_comparison = true;
                            }
                        };
                    }
                    let offset_node = self.children(avs, "positionOffset").first().copied();
                    let offset = offset_node
                        .map(|offset| trim_xml(&self.node(offset).text))
                        .unwrap_or(parent_offset);
                    if position_bounds.as_ref().is_some_and(|bounds| {
                        !decimal_is_within(offset, bounds.minimum, bounds.maximum)
                            || offset_node.is_some_and(|node| {
                                attr(self.node(node), "coordinate") != Some(bounds.coordinate)
                            })
                    }) {
                        errors.push(self.evidence(avs, "alternativeValueSet effective positionOffset is outside or mismatched with the parent interaction range"));
                    }
                }
            }
            if indeterminate_gain_comparison {
                self.operational_error.get_or_insert_with(|| {
                    "indeterminate cross-unit gain comparison at the precision boundary".into()
                });
            }

            let position_control_present = self
                .children(*object, "positionOffset")
                .into_iter()
                .chain(self.children(*object, "audioObjectInteraction"))
                .chain(self.children(*object, "alternativeValueSet"))
                .any(|index| {
                    node_or_descendants_use_position_control(
                        self.document,
                        index,
                        self.afe_namespace.as_ref(),
                    )
                });
            if position_control_present {
                match object_position_coordinate(
                    self.document,
                    *object,
                    self.afe_namespace.as_ref(),
                    &position_pack_index,
                ) {
                    Some(coordinate)
                        if position_control_coordinates_match(
                            self.document,
                            *object,
                            self.afe_namespace.as_ref(),
                            coordinate,
                        ) => {}
                    Some(coordinate) => errors.push(self.evidence(
                        *object,
                        format!(
                            "positionOffset and positionInteractionRange coordinates shall use {coordinate} for the referenced Objects blocks"
                        ),
                    )),
                    None => errors.push(self.evidence(
                        *object,
                        "position control requires a non-composite top-level object, a local Objects pack, and every referenced Objects block at the exact origin",
                    )),
                }
            }
        }
        self.push_rule("BS2168-2.1.5-INTERACTIVITY", "§ 2.1.5, Tables 9–13", "/audioFormatExtended/audioObject", "interactivity, gain, position offsets and alternative value sets use the emission-profile presence rules and ranges", errors);
    }

    fn check_interaction(&mut self, index: usize, errors: &mut Violations) {
        let node = self.node(index);
        if attr(node, "onOffInteract") != Some("0") {
            errors.push(self.evidence(index, "onOffInteract shall equal 0"));
        }
        for attribute in ["gainInteract", "positionInteract"] {
            if attr(node, attribute).is_some_and(|value| !matches!(value, "0" | "1")) {
                errors.push(self.evidence(index, format!("{attribute} shall be decimal 0 or 1")));
            }
        }
        for (attribute, child) in [
            ("gainInteract", "gainInteractionRange"),
            ("positionInteract", "positionInteractionRange"),
        ] {
            let present = attr(node, attribute).is_some();
            let count = self.children(index, child).len();
            if present != (count == 2) {
                errors.push(self.evidence(
                    index,
                    format!("{child} shall occur exactly twice iff {attribute} is present"),
                ));
            }
        }
        let gain_ranges = self.children(index, "gainInteractionRange");
        if gain_ranges.len() == 2 {
            let mut seen = HashSet::new();
            for range in gain_ranges {
                let bound = attr(self.node(range), "bound")
                    .unwrap_or_default()
                    .to_owned();
                seen.insert(bound.clone());
                match bound.as_str() {
                    "min" if !gain_in_range(self.node(range), None, 0) => {
                        errors.push(self.evidence(
                            range,
                            "minimum gain interaction range exceeds 0 dB or is invalid",
                        ))
                    }
                    "max" if !gain_in_range(self.node(range), Some(0), 21) => {
                        if gain_limit_is_indeterminate(self.node(range), 21) {
                            self.operational_error.get_or_insert_with(|| {
                                "indeterminate linear gain at the certified +21 dB interval".into()
                            });
                        } else {
                            errors.push(self.evidence(
                                range,
                                "maximum gain interaction range is outside 0..=21 dB",
                            ));
                        }
                    }
                    "min" | "max" => {}
                    _ => errors
                        .push(self.evidence(range, "gain interaction bound shall be min or max")),
                }
            }
            if seen != HashSet::from(["min".to_owned(), "max".to_owned()]) {
                errors.push(self.evidence(
                    index,
                    "gain interaction range shall contain one min and one max",
                ));
            } else if let Some(bounds) =
                interaction_gain_bounds(self.document, index, self.afe_namespace.as_ref())
            {
                match compare_gain_values(bounds.minimum, bounds.maximum) {
                    Some(Ordering::Greater) => errors.push(
                        self.evidence(index, "gain interaction minimum shall not exceed maximum"),
                    ),
                    Some(_) => {}
                    None => {
                        self.operational_error.get_or_insert_with(|| {
                            "indeterminate cross-unit gain-range ordering at the precision boundary"
                                .into()
                        });
                    }
                }
            }
        }
        let positions = self.children(index, "positionInteractionRange");
        if positions.len() == 2 {
            let mut seen = HashSet::new();
            let coordinate = positions
                .first()
                .and_then(|index| attr(self.node(*index), "coordinate"));
            for position in positions {
                let node = self.node(position);
                let bound = attr(node, "bound").unwrap_or_default();
                seen.insert(bound);
                let value = trim_xml(&node.text);
                let valid = match (coordinate, attr(node, "coordinate"), bound) {
                    (Some("azimuth"), Some("azimuth"), "min") => {
                        decimal_in_range(value, "-30", "0")
                    }
                    (Some("azimuth"), Some("azimuth"), "max") => decimal_in_range(value, "0", "30"),
                    (Some("X"), Some("X"), "min") => decimal_in_range(value, "-1", "0"),
                    (Some("X"), Some("X"), "max") => decimal_in_range(value, "0", "1"),
                    _ => false,
                };
                if !valid {
                    errors.push(self.evidence(
                        position,
                        "position interaction range coordinate, bound or value is invalid",
                    ));
                }
            }
            if seen != HashSet::from(["min", "max"]) {
                errors.push(self.evidence(
                    index,
                    "position interaction range shall contain one min and one max",
                ));
            } else if interaction_position_bounds(self.document, index, self.afe_namespace.as_ref())
                .is_none_or(|bounds| {
                    compare_decimal(bounds.minimum, bounds.maximum)
                        .is_none_or(|ordering| ordering == Ordering::Greater)
                })
            {
                errors.push(self.evidence(
                    index,
                    "position interaction minimum shall not exceed maximum",
                ));
            }
        }
    }

    fn audit_packs_channels(&mut self, definitions: &Definitions) {
        let mut pack_errors = Violations::new(self.max_evidence);
        let mut channel_errors = Violations::new(self.max_evidence);
        let mut pack_owners = HashMap::<String, usize>::new();
        let mut channel_owners = HashMap::<String, usize>::new();
        let mut matrix_pairs = HashSet::new();
        let mut audited_matrix_channels = HashSet::new();
        for object in definitions.objects.values() {
            for reference in child_texts(
                self.document,
                *object,
                self.afe_namespace.as_ref(),
                "audioPackFormatIDRef",
            ) {
                let id = canonical_id(reference);
                let local_objects = definitions
                    .packs
                    .get(&id)
                    .is_some_and(|pack| node_type(self.node(*pack)) == Some("0003"));
                if !local_objects && !DIRECT_SPEAKER_PACKS.contains(&id.as_str()) {
                    pack_errors.push(self.evidence(
                        *object,
                        format!("audioObject pack reference {reference} is neither a local Objects pack nor a Table-16 DirectSpeakers pack"),
                    ));
                }
                *pack_owners.entry(id).or_default() += 1;
            }
        }
        for (id, pack) in &definitions.packs {
            let pack_type = node_type(self.node(*pack));
            let channels = child_texts(
                self.document,
                *pack,
                self.afe_namespace.as_ref(),
                "audioChannelFormatIDRef",
            )
            .into_iter()
            .map(canonical_id)
            .collect::<Vec<_>>();
            for channel in &channels {
                if definitions
                    .channels
                    .get(channel)
                    .is_none_or(|index| node_type(self.node(*index)) != pack_type)
                {
                    pack_errors.push(self.evidence(
                        *pack,
                        format!(
                            "channel reference {channel} is unresolved or has a different type"
                        ),
                    ));
                } else {
                    *channel_owners.entry(channel.clone()).or_default() += 1;
                }
            }
            if pack_type == Some("0003") && pack_owners.get(id).copied().unwrap_or(0) == 0 {
                pack_errors
                    .push(self.evidence(*pack, format!("Objects pack {id} is unreferenced")));
            }
            if pack_type == Some("0002") {
                let input = child_texts(
                    self.document,
                    *pack,
                    self.afe_namespace.as_ref(),
                    "inputPackFormatIDRef",
                )
                .first()
                .map(|value| canonical_id(value));
                let output = child_texts(
                    self.document,
                    *pack,
                    self.afe_namespace.as_ref(),
                    "outputPackFormatIDRef",
                )
                .first()
                .map(|value| canonical_id(value));
                if input
                    .as_ref()
                    .is_none_or(|id| !DIRECT_SPEAKER_PACKS.contains(&id.as_str()))
                    || output
                        .as_ref()
                        .is_none_or(|id| !DIRECT_SPEAKER_MATRIX_OUTPUTS.contains(&id.as_str()))
                    || input == output
                {
                    pack_errors.push(self.evidence(*pack, "Matrix input/output DirectSpeakers pack references are invalid or identical"));
                }
                if let (Some(input), Some(output)) = (input, output) {
                    if pack_owners.get(&input).copied().unwrap_or(0) == 0 {
                        pack_errors.push(self.evidence(
                            *pack,
                            format!(
                                "Matrix input pack {input} is not referenced by any audioObject"
                            ),
                        ));
                    }
                    if !matrix_pairs.insert((input.clone(), output.clone())) {
                        pack_errors
                            .push(self.evidence(*pack, "duplicate Matrix input/output pack pair"));
                    }
                    self.audit_matrix_pack(
                        &channels,
                        &input,
                        &output,
                        definitions,
                        &mut audited_matrix_channels,
                        &mut pack_errors,
                    );
                }
            }
        }
        for (id, channel) in &definitions.channels {
            let owners = channel_owners.get(id).copied().unwrap_or(0);
            if owners != 1 {
                channel_errors.push(self.evidence(
                    *channel,
                    format!(
                        "audioChannelFormat {id} has {owners} audioPackFormat owners; expected one"
                    ),
                ));
            }
            if !matches!(node_type(self.node(*channel)), Some("0002" | "0003")) {
                channel_errors.push(self.evidence(
                    *channel,
                    "audioChannelFormat type shall be Matrix or Objects",
                ));
            }
        }
        self.push_rule("BS2168-2.1.6-PACKS", "§§ 2.1.6 and 2.4, Tables 14–17", "/audioFormatExtended/audioPackFormat", "pack references, DirectSpeakers allowlist, type topology and Matrix input/output pairs conform", pack_errors);
        self.push_rule("BS2168-2.1.7-CHANNELS", "§ 2.1.7, Tables 18–20", "/audioFormatExtended/audioChannelFormat", "each channel format has exactly one same-type pack owner and the required block cardinality", channel_errors);
    }

    fn audit_matrix_pack(
        &self,
        channel_ids: &[String],
        input_pack: &str,
        output_pack: &str,
        definitions: &Definitions,
        audited_channels: &mut HashSet<String>,
        errors: &mut Violations,
    ) -> usize {
        let Some(input_channels) = direct_speaker_channels(input_pack) else {
            return 0;
        };
        let Some(output_channels) = direct_speaker_channels(output_pack) else {
            return 0;
        };
        let mut mapped_outputs = BTreeSet::new();
        let mut coefficient_visits = 0_usize;
        for channel_id in channel_ids {
            if !audited_channels.insert(channel_id.clone()) {
                continue;
            }
            let Some(channel) = definitions.channels.get(channel_id) else {
                continue;
            };
            let blocks = self.children(*channel, "audioBlockFormat");
            let Some(block) = blocks.first().copied() else {
                continue;
            };
            let output_refs = child_texts(
                self.document,
                block,
                self.afe_namespace.as_ref(),
                "outputChannelFormatIDRef",
            )
            .into_iter()
            .map(canonical_id)
            .collect::<Vec<_>>();
            if output_refs.len() == 1 {
                let output = output_refs[0].clone();
                if !output_channels.contains(&output) || !mapped_outputs.insert(output.clone()) {
                    errors.push(self.evidence(
                        block,
                        format!("Matrix output {output} is outside {output_pack} or repeated"),
                    ));
                }
            }
            for matrix in self.children(block, "matrix") {
                let mut coefficient_refs = BTreeSet::new();
                for coefficient in self.children(matrix, "coefficient") {
                    coefficient_visits = coefficient_visits.saturating_add(1);
                    let coefficient_node = self.node(coefficient);
                    let reference = canonical_id(trim_xml(&coefficient_node.text));
                    if !coefficient_refs.insert(reference.clone()) {
                        errors.push(self.evidence(
                            coefficient,
                            format!("duplicate Matrix coefficient reference {reference}"),
                        ));
                    }
                    if !input_channels.contains(&reference) {
                        errors.push(self.evidence(
                            coefficient,
                            format!("Matrix coefficient reference {reference} is not a channel of input pack {input_pack}"),
                        ));
                    }
                    if attr(coefficient_node, "gainUnit")
                        .is_some_and(|unit| !matches!(unit, "linear" | "dB"))
                    {
                        errors.push(self.evidence(
                            coefficient,
                            "Matrix coefficient gainUnit shall be linear or dB",
                        ));
                    }
                    if let Some(raw) = attr(coefficient_node, "gain") {
                        let proxy = Node {
                            text: raw.to_owned(),
                            attributes: coefficient_node
                                .attributes
                                .iter()
                                .filter(|attribute| attribute.name.local == "gainUnit")
                                .cloned()
                                .collect(),
                            ..Node::default()
                        };
                        if !valid_matrix_coefficient_gain(&proxy) {
                            errors.push(self.evidence(
                                coefficient,
                                "Matrix coefficient gain exceeds +20 dB or is invalid",
                            ));
                        }
                    }
                }
            }
        }
        coefficient_visits
    }

    fn audit_blocks(&mut self, definitions: &Definitions, essence: EssenceInfo) {
        let mut errors = Violations::new(self.max_evidence);
        let mut coordinate_system = None::<bool>; // true = Cartesian
        for channel in definitions.channels.values() {
            let matrix = node_type(self.node(*channel)) == Some("0002");
            let blocks = self.children(*channel, "audioBlockFormat");
            if matrix {
                for block in blocks {
                    if attr(self.node(block), "audioBlockFormatID")
                        .is_none_or(|id| !id.ends_with("_00000001"))
                    {
                        errors.push(
                            self.evidence(block, "Matrix block ID counter shall be 00000001"),
                        );
                    }
                }
                continue;
            }
            let zero = ParsedTime::exact(ExactTime::new(0, 1).unwrap());
            let minimum = ParsedTime::exact(ExactTime::new(5, 1000).unwrap());
            let mut previous_block = None::<(ParsedTime, ParsedTime)>;
            for block in blocks {
                let rtime = attr(self.node(block), "rtime").and_then(parse_time);
                let duration = attr(self.node(block), "duration").and_then(parse_time);
                let rtime_matches = match (rtime.as_ref(), previous_block.as_ref()) {
                    (Some(rtime), Some((previous_rtime, previous_duration))) => {
                        time_sums_match(&[rtime], &[previous_rtime, previous_duration])
                    }
                    (Some(rtime), None) => time_sums_match(&[rtime], &[&zero]),
                    (None, _) => false,
                };
                if !rtime_matches {
                    errors.push(self.evidence(
                        block,
                        format!(
                            "rtime {} does not equal the immediately preceding rtime + duration within the minimum lexical rounding error",
                            attr(self.node(block), "rtime").unwrap_or("invalid"),
                        ),
                    ));
                }
                let Some(duration) = duration else {
                    errors.push(self.evidence(block, "duration is missing or invalid"));
                    previous_block = None;
                    continue;
                };
                let duration_is_zero =
                    compare_time_sums(&[&duration], &[&zero]) == Some(Ordering::Equal);
                if !duration_is_zero
                    && compare_time_sums(&[&duration], &[&minimum]) == Some(Ordering::Less)
                {
                    errors.push(self.evidence(block, "duration is non-zero and less than 5 ms"));
                }
                previous_block = rtime.map(|rtime| (rtime, duration));
                let cartesian_nodes = self.children(block, "cartesian");
                let cartesian = cartesian_nodes
                    .first()
                    .and_then(|index| parse_bool_text(self.node(*index)))
                    .unwrap_or(false);
                if let Some(expected) = coordinate_system {
                    if expected != cartesian {
                        errors.push(self.evidence(
                            block,
                            "coordinate system differs from another Objects block",
                        ));
                    }
                } else {
                    coordinate_system = Some(cartesian);
                }
                let positions = self.children(block, "position");
                let expected_axes: BTreeSet<&str> = if cartesian {
                    BTreeSet::from(["X", "Y", "Z"])
                } else {
                    BTreeSet::from(["azimuth", "elevation", "distance"])
                };
                let axes = positions
                    .iter()
                    .filter_map(|index| attr(self.node(*index), "coordinate"))
                    .collect::<BTreeSet<_>>();
                if axes != expected_axes {
                    errors.push(self.evidence(
                        block,
                        format!("position axes are {axes:?}, expected {expected_axes:?}"),
                    ));
                }
                for position in positions {
                    if !valid_position(self.node(position), cartesian) {
                        errors.push(self.evidence(
                            position,
                            "position coordinate value is outside the profile range",
                        ));
                    }
                }
                for divergence in self.children(block, "objectDivergence") {
                    if !valid_divergence(self.node(divergence), cartesian) {
                        errors.push(
                            self.evidence(divergence, "objectDivergence or its range is invalid"),
                        );
                    }
                }
                for gain in self.children(block, "gain") {
                    if !gain_at_most(self.node(gain), 10) {
                        if gain_limit_is_indeterminate(self.node(gain), 10) {
                            self.operational_error.get_or_insert_with(|| {
                                "indeterminate linear gain at the certified +10 dB interval".into()
                            });
                        } else {
                            errors.push(
                                self.evidence(gain, "block gain exceeds +10 dB or is invalid"),
                            );
                        }
                    }
                }
            }
            let essence_duration = ParsedTime::exact(essence.duration);
            match previous_block.as_ref() {
                Some((rtime, duration))
                    if time_sums_match(
                        &[rtime, duration],
                        &[&essence_duration],
                    ) => {}
                Some(_) => errors.push(self.evidence(
                    *channel,
                    format!(
                        "last rtime + duration does not equal PCM essence {} within the minimum lexical rounding error",
                        essence.duration
                    ),
                )),
                None => errors.push(self.evidence(
                    *channel,
                    "block sequence has no valid terminal rtime + duration",
                )),
            }
        }
        self.push_rule("BS2168-2.1.8-BLOCKS", "§ 2.1.8, Tables 21–24", "/audioFormatExtended/audioChannelFormat/audioBlockFormat", "file-based blocks use exact IDs and contiguous rtime/duration covering the full PCM essence, valid coordinates, gains and Matrix mappings", errors);
    }

    fn audit_tracks_chna(
        &mut self,
        definitions: &Definitions,
        chna: &[u8],
        essence: EssenceInfo,
        chna_count: usize,
    ) {
        let mut errors = Violations::new(self.max_evidence);
        errors.extend(self.chna_profile_errors(chna, essence, &definitions.tracks));
        let mut owners = HashMap::<String, usize>::new();
        for object in definitions.objects.values() {
            let pack = child_texts(
                self.document,
                *object,
                self.afe_namespace.as_ref(),
                "audioPackFormatIDRef",
            )
            .first()
            .map(|value| canonical_id(value));
            let object_tracks = child_texts(
                self.document,
                *object,
                self.afe_namespace.as_ref(),
                "audioTrackUIDRef",
            )
            .into_iter()
            .map(canonical_id)
            .collect::<Vec<_>>();
            let pack_channels =
                pack.as_ref()
                    .and_then(|id| definitions.packs.get(id))
                    .map(|index| {
                        child_texts(
                            self.document,
                            *index,
                            self.afe_namespace.as_ref(),
                            "audioChannelFormatIDRef",
                        )
                        .into_iter()
                        .map(canonical_id)
                        .collect::<BTreeSet<_>>()
                    });
            let allowed_channels = pack_channels
                .clone()
                .or_else(|| pack.as_deref().and_then(direct_speaker_channels));
            let expected_channels = allowed_channels.as_ref().map(BTreeSet::len);
            if expected_channels.is_some_and(|count| count != object_tracks.len()) {
                errors.push(self.evidence(
                    *object,
                    format!(
                        "audioTrackUIDRef count {} does not equal referenced pack channel count {}",
                        object_tracks.len(),
                        expected_channels.unwrap()
                    ),
                ));
            }
            let mut track_channels = BTreeSet::new();
            for track in object_tracks {
                *owners.entry(track.clone()).or_default() += 1;
                let Some(index) = definitions.tracks.get(&track) else {
                    errors.push(self.evidence(
                        *object,
                        format!("unresolved or silent audioTrackUIDRef {track}"),
                    ));
                    continue;
                };
                let track_pack = child_texts(
                    self.document,
                    *index,
                    self.afe_namespace.as_ref(),
                    "audioPackFormatIDRef",
                )
                .first()
                .map(|value| canonical_id(value));
                let channel = child_texts(
                    self.document,
                    *index,
                    self.afe_namespace.as_ref(),
                    "audioChannelFormatIDRef",
                )
                .first()
                .map(|value| canonical_id(value));
                if track_pack != pack {
                    errors.push(self.evidence(
                        *index,
                        "audioTrackUID does not refer back to its audioObject pack",
                    ));
                }
                if let Some(channel) = channel {
                    if !track_channels.insert(channel.clone())
                        || allowed_channels
                            .as_ref()
                            .is_some_and(|channels| !channels.contains(&channel))
                    {
                        errors.push(self.evidence(*index, format!("audioTrackUID channel {channel} is repeated or outside the object pack")));
                    }
                } else {
                    errors.push(
                        self.evidence(*index, "audioTrackUID has no audioChannelFormatIDRef"),
                    );
                }
            }
        }
        for (id, track) in &definitions.tracks {
            let owner_count = owners.get(id).copied().unwrap_or(0);
            if owner_count != 1 {
                errors.push(self.evidence(
                    *track,
                    format!(
                        "audioTrackUID {id} has {owner_count} audioObject owners; expected one"
                    ),
                ));
            }
            let pack = child_texts(
                self.document,
                *track,
                self.afe_namespace.as_ref(),
                "audioPackFormatIDRef",
            )
            .first()
            .map(|value| canonical_id(value));
            let channel = child_texts(
                self.document,
                *track,
                self.afe_namespace.as_ref(),
                "audioChannelFormatIDRef",
            )
            .first()
            .map(|value| canonical_id(value));
            let valid_pair = match (pack.as_deref(), channel.as_deref()) {
                (Some(pack), Some(channel)) => {
                    if let Some(common_channels) = direct_speaker_channels(pack) {
                        common_channels.contains(channel)
                    } else if let Some(pack_index) = definitions.packs.get(pack) {
                        node_type(self.node(*pack_index)) == Some("0003")
                            && definitions
                                .channels
                                .get(channel)
                                .is_some_and(|channel_index| {
                                    node_type(self.node(*channel_index)) == Some("0003")
                                })
                            && child_texts(
                                self.document,
                                *pack_index,
                                self.afe_namespace.as_ref(),
                                "audioChannelFormatIDRef",
                            )
                            .into_iter()
                            .map(canonical_id)
                            .any(|member| member == channel)
                    } else {
                        false
                    }
                }
                _ => false,
            };
            if !valid_pair {
                errors.push(self.evidence(
                    *track,
                    "audioTrackUID pack/channel pair shall identify one local Objects channel or one exact Table-16 DirectSpeakers channel",
                ));
            }
            if let Some(rate) = attr(self.node(*track), "sampleRate") {
                if parse_unsigned_integer(rate) != Some(u64::from(essence.sample_rate)) {
                    errors.push(self.evidence(
                        *track,
                        format!(
                            "sampleRate {rate} does not match PCM {}",
                            essence.sample_rate
                        ),
                    ));
                }
            }
            if let Some(depth) = attr(self.node(*track), "bitDepth") {
                let declared = parse_unsigned_integer(depth);
                if declared != Some(u64::from(essence.container_bit_depth))
                    && declared != Some(u64::from(essence.valid_bit_depth))
                {
                    errors.push(self.evidence(
                        *track,
                        format!(
                            "bitDepth {depth} does not match PCM container/valid widths {}/{}",
                            essence.container_bit_depth, essence.valid_bit_depth
                        ),
                    ));
                }
            }
        }
        self.push_rule("BS2168-2.1.9-TRACK-UID", "§ 2.1.9, Tables 25–26", "/audioFormatExtended/audioTrackUID", "each TrackUID has one object owner and reconciles its pack, channel, sample rate and bit depth", errors);
        let chna_errors = self.chna_carrier_errors(chna, essence, chna_count);
        self.push_rule_with_authority(
            "BS2088-8-9-CHNA-CARRIER",
            "ITU-R BS.2088-2",
            "§§ 8–9",
            "/chna",
            "a structurally valid chna carrier is present when required",
            chna_errors,
        );
    }

    fn chna_carrier_errors(
        &self,
        body: &[u8],
        essence: EssenceInfo,
        chna_count: usize,
    ) -> Violations {
        let mut errors = Violations::new(self.max_evidence);
        if chna_count != 1 {
            errors.push(Evidence {
                path: "/chna".into(),
                observed: format!("found {chna_count} chna chunks; expected exactly one"),
            });
        }
        if body.len() < 4 {
            errors.push(Evidence {
                path: "/chna".into(),
                observed: "chna is shorter than its four-byte header".into(),
            });
            return errors;
        }
        let num_tracks = u16::from_le_bytes(body[0..2].try_into().unwrap());
        let num_uids = u16::from_le_bytes(body[2..4].try_into().unwrap());
        let used = usize::from(num_uids).saturating_mul(40);
        let records = &body[4..];
        if !records.len().is_multiple_of(40)
            || used > records.len()
            || records
                .get(used..)
                .is_none_or(|tail| tail.iter().any(|byte| *byte != 0))
        {
            errors.push(Evidence {
                path: "/chna".into(),
                observed: format!(
                    "invalid chna allocation: {} bytes for {num_uids} UID(s)",
                    body.len()
                ),
            });
            return errors;
        }
        if num_tracks != essence.channels {
            errors.push(Evidence {
                path: "/chna/@numTracks".into(),
                observed: format!("{num_tracks} declared, {} PCM channels", essence.channels),
            });
        }
        let mut uids = BTreeSet::new();
        for (offset, record) in records[..used].as_chunks::<40>().0.iter().enumerate() {
            let index = u16::from_le_bytes(record[..2].try_into().unwrap());
            let path = format!("/chna/audioID[{}]", offset + 1);
            let Some(uid_raw) = padded_chna_ascii(&record[2..14]) else {
                errors.push(Evidence {
                    path: path.clone(),
                    observed: "UID field is not ASCII with only trailing NUL padding".into(),
                });
                continue;
            };
            let Some(track_ref_raw) = padded_chna_ascii(&record[14..28]) else {
                errors.push(Evidence {
                    path: path.clone(),
                    observed:
                        "audioTrackFormatIDRef field is not ASCII with only trailing NUL padding"
                            .into(),
                });
                continue;
            };
            let Some(pack_ref_raw) = padded_chna_ascii(&record[28..39]) else {
                errors.push(Evidence {
                    path: path.clone(),
                    observed:
                        "audioPackFormatIDRef field is not ASCII with only trailing NUL padding"
                            .into(),
                });
                continue;
            };
            let uid = canonical_id(uid_raw);
            if !valid_track_id(&uid) {
                errors.push(Evidence {
                    path: path.clone(),
                    observed: format!("UID field {uid_raw:?} is not an ATU_vvvvvvvv ID"),
                });
            }
            if !valid_chna_track_ref(track_ref_raw) {
                errors.push(Evidence {
                    path: path.clone(),
                    observed: format!(
                        "audioTrackFormatIDRef field {track_ref_raw:?} is neither empty nor a serialized AT/AC reference"
                    ),
                });
            }
            if !pack_ref_raw.is_empty() && !valid_reference_id(pack_ref_raw, "AP_") {
                errors.push(Evidence {
                    path: path.clone(),
                    observed: format!(
                        "audioPackFormatIDRef field {pack_ref_raw:?} is neither empty nor a serialized AP reference"
                    ),
                });
            }
            if record[39] != 0 {
                errors.push(Evidence {
                    path: path.clone(),
                    observed: format!("reserved CHNA pad byte is {}, expected 0", record[39]),
                });
            }
            if !(1..=num_tracks).contains(&index) {
                errors.push(Evidence {
                    path: path.clone(),
                    observed: format!("trackIndex {index} is outside 1..={num_tracks}"),
                });
            }
            if !uids.insert(uid.clone()) {
                errors.push(Evidence {
                    path,
                    observed: format!("UID {uid} is repeated"),
                });
            }
        }
        errors
    }

    fn chna_profile_errors(
        &self,
        body: &[u8],
        essence: EssenceInfo,
        tracks: &BTreeMap<String, usize>,
    ) -> Violations {
        let mut errors = Violations::new(self.max_evidence);
        if !essence.integer_pcm {
            errors.push(Evidence {
                path: "/fmt".into(),
                observed: "emission profile requires integer PCM essence, found IEEE float".into(),
            });
        }
        if !essence.aligned {
            errors.push(Evidence {
                path: "/data".into(),
                observed: "PCM data size is not an integral number of sample frames".into(),
            });
        }
        if !essence.probe_data_size_matches {
            errors.push(Evidence {
                path: "/data".into(),
                observed: "strict scanner data size differs from WavReader probe".into(),
            });
        }
        if !essence.ds64_sample_count_matches {
            errors.push(Evidence {
                path: "/ds64/@sampleCount".into(),
                observed: "ds64 sampleCount differs from PCM data frame count".into(),
            });
        }
        if body.len() < 4 {
            return errors;
        }
        let num_uids = u16::from_le_bytes(body[2..4].try_into().unwrap());
        let used = usize::from(num_uids).saturating_mul(40);
        let records = &body[4..];
        if !records.len().is_multiple_of(40) || used > records.len() {
            return errors;
        }
        let mut indices = HashSet::new();
        let mut uids = BTreeSet::new();
        for (offset, record) in records[..used].as_chunks::<40>().0.iter().enumerate() {
            let index = u16::from_le_bytes(record[..2].try_into().unwrap());
            let path = format!("/chna/audioID[{}]", offset + 1);
            let (Some(uid_raw), Some(track_ref_raw), Some(pack_ref_raw)) = (
                exact_chna_ascii(&record[2..14]),
                exact_chna_ascii(&record[14..28]),
                exact_chna_ascii(&record[28..39]),
            ) else {
                errors.push(Evidence {
                    path,
                    observed: "emission profile requires exact fixed-width UID, AC trackRef and packRef fields".into(),
                });
                continue;
            };
            let uid = canonical_id(uid_raw);
            let track_ref = canonical_id(track_ref_raw);
            let pack_ref = canonical_id(pack_ref_raw);
            if !valid_track_id(&uid) {
                errors.push(Evidence {
                    path: path.clone(),
                    observed: format!("UID field {uid_raw:?} is not an ATU_vvvvvvvv ID"),
                });
            }
            if !track_ref
                .strip_suffix("_00")
                .is_some_and(|channel| valid_reference_id(channel, "AC_"))
            {
                errors.push(Evidence {
                    path: path.clone(),
                    observed: format!(
                        "audioTrackFormatIDRef field {track_ref_raw:?} is not AC_xxxxxxxx_00"
                    ),
                });
            }
            if !valid_reference_id(&pack_ref, "AP_") {
                errors.push(Evidence {
                    path: path.clone(),
                    observed: format!("audioPackFormatIDRef field {pack_ref_raw:?} is invalid"),
                });
            }
            if !(1..=essence.channels).contains(&index) || !indices.insert(index) {
                errors.push(Evidence {
                    path: path.clone(),
                    observed: format!(
                        "physical trackIndex {index} is invalid or assigned more than once"
                    ),
                });
            }
            if !uids.insert(uid.clone()) || !tracks.contains_key(&uid) {
                errors.push(Evidence {
                    path: path.clone(),
                    observed: format!("UID {uid} is repeated or absent from axml"),
                });
            }
            if let Some(track) = tracks.get(&uid) {
                let expected_pack = child_texts(
                    self.document,
                    *track,
                    self.afe_namespace.as_ref(),
                    "audioPackFormatIDRef",
                )
                .first()
                .map(|value| canonical_id(value));
                let expected_channel = child_texts(
                    self.document,
                    *track,
                    self.afe_namespace.as_ref(),
                    "audioChannelFormatIDRef",
                )
                .first()
                .map(|value| format!("{}_00", canonical_id(value)));
                if expected_channel.as_deref() != Some(track_ref.as_str()) {
                    errors.push(Evidence {
                        path: path.clone(),
                        observed: format!(
                            "track reference {track_ref} does not match axml {}",
                            expected_channel.as_deref().unwrap_or("missing")
                        ),
                    });
                }
                if expected_pack.as_deref() != Some(pack_ref.as_str()) {
                    errors.push(Evidence {
                        path,
                        observed: format!(
                            "pack {pack_ref} does not match axml {}",
                            expected_pack.as_deref().unwrap_or("missing")
                        ),
                    });
                }
            }
        }
        let expected = tracks.keys().cloned().collect::<BTreeSet<_>>();
        if uids != expected {
            errors.push(Evidence {
                path: "/chna".into(),
                observed: format!(
                    "chna UID set size {} differs from axml size {}",
                    uids.len(),
                    expected.len()
                ),
            });
        }
        if indices.len() != usize::from(essence.channels) {
            errors.push(Evidence {
                path: "/chna".into(),
                observed: format!(
                    "emission mapping covers {} of {} PCM tracks",
                    indices.len(),
                    essence.channels
                ),
            });
        }
        errors
    }
}

const DIRECT_SPEAKER_PACKS: &[&str] = &[
    "AP_00010001",
    "AP_00010801",
    "AP_00010002",
    "AP_00010802",
    "AP_0001000A",
    "AP_0001080A",
    "AP_00010003",
    "AP_00010803",
    "AP_0001000C",
    "AP_0001080C",
    "AP_0001000F",
    "AP_0001080F",
    "AP_0001001B",
    "AP_0001081B",
    "AP_00010004",
    "AP_00010804",
    "AP_0001001C",
    "AP_0001081C",
    "AP_00010005",
    "AP_00010805",
    "AP_0001001E",
    "AP_0001081E",
    "AP_00010017",
    "AP_00010817",
    "AP_0001001F",
    "AP_0001081F",
    "AP_00010009",
    "AP_00010809",
    "AP_00010010",
    "AP_00010810",
];

const DIRECT_SPEAKER_MATRIX_OUTPUTS: &[&str] = &[
    "AP_00010001",
    "AP_00010801",
    "AP_00010002",
    "AP_00010802",
    "AP_00010003",
    "AP_00010803",
    "AP_0001000F",
    "AP_0001080F",
    "AP_00010004",
    "AP_00010804",
    "AP_00010005",
    "AP_00010805",
    "AP_00010017",
    "AP_00010817",
    "AP_00010009",
    "AP_00010809",
];

fn direct_speaker_channel_suffixes(id: &str) -> Option<&'static [&'static str]> {
    Some(match id {
        "AP_00010001" | "AP_00010801" => &["03"],
        "AP_00010002" | "AP_00010802" => &["01", "02"],
        "AP_0001000A" | "AP_0001080A" => &["01", "02", "03"],
        "AP_0001000C" | "AP_0001080C" => &["01", "02", "03", "05", "06"],
        "AP_00010003" | "AP_00010803" => &["01", "02", "03", "04", "05", "06"],
        "AP_0001001B" => &["01", "02", "03", "0A", "0B", "1C", "1D"],
        "AP_0001081B" => &["01", "02", "03", "0A", "0B", "05", "06"],
        "AP_0001000F" => &["01", "02", "03", "04", "0A", "0B", "1C", "1D"],
        "AP_0001080F" => &["01", "02", "03", "04", "0A", "0B", "05", "06"],
        "AP_0001001C" | "AP_0001081C" => &["01", "02", "03", "05", "06", "0D", "0F"],
        "AP_00010004" | "AP_00010804" => &["01", "02", "03", "04", "05", "06", "0D", "0F"],
        "AP_0001001E" | "AP_0001081E" => &["01", "02", "03", "05", "06", "0D", "0F", "10", "12"],
        "AP_00010005" | "AP_00010805" => {
            &["01", "02", "03", "04", "05", "06", "0D", "0F", "10", "12"]
        }
        "AP_0001001F" => &[
            "01", "02", "03", "0A", "0B", "1C", "1D", "22", "23", "1E", "1F",
        ],
        "AP_0001081F" => &[
            "01", "02", "03", "0A", "0B", "05", "06", "0D", "0F", "10", "12",
        ],
        "AP_00010017" => &[
            "01", "02", "03", "04", "0A", "0B", "1C", "1D", "22", "23", "1E", "1F",
        ],
        "AP_00010817" => &[
            "01", "02", "03", "04", "0A", "0B", "05", "06", "0D", "0F", "10", "12",
        ],
        "AP_00010010" => &[
            "18", "19", "03", "1C", "1D", "01", "02", "09", "0A", "0B", "22", "23", "0E", "0C",
            "1E", "1F", "13", "14", "11", "15", "16", "17",
        ],
        "AP_00010810" => &[
            "01", "02", "03", "05", "06", "07", "08", "09", "0A", "0B", "0D", "0F", "0E", "0C",
            "10", "12", "13", "14", "11", "15", "16", "17",
        ],
        "AP_00010009" => &[
            "18", "19", "03", "20", "1C", "1D", "01", "02", "09", "21", "0A", "0B", "22", "23",
            "0E", "0C", "1E", "1F", "13", "14", "11", "15", "16", "17",
        ],
        "AP_00010809" => &[
            "01", "02", "03", "20", "05", "06", "07", "08", "09", "21", "0A", "0B", "0D", "0F",
            "0E", "0C", "10", "12", "13", "14", "11", "15", "16", "17",
        ],
        _ => return None,
    })
}

#[cfg(test)]
fn direct_speaker_channel_count(id: &str) -> Option<usize> {
    direct_speaker_channel_suffixes(id).map(<[_]>::len)
}

fn direct_speaker_channels(id: &str) -> Option<BTreeSet<String>> {
    let suffixes = direct_speaker_channel_suffixes(id)?;
    let prefix = if id.starts_with("AP_000108") {
        "AC_000108"
    } else {
        "AC_000100"
    };
    Some(
        suffixes
            .iter()
            .map(|suffix| format!("{prefix}{suffix}"))
            .collect(),
    )
}

fn canonical_name(name: &str) -> &str {
    name
}

fn is_container_name(name: &str) -> bool {
    matches!(
        name,
        "audioFormatExtended"
            | "audioProgramme"
            | "audioContent"
            | "audioObject"
            | "audioPackFormat"
            | "audioChannelFormat"
            | "audioBlockFormat"
            | "audioTrackUID"
            | "profileList"
            | "loudnessMetadata"
            | "alternativeValueSet"
            | "audioObjectInteraction"
            | "matrix"
    )
}

fn valid_dialogue(node: &Node) -> bool {
    let value = trim_xml(&node.text);
    let expected = match value {
        "0" => ("nonDialogueContentKind", 0_u64, 3_u64),
        "1" => ("dialogueContentKind", 0, 6),
        "2" => ("mixedContentKind", 0, 4),
        _ => return false,
    };
    let kind_attributes = [
        "nonDialogueContentKind",
        "dialogueContentKind",
        "mixedContentKind",
    ];
    let present = kind_attributes
        .iter()
        .filter_map(|name| attr(node, name).map(|value| (*name, value)))
        .collect::<Vec<_>>();
    present.len() == 1
        && present[0].0 == expected.0
        && parse_unsigned_integer(present[0].1)
            .is_some_and(|kind| (expected.1..=expected.2).contains(&kind))
}

fn canonical_id(value: &str) -> String {
    for prefix in ["APR_", "ACO_", "AO_", "AVS_", "AP_", "AC_", "AB_", "ATU_"] {
        if let Some(hex_fields) = value.strip_prefix(prefix) {
            if hex_fields
                .bytes()
                .all(|byte| byte == b'_' || byte.is_ascii_hexdigit())
            {
                return format!("{prefix}{}", hex_fields.to_ascii_uppercase());
            }
        }
    }
    value.to_owned()
}

fn attr<'a>(node: &'a Node, name: &str) -> Option<&'a str> {
    node.attributes
        .iter()
        .find(|attribute| attribute.name.namespace.is_none() && attribute.name.local == name)
        .map(|attribute| attribute.value.as_str())
}

fn node_type(node: &Node) -> Option<&str> {
    attr(node, "typeLabel")
}

fn parent_type(document: &ParsedDocument, index: usize) -> Option<&str> {
    document.nodes[index]
        .parent
        .and_then(|parent| node_type(&document.nodes[parent]))
}

fn build_path_ordinals(document: &ParsedDocument) -> Vec<usize> {
    let mut ordinals = Vec::with_capacity(document.nodes.len());
    let mut sibling_counts = HashMap::<(Option<usize>, XmlName), usize>::new();
    for (index, node) in document.nodes.iter().enumerate() {
        let count = sibling_counts
            .entry((node.parent, node.name.clone()))
            .or_default();
        *count = count.saturating_add(1);
        ordinals.push(*count);
        debug_assert_eq!(ordinals.len(), index + 1);
    }
    ordinals
}

fn bounded_node_path(document: &ParsedDocument, ordinals: &[usize], index: usize) -> String {
    let mut ancestors = Vec::with_capacity(16);
    let mut current = Some(index);
    while let Some(node) = current {
        ancestors.push(node);
        current = document.nodes[node].parent;
    }
    let mut path = String::new();
    for node in ancestors.into_iter().rev() {
        let suffix = format!("[{}]", ordinals[node]);
        let available = MAX_EVIDENCE_PATH_BYTES
            .saturating_sub(path.len())
            .saturating_sub(1 + suffix.len());
        if available == 0 {
            if path.len() + "/…".len() <= MAX_EVIDENCE_PATH_BYTES {
                path.push_str("/…");
            }
            break;
        }
        path.push('/');
        path.push_str(&bounded_utf8(&document.nodes[node].name.local, available));
        path.push_str(&suffix);
        if path.len() >= MAX_EVIDENCE_PATH_BYTES {
            break;
        }
    }
    path
}

fn bounded_utf8(value: &str, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value.to_owned();
    }
    let marker = "…";
    if maximum_bytes < marker.len() {
        let mut end = maximum_bytes.min(value.len());
        while !value.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        return value[..end].to_owned();
    }
    let mut end = maximum_bytes.saturating_sub(marker.len()).min(value.len());
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let mut bounded = value[..end].to_owned();
    bounded.push_str(marker);
    bounded
}

fn child_texts<'a>(
    document: &'a ParsedDocument,
    parent: usize,
    namespace: Option<&Arc<str>>,
    name: &str,
) -> Vec<&'a str> {
    document.nodes[parent]
        .children
        .iter()
        .copied()
        .filter_map(|index| {
            let node = &document.nodes[index];
            (node.name.namespace.as_ref() == namespace && canonical_name(&node.name.local) == name)
                .then_some(trim_xml(&node.text))
        })
        .collect()
}

fn trim_xml(value: &str) -> &str {
    value.trim_matches(is_xml_space)
}

// Sorted alpha-3 and bibliographic aliases from the ISO Codes 639-2 registry
// (which mirrors the Library of Congress list). qaa..=qtz is its reserved local-use range.
const ISO_639_2_CODES: &str = "aar abk ace ach ada ady afa afh afr ain aka akk alb ale alg alt amh ang anp apa ara arc arg arm arn arp art arw asm ast ath aus ava ave awa aym aze bad bai bak bal bam ban baq bas bat bej bel bem ben ber bho bih bik bin bis bla bnt bod bos bra bre btk bua bug bul bur byn cad cai car cat cau ceb cel ces cha chb che chg chi chk chm chn cho chp chr chu chv chy cmc cnr cop cor cos cpe cpf cpp cre crh crp csb cus cym cze dak dan dar day del den deu dgr din div doi dra dsb dua dum dut dyu dzo efi egy eka ell elx eng enm epo est eus ewe ewo fan fao fas fat fij fil fin fiu fon fra fre frm fro frr frs fry ful fur gaa gay gba gem geo ger gez gil gla gle glg glv gmh goh gon gor got grb grc gre grn gsw guj gwi hai hat hau haw heb her hil him hin hit hmn hmo hrv hsb hun hup hye iba ibo ice ido iii ijo iku ile ilo ina inc ind ine inh ipk ira iro isl ita jav jbo jpn jpr jrb kaa kab kac kal kam kan kar kas kat kau kaw kaz kbd kha khi khm kho kik kin kir kmb kok kom kon kor kos kpe krc krl kro kru kua kum kur kut lad lah lam lao lat lav lez lim lin lit lol loz ltz lua lub lug lui lun luo lus mac mad mag mah mai mak mal man mao map mar mas may mdf mdr men mga mic min mis mkd mkh mlg mlt mnc mni mno moh mon mos mri msa mul mun mus mwl mwr mya myn myv nah nai nap nau nav nbl nde ndo nds nep new nia nic niu nld nno nob nog non nor nqo nso nub nwc nya nym nyn nyo nzi oci oji ori orm osa oss ota oto paa pag pal pam pan pap pau peo per phi phn pli pol pon por pra pro pus que raj rap rar roa roh rom ron rum run rup rus sad sag sah sai sal sam san sas sat scn sco sel sem sga sgn shn sid sin sio sit sla slk slo slv sma sme smi smj smn smo sms sna snd snk sog som son sot spa sqi srd srn srp srr ssa ssw suk sun sus sux swa swe syc syr tah tai tam tat tel tem ter tet tgk tgl tha tib tig tir tiv tkl tlh tli tmh tog ton tpi tsi tsn tso tuk tum tup tur tut tvl twi tyv udm uga uig ukr umb und urd uzb vai ven vie vol vot wak wal war was wel wen wln wol xal xho yao yap yid yor ypk zap zbl zen zgh zha zho znd zul zun zxx zza";

fn valid_language(value: &str) -> bool {
    if value.len() != 3 || !value.bytes().all(|byte| byte.is_ascii_lowercase()) {
        return false;
    }
    if ("qaa"..="qtz").contains(&value) {
        return true;
    }
    let mut low = 0_usize;
    let mut high = (ISO_639_2_CODES.len() + 1) / 4;
    while low < high {
        let middle = low + (high - low) / 2;
        let candidate = &ISO_639_2_CODES[middle * 4..middle * 4 + 3];
        match candidate.cmp(value) {
            std::cmp::Ordering::Less => low = middle + 1,
            std::cmp::Ordering::Greater => high = middle,
            std::cmp::Ordering::Equal => return true,
        }
    }
    false
}

fn valid_short_id(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|hex| {
        hex.len() == 4
            && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
            && u16::from_str_radix(hex, 16).is_ok_and(|number| number >= 0x1001)
    })
}

fn valid_format_id(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|hex| {
        hex.len() == 8
            && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
            && matches!(&hex[..4], "0002" | "0003")
            && u16::from_str_radix(&hex[4..], 16).is_ok_and(|number| number >= 0x1001)
    })
}

fn valid_reference_id(value: &str, prefix: &str) -> bool {
    value
        .strip_prefix(prefix)
        .is_some_and(|hex| hex.len() == 8 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn valid_track_id(value: &str) -> bool {
    value.strip_prefix("ATU_").is_some_and(|hex| {
        hex.len() == 8
            && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
            && u32::from_str_radix(hex, 16).is_ok_and(|number| number > 0)
    })
}

fn check_limit(
    errors: &mut Violations,
    path: String,
    name: &str,
    observed: usize,
    maximum: Option<usize>,
) {
    if maximum.is_some_and(|maximum| observed > maximum) {
        errors.push(Evidence {
            path,
            observed: format!("{name}={observed}, maximum={}", maximum.unwrap()),
        });
    }
}

fn check_range_limit(
    errors: &mut Violations,
    path: String,
    name: &str,
    observed: usize,
    minimum: usize,
    maximum: Option<usize>,
) {
    if observed < minimum || maximum.is_some_and(|maximum| observed > maximum) {
        errors.push(Evidence {
            path,
            observed: format!(
                "{name}={observed}, required {minimum}..={}",
                maximum.map_or("unlimited".into(), |value| value.to_string())
            ),
        });
    }
}

struct GraphAnalysis {
    depths: BTreeMap<String, usize>,
    track_counts: BTreeMap<String, usize>,
    track_counts_truncated: BTreeSet<String>,
    cyclic: BTreeSet<String>,
}

#[derive(Clone, Copy)]
struct ComplementaryMembership {
    first_group: usize,
    duplicate_group: bool,
}

fn complementary_group_membership_index(
    groups: &[BTreeSet<String>],
) -> HashMap<&str, ComplementaryMembership> {
    let mut index = HashMap::<&str, ComplementaryMembership>::new();
    for (group_index, group) in groups.iter().enumerate() {
        for member in group {
            index
                .entry(member.as_str())
                .and_modify(|membership| membership.duplicate_group = true)
                .or_insert(ComplementaryMembership {
                    first_group: group_index,
                    duplicate_group: false,
                });
        }
    }
    index
}

fn count_included_complementary_groups(
    included: &BTreeSet<String>,
    groups_by_member: &HashMap<&str, ComplementaryMembership>,
) -> (BTreeMap<usize, usize>, usize) {
    let mut counts = BTreeMap::<usize, usize>::new();
    let mut membership_visits = 0_usize;
    for member in included {
        if let Some(membership) = groups_by_member.get(member.as_str()) {
            membership_visits = membership_visits.saturating_add(1);
            if !membership.duplicate_group {
                *counts.entry(membership.first_group).or_default() += 1;
            }
        }
    }
    (counts, membership_visits)
}

fn analyze_object_graph(
    edges: &BTreeMap<String, Vec<String>>,
    definitions: &Definitions,
    document: &ParsedDocument,
    namespace: Option<&Arc<str>>,
) -> GraphAnalysis {
    let mut indegree = definitions
        .objects
        .keys()
        .map(|id| (id.clone(), 0_usize))
        .collect::<BTreeMap<_, _>>();
    for children in edges.values() {
        for child in children {
            if let Some(value) = indegree.get_mut(child) {
                *value = value.saturating_add(1);
            }
        }
    }
    let mut ready = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(id, _)| id.clone())
        .collect::<BTreeSet<_>>();
    let mut depths = definitions
        .objects
        .keys()
        .map(|id| (id.clone(), 1_usize))
        .collect::<BTreeMap<_, _>>();
    let mut order = Vec::with_capacity(definitions.objects.len());
    while let Some(id) = ready.pop_first() {
        order.push(id.clone());
        let parent_depth = depths[&id];
        if let Some(children) = edges.get(&id) {
            for child in children {
                if !definitions.objects.contains_key(child) {
                    continue;
                }
                if let Some(depth) = depths.get_mut(child) {
                    *depth = (*depth).max(parent_depth.saturating_add(1));
                }
                if let Some(degree) = indegree.get_mut(child) {
                    *degree = degree.saturating_sub(1);
                    if *degree == 0 {
                        ready.insert(child.clone());
                    }
                }
            }
        }
    }
    let cyclic = indegree
        .into_iter()
        .filter(|(_, degree)| *degree != 0)
        .map(|(id, _)| id)
        .collect::<BTreeSet<_>>();
    let mut track_counts = definitions
        .objects
        .iter()
        .map(|(id, index)| {
            (
                id.clone(),
                child_texts(document, *index, namespace, "audioTrackUIDRef").len(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut track_counts_truncated = BTreeSet::new();
    for id in order.into_iter().rev() {
        let mut total = track_counts.get(&id).copied().unwrap_or(0);
        let mut truncated = false;
        if let Some(children) = edges.get(&id) {
            for child in children {
                truncated |= track_counts_truncated.contains(child);
                if let Some(child_count) = track_counts.get(child).copied() {
                    let (next, addition_truncated) = bounded_count_add(total, child_count);
                    total = next;
                    truncated |= addition_truncated;
                }
            }
        }
        if let Some(stored) = track_counts.get_mut(&id) {
            *stored = total;
        }
        if truncated {
            track_counts_truncated.insert(id);
        }
    }
    GraphAnalysis {
        depths,
        track_counts,
        track_counts_truncated,
        cyclic,
    }
}

fn bounded_count_add(left: usize, right: usize) -> (usize, bool) {
    match left.checked_add(right) {
        Some(total) if total <= MAX_SERIALIZED_COUNT => (total, false),
        _ => (MAX_SERIALIZED_COUNT, true),
    }
}

fn object_configuration_signature(
    document: &ParsedDocument,
    index: usize,
    namespace: Option<&Arc<str>>,
    subtree_digests: &[[u8; 32]],
) -> [u8; 32] {
    let node = &document.nodes[index];
    let mut hasher = Sha256::new();
    hash_framed(&mut hasher, b"ADM-OBJECT-CONFIGURATION-V1");
    hash_framed(
        &mut hasher,
        attr(node, "interact").unwrap_or_default().as_bytes(),
    );
    for name in ["gain", "positionOffset", "audioObjectInteraction"] {
        hash_framed(&mut hasher, name.as_bytes());
        for child in node.children.iter().copied().filter(|child| {
            document.nodes[*child].name.namespace.as_ref() == namespace
                && canonical_name(&document.nodes[*child].name.local) == name
        }) {
            hasher.update(subtree_digests[child]);
        }
    }
    hasher.finalize().into()
}

fn object_pack_type(
    document: &ParsedDocument,
    index: usize,
    namespace: Option<&Arc<str>>,
    definitions: &Definitions,
) -> Option<String> {
    let reference = child_texts(document, index, namespace, "audioPackFormatIDRef")
        .first()
        .map(|value| canonical_id(value))?;
    if direct_speaker_channel_suffixes(&reference).is_some() {
        return Some("0001".into());
    }
    definitions
        .packs
        .get(&reference)
        .and_then(|pack| node_type(&document.nodes[*pack]))
        .map(str::to_owned)
}

fn subtree_digest_cache(document: &ParsedDocument, ignored_attributes: &[&str]) -> Vec<[u8; 32]> {
    let mut digests = vec![[0_u8; 32]; document.nodes.len()];
    let mut visited = vec![false; document.nodes.len()];
    let mut stack = Vec::<(usize, bool)>::new();

    for root in (0..document.nodes.len())
        .filter(|index| document.nodes[*index].parent.is_none())
        .chain(0..document.nodes.len())
    {
        if visited[root] {
            continue;
        }
        stack.push((root, false));
        while let Some((index, expanded)) = stack.pop() {
            if expanded {
                let node = &document.nodes[index];
                let mut hasher = Sha256::new();
                hash_framed(&mut hasher, b"ADM-SUBTREE-V1");
                hash_optional_text(&mut hasher, node.name.namespace.as_deref());
                hash_framed(&mut hasher, canonical_name(&node.name.local).as_bytes());

                let mut attributes = node
                    .attributes
                    .iter()
                    .filter(|attribute| {
                        !ignored_attributes.contains(&attribute.name.local.as_str())
                    })
                    .collect::<Vec<_>>();
                attributes.sort_by(|left, right| {
                    left.name
                        .cmp(&right.name)
                        .then(left.value.cmp(&right.value))
                });
                hash_usize(&mut hasher, attributes.len());
                for attribute in attributes {
                    hash_optional_text(&mut hasher, attribute.name.namespace.as_deref());
                    hash_framed(&mut hasher, attribute.name.local.as_bytes());
                    hash_framed(&mut hasher, attribute.value.as_bytes());
                }
                hash_framed(&mut hasher, trim_xml(&node.text).as_bytes());
                hash_usize(&mut hasher, node.children.len());
                for child in &node.children {
                    hasher.update(digests[*child]);
                }
                digests[index] = hasher.finalize().into();
                visited[index] = true;
                continue;
            }
            if visited[index] {
                continue;
            }
            stack.push((index, true));
            for child in document.nodes[index].children.iter().rev() {
                if !visited[*child] {
                    stack.push((*child, false));
                }
            }
        }
    }
    digests
}

fn hash_optional_text(hasher: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            hasher.update([1]);
            hash_framed(hasher, value.as_bytes());
        }
        None => hasher.update([0]),
    }
}

fn hash_framed(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(value);
}

fn hash_usize(hasher: &mut Sha256, value: usize) {
    hasher.update(u64::try_from(value).unwrap_or(u64::MAX).to_be_bytes());
}

#[derive(Clone, Copy)]
struct GainBounds<'a> {
    minimum: GainValue<'a>,
    maximum: GainValue<'a>,
}

fn interaction_gain_bounds<'a>(
    document: &'a ParsedDocument,
    interaction: usize,
    namespace: Option<&Arc<str>>,
) -> Option<GainBounds<'a>> {
    let ranges = direct_child_indices(document, interaction, namespace, "gainInteractionRange");
    if ranges.len() != 2 {
        return None;
    }
    let minimum = ranges
        .iter()
        .find(|index| attr(&document.nodes[**index], "bound") == Some("min"))
        .and_then(|index| gain_value(&document.nodes[*index]))?;
    let maximum = ranges
        .iter()
        .find(|index| attr(&document.nodes[**index], "bound") == Some("max"))
        .and_then(|index| gain_value(&document.nodes[*index]))?;
    Some(GainBounds { minimum, maximum })
}

#[derive(Clone, Copy)]
struct PositionBounds<'a> {
    coordinate: &'a str,
    minimum: &'a str,
    maximum: &'a str,
}

fn interaction_position_bounds<'a>(
    document: &'a ParsedDocument,
    interaction: usize,
    namespace: Option<&Arc<str>>,
) -> Option<PositionBounds<'a>> {
    let ranges = direct_child_indices(document, interaction, namespace, "positionInteractionRange");
    if ranges.len() != 2 {
        return None;
    }
    let minimum_index = *ranges
        .iter()
        .find(|index| attr(&document.nodes[**index], "bound") == Some("min"))?;
    let maximum_index = *ranges
        .iter()
        .find(|index| attr(&document.nodes[**index], "bound") == Some("max"))?;
    let coordinate = attr(&document.nodes[minimum_index], "coordinate")?;
    if attr(&document.nodes[maximum_index], "coordinate") != Some(coordinate) {
        return None;
    }
    let minimum = trim_xml(&document.nodes[minimum_index].text);
    let maximum = trim_xml(&document.nodes[maximum_index].text);
    valid_decimal_lexical(minimum).then_some(())?;
    valid_decimal_lexical(maximum).then_some(())?;
    Some(PositionBounds {
        coordinate,
        minimum,
        maximum,
    })
}

fn direct_child_indices(
    document: &ParsedDocument,
    parent: usize,
    namespace: Option<&Arc<str>>,
    name: &str,
) -> Vec<usize> {
    document.nodes[parent]
        .children
        .iter()
        .copied()
        .filter(|index| {
            document.nodes[*index].name.namespace.as_ref() == namespace
                && document.nodes[*index].name.local == name
        })
        .collect()
}

fn node_or_descendants_use_position_control(
    document: &ParsedDocument,
    root: usize,
    namespace: Option<&Arc<str>>,
) -> bool {
    let mut pending = vec![root];
    while let Some(index) = pending.pop() {
        let node = &document.nodes[index];
        if node.name.namespace.as_ref() == namespace
            && (node.name.local == "positionOffset"
                || node.name.local == "positionInteractionRange"
                || (node.name.local == "audioObjectInteraction"
                    && attr(node, "positionInteract").is_some()))
        {
            return true;
        }
        pending.extend(node.children.iter().copied());
    }
    false
}

type PositionPackIndex = BTreeMap<String, Option<&'static str>>;

fn build_position_pack_index(
    document: &ParsedDocument,
    namespace: Option<&Arc<str>>,
    definitions: &Definitions,
) -> (PositionPackIndex, usize) {
    let mut channel_coordinates = BTreeMap::<String, Option<&'static str>>::new();
    let mut inspected_blocks = 0_usize;
    for (channel_id, channel) in &definitions.channels {
        if node_type(&document.nodes[*channel]) != Some("0003") {
            continue;
        }
        let blocks = direct_child_indices(document, *channel, namespace, "audioBlockFormat");
        let mut coordinate = None;
        let mut valid = true;
        for block in blocks {
            inspected_blocks = inspected_blocks.saturating_add(1);
            let cartesian = direct_child_indices(document, block, namespace, "cartesian")
                .first()
                .and_then(|index| parse_bool_text(&document.nodes[*index]))
                .unwrap_or(false);
            let current = if cartesian { "X" } else { "azimuth" };
            if coordinate.is_some_and(|coordinate| coordinate != current)
                || !object_block_is_at_origin(document, block, namespace)
            {
                valid = false;
                break;
            }
            coordinate = Some(current);
        }
        channel_coordinates.insert(channel_id.clone(), valid.then_some(coordinate).flatten());
    }

    let mut coordinates = BTreeMap::new();
    for (pack_id, pack) in &definitions.packs {
        if node_type(&document.nodes[*pack]) != Some("0003") {
            continue;
        }
        let channel_ids = child_texts(document, *pack, namespace, "audioChannelFormatIDRef");
        let coordinate = if channel_ids.len() != 1 {
            None
        } else {
            channel_coordinates
                .get(&canonical_id(channel_ids[0]))
                .copied()
                .flatten()
        };
        coordinates.insert(pack_id.clone(), coordinate);
    }
    (coordinates, inspected_blocks)
}

fn object_position_coordinate(
    document: &ParsedDocument,
    object: usize,
    namespace: Option<&Arc<str>>,
    position_pack_index: &PositionPackIndex,
) -> Option<&'static str> {
    if !direct_child_indices(document, object, namespace, "audioObjectIDRef").is_empty() {
        return None;
    }
    let pack_id = child_texts(document, object, namespace, "audioPackFormatIDRef")
        .first()
        .map(|value| canonical_id(value))?;
    position_pack_index.get(&pack_id).copied().flatten()
}

fn position_control_coordinates_match(
    document: &ParsedDocument,
    object: usize,
    namespace: Option<&Arc<str>>,
    expected: &str,
) -> bool {
    let mut pending = vec![object];
    while let Some(index) = pending.pop() {
        let node = &document.nodes[index];
        if node.name.namespace.as_ref() == namespace
            && matches!(
                node.name.local.as_str(),
                "positionOffset" | "positionInteractionRange"
            )
            && attr(node, "coordinate") != Some(expected)
        {
            return false;
        }
        for child in &node.children {
            if document.nodes[*child].name.namespace.as_ref() == namespace
                && !matches!(
                    document.nodes[*child].name.local.as_str(),
                    "audioObject" | "audioPackFormat" | "audioChannelFormat"
                )
            {
                pending.push(*child);
            }
        }
    }
    true
}

fn object_block_is_at_origin(
    document: &ParsedDocument,
    block: usize,
    namespace: Option<&Arc<str>>,
) -> bool {
    let cartesian = direct_child_indices(document, block, namespace, "cartesian")
        .first()
        .and_then(|index| parse_bool_text(&document.nodes[*index]))
        .unwrap_or(false);
    let positions = direct_child_indices(document, block, namespace, "position");
    let expected: &[(&str, &str)] = if cartesian {
        &[("X", "0"), ("Y", "1"), ("Z", "0")]
    } else {
        &[("azimuth", "0"), ("elevation", "0"), ("distance", "1")]
    };
    positions.len() == 3
        && expected.iter().all(|(coordinate, value)| {
            positions.iter().any(|index| {
                attr(&document.nodes[*index], "coordinate") == Some(*coordinate)
                    && compare_decimal(trim_xml(&document.nodes[*index].text), value)
                        == Some(Ordering::Equal)
            })
        })
}

#[derive(Clone, Copy)]
struct DecimalParts<'a> {
    negative: bool,
    whole: &'a str,
    fraction: &'a str,
}

fn decimal_parts(value: &str) -> Option<DecimalParts<'_>> {
    if !valid_decimal_lexical(value) {
        return None;
    }
    let (negative, unsigned) = if let Some(unsigned) = value.strip_prefix('-') {
        (true, unsigned)
    } else {
        (false, value.strip_prefix('+').unwrap_or(value))
    };
    let (whole, fraction) = unsigned
        .split_once('.')
        .map_or((unsigned, ""), |(whole, fraction)| (whole, fraction));
    let zero = whole == "0" && fraction.bytes().all(|digit| digit == b'0');
    Some(DecimalParts {
        negative: negative && !zero,
        whole,
        fraction: fraction.trim_end_matches('0'),
    })
}

fn compare_decimal(left: &str, right: &str) -> Option<Ordering> {
    let left = decimal_parts(left)?;
    let right = decimal_parts(right)?;
    if left.negative != right.negative {
        return Some(if left.negative {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    let magnitude = compare_decimal_magnitude(left, right);
    Some(if left.negative {
        magnitude.reverse()
    } else {
        magnitude
    })
}

fn compare_decimal_magnitude(left: DecimalParts<'_>, right: DecimalParts<'_>) -> Ordering {
    left.whole
        .len()
        .cmp(&right.whole.len())
        .then_with(|| left.whole.cmp(right.whole))
        .then_with(|| {
            let width = left.fraction.len().max(right.fraction.len());
            (0..width)
                .map(|index| {
                    (
                        left.fraction.as_bytes().get(index).copied().unwrap_or(b'0'),
                        right
                            .fraction
                            .as_bytes()
                            .get(index)
                            .copied()
                            .unwrap_or(b'0'),
                    )
                })
                .find_map(|(left, right)| (left != right).then_some(left.cmp(&right)))
                .unwrap_or(Ordering::Equal)
        })
}

fn decimal_in_range(value: &str, minimum: &str, maximum: &str) -> bool {
    compare_decimal(value, minimum).is_some_and(|order| order != Ordering::Less)
        && compare_decimal(value, maximum).is_some_and(|order| order != Ordering::Greater)
}

fn decimal_is_within(value: &str, minimum: &str, maximum: &str) -> bool {
    decimal_in_range(value, minimum, maximum)
        && compare_decimal(minimum, maximum).is_some_and(|ordering| ordering != Ordering::Greater)
}

fn decimal_magnitude_at_most(value: &str, maximum: &str) -> bool {
    decimal_parts(value).is_some_and(|value| {
        compare_decimal_magnitude(
            DecimalParts {
                negative: false,
                ..value
            },
            decimal_parts(maximum).expect("fixed decimal bound is valid"),
        ) != Ordering::Greater
    })
}

fn valid_decimal_lexical(value: &str) -> bool {
    let unsigned = value
        .strip_prefix('+')
        .or_else(|| value.strip_prefix('-'))
        .unwrap_or(value);
    let (whole, fraction) = unsigned
        .split_once('.')
        .map_or((unsigned, None), |(whole, fraction)| {
            (whole, Some(fraction))
        });
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || (whole.len() > 1 && whole.starts_with('0'))
    {
        return false;
    }
    fraction.is_none_or(|fraction| {
        !fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn parse_unsigned_integer(value: &str) -> Option<u64> {
    let unsigned = value.strip_prefix('+').unwrap_or(value);
    if unsigned.is_empty()
        || !unsigned.bytes().all(|byte| byte.is_ascii_digit())
        || (unsigned.len() > 1 && unsigned.starts_with('0'))
    {
        return None;
    }
    unsigned.parse().ok()
}

#[derive(Clone, Copy)]
enum GainValue<'a> {
    NegativeInfinity,
    Db(&'a str),
    Linear(&'a str),
}

fn gain_value(node: &Node) -> Option<GainValue<'_>> {
    let raw = trim_xml(&node.text);
    match attr(node, "gainUnit") {
        Some("dB") => {
            if raw == "-inf" {
                Some(GainValue::NegativeInfinity)
            } else {
                valid_decimal_lexical(raw).then_some(GainValue::Db(raw))
            }
        }
        None | Some("linear") => compare_decimal(raw, "0")
            .is_some_and(|ordering| ordering != Ordering::Less)
            .then_some(GainValue::Linear(raw)),
        _ => None,
    }
}

fn compare_gain_values(left: GainValue<'_>, right: GainValue<'_>) -> Option<Ordering> {
    match (left, right) {
        (GainValue::NegativeInfinity, GainValue::NegativeInfinity) => Some(Ordering::Equal),
        (GainValue::NegativeInfinity, _) => Some(Ordering::Less),
        (_, GainValue::NegativeInfinity) => Some(Ordering::Greater),
        (GainValue::Db(left), GainValue::Db(right))
        | (GainValue::Linear(left), GainValue::Linear(right)) => compare_decimal(left, right),
        (GainValue::Db(db), GainValue::Linear(linear)) => {
            compare_cross_unit_gain(db, linear).map(Ordering::reverse)
        }
        (GainValue::Linear(linear), GainValue::Db(db)) => compare_cross_unit_gain(db, linear),
    }
}

// Return the ordering of a linear gain against a dB gain. Same-unit values are
// always compared lexically and exactly. Cross-unit comparison necessarily
// involves log10; exact common identities are handled first and results too
// close to f64 precision are rejected conservatively (`None`).
fn compare_cross_unit_gain(db: &str, linear: &str) -> Option<Ordering> {
    if compare_decimal(linear, "0") == Some(Ordering::Equal) {
        return Some(Ordering::Less);
    }
    for (exact_db, exact_linear) in [("0", "1"), ("20", "10")] {
        if compare_decimal(db, exact_db) == Some(Ordering::Equal)
            && compare_decimal(linear, exact_linear) == Some(Ordering::Equal)
        {
            return Some(Ordering::Equal);
        }
    }
    let db = db.parse::<f64>().ok().filter(|value| value.is_finite())?;
    let linear = linear
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value > 0.0)?;
    let linear_db = 20.0 * linear.log10();
    let tolerance = 128.0 * f64::EPSILON * db.abs().max(linear_db.abs()).max(1.0);
    ((linear_db - db).abs() > tolerance).then(|| linear_db.total_cmp(&db))
}

fn gain_value_is_within(value: GainValue<'_>, bounds: GainBounds<'_>) -> Option<bool> {
    let above_minimum = compare_gain_values(value, bounds.minimum)? != Ordering::Less;
    let below_maximum = compare_gain_values(value, bounds.maximum)? != Ordering::Greater;
    let ordered = compare_gain_values(bounds.minimum, bounds.maximum)? != Ordering::Greater;
    Some(above_minimum && below_maximum && ordered)
}

fn gain_at_most(node: &Node, maximum_db: i32) -> bool {
    let raw = trim_xml(&node.text);
    match attr(node, "gainUnit") {
        Some("dB") => {
            raw == "-inf"
                || compare_decimal(raw, &maximum_db.to_string())
                    .is_some_and(|order| order != Ordering::Greater)
        }
        None | Some("linear") => {
            compare_decimal(raw, "0").is_some_and(|order| order != Ordering::Less)
                && linear_gain_at_most_db(raw, maximum_db)
        }
        _ => false,
    }
}

fn gain_in_range(node: &Node, minimum_db: Option<i32>, maximum_db: i32) -> bool {
    let raw = trim_xml(&node.text);
    match attr(node, "gainUnit") {
        Some("dB") => {
            if raw == "-inf" {
                return minimum_db.is_none();
            }
            minimum_db.is_none_or(|minimum| {
                compare_decimal(raw, &minimum.to_string())
                    .is_some_and(|order| order != Ordering::Less)
            }) && compare_decimal(raw, &maximum_db.to_string())
                .is_some_and(|order| order != Ordering::Greater)
        }
        None | Some("linear") => {
            let above_minimum = minimum_db.is_none_or(|minimum| {
                linear_gain_threshold(minimum).is_some_and(|threshold| {
                    compare_decimal(raw, threshold).is_some_and(|order| order != Ordering::Less)
                })
            });
            compare_decimal(raw, "0").is_some_and(|order| order != Ordering::Less)
                && above_minimum
                && linear_gain_at_most_db(raw, maximum_db)
        }
        _ => false,
    }
}

fn linear_gain_threshold(decibels: i32) -> Option<&'static str> {
    match decibels {
        0 => Some("1"),
        // Certified decimal lower bounds for 10^(dB/20). A maximum check may
        // safely accept values no greater than these strings. The tiny open
        // interval between the stored lower bound and the next decimal unit is
        // rejected conservatively instead of rounding a non-conforming value
        // into range.
        10 => Some("3.162277660168379331998893544432718533719555139325216826857504852792594438639238221344248108379300295"),
        20 => Some("10"),
        21 => Some("11.22018454301963435591038946477905736722308507360552962445074448170103302686224355942322410693190479"),
        _ => None,
    }
}

fn linear_gain_threshold_upper(decibels: i32) -> Option<&'static str> {
    match decibels {
        10 => Some("3.162277660168379331998893544432718533719555139325216826857504852792594438639238221344248108379300296"),
        21 => Some("11.22018454301963435591038946477905736722308507360552962445074448170103302686224355942322410693190480"),
        _ => linear_gain_threshold(decibels),
    }
}

fn linear_gain_at_most_db(value: &str, maximum_db: i32) -> bool {
    linear_gain_threshold(maximum_db).is_some_and(|maximum| {
        compare_decimal(value, maximum).is_some_and(|order| order != Ordering::Greater)
    })
}

fn gain_limit_is_indeterminate(node: &Node, maximum_db: i32) -> bool {
    if !matches!(attr(node, "gainUnit"), None | Some("linear")) {
        return false;
    }
    let raw = trim_xml(&node.text);
    let Some(lower) = linear_gain_threshold(maximum_db) else {
        return false;
    };
    let Some(upper) = linear_gain_threshold_upper(maximum_db) else {
        return false;
    };
    lower != upper
        && compare_decimal(raw, lower) == Some(Ordering::Greater)
        && compare_decimal(raw, upper) == Some(Ordering::Less)
}

fn valid_matrix_coefficient_gain(node: &Node) -> bool {
    let Some(raw) = attr(node, "gain") else {
        return true;
    };
    match attr(node, "gainUnit") {
        Some("dB") => {
            raw == "-inf"
                || compare_decimal(raw, "20").is_some_and(|order| order != Ordering::Greater)
        }
        None | Some("linear") => decimal_magnitude_at_most(raw, "10"),
        _ => false,
    }
}

fn valid_offset(node: &Node) -> bool {
    let value = trim_xml(&node.text);
    match attr(node, "coordinate") {
        Some("azimuth") => decimal_in_range(value, "-30", "30"),
        Some("X") => decimal_in_range(value, "-1", "1"),
        _ => false,
    }
}

fn valid_position(node: &Node, cartesian: bool) -> bool {
    let value = trim_xml(&node.text);
    match (cartesian, attr(node, "coordinate")) {
        (true, Some("X" | "Y" | "Z")) => decimal_in_range(value, "-1", "1"),
        (false, Some("azimuth")) => decimal_in_range(value, "-180", "180"),
        (false, Some("elevation")) => decimal_in_range(value, "-90", "90"),
        (false, Some("distance")) => decimal_in_range(value, "0", "1"),
        _ => false,
    }
}

fn valid_divergence(node: &Node, cartesian: bool) -> bool {
    if !decimal_in_range(trim_xml(&node.text), "0", "1") {
        return false;
    }
    match (
        cartesian,
        attr(node, "azimuthRange"),
        attr(node, "positionRange"),
    ) {
        (false, Some(range), None) => decimal_in_range(range, "0", "180"),
        (true, None, Some(range)) => decimal_in_range(range, "0", "1"),
        _ => false,
    }
}

fn parse_bool_text(node: &Node) -> Option<bool> {
    match trim_xml(&node.text) {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

fn parse_time(value: &str) -> Option<ParsedTime> {
    if let Some((time, rate_text)) = value.split_once('S') {
        if time.is_empty() || rate_text.is_empty() || rate_text.contains('S') {
            return None;
        }
        let rate = parse_short_time_digits(rate_text)?;
        if rate == 0 {
            return None;
        }
        if !time.contains(':') {
            return Some(ParsedTime::exact(ExactTime::new(
                parse_short_time_digits(time)?,
                rate,
            )?));
        }
        let (clock, sample_text) = time.rsplit_once('.')?;
        if sample_text.is_empty()
            || sample_text.len() != rate_text.len()
            || sample_text.contains('.')
        {
            return None;
        }
        let samples = parse_time_digits(sample_text)?;
        if samples >= rate {
            return None;
        }
        return Some(ParsedTime::exact(ExactTime::new(
            parse_long_clock(clock)?
                .checked_mul(rate)?
                .checked_add(samples)?,
            rate,
        )?));
    }
    if value.contains(':') {
        let (clock, fraction) = value.rsplit_once('.')?;
        return decimal_time(parse_long_clock(clock)?, fraction);
    }
    let (whole, fraction) = value.split_once('.')?;
    decimal_time(parse_short_time_digits(whole)?, fraction)
}

fn decimal_time(whole: u128, fraction: &str) -> Option<ParsedTime> {
    if fraction.len() < 5 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mut coefficient = BigNat::from_u128(whole);
    coefficient.mul_pow10(fraction.len());
    coefficient.add_assign(&BigNat::from_decimal(fraction)?);
    Some(ParsedTime {
        value: TimeValue::Decimal {
            coefficient,
            scale: fraction.len(),
        },
    })
}

fn parse_long_clock(value: &str) -> Option<u128> {
    let mut fields = value.split(':');
    let hours = fields.next()?;
    let minutes = fields.next()?;
    let seconds = fields.next()?;
    if fields.next().is_some() || hours.len() != 2 || minutes.len() != 2 || seconds.len() != 2 {
        return None;
    }
    let hours = parse_time_digits(hours)?;
    let minutes = parse_time_digits(minutes)?;
    let seconds = parse_time_digits(seconds)?;
    if minutes >= 60 || seconds >= 60 {
        return None;
    }
    hours
        .checked_mul(3_600)?
        .checked_add(minutes.checked_mul(60)?)?
        .checked_add(seconds)
}

fn parse_time_digits(value: &str) -> Option<u128> {
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

fn parse_short_time_digits(value: &str) -> Option<u128> {
    if value.len() > 1 && value.starts_with('0') {
        return None;
    }
    parse_time_digits(value)
}

fn compare_time_sums(left: &[&ParsedTime], right: &[&ParsedTime]) -> Option<Ordering> {
    let (left, right, _) = scale_time_sums(left, right)?;
    Some(left.cmp_value(&right))
}

fn time_sums_match(left: &[&ParsedTime], right: &[&ParsedTime]) -> bool {
    let Some((left, right, tolerance_ulps)) = scale_time_sums(left, right) else {
        return false;
    };
    let mut doubled_difference = left.abs_diff(&right);
    doubled_difference.mul_assign(&BigNat::from_u128(2));
    doubled_difference.cmp_value(&tolerance_ulps) != Ordering::Greater
}

// Scale a sum of exact sample fractions and arbitrary-precision decimal times
// to one integer denominator. Decimal denominators are powers of ten, so the
// largest scale is used rather than multiplying them together. There are at
// most three terms in the block comparisons, keeping multiplication linear in
// the lexical digit count even at the XML text safety limit. The third return
// value is twice the permitted half-ULP rounding error in the same units.
fn scale_time_sums(
    left: &[&ParsedTime],
    right: &[&ParsedTime],
) -> Option<(BigNat, BigNat, BigNat)> {
    let terms = left.iter().chain(right.iter()).copied().collect::<Vec<_>>();
    let max_scale = terms
        .iter()
        .filter_map(|time| match &time.value {
            TimeValue::Decimal { scale, .. } => Some(*scale),
            TimeValue::Exact(_) => None,
        })
        .max()
        .unwrap_or(0);
    let exact_denominators = terms
        .iter()
        .enumerate()
        .filter_map(|(index, time)| match &time.value {
            TimeValue::Exact(value) => Some((index, value.denominator)),
            TimeValue::Decimal { .. } => None,
        })
        .collect::<Vec<_>>();

    let scale_term = |term_index: usize, time: &ParsedTime| {
        let mut scaled = match &time.value {
            TimeValue::Exact(value) => {
                let mut value = BigNat::from_u128(value.numerator);
                value.mul_pow10(max_scale);
                value
            }
            TimeValue::Decimal { coefficient, scale } => {
                let mut value = coefficient.clone();
                value.mul_pow10(max_scale.checked_sub(*scale)?);
                value
            }
        };
        for (denominator_index, denominator) in &exact_denominators {
            if matches!(&time.value, TimeValue::Decimal { .. }) || *denominator_index != term_index
            {
                scaled.mul_assign(&BigNat::from_u128(*denominator));
            }
        }
        Some(scaled)
    };

    let mut scaled_left = BigNat::zero();
    for (index, time) in left.iter().enumerate() {
        scaled_left.add_assign(&scale_term(index, time)?);
    }
    let mut scaled_right = BigNat::zero();
    for (right_index, time) in right.iter().enumerate() {
        scaled_right.add_assign(&scale_term(left.len() + right_index, time)?);
    }

    let mut tolerance_ulps = BigNat::zero();
    let mut exact_denominator_product = BigNat::one();
    for (_, denominator) in &exact_denominators {
        exact_denominator_product.mul_assign(&BigNat::from_u128(*denominator));
    }
    for time in terms {
        if let TimeValue::Decimal { scale, .. } = &time.value {
            let mut ulp = BigNat::one();
            ulp.mul_pow10(max_scale.checked_sub(*scale)?);
            ulp.mul_assign(&exact_denominator_product);
            tolerance_ulps.add_assign(&ulp);
        }
    }
    Some((scaled_left, scaled_right, tolerance_ulps))
}

fn exact_chna_ascii(bytes: &[u8]) -> Option<&str> {
    if bytes.iter().any(|byte| *byte == 0 || !byte.is_ascii()) {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

fn padded_chna_ascii(bytes: &[u8]) -> Option<&str> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    if !bytes[..end].iter().all(u8::is_ascii) || bytes[end..].iter().any(|byte| *byte != 0) {
        return None;
    }
    std::str::from_utf8(&bytes[..end]).ok()
}

fn valid_chna_track_ref(value: &str) -> bool {
    let Some((format, subtrack)) = value.rsplit_once('_') else {
        return false;
    };
    subtrack.len() == 2
        && subtrack.bytes().all(|byte| byte.is_ascii_hexdigit())
        && (valid_reference_id(format, "AT_")
            || (valid_reference_id(format, "AC_") && subtrack == "00"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Options {
        Options::new("unused.wav", Level::Level1)
    }

    #[test]
    fn level_parsing_and_limits_are_stable() {
        assert_eq!(Level::parse("0").unwrap(), Level::Level0);
        assert_eq!(Level::parse("1").unwrap().to_string(), "1");
        assert!(Level::parse("3").is_err());
        let level1 = ProfileLimits::for_level(Level::Level1);
        assert_eq!(level1.programme, Some(8));
        assert_eq!(level1.non_comp_tracks, Some(16));
        assert_eq!(level1.channels_layout, Some(12));
        let level2 = ProfileLimits::for_level(Level::Level2);
        assert_eq!(level2.object, Some(84));
        assert_eq!(level2.comp_groups, Some(14));
        assert_eq!(ProfileLimits::for_level(Level::Level0).track_uid, None);
    }

    #[test]
    fn exact_time_accepts_bs2076_long_short_and_decimal_forms() {
        let equals = |text, exact| {
            let parsed = parse_time(text).unwrap();
            let exact = ParsedTime::exact(exact);
            compare_time_sums(&[&parsed], &[&exact]) == Some(Ordering::Equal)
        };
        assert!(equals("00:00:00.00500", ExactTime::new(1, 200).unwrap()));
        assert!(equals(
            "00:00:00.00240S48000",
            ExactTime::new(1, 200).unwrap()
        ));
        assert!(equals("240S48000", ExactTime::new(1, 200).unwrap()));
        assert!(equals("0.00500", ExactTime::new(1, 200).unwrap()));
        assert!(parse_time("0.123456789012345678901234567890123456789").is_some());
        assert!(parse_time("00:00:00.005").is_none());
        assert!(parse_time("0.005").is_none());
        assert!(parse_time("00.00000").is_none());
        assert!(parse_time("01.00000").is_none());
        assert!(parse_time("00:00:00.2400S48000").is_none());
        assert!(parse_time("00:00:00.48000S48000").is_none());
        assert!(parse_time("00:00:00.000000S048000").is_none());
        assert!(parse_time("0S048000").is_none());
        assert!(parse_time("0S48000S48000").is_none());
        assert!(parse_time("00:60:00.0").is_none());
        assert!(parse_time("1S0").is_none());
    }

    fn block_rule(xml: &str, duration: ExactTime, sample_rate: u32) -> Rule {
        let document = parse_xml(xml.as_bytes(), &options()).unwrap();
        let afe = document.afe.unwrap();
        let definitions = Definitions::build(&document, afe, None, 64);
        let mut audit = Audit::new(&document, 64, 128);
        audit.audit_blocks(
            &definitions,
            EssenceInfo {
                channels: 1,
                sample_rate,
                container_bit_depth: 24,
                valid_bit_depth: 24,
                duration,
                aligned: true,
                integer_pcm: true,
                probe_data_size_matches: true,
                ds64_sample_count_matches: true,
            },
        );
        audit.rules.pop().unwrap()
    }

    fn objects_block(id: usize, rtime: &str, duration: &str) -> String {
        format!(
            r#"<audioBlockFormat audioBlockFormatID="AB_00031001_{id:08X}" rtime="{rtime}" duration="{duration}">
            <position coordinate="azimuth">0</position><position coordinate="elevation">0</position><position coordinate="distance">1</position>
            </audioBlockFormat>"#
        )
    }

    #[test]
    fn block_timing_accepts_nearest_rounded_44100_and_arbitrary_precision() {
        let rounded = format!(
            r#"<audioFormatExtended version="ITU-R_BS.2076-3"><audioChannelFormat audioChannelFormatID="AC_00031001" typeLabel="0003">{}</audioChannelFormat></audioFormatExtended>"#,
            objects_block(1, "0.00000", "0.02268")
        );
        assert!(block_rule(&rounded, ExactTime::new(1_000, 44_100).unwrap(), 44_100).passed);

        let precise = format!(
            r#"<audioFormatExtended version="ITU-R_BS.2076-3"><audioChannelFormat audioChannelFormatID="AC_00031001" typeLabel="0003">{}</audioChannelFormat></audioFormatExtended>"#,
            objects_block(
                1,
                "0.000000000000000000000000000000000000000",
                "0.005000000000000000000000000000000000000"
            )
        );
        assert!(block_rule(&precise, ExactTime::new(1, 200).unwrap(), 48_000).passed);
    }

    #[test]
    fn block_timing_does_not_add_internal_or_historical_rounding_error() {
        let exact = format!("005{}", "0".repeat(37));
        let mismatch = format!("005{}40", "0".repeat(35));
        assert_eq!(exact.len(), 40);
        assert_eq!(mismatch.len(), 40);
        let xml = format!(
            r#"<audioFormatExtended version="ITU-R_BS.2076-3"><audioChannelFormat audioChannelFormatID="AC_00031001" typeLabel="0003">{}{}</audioChannelFormat></audioFormatExtended>"#,
            objects_block(1, &format!("0.{}", "0".repeat(40)), &format!("0.{exact}")),
            objects_block(2, &format!("0.{mismatch}"), &format!("0.{exact}")),
        );
        let rule = block_rule(&xml, ExactTime::new(1, 100).unwrap(), 48_000);
        assert!(!rule.passed);
        assert!(rule.evidence.iter().any(|evidence| evidence
            .observed
            .contains("immediately preceding rtime + duration")));

        let mut drifting_blocks = String::new();
        for index in 0_usize..10 {
            use std::fmt::Write as _;
            let microseconds = index * 5_000 + index.saturating_sub(1) * 4;
            write!(
                &mut drifting_blocks,
                "{}",
                objects_block(index + 1, &format!("0.{microseconds:06}"), "0.00500")
            )
            .unwrap();
        }
        let drifting = format!(
            r#"<audioFormatExtended version="ITU-R_BS.2076-3"><audioChannelFormat audioChannelFormatID="AC_00031001" typeLabel="0003">{drifting_blocks}</audioChannelFormat></audioFormatExtended>"#
        );
        let rule = block_rule(&drifting, ExactTime::new(1, 20).unwrap(), 1_000_000);
        assert!(!rule.passed);
        assert!(rule
            .evidence
            .iter()
            .any(|evidence| evidence.observed.contains("PCM essence")));
    }

    #[test]
    fn profile_allows_distinct_valid_levels_and_requires_the_requested_level() {
        let document = parse_xml(
            format!(
                r#"<audioFormatExtended><profileList>
                <profile profileName="{PROFILE_NAME}" profileVersion="1" profileLevel="0">{PROFILE_TEXT}</profile>
                <profile profileName="{PROFILE_NAME}" profileVersion="1" profileLevel="1">{PROFILE_TEXT}</profile>
                </profileList></audioFormatExtended>"#
            )
            .as_bytes(),
            &options(),
        )
        .unwrap();
        let afe = document.afe.unwrap();

        let mut level_one = Audit::new(&document, 64, 128);
        level_one.audit_profile(afe, Level::Level1);
        assert!(level_one.rules[0].passed);

        let mut level_two = Audit::new(&document, 64, 128);
        level_two.audit_profile(afe, Level::Level2);
        assert!(!level_two.rules[0].passed);
        assert!(level_two.rules[0].evidence.iter().any(|evidence| evidence
            .observed
            .contains("no exact declaration for requested level 2")));
    }

    fn graph_rule_for_xml(xml: &str) -> (Rule, Counts) {
        let document = parse_xml(xml.as_bytes(), &options()).unwrap();
        let afe = document.afe.unwrap();
        let definitions = Definitions::build(&document, afe, None, 512);
        let mut audit = Audit::new(&document, 512, 1_024);
        audit.audit_graph(&definitions);
        (audit.rules.pop().unwrap(), audit.counts)
    }

    #[test]
    fn nested_object_requires_a_present_local_objects_pack() {
        let (rule, _) = graph_rule_for_xml(
            r#"<audioFormatExtended>
            <audioProgramme audioProgrammeID="APR_1001"><audioContentIDRef>ACO_1001</audioContentIDRef></audioProgramme>
            <audioContent audioContentID="ACO_1001"><audioObjectIDRef>AO_1001</audioObjectIDRef></audioContent>
            <audioObject audioObjectID="AO_1001"><audioObjectIDRef>AO_1002</audioObjectIDRef></audioObject>
            <audioObject audioObjectID="AO_1002"><audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef></audioObject>
            </audioFormatExtended>"#,
        );
        assert!(!rule.passed);
        assert!(rule.evidence.iter().any(|evidence| evidence
            .observed
            .contains("expected a present local Objects audioPackFormat of typeLabel 0003")));
    }

    #[test]
    fn complementary_group_label_is_restricted_to_its_leader() {
        let (rule, _) = graph_rule_for_xml(
            r#"<audioFormatExtended>
            <audioProgramme audioProgrammeID="APR_1001"><audioContentIDRef>ACO_1001</audioContentIDRef></audioProgramme>
            <audioContent audioContentID="ACO_1001"><audioObjectIDRef>AO_1001</audioObjectIDRef></audioContent>
            <audioObject audioObjectID="AO_1001"><audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef><audioComplementaryObjectGroupLabel language="eng">not a group</audioComplementaryObjectGroupLabel></audioObject>
            </audioFormatExtended>"#,
        );
        assert!(!rule.passed);
        assert!(rule.evidence.iter().any(|evidence| evidence
            .observed
            .contains("has a complementary-group label but is not the leader")));
    }

    #[test]
    fn namespace_aware_parser_locates_wrapped_afe() {
        let document = parse_xml(
            br#"<?xml version="1.0" encoding="UTF-8"?><e:root xmlns:e="urn:wrapper"><a:audioFormatExtended xmlns:a="urn:adm" version="ITU-R_BS.2076-3"/></e:root>"#,
            &options(),
        )
        .unwrap();
        assert_eq!(document.afe_count, 1);
        assert_eq!(
            document.nodes[document.afe.unwrap()]
                .name
                .namespace
                .as_deref(),
            Some("urn:adm")
        );
    }

    #[test]
    fn namespace_resolution_ignores_foreign_same_local_elements() {
        let document = parse_xml(
            br#"<w:root xmlns:w="urn:wrapper" xmlns:f="urn:foreign" xmlns:a="urn:adm">
            <f:audioFormatExtended/>
            <a:audioFormatExtended version="ITU-R_BS.2076-3"/>
            </w:root>"#,
            &options(),
        )
        .unwrap();
        assert_eq!(document.afe_count, 1);
        assert_eq!(
            document.nodes[document.afe.unwrap()]
                .name
                .namespace
                .as_deref(),
            Some("urn:adm")
        );

        let ambiguous = parse_xml(
            br#"<root xmlns:a="urn:a" xmlns:b="urn:b"><a:audioFormatExtended version="ITU-R_BS.2076-3"/><b:audioFormatExtended version="ITU-R_BS.2076-3"/></root>"#,
            &options(),
        )
        .unwrap();
        assert_eq!(ambiguous.afe_count, 2);
        assert!(ambiguous.afe.is_none());

        let duplicate_in_selected_namespace = parse_xml(
            br#"<w:root xmlns:w="urn:wrapper" xmlns:f="urn:foreign" xmlns:a="urn:adm">
            <f:audioFormatExtended/>
            <a:audioFormatExtended version="ITU-R_BS.2076-3"/>
            <a:audioFormatExtended/>
            </w:root>"#,
            &options(),
        )
        .unwrap();
        assert_eq!(duplicate_in_selected_namespace.afe_count, 2);
        assert!(duplicate_in_selected_namespace.afe.is_none());
    }

    #[test]
    fn afe_location_rule_does_not_claim_wave_carrier_requirements() {
        let document = parse_xml(
            br#"<audioFormatExtended version="ITU-R_BS.2076-3"/>"#,
            &options(),
        )
        .unwrap();
        let mut audit = Audit::new(&document, 64, 128);
        audit.run(
            Level::Level0,
            ProfileLimits::unlimited(),
            &[],
            EssenceInfo {
                channels: 1,
                sample_rate: 48_000,
                container_bit_depth: 24,
                valid_bit_depth: 24,
                duration: ExactTime::new(1, 1).unwrap(),
                aligned: true,
                integer_pcm: true,
                probe_data_size_matches: true,
                ds64_sample_count_matches: true,
            },
            0,
        );
        let location = audit
            .rules
            .iter()
            .find(|rule| rule.rule_id == "BS2168-3-AFE-LOCATION")
            .unwrap();
        assert!(location.passed);
        assert!(!location.requirement.contains("axml"));
        assert!(audit
            .rules
            .iter()
            .any(|rule| rule.rule_id == "BS2088-8-9-CHNA-CARRIER"));
    }

    #[test]
    fn parser_rejects_dtd_duplicate_expanded_attrs_and_multiple_roots() {
        assert!(parse_xml(br#"<!DOCTYPE x><x/>"#, &options()).is_err());
        assert!(parse_xml(br#"<x/><y/>"#, &options()).is_err());
        assert!(parse_xml(
            br#"<x xmlns:a="urn:x" xmlns:b="urn:x" a:id="1" b:id="2"/>"#,
            &options(),
        )
        .is_err());
    }

    #[test]
    fn parser_enforces_configured_depth_and_text_limits() {
        let mut bounded = options();
        bounded.max_xml_depth = 2;
        assert!(parse_xml(br#"<a><b><c/></b></a>"#, &bounded).is_err());
        bounded.max_xml_depth = DEFAULT_MAX_XML_DEPTH;
        bounded.max_xml_text_bytes = 2;
        assert!(parse_xml(br#"<a>abc</a>"#, &bounded).is_err());
    }

    #[test]
    fn direct_speaker_layout_channel_counts_cover_allowlist_edges() {
        assert_eq!(direct_speaker_channel_count("AP_00010001"), Some(1));
        assert_eq!(direct_speaker_channel_count("AP_00010817"), Some(12));
        assert_eq!(direct_speaker_channel_count("AP_00010009"), Some(24));
        assert_eq!(direct_speaker_channel_count("AP_00019999"), None);
        assert_eq!(
            direct_speaker_channels("AP_0001001B").unwrap(),
            BTreeSet::from([
                "AC_00010001".to_owned(),
                "AC_00010002".to_owned(),
                "AC_00010003".to_owned(),
                "AC_0001000A".to_owned(),
                "AC_0001000B".to_owned(),
                "AC_0001001C".to_owned(),
                "AC_0001001D".to_owned(),
            ])
        );
        assert!(direct_speaker_channels("AP_0001081B")
            .unwrap()
            .contains("AC_00010805"));
        assert!(!DIRECT_SPEAKER_MATRIX_OUTPUTS.contains(&"AP_00010010"));
    }

    fn matrix_audit(xml: &str, input: &str, output: &str) -> (Violations, usize) {
        let document = parse_xml(xml.as_bytes(), &options()).unwrap();
        let afe = document.afe.unwrap();
        let definitions = Definitions::build(&document, afe, None, 64);
        let channels = definitions.channels.keys().cloned().collect::<Vec<_>>();
        let audit = Audit::new(&document, 64, 128);
        let mut errors = Violations::new(64);
        let mut audited = HashSet::new();
        let visits = audit.audit_matrix_pack(
            &channels,
            input,
            output,
            &definitions,
            &mut audited,
            &mut errors,
        );
        (errors, visits)
    }

    #[test]
    fn matrix_mapping_allows_sparse_injective_coefficients_and_outputs() {
        let (errors, visits) = matrix_audit(
            r#"<audioFormatExtended version="ITU-R_BS.2076-3">
            <audioChannelFormat audioChannelFormatID="AC_00021001" typeLabel="0002">
              <audioBlockFormat audioBlockFormatID="AB_00021001_00000001">
                <outputChannelFormatIDRef>AC_00010001</outputChannelFormatIDRef>
                <matrix><coefficient gain="-0.5" gainUnit="linear">AC_00010003</coefficient></matrix>
              </audioBlockFormat>
            </audioChannelFormat>
            </audioFormatExtended>"#,
            "AP_00010001",
            "AP_00010002",
        );
        assert_eq!(errors.total, 0);
        assert_eq!(visits, 1);
    }

    #[test]
    fn matrix_mapping_rejects_duplicate_and_out_of_pack_coefficients() {
        let (errors, _) = matrix_audit(
            r#"<audioFormatExtended version="ITU-R_BS.2076-3">
            <audioChannelFormat audioChannelFormatID="AC_00021001" typeLabel="0002">
              <audioBlockFormat audioBlockFormatID="AB_00021001_00000001">
                <outputChannelFormatIDRef>AC_00010001</outputChannelFormatIDRef>
                <matrix>
                  <coefficient>AC_00010001</coefficient>
                  <coefficient>AC_00010001</coefficient>
                  <coefficient>AC_00010003</coefficient>
                </matrix>
              </audioBlockFormat>
            </audioChannelFormat>
            </audioFormatExtended>"#,
            "AP_00010002",
            "AP_00010002",
        );
        assert!(errors.total >= 2);
        assert!(errors
            .retained
            .iter()
            .any(|evidence| evidence.observed.contains("duplicate")));
        assert!(errors
            .retained
            .iter()
            .any(|evidence| evidence.observed.contains("not a channel")));
    }

    #[test]
    fn matrix_gain_unit_and_signed_linear_limits_are_exact() {
        let node = |gain: &str, unit: &str| Node {
            attributes: vec![
                XmlAttribute {
                    name: XmlName {
                        namespace: None,
                        local: "gain".into(),
                    },
                    value: gain.into(),
                },
                XmlAttribute {
                    name: XmlName {
                        namespace: None,
                        local: "gainUnit".into(),
                    },
                    value: unit.into(),
                },
            ],
            ..Node::default()
        };
        assert!(valid_matrix_coefficient_gain(&node("-0.5", "linear")));
        assert!(!valid_matrix_coefficient_gain(&node(
            "-10.0000000000000000000000000000000000000001",
            "linear"
        )));
        assert!(!valid_matrix_coefficient_gain(&node("1", "bogus")));

        let (errors, _) = matrix_audit(
            r#"<audioFormatExtended version="ITU-R_BS.2076-3">
            <audioChannelFormat audioChannelFormatID="AC_00021001" typeLabel="0002">
              <audioBlockFormat audioBlockFormatID="AB_00021001_00000001">
                <outputChannelFormatIDRef>AC_00010001</outputChannelFormatIDRef>
                <matrix><coefficient gainUnit="bogus">AC_00010001</coefficient></matrix>
              </audioBlockFormat>
            </audioChannelFormat>
            </audioFormatExtended>"#,
            "AP_00010002",
            "AP_00010002",
        );
        assert_ne!(errors.total, 0);
    }

    #[test]
    fn identifiers_only_fold_hex_digits_after_exact_uppercase_prefixes() {
        assert_eq!(canonical_id("AO_10af"), "AO_10AF");
        assert_eq!(canonical_id("AB_000310af_0000000a"), "AB_000310AF_0000000A");
        assert_eq!(canonical_id("ao_10af"), "ao_10af");
        assert_eq!(canonical_id(" AO_10af "), " AO_10af ");
        assert!(valid_short_id("AO_10af", "AO_"));
        assert!(!valid_short_id("ao_10af", "AO_"));
        assert!(!valid_short_id("AO_0FFF", "AO_"));
        assert!(valid_format_id("AP_000310af", "AP_"));
        assert!(!valid_format_id("AP_000110AF", "AP_"));
    }

    #[test]
    fn decimal_and_integer_lexical_rules_are_strict() {
        for value in ["0", "+0", "-0", "1.25", "+1.25", "-1.25"] {
            assert!(valid_decimal_lexical(value), "{value}");
        }
        for value in ["", ".5", "01", "00.5", "1.", "1e2", "NaN", "+-1"] {
            assert!(!valid_decimal_lexical(value), "{value}");
        }
        assert_eq!(parse_unsigned_integer("+48000"), Some(48_000));
        assert_eq!(parse_unsigned_integer("24"), Some(24));
        assert_eq!(parse_unsigned_integer("048000"), None);
        assert_eq!(parse_unsigned_integer("24.0"), None);
    }

    fn gain_node<'a>(text: &'a str, unit: &'a str) -> Node {
        Node {
            text: text.into(),
            attributes: vec![XmlAttribute {
                name: XmlName {
                    namespace: None,
                    local: "gainUnit".into(),
                },
                value: unit.into(),
            }],
            ..Node::default()
        }
    }

    #[test]
    fn decimal_boundaries_and_interaction_containment_do_not_round_to_f64() {
        assert!(gain_at_most(&gain_node("21", "dB"), 21));
        assert!(!gain_at_most(
            &gain_node("21.0000000000000000000000000000000000000001", "dB"),
            21
        ));
        assert!(linear_gain_at_most_db(
            "3.162277660168379331998893544432718533719555139325216826857504852792594438639238221344248108379300295",
            10
        ));
        assert!(!linear_gain_at_most_db(
            "3.162277660168379331998893544432718533719555139325216826857504852792594438639238221344248108379300296",
            10
        ));
        assert!(gain_limit_is_indeterminate(
            &gain_node(
                "3.1622776601683793319988935444327185337195551393252168268575048527925944386392382213442481083793002951",
                "linear"
            ),
            10
        ));
        assert!(linear_gain_at_most_db(
            "11.22018454301963435591038946477905736722308507360552962445074448170103302686224355942322410693190479",
            21
        ));
        assert!(!linear_gain_at_most_db(
            "11.22018454301963435591038946477905736722308507360552962445074448170103302686224355942322410693190480",
            21
        ));
        assert!(gain_limit_is_indeterminate(
            &gain_node(
                "11.220184543019634355910389464779057367223085073605529624450744481701033026862243559423224106931904798",
                "linear"
            ),
            21
        ));

        let beyond_x = Node {
            text: "1.0000000000000000000000000000000000000001".into(),
            attributes: vec![XmlAttribute {
                name: XmlName {
                    namespace: None,
                    local: "coordinate".into(),
                },
                value: "X".into(),
            }],
            ..Node::default()
        };
        assert!(!valid_position(&beyond_x, true));
        assert_ne!(
            compare_decimal("0.0000000000000000000000000000000000000001", "0"),
            Some(Ordering::Equal)
        );
        assert!(!decimal_is_within(
            "0.1000000000000000000000000000000000000001",
            "0",
            "0.1"
        ));
        assert!(!decimal_is_within("0.15", "0.2", "0.1"));

        let document = parse_xml(
            br#"<root><audioObjectInteraction onOffInteract="0" gainInteract="1">
            <gainInteractionRange bound="min" gainUnit="linear">0</gainInteractionRange>
            <gainInteractionRange bound="max" gainUnit="linear">1</gainInteractionRange>
            </audioObjectInteraction></root>"#,
            &options(),
        )
        .unwrap();
        let interaction = document.nodes[0].children[0];
        let bounds = interaction_gain_bounds(&document, interaction, None).unwrap();
        assert_eq!(
            gain_value_is_within(
                gain_value(&gain_node(
                    "1.0000000000000000000000000000000000000001",
                    "linear"
                ))
                .unwrap(),
                bounds
            ),
            Some(false)
        );
    }

    #[test]
    fn track_uid_bit_depth_accepts_container_or_valid_extensible_width() {
        let track_rule = |bit_depth: &str| {
            let xml = format!(
                r#"<audioFormatExtended version="ITU-R_BS.2076-3">
                <audioObject audioObjectID="AO_1001"><audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef><audioTrackUIDRef>ATU_00000001</audioTrackUIDRef></audioObject>
                <audioTrackUID UID="ATU_00000001" sampleRate="48000" bitDepth="{bit_depth}"><audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef><audioChannelFormatIDRef>AC_00010003</audioChannelFormatIDRef></audioTrackUID>
                </audioFormatExtended>"#
            );
            let document = parse_xml(xml.as_bytes(), &options()).unwrap();
            let definitions = Definitions::build(&document, document.afe.unwrap(), None, 64);
            let mut audit = Audit::new(&document, 64, 128);
            audit.audit_tracks_chna(
                &definitions,
                &[],
                EssenceInfo {
                    channels: 1,
                    sample_rate: 48_000,
                    container_bit_depth: 24,
                    valid_bit_depth: 20,
                    duration: ExactTime::new(1, 1).unwrap(),
                    aligned: true,
                    integer_pcm: true,
                    probe_data_size_matches: true,
                    ds64_sample_count_matches: true,
                },
                0,
            );
            audit.rules.remove(0)
        };
        assert!(track_rule("20").passed);
        assert!(track_rule("24").passed);
        assert!(!track_rule("16").passed);
    }

    #[test]
    fn duplicate_physical_track_is_bs2168_not_bs2088_failure() {
        fn record(index: u16, uid: &str) -> [u8; 40] {
            let mut record = [0_u8; 40];
            record[..2].copy_from_slice(&index.to_le_bytes());
            record[2..14].copy_from_slice(uid.as_bytes());
            record[14..28].copy_from_slice(b"AC_00010003_00");
            record[28..39].copy_from_slice(b"AP_00010001");
            record
        }

        let document = parse_xml(
            br#"<audioFormatExtended version="ITU-R_BS.2076-3">
            <audioTrackUID UID="ATU_00000001"><audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef><audioChannelFormatIDRef>AC_00010003</audioChannelFormatIDRef></audioTrackUID>
            <audioTrackUID UID="ATU_00000002"><audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef><audioChannelFormatIDRef>AC_00010003</audioChannelFormatIDRef></audioTrackUID>
            </audioFormatExtended>"#,
            &options(),
        )
        .unwrap();
        let definitions = Definitions::build(&document, document.afe.unwrap(), None, 64);
        let audit = Audit::new(&document, 64, 128);
        let essence = EssenceInfo {
            channels: 1,
            sample_rate: 48_000,
            container_bit_depth: 24,
            valid_bit_depth: 24,
            duration: ExactTime::new(1, 1).unwrap(),
            aligned: true,
            integer_pcm: true,
            probe_data_size_matches: true,
            ds64_sample_count_matches: true,
        };
        let mut chna = vec![1, 0, 2, 0];
        chna.extend_from_slice(&record(1, "ATU_00000001"));
        chna.extend_from_slice(&record(1, "ATU_00000002"));

        let carrier = audit.chna_carrier_errors(&chna, essence, 1);
        assert_eq!(carrier.total, 0);
        let profile = audit.chna_profile_errors(&chna, essence, &definitions.tracks);
        assert!(profile.retained.iter().any(|evidence| evidence
            .observed
            .contains("physical trackIndex 1 is invalid or assigned more than once")));
    }

    #[test]
    fn chna_carrier_accepts_at_or_ac_references_and_rejects_garbage() {
        fn body(track_ref: &str, pack_ref: &str) -> Vec<u8> {
            let mut body = vec![1, 0, 1, 0];
            let mut record = [0_u8; 40];
            record[..2].copy_from_slice(&1_u16.to_le_bytes());
            record[2..14].copy_from_slice(b"ATU_00000001");
            record[14..14 + track_ref.len()].copy_from_slice(track_ref.as_bytes());
            record[28..28 + pack_ref.len()].copy_from_slice(pack_ref.as_bytes());
            body.extend_from_slice(&record);
            body
        }

        let document = parse_xml(
            br#"<audioFormatExtended version="ITU-R_BS.2076-3">
            <audioTrackUID UID="ATU_00000001"><audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef><audioChannelFormatIDRef>AC_00010003</audioChannelFormatIDRef></audioTrackUID>
            </audioFormatExtended>"#,
            &options(),
        )
        .unwrap();
        let definitions = Definitions::build(&document, document.afe.unwrap(), None, 64);
        let audit = Audit::new(&document, 64, 128);
        let essence = EssenceInfo {
            channels: 1,
            sample_rate: 48_000,
            container_bit_depth: 24,
            valid_bit_depth: 24,
            duration: ExactTime::new(1, 1).unwrap(),
            aligned: true,
            integer_pcm: true,
            probe_data_size_matches: true,
            ds64_sample_count_matches: true,
        };

        let at = body("AT_00010001_01", "AP_00010001");
        assert_eq!(audit.chna_carrier_errors(&at, essence, 1).total, 0);

        let ac = body("AC_00010003_00", "AP_00010001");
        assert_eq!(audit.chna_carrier_errors(&ac, essence, 1).total, 0);
        assert_eq!(
            audit
                .chna_profile_errors(&ac, essence, &definitions.tracks)
                .total,
            0
        );

        assert!(
            audit
                .chna_carrier_errors(&body("AC_00010003_01", "AP_00010001"), essence, 1)
                .total
                > 0
        );
        assert!(
            audit
                .chna_carrier_errors(&body("", "AP_00010001"), essence, 1)
                .total
                > 0
        );

        let empty_pack = body("AT_00010001_01", "");
        assert_eq!(audit.chna_carrier_errors(&empty_pack, essence, 1).total, 0);
        assert!(
            audit
                .chna_profile_errors(&empty_pack, essence, &definitions.tracks)
                .total
                > 0
        );

        let garbage = body("garbage", "bad");
        assert!(audit.chna_carrier_errors(&garbage, essence, 1).total >= 2);

        let no_afe = parse_xml(b"<root/>", &options()).unwrap();
        let mut no_afe_audit = Audit::new(&no_afe, 64, 128);
        no_afe_audit.run(
            Level::Level0,
            ProfileLimits::unlimited(),
            &garbage,
            essence,
            1,
        );
        let carrier = no_afe_audit
            .rules
            .iter()
            .find(|rule| rule.rule_id == "BS2088-8-9-CHNA-CARRIER")
            .unwrap();
        assert!(!carrier.passed);
    }

    #[test]
    fn iso_639_2_registry_includes_synonyms_specials_and_local_range() {
        for language in [
            "eng", "deu", "ger", "fra", "fre", "und", "mul", "zxx", "qaa", "qtz",
        ] {
            assert!(valid_language(language), "{language}");
        }
        for language in ["ENG", "en", "qzz", "zzz"] {
            assert!(!valid_language(language), "{language}");
        }
    }

    #[test]
    fn mixed_container_text_and_non_leaf_profile_are_structural_violations() {
        let document = parse_xml(
            br#"<audioFormatExtended version="ITU-R_BS.2076-3">mixed<profileList><profile profileName="x" profileVersion="1" profileLevel="1"><gain>1</gain></profile></profileList></audioFormatExtended>"#,
            &options(),
        )
        .unwrap();
        let afe = document.afe.unwrap();
        let mut audit = Audit::new(&document, 64, 128);
        audit.audit_structure(afe);
        let evidence = &audit.rules[0].evidence;
        assert!(evidence
            .iter()
            .any(|item| item.observed.contains("mixed character data")));
        assert!(evidence
            .iter()
            .any(|item| item.observed.contains("leaf element contains child")));
    }

    #[test]
    fn violation_collection_retains_only_the_configured_bound() {
        let mut violations = Violations::new(3);
        for index in 0..10_000 {
            violations.push(Evidence {
                path: format!("/{index}"),
                observed: "invalid".into(),
            });
        }
        assert_eq!(violations.total, 10_000);
        assert_eq!(violations.retained.len(), 3);
    }

    #[test]
    fn iterative_graph_analysis_handles_long_chains_and_cycles() {
        const NODE_COUNT: usize = 4_096;
        let mut document = ParsedDocument {
            nodes: Vec::with_capacity(NODE_COUNT),
            afe: None,
            afe_count: 0,
        };
        let mut definitions = Definitions::default();
        let mut chain = BTreeMap::new();
        for index in 0..NODE_COUNT {
            let id = format!("AO_{:04X}", 0x1001 + index);
            definitions.objects.insert(id.clone(), index);
            document.nodes.push(Node {
                name: XmlName {
                    namespace: None,
                    local: "audioObject".into(),
                },
                ..Node::default()
            });
            let children = (index + 1 < NODE_COUNT)
                .then(|| vec![format!("AO_{:04X}", 0x1002 + index)])
                .unwrap_or_default();
            chain.insert(id, children);
        }
        let analysis = analyze_object_graph(&chain, &definitions, &document, None);
        assert!(analysis.cyclic.is_empty());
        assert_eq!(
            analysis.depths[&format!("AO_{:04X}", 0x1000 + NODE_COUNT)],
            NODE_COUNT
        );

        let last = format!("AO_{:04X}", 0x1000 + NODE_COUNT);
        chain.get_mut(&last).unwrap().push("AO_1001".to_owned());
        let analysis = analyze_object_graph(&chain, &definitions, &document, None);
        assert_eq!(analysis.cyclic.len(), NODE_COUNT);
    }

    #[test]
    fn iterative_graph_analysis_handles_dense_layered_invalid_graph() {
        const LAYERS: usize = 128;
        const WIDTH: usize = 8;
        let count = LAYERS * WIDTH;
        let mut document = ParsedDocument {
            nodes: Vec::with_capacity(count),
            afe: None,
            afe_count: 0,
        };
        let mut definitions = Definitions::default();
        let mut edges = BTreeMap::new();
        for index in 0..count {
            let id = format!("AO_{:04X}", 0x1001 + index);
            definitions.objects.insert(id.clone(), index);
            document.nodes.push(Node {
                name: XmlName {
                    namespace: None,
                    local: "audioObject".into(),
                },
                ..Node::default()
            });
            let layer = index / WIDTH;
            let children = if layer + 1 < LAYERS {
                ((layer + 1) * WIDTH..(layer + 2) * WIDTH)
                    .map(|child| format!("AO_{:04X}", 0x1001 + child))
                    .collect()
            } else {
                Vec::new()
            };
            edges.insert(id, children);
        }
        let analysis = analyze_object_graph(&edges, &definitions, &document, None);
        assert!(analysis.cyclic.is_empty());
        assert_eq!(analysis.depths.values().copied().max(), Some(LAYERS));
    }

    #[test]
    fn complementary_membership_work_stays_linear_for_duplicate_invalid_groups() {
        const GROUPS: usize = 20_000;
        const PROGRAMMES: usize = 20_000;
        let groups = (0..GROUPS)
            .map(|index| BTreeSet::from(["AO_SHARED".to_owned(), format!("AO_{index:08X}")]))
            .collect::<Vec<_>>();
        let index = complementary_group_membership_index(&groups);
        assert!(index["AO_SHARED"].duplicate_group);
        let included = BTreeSet::from(["AO_SHARED".to_owned()]);
        let mut visits = 0_usize;
        for _ in 0..PROGRAMMES {
            let (counts, current_visits) = count_included_complementary_groups(&included, &index);
            assert!(counts.is_empty());
            visits += current_visits;
        }
        assert_eq!(visits, PROGRAMMES);
    }

    #[test]
    fn position_pack_index_inspects_shared_channel_blocks_once() {
        const PACKS: usize = 10_000;
        const BLOCKS: usize = 16;
        let mut xml = String::from(
            r#"<audioFormatExtended version="ITU-R_BS.2076-3"><audioChannelFormat audioChannelFormatID="AC_00031001" typeLabel="0003">"#,
        );
        for index in 0..BLOCKS {
            xml.push_str(&objects_block(index + 1, "0.00000", "0.00500"));
        }
        xml.push_str("</audioChannelFormat>");
        for index in 0..PACKS {
            use std::fmt::Write as _;
            write!(
                &mut xml,
                r#"<audioPackFormat audioPackFormatID="AP_0003{:04X}" typeLabel="0003"><audioChannelFormatIDRef>AC_00031001</audioChannelFormatIDRef></audioPackFormat>"#,
                0x1001 + index
            )
            .unwrap();
        }
        xml.push_str("</audioFormatExtended>");
        let mut bounded = options();
        bounded.max_xml_nodes = HARD_MAX_XML_NODES;
        let document = parse_xml(xml.as_bytes(), &bounded).unwrap();
        let definitions = Definitions::build(&document, document.afe.unwrap(), None, 64);
        let (index, inspected_blocks) = build_position_pack_index(&document, None, &definitions);
        assert_eq!(index.len(), PACKS);
        assert_eq!(inspected_blocks, BLOCKS);
    }

    #[test]
    fn matrix_channel_detail_is_audited_once_when_invalidly_shared() {
        let coefficients = direct_speaker_channels("AP_00010009")
            .unwrap()
            .into_iter()
            .map(|channel| format!("<coefficient>{channel}</coefficient>"))
            .collect::<String>();
        let xml = format!(
            r#"<audioFormatExtended version="ITU-R_BS.2076-3">
            <audioChannelFormat audioChannelFormatID="AC_00021001" typeLabel="0002">
            <audioBlockFormat audioBlockFormatID="AB_00021001_00000001">
            <outputChannelFormatIDRef>AC_00010001</outputChannelFormatIDRef>
            <matrix>{coefficients}</matrix></audioBlockFormat></audioChannelFormat>
            </audioFormatExtended>"#
        );
        let document = parse_xml(xml.as_bytes(), &options()).unwrap();
        let definitions = Definitions::build(&document, document.afe.unwrap(), None, 64);
        let audit = Audit::new(&document, 64, 128);
        let channels = vec!["AC_00021001".to_owned()];
        let mut audited = HashSet::new();
        let mut errors = Violations::new(64);
        let mut visits = 0_usize;
        for _ in 0..10_000 {
            visits += audit.audit_matrix_pack(
                &channels,
                "AP_00010009",
                "AP_00010002",
                &definitions,
                &mut audited,
                &mut errors,
            );
        }
        assert_eq!(visits, 24);
    }

    #[test]
    fn derived_track_count_is_bounded_and_truncation_is_reported() {
        const OBJECTS: usize = 24;
        let mut xml = String::from(
            r#"<audioFormatExtended>
            <audioProgramme audioProgrammeID="APR_1001"><audioContentIDRef>ACO_1001</audioContentIDRef></audioProgramme>
            <audioContent audioContentID="ACO_1001"><audioObjectIDRef>AO_1001</audioObjectIDRef></audioContent>"#,
        );
        for offset in 0..OBJECTS {
            use std::fmt::Write as _;
            let id = 0x1001 + offset;
            write!(&mut xml, "<audioObject audioObjectID=\"AO_{id:04X}\">").unwrap();
            if offset + 1 < OBJECTS {
                let child = id + 1;
                write!(
                    &mut xml,
                    "<audioObjectIDRef>AO_{child:04X}</audioObjectIDRef><audioObjectIDRef>AO_{child:04X}</audioObjectIDRef>"
                )
                .unwrap();
            } else {
                xml.push_str("<audioPackFormatIDRef>AP_00031001</audioPackFormatIDRef><audioTrackUIDRef>ATU_00000001</audioTrackUIDRef>");
            }
            xml.push_str("</audioObject>");
        }
        xml.push_str(
            r#"<audioPackFormat audioPackFormatID="AP_00031001" typeLabel="0003"/>
            </audioFormatExtended>"#,
        );

        let (rule, counts) = graph_rule_for_xml(&xml);
        assert_eq!(counts.non_complementary_tracks, MAX_SERIALIZED_COUNT);
        assert!(rule.evidence.iter().any(|evidence| evidence
            .observed
            .contains("reported count is explicitly truncated")));
        assert_eq!(
            bounded_count_add(MAX_SERIALIZED_COUNT - 1, 2),
            (MAX_SERIALIZED_COUNT, true)
        );
    }

    #[test]
    fn subtree_signatures_are_fixed_length_iterative_and_mask_only_named_attributes() {
        const DEPTH: usize = 8_192;
        let mut nodes = Vec::with_capacity(DEPTH);
        for index in 0..DEPTH {
            nodes.push(Node {
                name: XmlName {
                    namespace: None,
                    local: "node".into(),
                },
                parent: index.checked_sub(1),
                children: (index + 1 < DEPTH)
                    .then_some(vec![index + 1])
                    .unwrap_or_default(),
                text: index.to_string(),
                ..Node::default()
            });
        }
        let deep = ParsedDocument {
            nodes,
            afe: None,
            afe_count: 0,
        };
        let digests = subtree_digest_cache(&deep, &[]);
        assert_eq!(digests.len(), DEPTH);
        assert_ne!(digests[0], [0; 32]);

        let variants = parse_xml(
            br#"<root><alternativeValueSet alternativeValueSetID="AVS_1001_0001"><gain gainUnit="dB">1</gain></alternativeValueSet><alternativeValueSet alternativeValueSetID="AVS_1001_0002"><gain gainUnit="dB">1</gain></alternativeValueSet></root>"#,
            &options(),
        )
        .unwrap();
        let exact = subtree_digest_cache(&variants, &[]);
        let masked = subtree_digest_cache(&variants, &["alternativeValueSetID"]);
        let first = variants.nodes[0].children[0];
        let second = variants.nodes[0].children[1];
        assert_ne!(exact[first], exact[second]);
        assert_eq!(masked[first], masked[second]);
    }

    #[test]
    fn path_precomputation_handles_many_siblings_deterministically() {
        const SIBLINGS: usize = 10_000;
        let mut document = ParsedDocument {
            nodes: Vec::with_capacity(SIBLINGS + 1),
            afe: None,
            afe_count: 0,
        };
        document.nodes.push(Node {
            name: XmlName {
                namespace: None,
                local: "root".into(),
            },
            children: (1..=SIBLINGS).collect(),
            ..Node::default()
        });
        for _ in 0..SIBLINGS {
            document.nodes.push(Node {
                name: XmlName {
                    namespace: None,
                    local: "child".into(),
                },
                parent: Some(0),
                ..Node::default()
            });
        }
        let ordinals = build_path_ordinals(&document);
        assert_eq!(
            bounded_node_path(&document, &ordinals, 1),
            "/root[1]/child[1]"
        );
        assert_eq!(
            bounded_node_path(&document, &ordinals, SIBLINGS),
            "/root[1]/child[10000]"
        );
        document.nodes[SIBLINGS].name.local = "名".repeat(10_000);
        assert!(bounded_node_path(&document, &ordinals, SIBLINGS).len() <= MAX_EVIDENCE_PATH_BYTES);
        assert!(
            bounded_utf8(&"値".repeat(10_000), MAX_EVIDENCE_OBSERVED_BYTES).len()
                <= MAX_EVIDENCE_OBSERVED_BYTES
        );
    }

    #[test]
    fn report_output_same_file_detection_catches_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.wav");
        let other = directory.path().join("report.json");
        std::fs::write(&input, b"input").unwrap();
        std::fs::write(&other, b"report").unwrap();
        assert!(paths_identify_same_existing_file(&input, &input).unwrap());
        assert!(!paths_identify_same_existing_file(&input, &other).unwrap());
        assert!(
            !paths_identify_same_existing_file(&input, &directory.path().join("absent.json"))
                .unwrap()
        );

        #[cfg(unix)]
        {
            let hardlink = directory.path().join("input-link.wav");
            std::fs::hard_link(&input, &hardlink).unwrap();
            assert!(paths_identify_same_existing_file(&input, &hardlink).unwrap());
        }
    }
}
