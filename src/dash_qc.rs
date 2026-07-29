//! ISO/IEC 23009-1 DASH MPD validation with bounded local CMAF checks.

use crate::{container_qc, dash_observe, dash_patch};
use base64::Engine as _;
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const DASH_QC_SCHEMA: &str = "https://penguin425.github.io/audio-normalizer/schema/dash-qc-v1";
const MAX_MPD_BYTES: u64 = 16 * 1024 * 1024;
const MAX_INDEX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ELEMENTS: usize = 200_000;
const MAX_LOCAL_SEGMENTS: usize = 4_096;
const ADAPTATION_SET_SWITCHING_SCHEME: &str = "urn:mpeg:dash:adaptation-set-switching:2016";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DashProfile {
    Iso23009,
    DashIfIop,
    DashLive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Serialize)]
pub struct DashFinding {
    pub rule_id: &'static str,
    pub severity: Severity,
    pub passed: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct DashAudit {
    pub schema: &'static str,
    pub generator: &'static str,
    pub path: String,
    pub profile: DashProfile,
    pub kind: String,
    pub passed: bool,
    pub warning_count: usize,
    pub findings: Vec<DashFinding>,
    pub properties: Value,
}

#[derive(Clone, Default)]
struct SegmentTemplate {
    initialization: Option<String>,
    media: Option<String>,
    timescale: Option<u64>,
    duration: Option<u64>,
    start_number: Option<u64>,
    presentation_time_offset: Option<u64>,
    availability_time_offset: Option<f64>,
    availability_time_complete: Option<bool>,
    timeline: Vec<TimelineEntry>,
}

#[derive(Clone, Default)]
struct SegmentBase {
    timescale: Option<u64>,
    presentation_time_offset: Option<u64>,
    index_range: Option<ByteRange>,
    index_range_exact: Option<bool>,
    availability_time_offset: Option<f64>,
    availability_time_complete: Option<bool>,
    initialization: Option<Initialization>,
}

#[derive(Clone, Default)]
struct SegmentList {
    base: SegmentBase,
    duration: Option<u64>,
    start_number: Option<u64>,
    timeline: Vec<TimelineEntry>,
    segment_urls: Vec<SegmentUrl>,
}

#[derive(Clone, Default)]
struct Initialization {
    source_url: Option<String>,
    range: Option<ByteRange>,
}

#[derive(Clone, Default)]
struct SegmentUrl {
    media: Option<String>,
    media_range: Option<ByteRange>,
    index: Option<String>,
    index_range: Option<ByteRange>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ByteRange {
    start: u64,
    end: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AddressingKind {
    Template,
    List,
    Base,
}

impl AddressingKind {
    fn label(self) -> &'static str {
        match self {
            Self::Template => "SegmentTemplate",
            Self::List => "SegmentList",
            Self::Base => "SegmentBase",
        }
    }
}

enum ResolvedAddressing {
    Template(SegmentTemplate),
    List(SegmentList),
    Base(SegmentBase),
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct TimelineEntry {
    time: Option<u64>,
    duration: u64,
    repeat: i64,
}

#[derive(Clone, Default)]
struct BaseUrl {
    value: Option<String>,
    availability_time_offset: Option<f64>,
    availability_time_complete: Option<bool>,
}

#[derive(Clone, Default)]
struct Descriptor {
    scheme_id_uri: Option<String>,
    value: Option<String>,
}

#[derive(Clone, Default)]
struct ContentProtection {
    scheme_id_uri: Option<String>,
    value: Option<String>,
    default_kid: Option<String>,
    pssh: Vec<String>,
}

#[derive(Clone, Default)]
struct MpdEvent {
    id: Option<u64>,
    presentation_time: u64,
    duration: Option<u64>,
}

#[derive(Clone, Default)]
struct EventStream {
    scheme_id_uri: Option<String>,
    value: Option<String>,
    timescale: u64,
    presentation_time_offset: u64,
    events: Vec<MpdEvent>,
}

#[derive(Clone, Copy, Default)]
struct Latency {
    target: Option<u64>,
    min: Option<u64>,
    max: Option<u64>,
    reference_id: Option<u64>,
}

#[derive(Clone, Copy, Default)]
struct PlaybackRate {
    min: Option<f64>,
    max: Option<f64>,
}

#[derive(Clone, Default)]
struct ServiceDescription {
    id: Option<u64>,
    latency: Option<Latency>,
    playback_rate: Option<PlaybackRate>,
}

#[derive(Clone, Default)]
struct ProducerReferenceTime {
    id: Option<u64>,
    kind: Option<String>,
    inband: Option<bool>,
    wall_clock_time: Option<String>,
    presentation_time: Option<u64>,
}

#[derive(Default)]
struct Representation {
    id: Option<String>,
    base_url: Option<BaseUrl>,
    bandwidth: Option<u64>,
    mime_type: Option<String>,
    codecs: Option<String>,
    audio_sampling_rate: Option<u64>,
    audio_channel_configuration: Option<(String, String)>,
    content_protections: Vec<ContentProtection>,
    producer_reference_times: Vec<ProducerReferenceTime>,
    template: Option<SegmentTemplate>,
    segment_base: Option<SegmentBase>,
    segment_list: Option<SegmentList>,
}

#[derive(Default)]
struct AdaptationSet {
    id: Option<String>,
    base_url: Option<BaseUrl>,
    segment_alignment: Option<bool>,
    subsegment_alignment: Option<bool>,
    content_type: Option<String>,
    mime_type: Option<String>,
    codecs: Option<String>,
    lang: Option<String>,
    audio_sampling_rate: Option<u64>,
    audio_channel_configuration: Option<(String, String)>,
    supplemental_properties: Vec<Descriptor>,
    content_protections: Vec<ContentProtection>,
    producer_reference_times: Vec<ProducerReferenceTime>,
    template: Option<SegmentTemplate>,
    segment_base: Option<SegmentBase>,
    segment_list: Option<SegmentList>,
    representations: Vec<Representation>,
}

#[derive(Default)]
struct Period {
    id: Option<String>,
    base_url: Option<BaseUrl>,
    start: Option<f64>,
    duration: Option<f64>,
    event_streams: Vec<EventStream>,
    template: Option<SegmentTemplate>,
    segment_base: Option<SegmentBase>,
    segment_list: Option<SegmentList>,
    adaptations: Vec<AdaptationSet>,
}

#[derive(Default)]
struct Mpd {
    root_count: usize,
    id: Option<String>,
    namespace: Option<String>,
    base_url: Option<BaseUrl>,
    profiles: Vec<String>,
    kind: String,
    availability_start_time: Option<String>,
    publish_time: Option<String>,
    media_presentation_duration: Option<f64>,
    min_buffer_time: Option<f64>,
    minimum_update_period: Option<f64>,
    time_shift_buffer_depth: Option<f64>,
    suggested_presentation_delay: Option<f64>,
    max_segment_duration: Option<f64>,
    utc_timings: Vec<Descriptor>,
    service_descriptions: Vec<ServiceDescription>,
    periods: Vec<Period>,
    element_count: usize,
}

pub fn audit(path: &Path, profile: DashProfile) -> Result<DashAudit, String> {
    let mpd = load_mpd(path)?;
    audit_mpd(path, &mpd, profile)
}

pub fn audit_with_previous(
    path: &Path,
    previous_path: &Path,
    profile: DashProfile,
) -> Result<DashAudit, String> {
    let previous = load_mpd(previous_path)?;
    let current = load_mpd(path)?;
    let audit = audit_mpd(path, &current, profile)?;
    Ok(complete_update_audit(
        audit,
        previous_path,
        &previous,
        &current,
        profile,
        Vec::new(),
    ))
}

pub fn audit_with_patch(
    base_path: &Path,
    patch_path: &Path,
    profile: DashProfile,
) -> Result<DashAudit, String> {
    let base_xml = load_bounded_xml(base_path, "MPD")?;
    let patch_xml = load_bounded_xml(patch_path, "MPD Patch")?;
    let previous = parse_mpd(&base_xml)?;
    let applied = dash_patch::apply(&base_xml, &patch_xml)?;
    if applied.xml.len() as u64 > MAX_MPD_BYTES {
        return Err(format!(
            "patched MPD exceeds the {MAX_MPD_BYTES} byte safety limit"
        ));
    }
    let current = parse_mpd(&applied.xml)?;
    let audit = audit_mpd(base_path, &current, profile)?;
    let mut patch_findings = Vec::new();
    validate_patch_envelope(&previous, &current, &applied, &mut patch_findings);
    let mut audit = complete_update_audit(
        audit,
        base_path,
        &previous,
        &current,
        profile,
        patch_findings,
    );
    if let Some(properties) = audit.properties.as_object_mut() {
        properties.insert(
            "patch_path".into(),
            Value::String(patch_path.to_string_lossy().into_owned()),
        );
        properties.insert("patch_mpd_id".into(), Value::String(applied.mpd_id));
        properties.insert(
            "patch_original_publish_time".into(),
            Value::String(applied.original_publish_time),
        );
        properties.insert(
            "patch_publish_time".into(),
            Value::String(applied.publish_time),
        );
        properties.insert(
            "patch_operation_count".into(),
            Value::from(applied.operation_count as u64),
        );
    }
    Ok(audit)
}

pub fn observation_targets(
    path: &Path,
) -> Result<Vec<dash_observe::DashObservationTarget>, String> {
    let mpd = load_mpd(path)?;
    Ok(observation_targets_for_mpd(&mpd))
}

pub fn observation_targets_with_patch(
    base_path: &Path,
    patch_path: &Path,
) -> Result<Vec<dash_observe::DashObservationTarget>, String> {
    let base_xml = load_bounded_xml(base_path, "MPD")?;
    let patch_xml = load_bounded_xml(patch_path, "MPD Patch")?;
    let applied = dash_patch::apply(&base_xml, &patch_xml)?;
    let mpd = parse_mpd(&applied.xml)?;
    Ok(observation_targets_for_mpd(&mpd))
}

pub fn attach_observation_report(
    audit: &mut DashAudit,
    report: dash_observe::DashObservationReport,
) -> Result<(), String> {
    for entry in &report.entries {
        let (rule_id, noun) = match entry.kind {
            dash_observe::DashObservationKind::UtcHttpXsdate
            | dash_observe::DashObservationKind::UtcHttpIso
            | dash_observe::DashObservationKind::UtcHttpHead => {
                ("FORGE-DASH-REMOTE-CLOCK", "clock")
            }
            dash_observe::DashObservationKind::OriginResource => {
                ("FORGE-DASH-REMOTE-ORIGIN", "origin resource")
            }
        };
        audit.findings.push(finding(
            rule_id,
            Severity::Error,
            entry.passed,
            format!("bounded {noun} observation passed: {}", entry.label),
            Some(
                serde_json::to_value(entry)
                    .map_err(|error| format!("serialize DASH observation entry: {error}"))?,
            ),
        ));
    }
    audit.findings.push(finding(
        "FORGE-DASH-REMOTE-OBSERVATION",
        Severity::Error,
        report.passed && !report.entries.is_empty(),
        "every planned DASH clock and origin observation completed within policy",
        Some(json!({
            "target_count": report.target_count,
            "request_count": report.request_count
        })),
    ));
    audit.warning_count = audit
        .findings
        .iter()
        .filter(|item| item.severity == Severity::Warning && !item.passed)
        .count();
    audit.passed = audit
        .findings
        .iter()
        .all(|item| item.severity == Severity::Warning || item.passed);
    audit
        .properties
        .as_object_mut()
        .ok_or_else(|| "DASH audit properties are not an object".to_string())?
        .insert(
            "remote_observation".into(),
            serde_json::to_value(report)
                .map_err(|error| format!("serialize DASH observation report: {error}"))?,
        );
    Ok(())
}

fn observation_targets_for_mpd(mpd: &Mpd) -> Vec<dash_observe::DashObservationTarget> {
    let mut targets = Vec::new();
    let mut unique = HashSet::new();
    for timing in &mpd.utc_timings {
        let (Some(scheme), Some(uri)) = (timing.scheme_id_uri.as_deref(), timing.value.as_deref())
        else {
            continue;
        };
        let kind = match scheme {
            "urn:mpeg:dash:utc:http-xsdate:2014" => {
                dash_observe::DashObservationKind::UtcHttpXsdate
            }
            "urn:mpeg:dash:utc:http-iso:2014" => dash_observe::DashObservationKind::UtcHttpIso,
            "urn:mpeg:dash:utc:http-head:2014" => dash_observe::DashObservationKind::UtcHttpHead,
            _ => continue,
        };
        push_observation_target(
            &mut targets,
            &mut unique,
            kind,
            uri.to_owned(),
            format!("UTCTiming {scheme}"),
        );
    }

    let period_starts = resolved_period_starts(&mpd.periods);
    for (period_index, period) in mpd.periods.iter().enumerate() {
        let period_start = period_starts
            .get(period_index)
            .copied()
            .flatten()
            .unwrap_or(0.0);
        let period_duration = period
            .duration
            .or_else(|| {
                period_starts
                    .get(period_index + 1)
                    .copied()
                    .flatten()
                    .map(|next| next - period_start)
            })
            .or_else(|| {
                mpd.media_presentation_duration
                    .map(|duration| duration - period_start)
            })
            .filter(|duration| *duration >= 0.0);
        for (adaptation_index, adaptation) in period.adaptations.iter().enumerate() {
            for (representation_index, representation) in
                adaptation.representations.iter().enumerate()
            {
                let Some(base_url) =
                    resolved_observation_base_url(mpd, period, adaptation, representation)
                else {
                    continue;
                };
                let (_, addressing) = resolve_addressing(representation, adaptation, period);
                let resource = addressing.and_then(|addressing| match addressing {
                    ResolvedAddressing::Template(template) => observation_template_resource(
                        representation,
                        &template,
                        &base_url,
                        period_duration,
                    ),
                    ResolvedAddressing::List(list) => list
                        .segment_urls
                        .last()
                        .and_then(|segment| segment.media.as_deref())
                        .and_then(|media| resolve_observation_uri(&base_url, media))
                        .or_else(|| {
                            list.base
                                .initialization
                                .as_ref()
                                .and_then(|initialization| {
                                    resolve_observation_uri(
                                        &base_url,
                                        initialization.source_url.as_deref().unwrap_or(""),
                                    )
                                })
                        }),
                    ResolvedAddressing::Base(_) => Some(base_url.clone()),
                });
                let Some(uri) = resource.filter(|uri| is_remote_http_uri(uri)) else {
                    continue;
                };
                let representation_label = representation
                    .id
                    .as_deref()
                    .map_or_else(|| representation_index.to_string(), str::to_owned);
                push_observation_target(
                    &mut targets,
                    &mut unique,
                    dash_observe::DashObservationKind::OriginResource,
                    uri,
                    format!(
                        "Period {period_index} AdaptationSet {adaptation_index} Representation {representation_label}"
                    ),
                );
            }
        }
    }
    if !targets
        .iter()
        .any(|target| target.kind == dash_observe::DashObservationKind::OriginResource)
    {
        if let Some(uri) = mpd
            .base_url
            .as_ref()
            .and_then(|base| base.value.as_ref())
            .filter(|uri| is_remote_http_uri(uri))
        {
            push_observation_target(
                &mut targets,
                &mut unique,
                dash_observe::DashObservationKind::OriginResource,
                uri.clone(),
                "MPD BaseURL".into(),
            );
        }
    }
    targets
}

fn observation_template_resource(
    representation: &Representation,
    template: &SegmentTemplate,
    base_url: &str,
    period_duration: Option<f64>,
) -> Option<String> {
    let id = representation.id.as_deref().unwrap_or("");
    if let (Some(media), Ok(segments)) = (
        template.media.as_deref(),
        expand_timeline(template, period_duration),
    ) {
        if let Some((index, time)) = segments.into_iter().enumerate().next_back() {
            let number = effective_start_number(template).saturating_add(index as u64);
            let resource = substitute_template(media, id, representation.bandwidth, number, time);
            if !resource.contains('$') {
                return resolve_observation_uri(base_url, &resource);
            }
        }
    }
    let resource = substitute_template(
        template.initialization.as_deref()?,
        id,
        representation.bandwidth,
        effective_start_number(template),
        0,
    );
    (!resource.contains('$'))
        .then(|| resolve_observation_uri(base_url, &resource))
        .flatten()
}

fn push_observation_target(
    targets: &mut Vec<dash_observe::DashObservationTarget>,
    unique: &mut HashSet<(dash_observe::DashObservationKind, String)>,
    kind: dash_observe::DashObservationKind,
    uri: String,
    label: String,
) {
    if unique.insert((kind, uri.clone())) {
        targets.push(dash_observe::DashObservationTarget { kind, uri, label });
    }
}

fn is_remote_http_uri(value: &str) -> bool {
    url::Url::parse(value)
        .ok()
        .is_some_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
}

fn resolved_observation_base_url(
    mpd: &Mpd,
    period: &Period,
    adaptation: &AdaptationSet,
    representation: &Representation,
) -> Option<String> {
    let mut result: Option<url::Url> = None;
    for layer in [
        mpd.base_url.as_ref(),
        period.base_url.as_ref(),
        adaptation.base_url.as_ref(),
        representation.base_url.as_ref(),
    ]
    .into_iter()
    .flatten()
    .filter_map(|base_url| base_url.value.as_deref())
    {
        result = Some(match result {
            Some(base) => base.join(layer).ok()?,
            None => url::Url::parse(layer).ok()?,
        });
    }
    result
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
        .map(Into::into)
}

fn resolve_observation_uri(base_url: &str, resource: &str) -> Option<String> {
    let base = url::Url::parse(base_url).ok()?;
    let resolved = base.join(resource).ok()?;
    (matches!(resolved.scheme(), "http" | "https") && resolved.host_str().is_some())
        .then(|| resolved.into())
}

fn complete_update_audit(
    mut audit: DashAudit,
    previous_path: &Path,
    previous: &Mpd,
    current: &Mpd,
    profile: DashProfile,
    initial_findings: Vec<DashFinding>,
) -> DashAudit {
    audit.findings.extend(initial_findings);
    let mut previous_findings = Vec::new();
    validate_mpd(previous_path, previous, profile, &mut previous_findings);
    for item in &mut previous_findings {
        item.message = format!("previous snapshot: {}", item.message);
    }
    let previous_passed = previous_findings
        .iter()
        .all(|item| item.severity == Severity::Warning || item.passed);
    audit.findings.extend(previous_findings);
    validate_mpd_update(previous, current, &mut audit.findings);
    audit.warning_count = audit
        .findings
        .iter()
        .filter(|item| item.severity == Severity::Warning && !item.passed)
        .count();
    audit.passed = audit
        .findings
        .iter()
        .all(|item| item.severity == Severity::Warning || item.passed);
    if let Some(properties) = audit.properties.as_object_mut() {
        properties.insert(
            "previous_path".into(),
            Value::String(previous_path.to_string_lossy().into_owned()),
        );
        properties.insert(
            "previous_publish_time".into(),
            previous
                .publish_time
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        properties.insert("previous_passed".into(), Value::Bool(previous_passed));
    }
    audit
}

fn load_mpd(path: &Path) -> Result<Mpd, String> {
    let xml = load_bounded_xml(path, "MPD")?;
    parse_mpd(&xml)
}

fn load_bounded_xml(path: &Path, label: &str) -> Result<Vec<u8>, String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if metadata.len() > MAX_MPD_BYTES {
        return Err(format!(
            "{} exceeds the {} byte {label} safety limit",
            path.display(),
            MAX_MPD_BYTES
        ));
    }
    fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))
}

fn audit_mpd(path: &Path, mpd: &Mpd, profile: DashProfile) -> Result<DashAudit, String> {
    let mut findings = Vec::new();
    validate_mpd(path, mpd, profile, &mut findings);
    let warning_count = findings
        .iter()
        .filter(|item| item.severity == Severity::Warning && !item.passed)
        .count();
    let passed = findings
        .iter()
        .all(|item| item.severity == Severity::Warning || item.passed);
    let adaptation_count = mpd
        .periods
        .iter()
        .map(|period| period.adaptations.len())
        .sum::<usize>();
    let representation_count = mpd
        .periods
        .iter()
        .flat_map(|period| &period.adaptations)
        .map(|adaptation| adaptation.representations.len())
        .sum::<usize>();
    let (segment_template_count, segment_list_count, segment_base_count) =
        addressing_element_counts(mpd);
    Ok(DashAudit {
        schema: DASH_QC_SCHEMA,
        generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")),
        path: path.to_string_lossy().into_owned(),
        profile,
        kind: mpd.kind.clone(),
        passed,
        warning_count,
        findings,
        properties: json!({
            "mpd_id": mpd.id,
            "namespace": mpd.namespace,
            "base_url": mpd.base_url.as_ref().and_then(|item| item.value.as_ref()),
            "profiles": mpd.profiles,
            "availability_start_time": mpd.availability_start_time,
            "publish_time": mpd.publish_time,
            "media_presentation_duration_seconds": mpd.media_presentation_duration,
            "minimum_buffer_time_seconds": mpd.min_buffer_time,
            "minimum_update_period_seconds": mpd.minimum_update_period,
            "time_shift_buffer_depth_seconds": mpd.time_shift_buffer_depth,
            "suggested_presentation_delay_seconds": mpd.suggested_presentation_delay,
            "maximum_segment_duration_seconds": mpd.max_segment_duration,
            "utc_timing_count": mpd.utc_timings.len(),
            "service_description_count": mpd.service_descriptions.len(),
            "producer_reference_time_count": mpd.periods.iter()
                .flat_map(|period| &period.adaptations)
                .map(|adaptation| adaptation.producer_reference_times.len()
                    + adaptation.representations.iter()
                        .map(|representation| representation.producer_reference_times.len())
                        .sum::<usize>())
                .sum::<usize>(),
            "event_stream_count": mpd.periods.iter()
                .map(|period| period.event_streams.len()).sum::<usize>(),
            "content_protection_count": mpd.periods.iter()
                .flat_map(|period| &period.adaptations)
                .map(|adaptation| adaptation.content_protections.len()
                    + adaptation.representations.iter()
                        .map(|representation| representation.content_protections.len())
                        .sum::<usize>())
                .sum::<usize>(),
            "adaptation_set_switching_descriptor_count": mpd.periods.iter()
                .flat_map(|period| &period.adaptations)
                .flat_map(|adaptation| &adaptation.supplemental_properties)
                .filter(|descriptor| descriptor.scheme_id_uri.as_deref()
                    == Some(ADAPTATION_SET_SWITCHING_SCHEME))
                .count(),
            "period_count": mpd.periods.len(),
            "adaptation_set_count": adaptation_count,
            "representation_count": representation_count,
            "segment_template_count": segment_template_count,
            "segment_list_count": segment_list_count,
            "segment_base_count": segment_base_count,
            "element_count": mpd.element_count,
        }),
    })
}

fn addressing_element_counts(mpd: &Mpd) -> (usize, usize, usize) {
    let mut templates = 0;
    let mut lists = 0;
    let mut bases = 0;
    for period in &mpd.periods {
        templates += usize::from(period.template.is_some());
        lists += usize::from(period.segment_list.is_some());
        bases += usize::from(period.segment_base.is_some());
        for adaptation in &period.adaptations {
            templates += usize::from(adaptation.template.is_some());
            lists += usize::from(adaptation.segment_list.is_some());
            bases += usize::from(adaptation.segment_base.is_some());
            for representation in &adaptation.representations {
                templates += usize::from(representation.template.is_some());
                lists += usize::from(representation.segment_list.is_some());
                bases += usize::from(representation.segment_base.is_some());
            }
        }
    }
    (templates, lists, bases)
}

fn parse_mpd(xml: &[u8]) -> Result<Mpd, String> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut mpd = Mpd::default();
    let mut stack = Vec::<String>::new();
    let mut active_period: Option<usize> = None;
    let mut active_adaptation: Option<usize> = None;
    let mut active_representation: Option<usize> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                let name = local_name(element.name().as_ref());
                stack.push(name);
                observe_element(
                    &reader,
                    &element,
                    &stack,
                    &mut mpd,
                    &mut active_period,
                    &mut active_adaptation,
                    &mut active_representation,
                )?;
            }
            Ok(Event::Empty(element)) => {
                let name = local_name(element.name().as_ref());
                stack.push(name);
                observe_element(
                    &reader,
                    &element,
                    &stack,
                    &mut mpd,
                    &mut active_period,
                    &mut active_adaptation,
                    &mut active_representation,
                )?;
                close_element(
                    stack.last().map(String::as_str).unwrap_or_default(),
                    &mut active_period,
                    &mut active_adaptation,
                    &mut active_representation,
                );
                stack.pop();
            }
            Ok(Event::End(element)) => {
                let name = local_name(element.name().as_ref());
                close_element(
                    &name,
                    &mut active_period,
                    &mut active_adaptation,
                    &mut active_representation,
                );
                stack.pop();
            }
            Ok(Event::Text(text)) => {
                let value = String::from_utf8_lossy(text.as_ref()).trim().to_owned();
                match stack.last().map(String::as_str) {
                    Some("BaseURL") if !value.is_empty() => {
                        set_base_url_value(
                            &mut mpd,
                            active_period,
                            active_adaptation,
                            active_representation,
                            value,
                        )?;
                    }
                    Some("pssh") if !value.is_empty() => {
                        let protection = current_content_protection_mut(
                            &mut mpd,
                            active_period,
                            active_adaptation,
                            active_representation,
                        )?;
                        if let Some(pssh) = protection.pssh.last_mut() {
                            pssh.push_str(&value);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Ok(Event::DocType(_)) => {
                return Err("MPD must not contain a DTD".into());
            }
            Err(error) => {
                return Err(format!(
                    "XML error at byte {}: {error}",
                    reader.error_position()
                ))
            }
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err("XML ended with unclosed elements".into());
    }
    Ok(mpd)
}

#[allow(clippy::too_many_arguments)]
fn observe_element(
    reader: &Reader<&[u8]>,
    element: &BytesStart<'_>,
    stack: &[String],
    mpd: &mut Mpd,
    active_period: &mut Option<usize>,
    active_adaptation: &mut Option<usize>,
    active_representation: &mut Option<usize>,
) -> Result<(), String> {
    mpd.element_count += 1;
    if mpd.element_count > MAX_ELEMENTS {
        return Err(format!(
            "MPD exceeds the {MAX_ELEMENTS} element safety limit"
        ));
    }
    let name = stack.last().map(String::as_str).unwrap_or_default();
    let attributes = attributes(reader, element)?;
    match name {
        "MPD" if stack.len() == 1 => {
            mpd.root_count += 1;
            mpd.id = attributes.get("id").cloned();
            mpd.namespace = attributes
                .get("xmlns")
                .cloned()
                .or_else(|| namespace_attribute(reader, element).ok().flatten());
            mpd.profiles = attributes
                .get("profiles")
                .map(|value| {
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            mpd.kind = attributes
                .get("type")
                .cloned()
                .unwrap_or_else(|| "static".into());
            mpd.availability_start_time = attributes.get("availabilityStartTime").cloned();
            mpd.publish_time = attributes.get("publishTime").cloned();
            mpd.media_presentation_duration = attributes
                .get("mediaPresentationDuration")
                .map(|value| parse_duration(value))
                .transpose()?;
            mpd.min_buffer_time = attributes
                .get("minBufferTime")
                .map(|value| parse_duration(value))
                .transpose()?;
            mpd.minimum_update_period = attributes
                .get("minimumUpdatePeriod")
                .map(|value| parse_duration(value))
                .transpose()?;
            mpd.time_shift_buffer_depth = attributes
                .get("timeShiftBufferDepth")
                .map(|value| parse_duration(value))
                .transpose()?;
            mpd.suggested_presentation_delay = attributes
                .get("suggestedPresentationDelay")
                .map(|value| parse_duration(value))
                .transpose()?;
            mpd.max_segment_duration = attributes
                .get("maxSegmentDuration")
                .map(|value| parse_duration(value))
                .transpose()?;
        }
        "Period" => {
            let period = Period {
                id: attributes.get("id").cloned(),
                start: attributes
                    .get("start")
                    .map(|value| parse_duration(value))
                    .transpose()?,
                duration: attributes
                    .get("duration")
                    .map(|value| parse_duration(value))
                    .transpose()?,
                ..Period::default()
            };
            mpd.periods.push(period);
            *active_period = Some(mpd.periods.len() - 1);
            *active_adaptation = None;
            *active_representation = None;
        }
        "AdaptationSet" => {
            let period = current_period_mut(mpd, *active_period)?;
            period.adaptations.push(AdaptationSet {
                id: attributes.get("id").cloned(),
                segment_alignment: parse_optional_bool(&attributes, "segmentAlignment")?,
                subsegment_alignment: parse_optional_bool(&attributes, "subsegmentAlignment")?,
                content_type: attributes.get("contentType").cloned(),
                mime_type: attributes.get("mimeType").cloned(),
                codecs: attributes.get("codecs").cloned(),
                lang: attributes.get("lang").cloned(),
                audio_sampling_rate: parse_optional_u64(&attributes, "audioSamplingRate")?,
                ..AdaptationSet::default()
            });
            *active_adaptation = Some(period.adaptations.len() - 1);
            *active_representation = None;
        }
        "Representation" => {
            let adaptation = current_adaptation_mut(mpd, *active_period, *active_adaptation)?;
            adaptation.representations.push(Representation {
                id: attributes.get("id").cloned(),
                bandwidth: parse_optional_u64(&attributes, "bandwidth")?,
                mime_type: attributes.get("mimeType").cloned(),
                codecs: attributes.get("codecs").cloned(),
                audio_sampling_rate: parse_optional_u64(&attributes, "audioSamplingRate")?,
                ..Representation::default()
            });
            *active_representation = Some(adaptation.representations.len() - 1);
        }
        "SegmentTemplate" => {
            let template = SegmentTemplate {
                initialization: attributes.get("initialization").cloned(),
                media: attributes.get("media").cloned(),
                timescale: parse_optional_u64(&attributes, "timescale")?,
                duration: parse_optional_u64(&attributes, "duration")?,
                start_number: parse_optional_u64(&attributes, "startNumber")?,
                presentation_time_offset: parse_optional_u64(
                    &attributes,
                    "presentationTimeOffset",
                )?,
                availability_time_offset: parse_optional_availability_offset(
                    &attributes,
                    "availabilityTimeOffset",
                )?,
                availability_time_complete: parse_optional_bool(
                    &attributes,
                    "availabilityTimeComplete",
                )?,
                timeline: Vec::new(),
            };
            if let Some(representation) = *active_representation {
                current_adaptation_mut(mpd, *active_period, *active_adaptation)?.representations
                    [representation]
                    .template = Some(template);
            } else if active_adaptation.is_some() {
                current_adaptation_mut(mpd, *active_period, *active_adaptation)?.template =
                    Some(template);
            } else {
                current_period_mut(mpd, *active_period)?.template = Some(template);
            }
        }
        "SegmentBase" => {
            let segment_base = parse_segment_base_attributes(&attributes)?;
            set_segment_base(
                mpd,
                *active_period,
                *active_adaptation,
                *active_representation,
                segment_base,
            )?;
        }
        "SegmentList" => {
            let segment_list = SegmentList {
                base: parse_segment_base_attributes(&attributes)?,
                duration: parse_optional_u64(&attributes, "duration")?,
                start_number: parse_optional_u64(&attributes, "startNumber")?,
                ..SegmentList::default()
            };
            set_segment_list(
                mpd,
                *active_period,
                *active_adaptation,
                *active_representation,
                segment_list,
            )?;
        }
        "Initialization"
            if matches!(
                stack.iter().rev().nth(1).map(String::as_str),
                Some("SegmentBase" | "SegmentList")
            ) =>
        {
            let initialization = Initialization {
                source_url: attributes.get("sourceURL").cloned(),
                range: parse_optional_byte_range(&attributes, "range")?,
            };
            let base = current_segment_base_like_mut(
                mpd,
                *active_period,
                *active_adaptation,
                *active_representation,
                stack.iter().rev().nth(1).map(String::as_str),
            )?;
            if base.initialization.replace(initialization).is_some() {
                return Err("duplicate Initialization in segment addressing element".into());
            }
        }
        "SegmentURL" if stack.iter().rev().nth(1).map(String::as_str) == Some("SegmentList") => {
            current_segment_list_mut(
                mpd,
                *active_period,
                *active_adaptation,
                *active_representation,
            )?
            .segment_urls
            .push(SegmentUrl {
                media: attributes.get("media").cloned(),
                media_range: parse_optional_byte_range(&attributes, "mediaRange")?,
                index: attributes.get("index").cloned(),
                index_range: parse_optional_byte_range(&attributes, "indexRange")?,
            });
        }
        "S" if stack.iter().rev().nth(1).map(String::as_str) == Some("SegmentTimeline") => {
            let duration = parse_optional_u64(&attributes, "d")?
                .ok_or_else(|| "SegmentTimeline S element is missing @d".to_string())?;
            let time = parse_optional_u64(&attributes, "t")?;
            let repeat = attributes
                .get("r")
                .map(|value| {
                    value
                        .parse::<i64>()
                        .map_err(|_| format!("invalid integer @r={value}"))
                })
                .transpose()?
                .unwrap_or(0);
            current_timeline_mut(
                mpd,
                *active_period,
                *active_adaptation,
                *active_representation,
                stack.iter().rev().nth(2).map(String::as_str),
            )?
            .push(TimelineEntry {
                time,
                duration,
                repeat,
            });
        }
        "AudioChannelConfiguration" => {
            let configuration = (
                attributes.get("schemeIdUri").cloned().unwrap_or_default(),
                attributes.get("value").cloned().unwrap_or_default(),
            );
            let adaptation = current_adaptation_mut(mpd, *active_period, *active_adaptation)?;
            if let Some(representation) = *active_representation {
                adaptation.representations[representation].audio_channel_configuration =
                    Some(configuration);
            } else {
                adaptation.audio_channel_configuration = Some(configuration);
            }
        }
        "BaseURL" => {
            let base_url = BaseUrl {
                availability_time_offset: parse_optional_availability_offset(
                    &attributes,
                    "availabilityTimeOffset",
                )?,
                availability_time_complete: parse_optional_bool(
                    &attributes,
                    "availabilityTimeComplete",
                )?,
                ..BaseUrl::default()
            };
            set_base_url(
                mpd,
                *active_period,
                *active_adaptation,
                *active_representation,
                base_url,
            )?;
        }
        "UTCTiming" if stack.len() == 2 => {
            mpd.utc_timings.push(Descriptor {
                scheme_id_uri: attributes.get("schemeIdUri").cloned(),
                value: attributes.get("value").cloned(),
            });
        }
        "ServiceDescription" if stack.len() == 2 => {
            mpd.service_descriptions.push(ServiceDescription {
                id: parse_optional_u64(&attributes, "id")?,
                ..ServiceDescription::default()
            });
        }
        "Latency"
            if stack.iter().rev().nth(1).map(String::as_str) == Some("ServiceDescription") =>
        {
            let service = mpd
                .service_descriptions
                .last_mut()
                .ok_or_else(|| "Latency has no enclosing ServiceDescription".to_string())?;
            service.latency = Some(Latency {
                target: parse_optional_u64(&attributes, "target")?,
                min: parse_optional_u64(&attributes, "min")?,
                max: parse_optional_u64(&attributes, "max")?,
                reference_id: parse_optional_u64(&attributes, "referenceId")?,
            });
        }
        "PlaybackRate"
            if stack.iter().rev().nth(1).map(String::as_str) == Some("ServiceDescription") =>
        {
            let service = mpd
                .service_descriptions
                .last_mut()
                .ok_or_else(|| "PlaybackRate has no enclosing ServiceDescription".to_string())?;
            service.playback_rate = Some(PlaybackRate {
                min: parse_optional_f64(&attributes, "min")?,
                max: parse_optional_f64(&attributes, "max")?,
            });
        }
        "EventStream" => {
            let period = current_period_mut(mpd, *active_period)?;
            period.event_streams.push(EventStream {
                scheme_id_uri: attributes.get("schemeIdUri").cloned(),
                value: attributes.get("value").cloned(),
                timescale: parse_optional_u64(&attributes, "timescale")?.unwrap_or(1),
                presentation_time_offset: parse_optional_u64(
                    &attributes,
                    "presentationTimeOffset",
                )?
                .unwrap_or(0),
                events: Vec::new(),
            });
        }
        "Event" if stack.iter().rev().nth(1).map(String::as_str) == Some("EventStream") => {
            let period = current_period_mut(mpd, *active_period)?;
            let stream = period
                .event_streams
                .last_mut()
                .ok_or_else(|| "Event has no enclosing EventStream".to_string())?;
            stream.events.push(MpdEvent {
                id: parse_optional_u64(&attributes, "id")?,
                presentation_time: parse_optional_u64(&attributes, "presentationTime")?
                    .unwrap_or(0),
                duration: parse_optional_u64(&attributes, "duration")?,
            });
        }
        "ProducerReferenceTime"
            if active_adaptation.is_some()
                && matches!(
                    stack.iter().rev().nth(1).map(String::as_str),
                    Some("AdaptationSet" | "Representation")
                ) =>
        {
            let reference = ProducerReferenceTime {
                id: parse_optional_u64(&attributes, "id")?,
                kind: attributes.get("type").cloned(),
                inband: parse_optional_bool(&attributes, "inband")?,
                wall_clock_time: attributes.get("wallClockTime").cloned(),
                presentation_time: parse_optional_u64(&attributes, "presentationTime")?,
            };
            if let Some(representation) = *active_representation {
                current_adaptation_mut(mpd, *active_period, *active_adaptation)?.representations
                    [representation]
                    .producer_reference_times
                    .push(reference);
            } else {
                current_adaptation_mut(mpd, *active_period, *active_adaptation)?
                    .producer_reference_times
                    .push(reference);
            }
        }
        "SupplementalProperty"
            if active_adaptation.is_some()
                && active_representation.is_none()
                && stack.iter().rev().nth(1).map(String::as_str) == Some("AdaptationSet") =>
        {
            current_adaptation_mut(mpd, *active_period, *active_adaptation)?
                .supplemental_properties
                .push(Descriptor {
                    scheme_id_uri: attributes.get("schemeIdUri").cloned(),
                    value: attributes.get("value").cloned(),
                });
        }
        "ContentProtection"
            if active_adaptation.is_some()
                && matches!(
                    stack.iter().rev().nth(1).map(String::as_str),
                    Some("AdaptationSet" | "Representation")
                ) =>
        {
            let protection = ContentProtection {
                scheme_id_uri: attributes.get("schemeIdUri").cloned(),
                value: attributes.get("value").cloned(),
                default_kid: attributes.get("default_KID").cloned(),
                pssh: Vec::new(),
            };
            if let Some(representation) = *active_representation {
                current_adaptation_mut(mpd, *active_period, *active_adaptation)?.representations
                    [representation]
                    .content_protections
                    .push(protection);
            } else {
                current_adaptation_mut(mpd, *active_period, *active_adaptation)?
                    .content_protections
                    .push(protection);
            }
        }
        "pssh"
            if active_adaptation.is_some()
                && stack.iter().rev().nth(1).map(String::as_str) == Some("ContentProtection") =>
        {
            current_content_protection_mut(
                mpd,
                *active_period,
                *active_adaptation,
                *active_representation,
            )?
            .pssh
            .push(String::new());
        }
        _ => {}
    }
    Ok(())
}

fn close_element(
    name: &str,
    period: &mut Option<usize>,
    adaptation: &mut Option<usize>,
    representation: &mut Option<usize>,
) {
    match name {
        "Representation" => *representation = None,
        "AdaptationSet" => {
            *representation = None;
            *adaptation = None;
        }
        "Period" => {
            *representation = None;
            *adaptation = None;
            *period = None;
        }
        _ => {}
    }
}

fn validate_patch_envelope(
    previous: &Mpd,
    current: &Mpd,
    patch: &dash_patch::AppliedPatch,
    findings: &mut Vec<DashFinding>,
) {
    findings.push(finding(
        "FORGE-DASH-PATCH-MPD-ID",
        Severity::Error,
        previous.id.as_deref() == Some(patch.mpd_id.as_str()),
        "Patch mpdId matches the base MPD id",
        Some(json!({
            "mpd_id": previous.id,
            "patch_mpd_id": patch.mpd_id
        })),
    ));
    let original_matches = previous
        .publish_time
        .as_deref()
        .and_then(parse_xs_datetime_seconds)
        .zip(parse_xs_datetime_seconds(&patch.original_publish_time))
        .is_some_and(|(mpd, patch)| mpd == patch);
    findings.push(finding(
        "FORGE-DASH-PATCH-ORIGINAL-PUBLISH-TIME",
        Severity::Error,
        original_matches,
        "Patch originalPublishTime matches the base MPD publishTime",
        Some(json!({
            "mpd_publish_time": previous.publish_time,
            "patch_original_publish_time": patch.original_publish_time
        })),
    ));
    let patch_time_order = parse_xs_datetime_seconds(&patch.original_publish_time)
        .zip(parse_xs_datetime_seconds(&patch.publish_time))
        .is_some_and(|(original, current)| current > original);
    findings.push(finding(
        "FORGE-DASH-PATCH-PUBLISH-TIME",
        Severity::Error,
        patch_time_order,
        "Patch publishTime is later than originalPublishTime",
        Some(json!({
            "original_publish_time": patch.original_publish_time,
            "publish_time": patch.publish_time
        })),
    ));
    let result_matches = current
        .publish_time
        .as_deref()
        .and_then(parse_xs_datetime_seconds)
        .zip(parse_xs_datetime_seconds(&patch.publish_time))
        .is_some_and(|(mpd, patch)| mpd == patch);
    findings.push(finding(
        "FORGE-DASH-PATCH-RESULT-PUBLISH-TIME",
        Severity::Error,
        result_matches,
        "patched MPD publishTime matches Patch publishTime",
        Some(json!({
            "mpd_publish_time": current.publish_time,
            "patch_publish_time": patch.publish_time
        })),
    ));
}

fn validate_mpd_update(previous: &Mpd, current: &Mpd, findings: &mut Vec<DashFinding>) {
    findings.push(finding(
        "FORGE-DASH-UPDATE-TYPE",
        Severity::Error,
        previous.kind == "dynamic" && matches!(current.kind.as_str(), "dynamic" | "static"),
        "successive MPD audit starts with a dynamic MPD and remains dynamic or finalizes as static",
        Some(json!({"previous": previous.kind, "current": current.kind})),
    ));
    findings.push(finding(
        "FORGE-DASH-UPDATE-ID",
        Severity::Error,
        previous
            .id
            .as_ref()
            .is_none_or(|id| current.id.as_ref() == Some(id)),
        "MPD id remains stable across the update",
        Some(json!({"previous": previous.id, "current": current.id})),
    ));
    findings.push(finding(
        "FORGE-DASH-UPDATE-AVAILABILITY-START",
        Severity::Error,
        previous.availability_start_time == current.availability_start_time,
        "availabilityStartTime remains stable across the update",
        Some(json!({
            "previous": previous.availability_start_time,
            "current": current.availability_start_time
        })),
    ));
    let publish_order = previous
        .publish_time
        .as_deref()
        .and_then(parse_xs_datetime_seconds)
        .zip(
            current
                .publish_time
                .as_deref()
                .and_then(parse_xs_datetime_seconds),
        )
        .is_some_and(|(previous, current)| current > previous);
    findings.push(finding(
        "FORGE-DASH-UPDATE-PUBLISH-TIME",
        Severity::Error,
        publish_order,
        "publishTime strictly increases across the update",
        Some(json!({
            "previous": previous.publish_time,
            "current": current.publish_time
        })),
    ));

    let previous_period_ids = stable_ids(
        previous.periods.iter().map(|period| period.id.as_deref()),
        "Period",
        "previous",
        findings,
    );
    let current_period_ids = stable_ids(
        current.periods.iter().map(|period| period.id.as_deref()),
        "Period",
        "current",
        findings,
    );
    findings.push(finding(
        "FORGE-DASH-UPDATE-PERIOD-ORDER",
        Severity::Error,
        common_order_is_stable(&previous_period_ids, &current_period_ids),
        "Periods retained across the update keep their relative order",
        Some(json!({
            "previous": previous_period_ids,
            "current": current_period_ids
        })),
    ));

    let previous_starts = resolved_period_starts(&previous.periods);
    let current_starts = resolved_period_starts(&current.periods);
    let previous_by_id = previous
        .periods
        .iter()
        .enumerate()
        .filter_map(|(index, period)| period.id.as_deref().map(|id| (id, (index, period))))
        .collect::<HashMap<_, _>>();
    for (current_index, current_period) in current.periods.iter().enumerate() {
        let Some(id) = current_period.id.as_deref() else {
            continue;
        };
        let Some((previous_index, previous_period)) = previous_by_id.get(id).copied() else {
            continue;
        };
        findings.push(finding(
            "FORGE-DASH-UPDATE-PERIOD-TIMING",
            Severity::Error,
            previous_starts.get(previous_index) == current_starts.get(current_index),
            format!("retained Period {id} keeps its resolved start"),
            Some(json!({
                "previous_start_seconds": previous_starts.get(previous_index).copied().flatten(),
                "current_start_seconds": current_starts.get(current_index).copied().flatten()
            })),
        ));
        validate_adaptation_update(previous_period, current_period, id, findings);
    }
}

fn stable_ids<'a>(
    ids: impl Iterator<Item = Option<&'a str>>,
    element: &str,
    snapshot: &str,
    findings: &mut Vec<DashFinding>,
) -> Vec<String> {
    let ids = ids.collect::<Vec<_>>();
    let mut unique = HashSet::new();
    let valid = ids
        .iter()
        .all(|id| id.is_some_and(|id| !id.is_empty() && unique.insert(id.to_owned())));
    findings.push(finding(
        "FORGE-DASH-UPDATE-IDENTITY",
        Severity::Error,
        valid,
        format!("{snapshot} MPD gives every {element} a unique non-empty id"),
        Some(json!({
            "snapshot": snapshot,
            "element": element,
            "ids": ids
        })),
    ));
    ids.into_iter().flatten().map(str::to_owned).collect()
}

fn common_order_is_stable(previous: &[String], current: &[String]) -> bool {
    let previous_set = previous.iter().map(String::as_str).collect::<HashSet<_>>();
    let current_set = current.iter().map(String::as_str).collect::<HashSet<_>>();
    previous
        .iter()
        .filter(|id| current_set.contains(id.as_str()))
        .eq(current
            .iter()
            .filter(|id| previous_set.contains(id.as_str())))
}

fn validate_adaptation_update(
    previous_period: &Period,
    current_period: &Period,
    period_id: &str,
    findings: &mut Vec<DashFinding>,
) {
    let previous_ids = stable_ids(
        previous_period
            .adaptations
            .iter()
            .map(|adaptation| adaptation.id.as_deref()),
        "AdaptationSet",
        "previous",
        findings,
    );
    let current_ids = stable_ids(
        current_period
            .adaptations
            .iter()
            .map(|adaptation| adaptation.id.as_deref()),
        "AdaptationSet",
        "current",
        findings,
    );
    findings.push(finding(
        "FORGE-DASH-UPDATE-ADAPTATION-ORDER",
        Severity::Error,
        common_order_is_stable(&previous_ids, &current_ids),
        format!("retained AdaptationSets in Period {period_id} keep their relative order"),
        Some(json!({"previous": previous_ids, "current": current_ids})),
    ));
    let previous_by_id = previous_period
        .adaptations
        .iter()
        .filter_map(|adaptation| adaptation.id.as_deref().map(|id| (id, adaptation)))
        .collect::<HashMap<_, _>>();
    for current_adaptation in &current_period.adaptations {
        let Some(id) = current_adaptation.id.as_deref() else {
            continue;
        };
        let Some(previous_adaptation) = previous_by_id.get(id).copied() else {
            continue;
        };
        validate_representation_update(
            previous_period,
            current_period,
            previous_adaptation,
            current_adaptation,
            period_id,
            id,
            findings,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_representation_update(
    previous_period: &Period,
    current_period: &Period,
    previous_adaptation: &AdaptationSet,
    current_adaptation: &AdaptationSet,
    period_id: &str,
    adaptation_id: &str,
    findings: &mut Vec<DashFinding>,
) {
    let previous_ids = stable_ids(
        previous_adaptation
            .representations
            .iter()
            .map(|representation| representation.id.as_deref()),
        "Representation",
        "previous",
        findings,
    );
    let current_ids = stable_ids(
        current_adaptation
            .representations
            .iter()
            .map(|representation| representation.id.as_deref()),
        "Representation",
        "current",
        findings,
    );
    findings.push(finding(
        "FORGE-DASH-UPDATE-REPRESENTATION-SET",
        Severity::Error,
        previous_ids == current_ids,
        format!(
            "retained AdaptationSet {adaptation_id} in Period {period_id} keeps its Representation id set and order"
        ),
        Some(json!({"previous": previous_ids, "current": current_ids})),
    ));
    let adaptation_compatible = previous_adaptation.content_type == current_adaptation.content_type
        && previous_adaptation.lang == current_adaptation.lang;
    findings.push(finding(
        "FORGE-DASH-UPDATE-FUNCTIONAL-EQUIVALENCE",
        Severity::Error,
        adaptation_compatible,
        format!(
            "retained AdaptationSet {adaptation_id} keeps functional audio and codec properties"
        ),
        None,
    ));
    let previous_by_id = previous_adaptation
        .representations
        .iter()
        .filter_map(|representation| representation.id.as_deref().map(|id| (id, representation)))
        .collect::<HashMap<_, _>>();
    for current_representation in &current_adaptation.representations {
        let Some(id) = current_representation.id.as_deref() else {
            continue;
        };
        let Some(previous_representation) = previous_by_id.get(id).copied() else {
            continue;
        };
        let properties_compatible = previous_representation.bandwidth
            == current_representation.bandwidth
            && previous_representation
                .mime_type
                .as_ref()
                .or(previous_adaptation.mime_type.as_ref())
                == current_representation
                    .mime_type
                    .as_ref()
                    .or(current_adaptation.mime_type.as_ref())
            && previous_representation
                .codecs
                .as_ref()
                .or(previous_adaptation.codecs.as_ref())
                == current_representation
                    .codecs
                    .as_ref()
                    .or(current_adaptation.codecs.as_ref())
            && previous_representation
                .audio_sampling_rate
                .or(previous_adaptation.audio_sampling_rate)
                == current_representation
                    .audio_sampling_rate
                    .or(current_adaptation.audio_sampling_rate)
            && previous_representation
                .audio_channel_configuration
                .as_ref()
                .or(previous_adaptation.audio_channel_configuration.as_ref())
                == current_representation
                    .audio_channel_configuration
                    .as_ref()
                    .or(current_adaptation.audio_channel_configuration.as_ref())
            && effective_content_protections(previous_adaptation, previous_representation)
                == effective_content_protections(current_adaptation, current_representation);
        findings.push(finding(
            "FORGE-DASH-UPDATE-FUNCTIONAL-EQUIVALENCE",
            Severity::Error,
            properties_compatible,
            format!("retained Representation {id} keeps functional media properties"),
            None,
        ));
        let (_, previous_addressing) = resolve_addressing(
            previous_representation,
            previous_adaptation,
            previous_period,
        );
        let (_, current_addressing) =
            resolve_addressing(current_representation, current_adaptation, current_period);
        let compatible = addressing_update_compatible(
            previous_addressing,
            current_addressing,
            previous_period,
            current_period,
        );
        findings.push(finding(
            "FORGE-DASH-UPDATE-SEGMENT-EQUIVALENCE",
            Severity::Error,
            compatible,
            format!("retained Representation {id} keeps equivalent segment references"),
            None,
        ));
    }
}

type EffectiveContentProtection<'a> = (
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
    &'a [String],
);

fn effective_content_protections<'a>(
    adaptation: &'a AdaptationSet,
    representation: &'a Representation,
) -> Vec<EffectiveContentProtection<'a>> {
    adaptation
        .content_protections
        .iter()
        .chain(&representation.content_protections)
        .map(|protection| {
            (
                protection.scheme_id_uri.as_deref(),
                protection.value.as_deref(),
                protection.default_kid.as_deref(),
                protection.pssh.as_slice(),
            )
        })
        .collect()
}

fn addressing_update_compatible(
    previous: Option<ResolvedAddressing>,
    current: Option<ResolvedAddressing>,
    previous_period: &Period,
    current_period: &Period,
) -> bool {
    match (previous, current) {
        (
            Some(ResolvedAddressing::Template(previous)),
            Some(ResolvedAddressing::Template(current)),
        ) => {
            effective_timescale(&previous) == effective_timescale(&current)
                && previous.initialization == current.initialization
                && previous.media == current.media
                && previous.duration == current.duration
                && previous.presentation_time_offset == current.presentation_time_offset
                && previous.start_number.unwrap_or(1) <= current.start_number.unwrap_or(1)
                && segment_timelines_compatible(
                    &previous,
                    previous_period.duration,
                    &current,
                    current_period.duration,
                )
        }
        (Some(ResolvedAddressing::List(previous)), Some(ResolvedAddressing::List(current))) => {
            segment_lists_compatible(
                &previous,
                previous_period.duration,
                &current,
                current_period.duration,
            )
        }
        (Some(ResolvedAddressing::Base(previous)), Some(ResolvedAddressing::Base(current))) => {
            previous.timescale.unwrap_or(1) == current.timescale.unwrap_or(1)
                && previous.presentation_time_offset == current.presentation_time_offset
        }
        _ => false,
    }
}

fn segment_timelines_compatible(
    previous: &SegmentTemplate,
    previous_duration: Option<f64>,
    current: &SegmentTemplate,
    current_duration: Option<f64>,
) -> bool {
    if previous.timeline == current.timeline {
        return true;
    }
    if previous.timeline.is_empty() && current.timeline.is_empty() {
        return true;
    }
    if previous.timeline.is_empty() || current.timeline.is_empty() {
        return false;
    }
    let Ok(previous) = expand_timeline_segments(previous, previous_duration) else {
        return false;
    };
    let Ok(current) = expand_timeline_segments(current, current_duration) else {
        return false;
    };
    if previous.len() == MAX_LOCAL_SEGMENTS || current.len() == MAX_LOCAL_SEGMENTS {
        return false;
    }
    overlapping_segments_match(&previous, &current)
}

fn segment_lists_compatible(
    previous: &SegmentList,
    previous_duration: Option<f64>,
    current: &SegmentList,
    current_duration: Option<f64>,
) -> bool {
    if previous.base.timescale.unwrap_or(1) != current.base.timescale.unwrap_or(1)
        || previous.base.presentation_time_offset != current.base.presentation_time_offset
        || previous
            .base
            .initialization
            .as_ref()
            .map(initialization_key)
            != current.base.initialization.as_ref().map(initialization_key)
        || previous.duration != current.duration
        || previous.start_number.unwrap_or(1) > current.start_number.unwrap_or(1)
    {
        return false;
    }
    let Ok(previous_times) = expand_segment_list_segments(previous, previous_duration) else {
        return false;
    };
    let Ok(current_times) = expand_segment_list_segments(current, current_duration) else {
        return false;
    };
    if previous_times.len() == MAX_LOCAL_SEGMENTS || current_times.len() == MAX_LOCAL_SEGMENTS {
        return false;
    }
    let previous = previous_times
        .into_iter()
        .zip(&previous.segment_urls)
        .map(|((time, duration), url)| (time, duration, segment_url_key(url)))
        .collect::<Vec<_>>();
    let current = current_times
        .into_iter()
        .zip(&current.segment_urls)
        .map(|((time, duration), url)| (time, duration, segment_url_key(url)))
        .collect::<Vec<_>>();
    overlapping_valued_segments_match(&previous, &current)
}

fn initialization_key(initialization: &Initialization) -> (Option<&str>, Option<ByteRange>) {
    (initialization.source_url.as_deref(), initialization.range)
}

fn segment_url_key(
    url: &SegmentUrl,
) -> (
    Option<&str>,
    Option<ByteRange>,
    Option<&str>,
    Option<ByteRange>,
) {
    (
        url.media.as_deref(),
        url.media_range,
        url.index.as_deref(),
        url.index_range,
    )
}

fn overlapping_segments_match(previous: &[(u64, u64)], current: &[(u64, u64)]) -> bool {
    overlapping_valued_segments_match(
        &previous
            .iter()
            .map(|(start, duration)| (*start, *duration, ()))
            .collect::<Vec<_>>(),
        &current
            .iter()
            .map(|(start, duration)| (*start, *duration, ()))
            .collect::<Vec<_>>(),
    )
}

fn overlapping_valued_segments_match<T: PartialEq>(
    previous: &[(u64, u64, T)],
    current: &[(u64, u64, T)],
) -> bool {
    if previous.is_empty() {
        return true;
    }
    if current.is_empty()
        || current[0].0 < previous[0].0
        || current
            .last()
            .and_then(|(start, duration, _)| start.checked_add(*duration))
            .is_none_or(|end| end <= previous[0].0)
    {
        return false;
    }
    let mut previous_index = 0;
    let mut current_index = 0;
    while previous_index < previous.len() && current_index < current.len() {
        let (previous_start, previous_duration, previous_value) = &previous[previous_index];
        let (current_start, current_duration, current_value) = &current[current_index];
        if previous_start == current_start {
            if previous_duration != current_duration || previous_value != current_value {
                return false;
            }
            previous_index += 1;
            current_index += 1;
        } else if previous_start < current_start {
            if previous_start
                .checked_add(*previous_duration)
                .is_none_or(|end| end > *current_start)
            {
                return false;
            }
            previous_index += 1;
        } else {
            if current_start
                .checked_add(*current_duration)
                .is_none_or(|end| end > *previous_start)
            {
                return false;
            }
            current_index += 1;
        }
    }
    true
}

fn validate_mpd(path: &Path, mpd: &Mpd, profile: DashProfile, findings: &mut Vec<DashFinding>) {
    findings.push(finding(
        "FORGE-DASH-MPD-ROOT",
        Severity::Error,
        mpd.root_count == 1,
        "document has exactly one MPD root",
        Some(json!(mpd.root_count)),
    ));
    let valid_namespace = mpd.namespace.as_deref() == Some("urn:mpeg:dash:schema:mpd:2011");
    findings.push(finding(
        "FORGE-DASH-NAMESPACE",
        Severity::Error,
        valid_namespace,
        "MPD declares the MPEG-DASH 2011 namespace",
        mpd.namespace.clone().map(Value::String),
    ));
    findings.push(finding(
        "FORGE-DASH-TYPE",
        Severity::Error,
        matches!(mpd.kind.as_str(), "static" | "dynamic"),
        "MPD type is static or dynamic",
        Some(json!(mpd.kind)),
    ));
    findings.push(finding(
        "FORGE-DASH-PERIOD",
        Severity::Error,
        !mpd.periods.is_empty(),
        "MPD contains at least one Period",
        Some(json!(mpd.periods.len())),
    ));
    if mpd.kind == "static" {
        let periods_explicitly_bounded = mpd
            .periods
            .last()
            .is_some_and(|period| period.duration.is_some())
            && mpd
                .periods
                .iter()
                .enumerate()
                .all(|(index, period)| index == 0 || period.start.is_some());
        let duration_known =
            mpd.media_presentation_duration.is_some() || periods_explicitly_bounded;
        findings.push(finding(
            "FORGE-DASH-STATIC-DURATION",
            Severity::Error,
            duration_known,
            "static MPD duration is explicitly bounded",
            Some(json!({
                "media_presentation_duration": mpd.media_presentation_duration,
                "period_starts": mpd.periods.iter().map(|item| item.start).collect::<Vec<_>>(),
                "period_durations": mpd.periods.iter().map(|item| item.duration).collect::<Vec<_>>()
            })),
        ));
    } else if mpd.kind == "dynamic" {
        findings.push(finding(
            "FORGE-DASH-DYNAMIC-TIMING",
            Severity::Error,
            mpd.availability_start_time
                .as_deref()
                .is_some_and(looks_like_xs_datetime),
            "dynamic MPD declares a valid availabilityStartTime",
            mpd.availability_start_time.clone().map(Value::String),
        ));
        findings.push(finding(
            "FORGE-DASH-UPDATE-PERIOD",
            Severity::Warning,
            mpd.minimum_update_period.is_some(),
            "dynamic MPD declares minimumUpdatePeriod",
            mpd.minimum_update_period.map(|value| json!(value)),
        ));
    }
    findings.push(finding(
        "FORGE-DASH-MIN-BUFFER-TIME",
        Severity::Error,
        mpd.min_buffer_time.is_some_and(|value| value > 0.0),
        "MPD minBufferTime is present and positive",
        mpd.min_buffer_time.map(|value| json!(value)),
    ));
    if matches!(profile, DashProfile::DashIfIop | DashProfile::DashLive) {
        findings.push(finding(
            "FORGE-DASHIF-PROFILE-DECLARATION",
            Severity::Warning,
            mpd.profiles
                .iter()
                .any(|value| value.contains("dashif.org/")),
            "MPD declares a registered DASH-IF interoperability profile",
            Some(json!(mpd.profiles)),
        ));
    }
    if profile == DashProfile::DashLive {
        validate_live_mpd(mpd, findings);
    }

    let mut period_ids = HashSet::new();
    for (period_index, period) in mpd.periods.iter().enumerate() {
        if let (Some(previous_period), Some(current)) = (
            period_index
                .checked_sub(1)
                .and_then(|index| mpd.periods.get(index)),
            period.start,
        ) {
            let previous = previous_period.start.unwrap_or(0.0);
            let minimum = previous + previous_period.duration.unwrap_or(0.0);
            findings.push(finding(
                "FORGE-DASH-PERIOD-TIMELINE",
                Severity::Error,
                current >= minimum,
                "explicit Period start does not overlap the previous Period",
                Some(json!({"minimum": minimum, "current": current})),
            ));
        }
        if let Some(id) = &period.id {
            findings.push(finding(
                "FORGE-DASH-UNIQUE-PERIOD-ID",
                Severity::Error,
                period_ids.insert(id.clone()),
                format!("Period id is unique: {id}"),
                Some(json!({"period": period_index, "id": id})),
            ));
        }
        findings.push(finding(
            "FORGE-DASH-ADAPTATION-SET",
            Severity::Error,
            !period.adaptations.is_empty(),
            format!("Period {period_index} contains an AdaptationSet"),
            Some(json!(period.adaptations.len())),
        ));
        validate_period(path, period_index, period, mpd, profile, findings);
    }
}

fn validate_live_mpd(mpd: &Mpd, findings: &mut Vec<DashFinding>) {
    findings.push(finding(
        "FORGE-DASH-LIVE-TYPE",
        Severity::Error,
        mpd.kind == "dynamic",
        "DASH live profile uses a dynamic MPD",
        Some(json!(mpd.kind)),
    ));
    findings.push(finding(
        "FORGE-DASH-LIVE-AVAILABILITY-ANCHOR",
        Severity::Error,
        mpd.availability_start_time
            .as_deref()
            .is_some_and(|value| looks_like_xs_datetime(value) && has_datetime_zone(value)),
        "availabilityStartTime is a timezone-qualified XML Schema date-time",
        mpd.availability_start_time.clone().map(Value::String),
    ));
    findings.push(finding(
        "FORGE-DASH-LIVE-PUBLISH-TIME",
        Severity::Warning,
        mpd.publish_time
            .as_deref()
            .is_some_and(|value| looks_like_xs_datetime(value) && has_datetime_zone(value)),
        "dynamic MPD declares a syntactically valid publishTime",
        mpd.publish_time.clone().map(Value::String),
    ));
    findings.push(finding(
        "FORGE-DASH-LIVE-UPDATE-PERIOD",
        Severity::Error,
        mpd.minimum_update_period.is_some_and(|value| value > 0.0),
        "dynamic MPD minimumUpdatePeriod is positive",
        mpd.minimum_update_period.map(|value| json!(value)),
    ));
    findings.push(finding(
        "FORGE-DASH-LIVE-BUFFER-DEPTH",
        Severity::Error,
        mpd.time_shift_buffer_depth.is_none_or(|value| value > 0.0),
        "timeShiftBufferDepth is positive when present",
        mpd.time_shift_buffer_depth.map(|value| json!(value)),
    ));
    findings.push(finding(
        "FORGE-DASHIF-AVAILABILITY-WINDOW",
        Severity::Warning,
        mpd.time_shift_buffer_depth.is_some(),
        "rolling live service declares timeShiftBufferDepth; otherwise the window starts at availabilityStartTime",
        Some(json!({
            "availability_start_time": mpd.availability_start_time,
            "time_shift_buffer_depth_seconds": mpd.time_shift_buffer_depth
        })),
    ));
    let presentation_delay_valid = mpd.suggested_presentation_delay.is_none_or(|delay| {
        delay > 0.0
            && mpd
                .time_shift_buffer_depth
                .is_none_or(|depth| delay < depth)
    });
    findings.push(finding(
        "FORGE-DASHIF-PRESENTATION-DELAY",
        Severity::Error,
        presentation_delay_valid,
        "suggestedPresentationDelay is positive and leaves a non-empty time-shift buffer",
        Some(json!({
            "suggested_presentation_delay_seconds": mpd.suggested_presentation_delay,
            "time_shift_buffer_depth_seconds": mpd.time_shift_buffer_depth
        })),
    ));
    findings.push(finding(
        "FORGE-DASHIF-PRESENTATION-DELAY",
        Severity::Warning,
        mpd.suggested_presentation_delay.is_some(),
        "live service declares suggestedPresentationDelay",
        mpd.suggested_presentation_delay.map(|value| json!(value)),
    ));

    findings.push(finding(
        "FORGE-DASH-LIVE-UTC-TIMING",
        Severity::Error,
        !mpd.utc_timings.is_empty(),
        "dynamic MPD declares at least one UTCTiming source",
        Some(json!(mpd.utc_timings.len())),
    ));
    let mut utc_pairs = HashSet::new();
    for timing in &mpd.utc_timings {
        let scheme = timing.scheme_id_uri.as_deref().unwrap_or_default();
        let value = timing.value.as_deref().unwrap_or_default();
        let allowed = matches!(
            scheme,
            "urn:mpeg:dash:utc:http-xsdate:2014"
                | "urn:mpeg:dash:utc:http-iso:2014"
                | "urn:mpeg:dash:utc:http-head:2014"
                | "urn:mpeg:dash:utc:direct:2014"
        );
        let value_valid = if scheme == "urn:mpeg:dash:utc:direct:2014" {
            looks_like_xs_datetime(value) && has_datetime_zone(value)
        } else {
            value.starts_with("https://") || value.starts_with("http://")
        };
        findings.push(finding(
            "FORGE-DASH-LIVE-UTC-TIMING",
            Severity::Error,
            allowed && value_valid && utc_pairs.insert((scheme.to_owned(), value.to_owned())),
            "UTCTiming uses a supported unique scheme and a matching value",
            Some(json!({"scheme_id_uri": scheme, "value": value})),
        ));
    }

    let starts = resolved_period_starts(&mpd.periods);
    findings.push(finding(
        "FORGE-DASH-LIVE-PERIOD-RESOLUTION",
        Severity::Error,
        starts.iter().all(Option::is_some),
        "every Period start is explicit or derivable from the previous Period duration",
        Some(json!(starts)),
    ));
    let monotonic = starts.windows(2).enumerate().all(|(index, pair)| {
        matches!(
            pair,
            [Some(previous), Some(current)]
                if mpd.periods[index]
                    .duration
                    .is_none_or(|duration| current + 1.0e-9 >= previous + duration)
        )
    });
    findings.push(finding(
        "FORGE-DASH-LIVE-PERIOD-RESOLUTION",
        Severity::Error,
        monotonic,
        "resolved Period starts are monotonic and do not overlap explicit durations",
        Some(json!(starts)),
    ));

    let producer_references = mpd
        .periods
        .iter()
        .flat_map(|period| &period.adaptations)
        .flat_map(|adaptation| {
            adaptation.producer_reference_times.iter().chain(
                adaptation
                    .representations
                    .iter()
                    .flat_map(|representation| &representation.producer_reference_times),
            )
        })
        .collect::<Vec<_>>();
    let mut producer_ids = HashSet::new();
    for reference in &producer_references {
        let unique_id = reference.id.is_some_and(|id| producer_ids.insert(id));
        let kind_valid = reference
            .kind
            .as_deref()
            .is_none_or(|kind| matches!(kind, "encoder" | "captured"));
        let clock_valid = reference
            .wall_clock_time
            .as_deref()
            .is_none_or(|value| looks_like_xs_datetime(value) && has_datetime_zone(value));
        let pair_valid =
            reference.wall_clock_time.is_some() == reference.presentation_time.is_some();
        let timing_available =
            reference.inband == Some(true) || reference.wall_clock_time.is_some();
        findings.push(finding(
            "FORGE-DASH-LIVE-PRODUCER-REFERENCE-TIME",
            Severity::Error,
            unique_id && kind_valid && clock_valid && pair_valid && timing_available,
            "ProducerReferenceTime has a unique id, valid type, and inband or paired wall-clock/media timing",
            Some(json!({
                "id": reference.id,
                "type": reference.kind,
                "inband": reference.inband,
                "wall_clock_time": reference.wall_clock_time,
                "presentation_time": reference.presentation_time
            })),
        ));
    }

    let mut service_ids = HashSet::new();
    for service in &mpd.service_descriptions {
        let unique_id = service.id.is_none_or(|id| service_ids.insert(id));
        let latency_valid = service.latency.is_none_or(|latency| {
            latency.target.is_some_and(|target| target > 0)
                && latency
                    .min
                    .is_none_or(|min| latency.target.is_some_and(|target| min <= target))
                && latency
                    .max
                    .is_none_or(|max| latency.target.is_some_and(|target| target <= max))
                && latency
                    .reference_id
                    .is_none_or(|reference| producer_ids.contains(&reference))
        });
        let playback_valid = service.playback_rate.is_none_or(|rate| {
            rate.min.is_none_or(|min| min > 0.0 && min <= 1.0)
                && rate.max.is_none_or(|max| max >= 1.0)
                && match (rate.min, rate.max) {
                    (Some(min), Some(max)) => min <= max,
                    _ => true,
                }
        });
        findings.push(finding(
            "FORGE-DASH-LIVE-SERVICE-DESCRIPTION",
            Severity::Error,
            unique_id && latency_valid && playback_valid,
            "ServiceDescription has a unique id and coherent latency/playback ranges",
            Some(json!({
                "id": service.id,
                "latency_target_ms": service.latency.and_then(|value| value.target),
                "latency_min_ms": service.latency.and_then(|value| value.min),
                "latency_max_ms": service.latency.and_then(|value| value.max),
                "latency_reference_id": service.latency.and_then(|value| value.reference_id),
                "playback_rate_min": service.playback_rate.and_then(|value| value.min),
                "playback_rate_max": service.playback_rate.and_then(|value| value.max)
            })),
        ));
    }

    let low_latency = mpd_uses_incomplete_segments(mpd);
    if low_latency {
        let latency_target = mpd
            .service_descriptions
            .iter()
            .filter_map(|service| service.latency.and_then(|latency| latency.target))
            .min();
        findings.push(finding(
            "FORGE-DASH-LL-SERVICE-DESCRIPTION",
            Severity::Error,
            latency_target.is_some_and(|target| target > 0),
            "incomplete early-available segments declare a positive latency target",
            latency_target.map(|value| json!(value)),
        ));
        findings.push(finding(
            "FORGE-DASHIF-LL-PRODUCER-REFERENCE-TIME",
            Severity::Warning,
            !producer_references.is_empty(),
            "low-latency service declares ProducerReferenceTime timing evidence",
            Some(json!(producer_references.len())),
        ));
    }
}

fn resolved_period_starts(periods: &[Period]) -> Vec<Option<f64>> {
    let mut starts: Vec<Option<f64>> = Vec::with_capacity(periods.len());
    for (index, period) in periods.iter().enumerate() {
        let start = period.start.or_else(|| {
            if index == 0 {
                Some(0.0)
            } else {
                starts[index - 1].and_then(|previous| {
                    periods[index - 1]
                        .duration
                        .map(|duration| previous + duration)
                })
            }
        });
        starts.push(start);
    }
    starts
}

fn mpd_uses_incomplete_segments(mpd: &Mpd) -> bool {
    mpd.periods.iter().any(|period| {
        period.adaptations.iter().any(|adaptation| {
            adaptation.representations.iter().any(|representation| {
                let (_, addressing) = resolve_addressing(representation, adaptation, period);
                addressing.is_some_and(|addressing| {
                    let base = match &addressing {
                        ResolvedAddressing::Template(template) => SegmentBase {
                            availability_time_offset: template.availability_time_offset,
                            availability_time_complete: template.availability_time_complete,
                            ..SegmentBase::default()
                        },
                        ResolvedAddressing::List(list) => list.base.clone(),
                        ResolvedAddressing::Base(base) => base.clone(),
                    };
                    effective_base_availability(mpd, period, adaptation, representation, &base).1
                        == Some(false)
                })
            })
        })
    })
}

fn validate_period(
    path: &Path,
    period_index: usize,
    period: &Period,
    mpd: &Mpd,
    profile: DashProfile,
    findings: &mut Vec<DashFinding>,
) {
    let mut adaptation_ids = HashSet::new();
    let mut representation_ids = HashSet::new();
    let period_start = resolved_period_starts(&mpd.periods)
        .get(period_index)
        .copied()
        .flatten()
        .unwrap_or(0.0);
    let period_duration = period
        .duration
        .or_else(|| {
            mpd.periods
                .get(period_index + 1)
                .and_then(|next| next.start)
                .map(|next| next - period_start)
        })
        .or_else(|| {
            mpd.media_presentation_duration
                .map(|duration| duration - period_start)
        })
        .filter(|duration| *duration >= 0.0);
    if profile == DashProfile::DashLive {
        validate_event_streams(period_index, period, period_duration, findings);
    }
    for (adaptation_index, adaptation) in period.adaptations.iter().enumerate() {
        if let Some(id) = &adaptation.id {
            findings.push(finding(
                "FORGE-DASH-UNIQUE-ADAPTATION-ID",
                Severity::Error,
                adaptation_ids.insert(id.clone()),
                format!("AdaptationSet id is unique within Period: {id}"),
                None,
            ));
        }
        if profile == DashProfile::DashLive {
            validate_period_continuity(mpd, period_index, adaptation_index, adaptation, findings);
            validate_content_protections(
                &format!("Period {period_index} AdaptationSet {adaptation_index}"),
                &adaptation.content_protections,
                true,
                findings,
            );
        }
        findings.push(finding(
            "FORGE-DASH-REPRESENTATION",
            Severity::Error,
            !adaptation.representations.is_empty(),
            format!(
                "Period {period_index} AdaptationSet {adaptation_index} contains a Representation"
            ),
            Some(json!(adaptation.representations.len())),
        ));
        let adaptation_audio = adaptation.content_type.as_deref() == Some("audio")
            || adaptation
                .mime_type
                .as_deref()
                .is_some_and(|value| value.starts_with("audio/"));
        if matches!(profile, DashProfile::DashIfIop | DashProfile::DashLive) && adaptation_audio {
            findings.push(finding(
                "FORGE-DASHIF-AUDIO-LANGUAGE",
                Severity::Warning,
                adaptation
                    .lang
                    .as_deref()
                    .is_some_and(|value| !value.is_empty()),
                "audio AdaptationSet declares a language",
                adaptation.lang.clone().map(Value::String),
            ));
        }
        let mut timelines = Vec::new();
        let mut addressing_kinds = Vec::new();
        for (representation_index, representation) in adaptation.representations.iter().enumerate()
        {
            let label = format!(
                "Period {period_index} AdaptationSet {adaptation_index} Representation {representation_index}"
            );
            if profile == DashProfile::DashLive {
                validate_content_protections(
                    &label,
                    &representation.content_protections,
                    false,
                    findings,
                );
                validate_protection_inheritance(&label, adaptation, representation, findings);
            }
            let has_id = representation
                .id
                .as_deref()
                .is_some_and(|value| !value.is_empty());
            let unique_id = representation
                .id
                .as_ref()
                .is_some_and(|id| representation_ids.insert(id.clone()));
            findings.push(finding(
                "FORGE-DASH-REPRESENTATION-ID",
                Severity::Error,
                has_id && unique_id,
                format!("{label} has an id unique within Period"),
                representation.id.clone().map(Value::String),
            ));
            findings.push(finding(
                "FORGE-DASH-BANDWIDTH",
                Severity::Error,
                representation.bandwidth.is_some_and(|value| value > 0),
                format!("{label} bandwidth is positive"),
                representation.bandwidth.map(|value| json!(value)),
            ));
            let mime = representation
                .mime_type
                .as_deref()
                .or(adaptation.mime_type.as_deref());
            let codecs = representation
                .codecs
                .as_deref()
                .or(adaptation.codecs.as_deref());
            findings.push(finding(
                "FORGE-DASH-CONTENT-TYPE",
                Severity::Error,
                mime.is_some() || adaptation.content_type.is_some(),
                format!("{label} has an inherited content or MIME type"),
                Some(json!({"content_type": adaptation.content_type, "mime_type": mime})),
            ));
            if adaptation_audio
                || mime.is_some_and(|value| value.starts_with("audio/"))
                || adaptation.content_type.as_deref() == Some("audio")
            {
                findings.push(finding(
                    "FORGE-DASH-AUDIO-CODEC",
                    Severity::Error,
                    codecs.is_some_and(|value| !value.is_empty()),
                    format!("{label} declares an inherited audio codec"),
                    codecs.map(|value| json!(value)),
                ));
                let sample_rate = representation
                    .audio_sampling_rate
                    .or(adaptation.audio_sampling_rate);
                findings.push(finding(
                    "FORGE-DASH-AUDIO-SAMPLE-RATE",
                    Severity::Warning,
                    sample_rate.is_some_and(|value| value > 0),
                    format!("{label} declares a positive audio sampling rate"),
                    sample_rate.map(|value| json!(value)),
                ));
                let channel_configuration = representation
                    .audio_channel_configuration
                    .as_ref()
                    .or(adaptation.audio_channel_configuration.as_ref());
                findings.push(finding(
                    "FORGE-DASH-AUDIO-CHANNEL-CONFIG",
                    Severity::Warning,
                    channel_configuration
                        .is_some_and(|(scheme, value)| !scheme.is_empty() && !value.is_empty()),
                    format!("{label} declares an inherited audio channel configuration"),
                    channel_configuration
                        .map(|(scheme, value)| json!({"scheme_id_uri": scheme, "value": value})),
                ));
            }
            let (declared_kinds, addressing) =
                resolve_addressing(representation, adaptation, period);
            let addressing_valid = declared_kinds.len() == 1 && addressing.is_some();
            findings.push(finding(
                "FORGE-DASH-SEGMENT-ADDRESSING",
                Severity::Error,
                addressing_valid,
                format!("{label} uses exactly one effective segment-addressing mode"),
                Some(json!(declared_kinds
                    .iter()
                    .map(|kind| kind.label())
                    .collect::<Vec<_>>())),
            ));
            let base_url = resolved_base_url(mpd, period, adaptation, representation);
            if let Some(kind) = declared_kinds.first().copied().filter(|_| addressing_valid) {
                addressing_kinds.push(kind);
            }
            match addressing {
                Some(ResolvedAddressing::Template(template)) => {
                    validate_template(&label, &template, period_duration, findings);
                    if profile == DashProfile::DashLive {
                        validate_live_template(
                            &label,
                            mpd,
                            period,
                            adaptation,
                            representation,
                            &template,
                            findings,
                        );
                    }
                    if let Ok(timeline) = expand_timeline(&template, period_duration) {
                        timelines.push(timeline);
                    }
                    audit_local_resources(
                        path,
                        representation,
                        &template,
                        base_url.as_deref(),
                        period_duration,
                        effective_availability(mpd, period, adaptation, representation, &template)
                            .1
                            == Some(false),
                        findings,
                    );
                }
                Some(ResolvedAddressing::List(list)) => {
                    validate_segment_list(&label, &list, period_duration, findings);
                    if profile == DashProfile::DashLive {
                        validate_live_segment_list(
                            &label,
                            mpd,
                            period,
                            adaptation,
                            representation,
                            &list,
                            findings,
                        );
                    }
                    if let Ok(timeline) = expand_segment_list(&list, period_duration) {
                        timelines.push(timeline);
                    }
                    audit_local_segment_list(
                        path,
                        &list,
                        base_url.as_deref(),
                        period_duration,
                        findings,
                    );
                }
                Some(ResolvedAddressing::Base(base)) => {
                    validate_segment_base(
                        &label,
                        &base,
                        profile,
                        (
                            effective_base_availability(
                                mpd,
                                period,
                                adaptation,
                                representation,
                                &base,
                            ),
                            availability_offset_declared(
                                mpd,
                                period,
                                adaptation,
                                representation,
                                &base,
                            ),
                        ),
                        findings,
                    );
                    audit_local_segment_base(path, &base, base_url.as_deref(), profile, findings);
                }
                None => {}
            }
        }
        if matches!(profile, DashProfile::DashIfIop | DashProfile::DashLive)
            && !addressing_kinds.is_empty()
        {
            let first = addressing_kinds[0];
            findings.push(finding(
                "FORGE-DASHIF-SEGMENT-ADDRESSING",
                Severity::Error,
                addressing_kinds.iter().all(|kind| *kind == first)
                    && addressing_kinds.len() == adaptation.representations.len(),
                "representations in an AdaptationSet use the same addressing mode",
                Some(json!(addressing_kinds
                    .iter()
                    .map(|kind| kind.label())
                    .collect::<Vec<_>>())),
            ));
        }
        if matches!(profile, DashProfile::DashIfIop | DashProfile::DashLive) && timelines.len() > 1
        {
            let first = &timelines[0];
            for timeline in &timelines[1..] {
                findings.push(finding(
                    "FORGE-DASHIF-SEGMENT-ALIGNMENT",
                    Severity::Error,
                    timeline == first,
                    "representations in an AdaptationSet have aligned SegmentTimeline boundaries",
                    Some(json!({"reference": first, "observed": timeline})),
                ));
            }
        }
    }
    if matches!(profile, DashProfile::DashIfIop | DashProfile::DashLive) {
        validate_adaptation_set_switching(period_index, period, period_duration, findings);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NormalizedSegment {
    start_numerator: i128,
    start_denominator: u64,
    duration_numerator: u64,
    duration_denominator: u64,
}

fn validate_adaptation_set_switching(
    period_index: usize,
    period: &Period,
    period_duration: Option<f64>,
    findings: &mut Vec<DashFinding>,
) {
    let adaptation_by_id = period
        .adaptations
        .iter()
        .enumerate()
        .filter_map(|(index, adaptation)| adaptation.id.as_deref().map(|id| (id, index)))
        .collect::<HashMap<_, _>>();

    for (source_index, source) in period.adaptations.iter().enumerate() {
        for descriptor in source.supplemental_properties.iter().filter(|descriptor| {
            descriptor.scheme_id_uri.as_deref() == Some(ADAPTATION_SET_SWITCHING_SCHEME)
        }) {
            let source_id = source.id.as_deref();
            let parsed_targets = descriptor.value.as_deref().map(parse_switching_targets);
            let syntax_valid = source_id.is_some()
                && parsed_targets.as_ref().is_some_and(|targets| {
                    targets.as_ref().is_ok_and(|targets| !targets.is_empty())
                });
            findings.push(finding(
                "FORGE-DASHIF-ADAPTATION-SWITCHING-DESCRIPTOR",
                Severity::Error,
                syntax_valid,
                format!(
                    "Period {period_index} AdaptationSet {source_index} switching descriptor has a source id and a non-empty, unique target-id list"
                ),
                Some(json!({
                    "source_id": source_id,
                    "value": descriptor.value,
                    "scheme_id_uri": descriptor.scheme_id_uri
                })),
            ));
            let (Some(source_id), Some(Ok(target_ids))) = (source_id, parsed_targets) else {
                continue;
            };
            for target_id in target_ids {
                let target_index = adaptation_by_id.get(target_id.as_str()).copied();
                let reference_valid = target_id != source_id
                    && target_index.is_some_and(|index| index != source_index);
                findings.push(finding(
                    "FORGE-DASHIF-ADAPTATION-SWITCHING-REFERENCE",
                    Severity::Error,
                    reference_valid,
                    format!(
                        "Period {period_index} AdaptationSet {source_id} references a distinct existing AdaptationSet {target_id}"
                    ),
                    Some(json!({"source_id": source_id, "target_id": target_id})),
                ));
                let Some(target_index) = target_index.filter(|index| *index != source_index) else {
                    continue;
                };
                let target = &period.adaptations[target_index];
                validate_switching_pair(
                    period_index,
                    period,
                    source,
                    target,
                    period_duration,
                    findings,
                );
            }
        }
    }
}

fn parse_switching_targets(value: &str) -> Result<Vec<String>, ()> {
    let mut seen = HashSet::new();
    let mut targets = Vec::new();
    for raw in value.split(',') {
        let target = raw.trim();
        if target.is_empty() || !seen.insert(target) {
            return Err(());
        }
        targets.push(target.to_owned());
    }
    if targets.is_empty() {
        Err(())
    } else {
        Ok(targets)
    }
}

fn validate_switching_pair(
    period_index: usize,
    period: &Period,
    source: &AdaptationSet,
    target: &AdaptationSet,
    period_duration: Option<f64>,
    findings: &mut Vec<DashFinding>,
) {
    let source_id = source
        .id
        .as_deref()
        .expect("validated switching source has an id");
    let target_id = target
        .id
        .as_deref()
        .expect("resolved switching target has an id");
    let media_types = (adaptation_media_type(source), adaptation_media_type(target));
    findings.push(finding(
        "FORGE-DASHIF-ADAPTATION-SWITCHING-TYPE",
        Severity::Error,
        media_types.0.is_some() && media_types.0 == media_types.1,
        format!(
            "Period {period_index} switching AdaptationSets {source_id} and {target_id} have the same media type"
        ),
        Some(json!({
            "source_id": source_id,
            "source_type": media_types.0,
            "source_language": source.lang,
            "target_id": target_id,
            "target_type": media_types.1,
            "target_language": target.lang
        })),
    ));

    for (rule_id, label, source_value, target_value) in [
        (
            "FORGE-DASHIF-SWITCHING-SEGMENT-ALIGNMENT",
            "segmentAlignment",
            source.segment_alignment,
            target.segment_alignment,
        ),
        (
            "FORGE-DASHIF-SWITCHING-SUBSEGMENT-ALIGNMENT",
            "subsegmentAlignment",
            source.subsegment_alignment,
            target.subsegment_alignment,
        ),
    ] {
        let aligned = source_value != Some(true) && target_value != Some(true)
            || source_value == Some(true) && target_value == Some(true);
        findings.push(finding(
            rule_id,
            Severity::Error,
            aligned,
            format!(
                "Period {period_index} switching AdaptationSets {source_id} and {target_id} have a consistent {label} claim"
            ),
            Some(json!({
                "source_id": source_id,
                "source": source_value,
                "target_id": target_id,
                "target": target_value
            })),
        ));
    }

    let source_signatures = switching_alignment_signatures(period, source, period_duration);
    let target_signatures = switching_alignment_signatures(period, target, period_duration);
    let evidence_complete = source_signatures.is_ok() && target_signatures.is_ok();
    findings.push(finding(
        "FORGE-DASHIF-SWITCHING-ALIGNMENT-EVIDENCE",
        Severity::Warning,
        evidence_complete,
        format!(
            "Period {period_index} switching AdaptationSets {source_id} and {target_id} expose bounded segment-boundary evidence"
        ),
        Some(json!({
            "source_id": source_id,
            "source_language": source.lang,
            "source_representation_count": source_signatures.as_ref().ok().map(|items| items.len()),
            "source_error": source_signatures.as_ref().err().map(String::as_str),
            "target_id": target_id,
            "target_language": target.lang,
            "target_representation_count": target_signatures.as_ref().ok().map(|items| items.len()),
            "target_error": target_signatures.as_ref().err().map(String::as_str)
        })),
    ));
    let (Ok(source_signatures), Ok(target_signatures)) = (source_signatures, target_signatures)
    else {
        return;
    };
    let reference = source_signatures.first();
    let aligned = reference.is_some()
        && source_signatures
            .iter()
            .chain(&target_signatures)
            .all(|signature| Some(signature) == reference);
    findings.push(finding(
        "FORGE-DASHIF-SWITCHING-BOUNDARY-ALIGNMENT",
        Severity::Error,
        aligned,
        format!(
            "Period {period_index} switching AdaptationSets {source_id} and {target_id} have identical normalized segment boundaries across all Representations"
        ),
        Some(json!({
            "source_id": source_id,
            "source_language": source.lang,
            "source_representation_count": source_signatures.len(),
            "target_id": target_id,
            "target_language": target.lang,
            "target_representation_count": target_signatures.len(),
            "segment_count": reference.map(Vec::len)
        })),
    ));
}

fn adaptation_media_type(adaptation: &AdaptationSet) -> Option<String> {
    if let Some(content_type) = &adaptation.content_type {
        return Some(content_type.clone());
    }
    if let Some(mime) = &adaptation.mime_type {
        return mime.split_once('/').map(|(kind, _)| kind.to_owned());
    }
    let mut media_types = adaptation
        .representations
        .iter()
        .filter_map(|representation| {
            representation
                .mime_type
                .as_deref()
                .and_then(|mime| mime.split_once('/').map(|(kind, _)| kind))
        });
    let first = media_types.next()?;
    media_types
        .all(|kind| kind == first)
        .then(|| first.to_owned())
}

fn switching_alignment_signatures(
    period: &Period,
    adaptation: &AdaptationSet,
    period_duration: Option<f64>,
) -> Result<Vec<Vec<NormalizedSegment>>, String> {
    adaptation
        .representations
        .iter()
        .map(|representation| {
            let (declared, addressing) = resolve_addressing(representation, adaptation, period);
            if declared.len() != 1 {
                return Err("Representation does not resolve exactly one addressing mode".into());
            }
            match addressing {
                Some(ResolvedAddressing::Template(template)) => {
                    let segments = expand_timeline_segments(&template, period_duration)?;
                    complete_normalized_segments(
                        segments,
                        effective_timescale(&template),
                        template.presentation_time_offset.unwrap_or(0),
                    )
                }
                Some(ResolvedAddressing::List(list)) => {
                    let segments = expand_segment_list_segments(&list, period_duration)?;
                    complete_normalized_segments(
                        segments,
                        list.base.timescale.unwrap_or(1),
                        list.base.presentation_time_offset.unwrap_or(0),
                    )
                }
                Some(ResolvedAddressing::Base(_)) => {
                    Err("SegmentBase has no MPD segment-boundary sequence".into())
                }
                None => Err("Representation has no resolved segment addressing".into()),
            }
        })
        .collect()
}

fn complete_normalized_segments(
    segments: Vec<(u64, u64)>,
    timescale: u64,
    presentation_time_offset: u64,
) -> Result<Vec<NormalizedSegment>, String> {
    if timescale == 0 {
        return Err("segment timescale is zero".into());
    }
    if segments.is_empty() || segments.len() == MAX_LOCAL_SEGMENTS {
        return Err("segment-boundary evidence is empty or truncated by the safety limit".into());
    }
    Ok(segments
        .into_iter()
        .map(|(start, duration)| {
            let (start_numerator, start_denominator) = reduce_signed_fraction(
                i128::from(start) - i128::from(presentation_time_offset),
                timescale,
            );
            let divisor = gcd_u64(duration, timescale);
            NormalizedSegment {
                start_numerator,
                start_denominator,
                duration_numerator: duration / divisor,
                duration_denominator: timescale / divisor,
            }
        })
        .collect())
}

fn reduce_signed_fraction(numerator: i128, denominator: u64) -> (i128, u64) {
    let magnitude = u64::try_from(numerator.unsigned_abs()).unwrap_or(u64::MAX);
    let divisor = gcd_u64(magnitude, denominator);
    (numerator / i128::from(divisor), denominator / divisor)
}

fn gcd_u64(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left.max(1)
}

fn validate_event_streams(
    period_index: usize,
    period: &Period,
    period_duration: Option<f64>,
    findings: &mut Vec<DashFinding>,
) {
    for (stream_index, stream) in period.event_streams.iter().enumerate() {
        let label = format!("Period {period_index} EventStream {stream_index}");
        findings.push(finding(
            "FORGE-DASH-EVENT-STREAM",
            Severity::Error,
            stream
                .scheme_id_uri
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
                && stream.timescale > 0,
            format!("{label} declares a scheme and positive timescale"),
            Some(json!({
                "scheme_id_uri": stream.scheme_id_uri,
                "value": stream.value,
                "timescale": stream.timescale,
                "presentation_time_offset": stream.presentation_time_offset
            })),
        ));
        let ordered = stream
            .events
            .windows(2)
            .all(|pair| pair[0].presentation_time <= pair[1].presentation_time);
        findings.push(finding(
            "FORGE-DASH-EVENT-ORDER",
            Severity::Error,
            ordered,
            format!("{label} events are ordered by presentation time"),
            Some(json!(stream
                .events
                .iter()
                .map(|event| event.presentation_time)
                .collect::<Vec<_>>())),
        ));
        let mut ids = HashSet::new();
        for (event_index, event) in stream.events.iter().enumerate() {
            let unique = event.id.as_ref().is_none_or(|id| ids.insert(id));
            let no_overflow = event
                .duration
                .is_none_or(|duration| event.presentation_time.checked_add(duration).is_some());
            findings.push(finding(
                "FORGE-DASH-EVENT-ID",
                Severity::Error,
                unique && no_overflow,
                format!("{label} Event {event_index} has a unique optional id and bounded timing"),
                Some(json!({
                    "id": event.id,
                    "presentation_time": event.presentation_time,
                    "duration": event.duration
                })),
            ));
            findings.push(finding(
                "FORGE-DASHIF-EVENT-ID",
                Severity::Warning,
                event.id.is_some(),
                format!("{label} Event {event_index} declares an id for update equivalence"),
                event.id.map(|value| json!(value)),
            ));
            if let (Some(period_duration), Some(duration)) = (period_duration, event.duration) {
                let end = (event.presentation_time as f64 + duration as f64
                    - stream.presentation_time_offset as f64)
                    / stream.timescale.max(1) as f64;
                findings.push(finding(
                    "FORGE-DASHIF-EVENT-CONTINUATION",
                    Severity::Warning,
                    end <= period_duration || event.id.is_some(),
                    format!(
                        "{label} Event {event_index} extending beyond the Period has an id for continuation"
                    ),
                    Some(json!({
                        "event_end_seconds": end,
                        "period_duration_seconds": period_duration
                    })),
                ));
            }
        }
    }
}

fn validate_period_continuity(
    mpd: &Mpd,
    period_index: usize,
    adaptation_index: usize,
    adaptation: &AdaptationSet,
    findings: &mut Vec<DashFinding>,
) {
    const CONTINUITY: &str = "urn:mpeg:dash:period-continuity:2015";
    const CONNECTIVITY: &str = "urn:mpeg:dash:period-connectivity:2015";
    let descriptors = adaptation
        .supplemental_properties
        .iter()
        .filter(|descriptor| {
            matches!(
                descriptor.scheme_id_uri.as_deref(),
                Some(CONTINUITY | CONNECTIVITY)
            )
        })
        .collect::<Vec<_>>();
    let has_continuity = descriptors
        .iter()
        .any(|descriptor| descriptor.scheme_id_uri.as_deref() == Some(CONTINUITY));
    let has_connectivity = descriptors
        .iter()
        .any(|descriptor| descriptor.scheme_id_uri.as_deref() == Some(CONNECTIVITY));
    findings.push(finding(
        "FORGE-DASH-PERIOD-CONTINUITY",
        Severity::Error,
        !(has_continuity && has_connectivity),
        "an AdaptationSet does not signal continuity and connectivity simultaneously",
        Some(json!({
            "period": period_index,
            "adaptation_set": adaptation_index,
            "continuity": has_continuity,
            "connectivity": has_connectivity
        })),
    ));
    let starts = resolved_period_starts(&mpd.periods);
    for descriptor in descriptors {
        let referenced_id = descriptor.value.as_deref().unwrap_or_default();
        let referenced = mpd
            .periods
            .iter()
            .enumerate()
            .take(period_index)
            .find(|(_, period)| period.id.as_deref() == Some(referenced_id));
        let target_adaptation = referenced.and_then(|(_, period)| {
            adaptation.id.as_ref().and_then(|id| {
                period
                    .adaptations
                    .iter()
                    .find(|candidate| candidate.id.as_ref() == Some(id))
            })
        });
        let reference_valid = !referenced_id.is_empty() && target_adaptation.is_some();
        findings.push(finding(
            "FORGE-DASH-PERIOD-CONTINUITY",
            Severity::Error,
            reference_valid,
            "Period continuity/connectivity references an earlier Period with the same AdaptationSet id",
            Some(json!({
                "period": period_index,
                "adaptation_set_id": adaptation.id,
                "scheme_id_uri": descriptor.scheme_id_uri,
                "referenced_period_id": referenced_id
            })),
        ));
        if descriptor.scheme_id_uri.as_deref() == Some(CONTINUITY) {
            let exact_boundary = referenced.is_some_and(|(reference_index, reference)| {
                let reference_duration = reference.duration.or_else(|| {
                    starts
                        .get(reference_index + 1)
                        .copied()
                        .flatten()
                        .zip(starts.get(reference_index).copied().flatten())
                        .map(|(next, start)| next - start)
                });
                match (
                    starts.get(reference_index).copied().flatten(),
                    reference_duration,
                    starts.get(period_index).copied().flatten(),
                ) {
                    (Some(start), Some(duration), Some(current)) => {
                        (start + duration - current).abs() <= 1.0e-9
                    }
                    _ => false,
                }
            });
            findings.push(finding(
                "FORGE-DASH-PERIOD-CONTINUITY",
                Severity::Error,
                exact_boundary,
                "period-continuity reference meets an exact Period boundary",
                Some(json!({
                    "period": period_index,
                    "referenced_period_id": referenced_id
                })),
            ));
        }
        if let Some(target) = target_adaptation {
            let compatible = optional_equal(&adaptation.content_type, &target.content_type)
                && optional_equal(&adaptation.mime_type, &target.mime_type)
                && optional_equal(&adaptation.codecs, &target.codecs);
            findings.push(finding(
                "FORGE-DASH-PERIOD-CONTINUITY",
                Severity::Error,
                compatible,
                "period-continuous/connective AdaptationSets retain compatible media properties",
                Some(json!({
                    "content_type": adaptation.content_type,
                    "mime_type": adaptation.mime_type,
                    "codecs": adaptation.codecs
                })),
            ));
            let current_mode =
                effective_adaptation_addressing(&mpd.periods[period_index], adaptation);
            let target_mode = referenced
                .map(|(_, target_period)| effective_adaptation_addressing(target_period, target))
                .unwrap_or(None);
            findings.push(finding(
                "FORGE-DASH-PERIOD-ADDRESSING",
                Severity::Error,
                current_mode.is_some() && current_mode == target_mode,
                "period-continuous/connective AdaptationSets retain the same addressing mode",
                Some(json!({
                    "current": current_mode.map(AddressingKind::label),
                    "referenced": target_mode.map(AddressingKind::label)
                })),
            ));
        }
    }
}

fn effective_adaptation_addressing(
    period: &Period,
    adaptation: &AdaptationSet,
) -> Option<AddressingKind> {
    let modes = adaptation
        .representations
        .iter()
        .map(|representation| {
            let (modes, addressing) = resolve_addressing(representation, adaptation, period);
            (modes.len() == 1 && addressing.is_some()).then_some(modes[0])
        })
        .collect::<Option<Vec<_>>>()?;
    let first = modes.first().copied()?;
    modes.iter().all(|mode| *mode == first).then_some(first)
}

fn optional_equal<T: PartialEq>(left: &Option<T>, right: &Option<T>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left == right,
        _ => true,
    }
}

fn validate_content_protections(
    label: &str,
    protections: &[ContentProtection],
    adaptation_level: bool,
    findings: &mut Vec<DashFinding>,
) {
    const MP4_PROTECTION: &str = "urn:mpeg:dash:mp4protection:2011";
    let mut schemes = HashSet::new();
    let mp4_count = protections
        .iter()
        .filter(|item| item.scheme_id_uri.as_deref() == Some(MP4_PROTECTION))
        .count();
    let has_drm = protections
        .iter()
        .any(|item| item.scheme_id_uri.as_deref().is_some_and(is_uuid_scheme));
    findings.push(finding(
        "FORGE-DASH-CONTENT-PROTECTION-SET",
        Severity::Error,
        mp4_count <= 1 && (!has_drm || !adaptation_level || mp4_count == 1),
        format!(
            "{label} has at most one mp4protection descriptor and pairs DRM descriptors with it"
        ),
        Some(json!({
            "descriptor_count": protections.len(),
            "mp4protection_count": mp4_count,
            "has_drm_descriptor": has_drm
        })),
    ));
    for protection in protections {
        let scheme = protection.scheme_id_uri.as_deref().unwrap_or_default();
        let scheme_valid = !scheme.trim().is_empty()
            && scheme.contains(':')
            && (!scheme.starts_with("urn:uuid:") || is_uuid_scheme(scheme));
        findings.push(finding(
            "FORGE-DASH-CONTENT-PROTECTION-SCHEME",
            Severity::Error,
            scheme_valid && schemes.insert(scheme.to_ascii_lowercase()),
            format!("{label} ContentProtection uses a unique valid scheme URI"),
            Some(json!(scheme)),
        ));
        if scheme == MP4_PROTECTION {
            let value_valid = matches!(
                protection.value.as_deref(),
                Some("cenc" | "cens" | "cbc1" | "cbcs")
            );
            let kid_valid = protection
                .default_kid
                .as_deref()
                .is_none_or(valid_default_kid_list);
            findings.push(finding(
                "FORGE-DASH-CENC-DEFAULT-KID",
                Severity::Error,
                value_valid && kid_valid,
                format!("{label} mp4protection declares a CENC scheme and canonical default KID"),
                Some(json!({
                    "value": protection.value,
                    "default_kid": protection.default_kid
                })),
            ));
            findings.push(finding(
                "FORGE-DASHIF-CENC-DEFAULT-KID",
                Severity::Warning,
                protection.default_kid.is_some(),
                format!("{label} declares cenc:default_KID unless sample groups rotate every key"),
                protection.default_kid.clone().map(Value::String),
            ));
        } else {
            findings.push(finding(
                "FORGE-DASH-CENC-DEFAULT-KID",
                Severity::Error,
                protection.default_kid.is_none(),
                format!("{label} places cenc:default_KID only on mp4protection"),
                protection.default_kid.clone().map(Value::String),
            ));
        }
        for pssh in &protection.pssh {
            let compact = pssh
                .bytes()
                .filter(|byte| !byte.is_ascii_whitespace())
                .collect::<Vec<_>>();
            let decoded = base64::engine::general_purpose::STANDARD.decode(&compact);
            let valid = decoded.as_ref().is_ok_and(|bytes| {
                bytes.len() >= 32
                    && bytes.get(4..8) == Some(b"pssh")
                    && bytes
                        .get(0..4)
                        .and_then(|value| value.try_into().ok())
                        .map(u32::from_be_bytes)
                        .is_some_and(|size| size as usize == bytes.len())
            });
            findings.push(finding(
                "FORGE-DASH-CENC-PSSH",
                Severity::Error,
                valid,
                format!("{label} cenc:pssh is a Base64-encoded complete pssh box"),
                Some(json!({"encoded_bytes": pssh.len()})),
            ));
        }
    }
}

fn validate_protection_inheritance(
    label: &str,
    adaptation: &AdaptationSet,
    representation: &Representation,
    findings: &mut Vec<DashFinding>,
) {
    const MP4_PROTECTION: &str = "urn:mpeg:dash:mp4protection:2011";
    let has_drm = representation
        .content_protections
        .iter()
        .any(|item| item.scheme_id_uri.as_deref().is_some_and(is_uuid_scheme));
    let has_mp4 = representation
        .content_protections
        .iter()
        .chain(&adaptation.content_protections)
        .any(|item| item.scheme_id_uri.as_deref() == Some(MP4_PROTECTION));
    findings.push(finding(
        "FORGE-DASH-CONTENT-PROTECTION-SET",
        Severity::Error,
        !has_drm || has_mp4,
        format!("{label} inherits or declares mp4protection for each DRM descriptor"),
        Some(json!({"representation_drm": has_drm, "effective_mp4protection": has_mp4})),
    ));
}

fn is_uuid_scheme(value: &str) -> bool {
    value.strip_prefix("urn:uuid:").is_some_and(valid_uuid)
}

fn valid_default_kid_list(value: &str) -> bool {
    let values = value.split_ascii_whitespace().collect::<Vec<_>>();
    !values.is_empty() && values.iter().all(|value| valid_uuid(value))
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.char_indices().all(|(index, character)| match index {
            8 | 13 | 18 | 23 => character == '-',
            _ => character.is_ascii_hexdigit(),
        })
}

fn validate_live_template(
    label: &str,
    mpd: &Mpd,
    period: &Period,
    adaptation: &AdaptationSet,
    representation: &Representation,
    template: &SegmentTemplate,
    findings: &mut Vec<DashFinding>,
) {
    let (offset, complete) =
        effective_availability(mpd, period, adaptation, representation, template);
    let offset_valid = offset >= 0.0 && !offset.is_nan();
    findings.push(finding(
        "FORGE-DASH-LIVE-AVAILABILITY-OFFSET",
        Severity::Error,
        offset_valid,
        format!("{label} effective availabilityTimeOffset is non-negative"),
        Some(json!({
            "effective_offset_seconds": finite_json_number(offset),
            "infinite": offset.is_infinite(),
            "availability_time_complete": complete
        })),
    ));
    if complete == Some(false) {
        let segment_duration = maximum_segment_duration_seconds(template);
        let latency_target = mpd
            .service_descriptions
            .iter()
            .filter_map(|service| service.latency.and_then(|latency| latency.target))
            .min()
            .map(|milliseconds| milliseconds as f64 / 1_000.0);
        let geometry_valid = offset.is_finite()
            && offset > 0.0
            && segment_duration.is_some_and(|duration| offset < duration);
        findings.push(finding(
            "FORGE-DASH-LL-AVAILABILITY",
            Severity::Error,
            geometry_valid,
            format!("{label} incomplete segment has finite positive ATO below segment duration"),
            Some(json!({
                "effective_offset_seconds": finite_json_number(offset),
                "maximum_segment_duration_seconds": segment_duration
            })),
        ));
        let media_has_sequence_token = template
            .media
            .as_deref()
            .is_some_and(|media| media.contains("$Number") ^ media.contains("$Time"));
        findings.push(finding(
            "FORGE-DASH-LL-MEDIA-TEMPLATE",
            Severity::Error,
            media_has_sequence_token,
            format!("{label} low-latency media template uses exactly one of $Number$ or $Time$"),
            template.media.clone().map(Value::String),
        ));
        let latency_valid = match (segment_duration, latency_target) {
            (Some(duration), Some(target)) => duration < target && duration - offset < target,
            _ => false,
        };
        findings.push(finding(
            "FORGE-DASH-LL-LATENCY-GEOMETRY",
            Severity::Error,
            latency_valid,
            format!("{label} segment duration/ATO are coherent with the latency target"),
            Some(json!({
                "maximum_segment_duration_seconds": segment_duration,
                "effective_offset_seconds": finite_json_number(offset),
                "latency_target_seconds": latency_target
            })),
        ));
        findings.push(finding(
            "FORGE-DASHIF-LL-SEGMENT-DURATION",
            Severity::Warning,
            match (segment_duration, latency_target) {
                (Some(duration), Some(target)) => duration <= target * 0.5 + 1.0e-9,
                _ => false,
            },
            format!("{label} segment duration is at most half the latency target"),
            Some(json!({
                "maximum_segment_duration_seconds": segment_duration,
                "latency_target_seconds": latency_target
            })),
        ));
        findings.push(finding(
            "FORGE-DASHIF-LL-SEGMENT-DURATION",
            Severity::Warning,
            segment_duration.is_some_and(|duration| duration >= 0.96),
            format!("{label} nominal low-latency segment duration is at least 960 ms"),
            segment_duration.map(|value| json!(value)),
        ));
    }
}

fn maximum_segment_duration_seconds(template: &SegmentTemplate) -> Option<f64> {
    let units = template
        .duration
        .or_else(|| template.timeline.iter().map(|entry| entry.duration).max())?;
    let timescale = effective_timescale(template);
    (timescale > 0).then_some(units as f64 / timescale as f64)
}

fn finite_json_number(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

fn validate_template(
    label: &str,
    template: &SegmentTemplate,
    period_duration: Option<f64>,
    findings: &mut Vec<DashFinding>,
) {
    findings.push(finding(
        "FORGE-DASH-TIMESCALE",
        Severity::Error,
        effective_timescale(template) > 0,
        format!("{label} SegmentTemplate timescale is positive"),
        Some(json!(effective_timescale(template))),
    ));
    findings.push(finding(
        "FORGE-DASH-MEDIA-TEMPLATE",
        Severity::Error,
        template
            .media
            .as_deref()
            .is_some_and(|value| !value.is_empty()),
        format!("{label} SegmentTemplate has a media template"),
        template.media.clone().map(Value::String),
    ));
    let has_addressing =
        template.duration.is_some_and(|duration| duration > 0) || !template.timeline.is_empty();
    findings.push(finding(
        "FORGE-DASH-SEGMENT-DURATION",
        Severity::Error,
        has_addressing,
        format!("{label} provides duration or SegmentTimeline addressing"),
        Some(json!({
            "duration": template.duration,
            "timeline_entries": template.timeline.len()
        })),
    ));
    for entry in &template.timeline {
        findings.push(finding(
            "FORGE-DASH-TIMELINE-ENTRY",
            Severity::Error,
            entry.duration > 0 && entry.repeat >= -1,
            format!("{label} SegmentTimeline entry has valid d/r values"),
            Some(json!({"t": entry.time, "d": entry.duration, "r": entry.repeat})),
        ));
    }
    if !template.timeline.is_empty() {
        let expanded = expand_timeline(template, period_duration);
        let monotonic = expanded
            .as_ref()
            .is_ok_and(|items| items.windows(2).all(|pair| pair[1] > pair[0]));
        findings.push(finding(
            "FORGE-DASH-TIMELINE-EXPANSION",
            Severity::Error,
            monotonic,
            format!("{label} SegmentTimeline expands to strictly increasing start times"),
            match expanded {
                Ok(items) => Some(json!({"segment_count": items.len()})),
                Err(error) => Some(json!({"error": error})),
            },
        ));
    }
}

fn validate_segment_list(
    label: &str,
    list: &SegmentList,
    period_duration: Option<f64>,
    findings: &mut Vec<DashFinding>,
) {
    let timescale = list.base.timescale.unwrap_or(1);
    findings.push(finding(
        "FORGE-DASH-TIMESCALE",
        Severity::Error,
        timescale > 0,
        format!("{label} SegmentList timescale is positive"),
        Some(json!(timescale)),
    ));
    findings.push(finding(
        "FORGE-DASH-SEGMENT-LIST",
        Severity::Error,
        !list.segment_urls.is_empty(),
        format!("{label} SegmentList contains SegmentURL entries"),
        Some(json!({"segment_url_count": list.segment_urls.len()})),
    ));
    let has_addressing =
        list.duration.is_some_and(|duration| duration > 0) || !list.timeline.is_empty();
    findings.push(finding(
        "FORGE-DASH-SEGMENT-DURATION",
        Severity::Error,
        has_addressing,
        format!("{label} SegmentList provides duration or SegmentTimeline addressing"),
        Some(json!({
            "duration": list.duration,
            "timeline_entries": list.timeline.len()
        })),
    ));
    for entry in &list.timeline {
        findings.push(finding(
            "FORGE-DASH-TIMELINE-ENTRY",
            Severity::Error,
            entry.duration > 0 && entry.repeat >= -1,
            format!("{label} SegmentList timeline entry has valid d/r values"),
            Some(json!({"t": entry.time, "d": entry.duration, "r": entry.repeat})),
        ));
    }
    let expanded = expand_segment_list(list, period_duration);
    let count_matches = expanded
        .as_ref()
        .is_ok_and(|items| items.len() == list.segment_urls.len());
    findings.push(finding(
        "FORGE-DASH-SEGMENT-LIST-COUNT",
        Severity::Error,
        count_matches,
        format!("{label} SegmentList timing count matches SegmentURL count"),
        Some(match expanded {
            Ok(items) => json!({
                "timeline_segment_count": items.len(),
                "segment_url_count": list.segment_urls.len()
            }),
            Err(error) => json!({"error": error}),
        }),
    ));
    for (index, segment) in list.segment_urls.iter().enumerate() {
        findings.push(finding(
            "FORGE-DASH-SEGMENT-URL",
            Severity::Error,
            segment
                .media
                .as_deref()
                .is_some_and(|value| !value.is_empty()),
            format!("{label} SegmentURL {index} declares media"),
            segment.media.clone().map(Value::String),
        ));
    }
}

fn expand_segment_list(
    list: &SegmentList,
    period_duration: Option<f64>,
) -> Result<Vec<u64>, String> {
    expand_segment_list_segments(list, period_duration).map(|segments| {
        segments
            .into_iter()
            .map(|(start, _duration)| start)
            .collect()
    })
}

fn expand_segment_list_segments(
    list: &SegmentList,
    period_duration: Option<f64>,
) -> Result<Vec<(u64, u64)>, String> {
    if list.timeline.is_empty() {
        let duration = list
            .duration
            .filter(|duration| *duration > 0)
            .ok_or_else(|| "SegmentList has neither positive duration nor timeline".to_string())?;
        let count = period_duration
            .map(|period_duration| {
                ((period_duration * list.base.timescale.unwrap_or(1) as f64) / duration as f64)
                    .ceil() as usize
            })
            .unwrap_or(list.segment_urls.len());
        return Ok((0..count.min(MAX_LOCAL_SEGMENTS))
            .map(|index| (index as u64 * duration, duration))
            .collect());
    }
    expand_timeline_segments(
        &SegmentTemplate {
            timescale: list.base.timescale,
            duration: list.duration,
            start_number: list.start_number,
            presentation_time_offset: list.base.presentation_time_offset,
            timeline: list.timeline.clone(),
            ..SegmentTemplate::default()
        },
        period_duration,
    )
}

fn validate_live_segment_list(
    label: &str,
    mpd: &Mpd,
    period: &Period,
    adaptation: &AdaptationSet,
    representation: &Representation,
    list: &SegmentList,
    findings: &mut Vec<DashFinding>,
) {
    let (offset, complete) =
        effective_base_availability(mpd, period, adaptation, representation, &list.base);
    let maximum_duration = list
        .duration
        .or_else(|| list.timeline.iter().map(|entry| entry.duration).max())
        .filter(|_| list.base.timescale.unwrap_or(1) > 0)
        .map(|duration| duration as f64 / list.base.timescale.unwrap_or(1) as f64);
    findings.push(finding(
        "FORGE-DASH-LIVE-AVAILABILITY-OFFSET",
        Severity::Error,
        offset >= 0.0 && !offset.is_nan(),
        format!("{label} effective availabilityTimeOffset is non-negative"),
        Some(json!({
            "effective_offset_seconds": finite_json_number(offset),
            "infinite": offset.is_infinite(),
            "availability_time_complete": complete
        })),
    ));
    if complete == Some(false) {
        let latency_target = mpd
            .service_descriptions
            .iter()
            .filter_map(|service| service.latency.and_then(|latency| latency.target))
            .min()
            .map(|milliseconds| milliseconds as f64 / 1_000.0);
        findings.push(finding(
            "FORGE-DASH-LL-AVAILABILITY",
            Severity::Error,
            offset.is_finite()
                && offset > 0.0
                && maximum_duration.is_some_and(|duration| offset < duration),
            format!(
                "{label} incomplete SegmentList has finite positive ATO below segment duration"
            ),
            Some(json!({
                "effective_offset_seconds": finite_json_number(offset),
                "maximum_segment_duration_seconds": maximum_duration
            })),
        ));
        findings.push(finding(
            "FORGE-DASH-LL-LATENCY-GEOMETRY",
            Severity::Error,
            match (maximum_duration, latency_target) {
                (Some(duration), Some(target)) => duration < target && duration - offset < target,
                _ => false,
            },
            format!("{label} SegmentList duration/ATO are coherent with the latency target"),
            Some(json!({
                "maximum_segment_duration_seconds": maximum_duration,
                "effective_offset_seconds": finite_json_number(offset),
                "latency_target_seconds": latency_target
            })),
        ));
    }
}

fn validate_segment_base(
    label: &str,
    base: &SegmentBase,
    profile: DashProfile,
    availability: ((f64, Option<bool>), bool),
    findings: &mut Vec<DashFinding>,
) {
    let timescale = base.timescale.unwrap_or(1);
    findings.push(finding(
        "FORGE-DASH-TIMESCALE",
        Severity::Error,
        timescale > 0,
        format!("{label} SegmentBase timescale is positive"),
        Some(json!(timescale)),
    ));
    let restricted = matches!(profile, DashProfile::DashIfIop | DashProfile::DashLive);
    findings.push(finding(
        "FORGE-DASH-SEGMENT-BASE-INDEX",
        if restricted {
            Severity::Error
        } else {
            Severity::Warning
        },
        base.index_range.is_some(),
        format!("{label} SegmentBase declares an indexRange"),
        base.index_range
            .map(|range| json!({"start": range.start, "end": range.end})),
    ));
    let initialization = base.initialization.as_ref();
    findings.push(finding(
        "FORGE-DASH-SEGMENT-BASE-INITIALIZATION",
        if restricted {
            Severity::Error
        } else {
            Severity::Warning
        },
        initialization.is_some_and(|item| item.range.is_some()),
        format!("{label} SegmentBase declares an Initialization byte range"),
        initialization.and_then(|item| {
            item.range
                .map(|range| json!({"start": range.start, "end": range.end}))
        }),
    ));
    if restricted {
        findings.push(finding(
            "FORGE-DASHIF-SEGMENT-BASE-INITIALIZATION",
            Severity::Error,
            initialization.is_none_or(|item| item.source_url.is_none()),
            format!("{label} indexed addressing keeps Initialization in the media resource"),
            initialization
                .and_then(|item| item.source_url.clone())
                .map(Value::String),
        ));
    }
    if profile == DashProfile::DashLive {
        let ((offset, complete), offset_declared) = availability;
        findings.push(finding(
            "FORGE-DASH-LIVE-SEGMENT-BASE-COMPLETE",
            Severity::Error,
            complete.unwrap_or(true),
            format!("{label} SegmentBase is advertised as a complete resource"),
            complete.map(Value::Bool),
        ));
        findings.push(finding(
            "FORGE-DASH-LIVE-AVAILABILITY-OFFSET",
            Severity::Error,
            offset_declared && offset.is_finite() && offset >= 0.0,
            format!("{label} SegmentBase has a finite effective availabilityTimeOffset"),
            Some(json!({
                "declared": offset_declared,
                "availability_time_offset_seconds": finite_json_number(offset),
                "infinite": offset.is_infinite()
            })),
        ));
    }
}

fn audit_local_segment_list(
    mpd_path: &Path,
    list: &SegmentList,
    base_url: Option<&str>,
    period_duration: Option<f64>,
    findings: &mut Vec<DashFinding>,
) {
    if let Some(initialization) = &list.base.initialization {
        let uri = apply_base_url(base_url, initialization.source_url.as_deref().unwrap_or(""));
        audit_local_range(
            mpd_path,
            &uri,
            initialization.range,
            "initialization",
            findings,
        );
    }
    if expand_segment_list(list, period_duration).is_err() {
        return;
    }
    for (index, segment) in list
        .segment_urls
        .iter()
        .take(MAX_LOCAL_SEGMENTS)
        .enumerate()
    {
        let Some(media) = segment.media.as_deref() else {
            continue;
        };
        let uri = apply_base_url(base_url, media);
        audit_local_range(
            mpd_path,
            &uri,
            segment.media_range,
            &format!("SegmentURL {index} media"),
            findings,
        );
        if let Some(index_uri) = segment.index.as_deref() {
            let uri = apply_base_url(base_url, index_uri);
            audit_local_range(
                mpd_path,
                &uri,
                segment.index_range,
                &format!("SegmentURL {index} index"),
                findings,
            );
        } else if segment.index_range.is_some() {
            audit_local_range(
                mpd_path,
                &uri,
                segment.index_range,
                &format!("SegmentURL {index} index"),
                findings,
            );
        }
    }
}

fn audit_local_range(
    mpd_path: &Path,
    uri: &str,
    range: Option<ByteRange>,
    label: &str,
    findings: &mut Vec<DashFinding>,
) {
    let Some(path) = local_reference(mpd_path, uri) else {
        findings.push(finding(
            "FORGE-DASH-REMOTE-REFERENCE",
            Severity::Warning,
            false,
            format!("remote or unresolved {label} resource was not fetched: {uri}"),
            Some(json!(uri)),
        ));
        return;
    };
    let length = fs::metadata(&path)
        .ok()
        .filter(|item| item.is_file())
        .map(|item| item.len());
    findings.push(finding(
        "FORGE-DASH-LOCAL-RESOURCE",
        Severity::Error,
        length.is_some(),
        format!("{label} resource exists: {}", path.display()),
        None,
    ));
    if let (Some(length), Some(range)) = (length, range) {
        findings.push(finding(
            "FORGE-DASH-BYTE-RANGE",
            Severity::Error,
            range.end < length,
            format!("{label} byte range is inside the local resource"),
            Some(json!({
                "path": path,
                "start": range.start,
                "end": range.end,
                "resource_bytes": length
            })),
        ));
    }
}

#[derive(Debug)]
struct SidxInfo {
    timescale: u32,
    earliest_presentation_time: u64,
    first_offset: u64,
    reference_count: u16,
    maximum_subsegment_duration: u32,
    total_duration: u64,
    total_referenced_size: u64,
}

fn audit_local_segment_base(
    mpd_path: &Path,
    base: &SegmentBase,
    base_url: Option<&str>,
    profile: DashProfile,
    findings: &mut Vec<DashFinding>,
) {
    let uri = base_url.unwrap_or("");
    let Some(path) = local_reference(mpd_path, uri) else {
        findings.push(finding(
            "FORGE-DASH-REMOTE-REFERENCE",
            Severity::Warning,
            false,
            format!("remote or unresolved SegmentBase resource was not fetched: {uri}"),
            Some(json!(uri)),
        ));
        return;
    };
    let length = fs::metadata(&path)
        .ok()
        .filter(|item| item.is_file())
        .map(|item| item.len());
    findings.push(finding(
        "FORGE-DASH-LOCAL-RESOURCE",
        Severity::Error,
        length.is_some(),
        format!("SegmentBase media resource exists: {}", path.display()),
        None,
    ));
    let Some(length) = length else {
        return;
    };
    if let Some(initialization) = &base.initialization {
        let initialization_uri = initialization
            .source_url
            .as_deref()
            .map(|source| apply_base_url(base_url, source))
            .unwrap_or_else(|| uri.to_owned());
        audit_local_range(
            mpd_path,
            &initialization_uri,
            initialization.range,
            "SegmentBase initialization",
            findings,
        );
    }
    let Some(range) = base.index_range else {
        return;
    };
    let in_bounds = range.end < length;
    findings.push(finding(
        "FORGE-DASH-BYTE-RANGE",
        Severity::Error,
        in_bounds,
        "SegmentBase indexRange is inside the local media resource",
        Some(json!({
            "path": path,
            "start": range.start,
            "end": range.end,
            "resource_bytes": length
        })),
    ));
    if !in_bounds {
        return;
    }
    match parse_sidx(&path, range) {
        Ok(info) => {
            let timescale_matches = base
                .timescale
                .is_some_and(|timescale| timescale == u64::from(info.timescale));
            findings.push(finding(
                "FORGE-DASH-SIDX",
                Severity::Error,
                info.timescale > 0 && info.reference_count > 0,
                "SegmentBase indexRange contains one bounded, valid sidx box",
                Some(json!({
                    "timescale": info.timescale,
                    "earliest_presentation_time": info.earliest_presentation_time,
                    "first_offset": info.first_offset,
                    "reference_count": info.reference_count,
                    "maximum_subsegment_duration": info.maximum_subsegment_duration,
                    "total_duration": info.total_duration,
                    "total_referenced_size": info.total_referenced_size
                })),
            ));
            let referenced_end = range
                .end
                .checked_add(1)
                .and_then(|value| value.checked_add(info.first_offset))
                .and_then(|value| value.checked_add(info.total_referenced_size));
            findings.push(finding(
                "FORGE-DASH-SIDX-RANGE",
                Severity::Error,
                referenced_end.is_some_and(|end| end <= length),
                "sidx references remain inside the local media resource",
                Some(json!({
                    "resource_bytes": length,
                    "exclusive_referenced_end": referenced_end
                })),
            ));
            findings.push(finding(
                "FORGE-DASH-SIDX-TIMESCALE",
                if matches!(profile, DashProfile::DashIfIop | DashProfile::DashLive) {
                    Severity::Error
                } else {
                    Severity::Warning
                },
                timescale_matches,
                "SegmentBase explicitly declares the sidx timescale",
                Some(json!({
                    "segment_base_timescale": base.timescale,
                    "sidx_timescale": info.timescale
                })),
            ));
        }
        Err(error) => findings.push(finding(
            "FORGE-DASH-SIDX",
            Severity::Error,
            false,
            error,
            Some(json!({"path": path, "start": range.start, "end": range.end})),
        )),
    }
}

fn parse_sidx(path: &Path, range: ByteRange) -> Result<SidxInfo, String> {
    let length = range
        .end
        .checked_sub(range.start)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| "invalid sidx byte range".to_string())?;
    if length > MAX_INDEX_BYTES {
        return Err(format!(
            "sidx range exceeds the {MAX_INDEX_BYTES} byte safety limit"
        ));
    }
    let mut file =
        fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    file.seek(SeekFrom::Start(range.start))
        .map_err(|error| format!("seek {}: {error}", path.display()))?;
    let mut bytes = vec![0_u8; length as usize];
    file.read_exact(&mut bytes)
        .map_err(|error| format!("read sidx from {}: {error}", path.display()))?;
    if bytes.len() < 12 {
        return Err("sidx range is shorter than a box header".into());
    }
    let size32 = be_u32(&bytes, 0)?;
    if bytes.get(4..8) != Some(b"sidx") {
        return Err("indexRange does not start with a sidx box".into());
    }
    let (box_size, mut cursor) = match size32 {
        0 => return Err("sidx must use an explicit bounded box size".into()),
        1 => (be_u64(&bytes, 8)?, 16),
        value => (u64::from(value), 8),
    };
    if box_size != bytes.len() as u64 {
        return Err("indexRange must exactly cover one sidx box".into());
    }
    let version = *bytes
        .get(cursor)
        .ok_or_else(|| "truncated sidx full-box header".to_string())?;
    cursor += 4;
    let _reference_id = be_u32(&bytes, cursor)?;
    cursor += 4;
    let timescale = be_u32(&bytes, cursor)?;
    cursor += 4;
    let (earliest_presentation_time, first_offset) = match version {
        0 => {
            let earliest = u64::from(be_u32(&bytes, cursor)?);
            let offset = u64::from(be_u32(&bytes, cursor + 4)?);
            cursor += 8;
            (earliest, offset)
        }
        1 => {
            let earliest = be_u64(&bytes, cursor)?;
            let offset = be_u64(&bytes, cursor + 8)?;
            cursor += 16;
            (earliest, offset)
        }
        _ => return Err(format!("unsupported sidx version {version}")),
    };
    cursor += 2;
    let reference_count = be_u16(&bytes, cursor)?;
    cursor += 2;
    let expected = cursor
        .checked_add(usize::from(reference_count) * 12)
        .ok_or_else(|| "sidx reference table size overflow".to_string())?;
    if expected != bytes.len() {
        return Err("sidx reference table does not exactly fill indexRange".into());
    }
    if timescale == 0 {
        return Err("sidx timescale is zero".into());
    }
    let mut total_duration = 0_u64;
    let mut total_referenced_size = 0_u64;
    let mut maximum_subsegment_duration = 0_u32;
    for _ in 0..reference_count {
        let reference = be_u32(&bytes, cursor)?;
        if reference >> 31 != 0 {
            return Err("hierarchical sidx references are not supported".into());
        }
        let referenced_size = reference & 0x7fff_ffff;
        if referenced_size == 0 {
            return Err("sidx reference has zero referenced_size".into());
        }
        let duration = be_u32(&bytes, cursor + 4)?;
        if duration == 0 {
            return Err("sidx reference has zero subsegment_duration".into());
        }
        total_referenced_size = total_referenced_size
            .checked_add(u64::from(referenced_size))
            .ok_or_else(|| "sidx referenced size overflow".to_string())?;
        total_duration = total_duration
            .checked_add(u64::from(duration))
            .ok_or_else(|| "sidx duration overflow".to_string())?;
        maximum_subsegment_duration = maximum_subsegment_duration.max(duration);
        cursor += 12;
    }
    Ok(SidxInfo {
        timescale,
        earliest_presentation_time,
        first_offset,
        reference_count,
        maximum_subsegment_duration,
        total_duration,
        total_referenced_size,
    })
}

fn be_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| "truncated sidx".to_string())?;
    Ok(u16::from_be_bytes([value[0], value[1]]))
}

fn be_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| "truncated sidx".to_string())?;
    Ok(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}

fn be_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| "truncated sidx".to_string())?;
    Ok(u64::from_be_bytes([
        value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
    ]))
}

fn audit_local_resources(
    mpd_path: &Path,
    representation: &Representation,
    template: &SegmentTemplate,
    base_url: Option<&str>,
    period_duration: Option<f64>,
    low_latency: bool,
    findings: &mut Vec<DashFinding>,
) {
    let Some(id) = representation.id.as_deref() else {
        return;
    };
    if let Some(initialization) = &template.initialization {
        let uri = substitute_template(
            initialization,
            id,
            representation.bandwidth,
            effective_start_number(template),
            0,
        );
        let uri = apply_base_url(base_url, &uri);
        audit_local_isobmff(mpd_path, &uri, true, findings);
    } else {
        findings.push(finding(
            "FORGE-DASH-CMAF-INITIALIZATION",
            Severity::Warning,
            false,
            format!("Representation {id} has no initialization template"),
            None,
        ));
    }
    let Some(media) = &template.media else {
        return;
    };
    let Ok(timeline) = expand_timeline(template, period_duration) else {
        return;
    };
    let mut previous_sequence = None;
    let mut decode_times = HashMap::<u64, u64>::new();
    for (index, time) in timeline.into_iter().take(MAX_LOCAL_SEGMENTS).enumerate() {
        let number = effective_start_number(template).saturating_add(index as u64);
        let uri = apply_base_url(
            base_url,
            &substitute_template(media, id, representation.bandwidth, number, time),
        );
        let Some(path) = local_reference(mpd_path, &uri) else {
            findings.push(finding(
                "FORGE-DASH-REMOTE-REFERENCE",
                Severity::Warning,
                false,
                format!("remote or unresolved segment was not fetched: {uri}"),
                Some(json!(uri)),
            ));
            continue;
        };
        let exists = path.is_file();
        findings.push(finding(
            "FORGE-DASH-LOCAL-RESOURCE",
            Severity::Error,
            exists,
            format!("media segment exists: {}", path.display()),
            None,
        ));
        if !exists {
            continue;
        }
        match container_qc::audit(&path) {
            Ok(audit) => {
                findings.push(finding(
                    "FORGE-DASH-CMAF-SEGMENT",
                    Severity::Error,
                    audit.passed
                        && audit.format == "isobmff"
                        && audit.properties["fragment_movie_relative"] == true,
                    format!(
                        "CMAF media segment is a valid relative-addressed fMP4: {}",
                        path.display()
                    ),
                    Some(json!({"passed": audit.passed, "format": audit.format})),
                ));
                let sequences = audit.properties["fragment_sequences"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_u64)
                    .collect::<Vec<_>>();
                if low_latency {
                    findings.push(finding(
                        "FORGE-DASH-LL-CMAF-CHUNKS",
                        Severity::Error,
                        sequences.len() > 1,
                        "locally available incomplete DASH segment contains multiple CMAF chunks",
                        Some(json!({
                            "path": path,
                            "movie_fragment_count": sequences.len()
                        })),
                    ));
                }
                if let (Some(previous), Some(current)) =
                    (previous_sequence, sequences.first().copied())
                {
                    findings.push(finding(
                        "FORGE-DASH-FRAGMENT-SEQUENCE",
                        Severity::Error,
                        current == previous + 1,
                        "fragment sequence continues across DASH segment boundaries",
                        Some(json!({"previous": previous, "current": current})),
                    ));
                }
                previous_sequence = sequences.last().copied().or(previous_sequence);
                for item in audit.properties["fragment_decode_times"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    let (Some(track), Some(time)) =
                        (item["track_id"].as_u64(), item["time"].as_u64())
                    else {
                        continue;
                    };
                    let monotonic = decode_times
                        .insert(track, time)
                        .is_none_or(|previous| time >= previous);
                    findings.push(finding(
                        "FORGE-DASH-FRAGMENT-TIMELINE",
                        Severity::Error,
                        monotonic,
                        "fragment decode time is monotonic across DASH segments",
                        Some(json!({"track_id": track, "time": time})),
                    ));
                }
            }
            Err(error) => findings.push(finding(
                "FORGE-DASH-CMAF-SEGMENT",
                Severity::Error,
                false,
                error,
                Some(json!(path)),
            )),
        }
    }
}

fn audit_local_isobmff(
    mpd_path: &Path,
    uri: &str,
    initialization: bool,
    findings: &mut Vec<DashFinding>,
) {
    let Some(path) = local_reference(mpd_path, uri) else {
        findings.push(finding(
            "FORGE-DASH-REMOTE-REFERENCE",
            Severity::Warning,
            false,
            format!("remote initialization resource was not fetched: {uri}"),
            Some(json!(uri)),
        ));
        return;
    };
    let exists = path.is_file();
    findings.push(finding(
        "FORGE-DASH-LOCAL-RESOURCE",
        Severity::Error,
        exists,
        format!("initialization resource exists: {}", path.display()),
        None,
    ));
    if !exists {
        return;
    }
    match container_qc::audit(&path) {
        Ok(audit) => {
            let durations_zero = audit.properties["movie_duration"].as_u64() == Some(0)
                && audit.properties["track_header_durations"]
                    .as_array()
                    .is_some_and(|items| {
                        !items.is_empty() && items.iter().all(|value| value.as_u64() == Some(0))
                    });
            findings.push(finding(
                "FORGE-DASH-CMAF-INITIALIZATION",
                Severity::Error,
                audit.passed
                    && audit.format == "isobmff"
                    && (!initialization || durations_zero)
                    && audit.properties["mvex_after_tracks"] == true,
                format!(
                    "CMAF initialization segment is structurally valid: {}",
                    path.display()
                ),
                Some(json!({
                    "passed": audit.passed,
                    "format": audit.format,
                    "zero_durations": durations_zero,
                    "mvex_after_tracks": audit.properties["mvex_after_tracks"]
                })),
            ));
        }
        Err(error) => findings.push(finding(
            "FORGE-DASH-CMAF-INITIALIZATION",
            Severity::Error,
            false,
            error,
            Some(json!(path)),
        )),
    }
}

fn expand_timeline(
    template: &SegmentTemplate,
    period_duration: Option<f64>,
) -> Result<Vec<u64>, String> {
    expand_timeline_segments(template, period_duration).map(|segments| {
        segments
            .into_iter()
            .map(|(start, _duration)| start)
            .collect()
    })
}

fn expand_timeline_segments(
    template: &SegmentTemplate,
    period_duration: Option<f64>,
) -> Result<Vec<(u64, u64)>, String> {
    if !template.timeline.is_empty() {
        let mut result = Vec::new();
        let mut current = 0_u64;
        for (entry_index, entry) in template.timeline.iter().enumerate() {
            if entry.duration == 0 || entry.repeat < -1 {
                return Err("SegmentTimeline entry has invalid d/r values".into());
            }
            if let Some(time) = entry.time {
                if !result.is_empty() && time < current {
                    return Err("SegmentTimeline start time overlaps or moves backwards".into());
                }
                current = time;
            }
            let repeats = if entry.repeat >= 0 {
                entry.repeat as usize
            } else {
                let next_time = template
                    .timeline
                    .get(entry_index + 1)
                    .and_then(|item| item.time);
                let end = next_time.or_else(|| {
                    period_duration
                        .map(|value| (value * effective_timescale(template) as f64).ceil() as u64)
                });
                let Some(end) = end else {
                    return Err("negative SegmentTimeline repeat has no bounding time".into());
                };
                let span = end.saturating_sub(current);
                if span % entry.duration != 0 {
                    return Err(
                        "negative SegmentTimeline repeat does not end on its boundary".into(),
                    );
                }
                span.div_ceil(entry.duration).saturating_sub(1) as usize
            };
            for _ in 0..=repeats {
                if result.len() == MAX_LOCAL_SEGMENTS {
                    return Ok(result);
                }
                result.push((current, entry.duration));
                current = current.saturating_add(entry.duration);
            }
        }
        return Ok(result);
    }
    let Some(duration) = template.duration else {
        return Err("SegmentTemplate has neither duration nor timeline".into());
    };
    if duration == 0 {
        return Err("SegmentTemplate duration is zero".into());
    }
    let Some(period_duration) = period_duration else {
        return Err("duration-addressed SegmentTemplate has no bounded Period duration".into());
    };
    let count = ((period_duration * effective_timescale(template) as f64) / duration as f64).ceil()
        as usize;
    Ok((0..count.min(MAX_LOCAL_SEGMENTS))
        .map(|index| (index as u64 * duration, duration))
        .collect())
}

fn resolve_template(
    representation: Option<&SegmentTemplate>,
    adaptation: Option<&SegmentTemplate>,
    period: Option<&SegmentTemplate>,
) -> Option<SegmentTemplate> {
    let layers = [period, adaptation, representation];
    if layers.iter().all(Option::is_none) {
        return None;
    }
    let mut resolved = SegmentTemplate::default();
    for layer in layers.into_iter().flatten() {
        if layer.initialization.is_some() {
            resolved.initialization.clone_from(&layer.initialization);
        }
        if layer.media.is_some() {
            resolved.media.clone_from(&layer.media);
        }
        if layer.timescale.is_some() {
            resolved.timescale = layer.timescale;
        }
        if layer.duration.is_some() {
            resolved.duration = layer.duration;
        }
        if layer.start_number.is_some() {
            resolved.start_number = layer.start_number;
        }
        if layer.presentation_time_offset.is_some() {
            resolved.presentation_time_offset = layer.presentation_time_offset;
        }
        if layer.availability_time_offset.is_some() {
            resolved.availability_time_offset = layer.availability_time_offset;
        }
        if layer.availability_time_complete.is_some() {
            resolved.availability_time_complete = layer.availability_time_complete;
        }
        if !layer.timeline.is_empty() {
            resolved.timeline.clone_from(&layer.timeline);
        }
    }
    Some(resolved)
}

fn resolve_addressing(
    representation: &Representation,
    adaptation: &AdaptationSet,
    period: &Period,
) -> (Vec<AddressingKind>, Option<ResolvedAddressing>) {
    let levels = [
        (
            representation.template.as_ref(),
            representation.segment_list.as_ref(),
            representation.segment_base.as_ref(),
        ),
        (
            adaptation.template.as_ref(),
            adaptation.segment_list.as_ref(),
            adaptation.segment_base.as_ref(),
        ),
        (
            period.template.as_ref(),
            period.segment_list.as_ref(),
            period.segment_base.as_ref(),
        ),
    ];
    let modes = levels
        .iter()
        .map(|(template, list, base)| {
            let mut modes = Vec::new();
            if template.is_some() {
                modes.push(AddressingKind::Template);
            }
            if list.is_some() {
                modes.push(AddressingKind::List);
            }
            if base.is_some() {
                modes.push(AddressingKind::Base);
            }
            modes
        })
        .find(|modes| !modes.is_empty())
        .unwrap_or_default();
    if modes.len() != 1 {
        return (modes, None);
    }
    let resolved = match modes[0] {
        AddressingKind::Template => resolve_template(
            representation.template.as_ref(),
            adaptation.template.as_ref(),
            period.template.as_ref(),
        )
        .map(ResolvedAddressing::Template),
        AddressingKind::List => resolve_segment_list(
            representation.segment_list.as_ref(),
            adaptation.segment_list.as_ref(),
            period.segment_list.as_ref(),
        )
        .map(ResolvedAddressing::List),
        AddressingKind::Base => resolve_segment_base(
            representation.segment_base.as_ref(),
            adaptation.segment_base.as_ref(),
            period.segment_base.as_ref(),
        )
        .map(ResolvedAddressing::Base),
    };
    (modes, resolved)
}

fn resolve_segment_base(
    representation: Option<&SegmentBase>,
    adaptation: Option<&SegmentBase>,
    period: Option<&SegmentBase>,
) -> Option<SegmentBase> {
    let layers = [period, adaptation, representation];
    if layers.iter().all(Option::is_none) {
        return None;
    }
    let mut resolved = SegmentBase::default();
    for layer in layers.into_iter().flatten() {
        inherit_segment_base(&mut resolved, layer);
    }
    Some(resolved)
}

fn inherit_segment_base(resolved: &mut SegmentBase, layer: &SegmentBase) {
    if layer.timescale.is_some() {
        resolved.timescale = layer.timescale;
    }
    if layer.presentation_time_offset.is_some() {
        resolved.presentation_time_offset = layer.presentation_time_offset;
    }
    if layer.index_range.is_some() {
        resolved.index_range = layer.index_range;
    }
    if layer.index_range_exact.is_some() {
        resolved.index_range_exact = layer.index_range_exact;
    }
    if layer.availability_time_offset.is_some() {
        resolved.availability_time_offset = layer.availability_time_offset;
    }
    if layer.availability_time_complete.is_some() {
        resolved.availability_time_complete = layer.availability_time_complete;
    }
    if layer.initialization.is_some() {
        resolved.initialization.clone_from(&layer.initialization);
    }
}

fn resolve_segment_list(
    representation: Option<&SegmentList>,
    adaptation: Option<&SegmentList>,
    period: Option<&SegmentList>,
) -> Option<SegmentList> {
    let layers = [period, adaptation, representation];
    if layers.iter().all(Option::is_none) {
        return None;
    }
    let mut resolved = SegmentList::default();
    for layer in layers.into_iter().flatten() {
        inherit_segment_base(&mut resolved.base, &layer.base);
        if layer.duration.is_some() {
            resolved.duration = layer.duration;
        }
        if layer.start_number.is_some() {
            resolved.start_number = layer.start_number;
        }
        if !layer.timeline.is_empty() {
            resolved.timeline.clone_from(&layer.timeline);
        }
        if !layer.segment_urls.is_empty() {
            resolved.segment_urls.clone_from(&layer.segment_urls);
        }
    }
    Some(resolved)
}

fn effective_timescale(template: &SegmentTemplate) -> u64 {
    template.timescale.unwrap_or(1)
}

fn effective_start_number(template: &SegmentTemplate) -> u64 {
    template.start_number.unwrap_or(1)
}

fn set_base_url(
    mpd: &mut Mpd,
    period: Option<usize>,
    adaptation: Option<usize>,
    representation: Option<usize>,
    value: BaseUrl,
) -> Result<(), String> {
    let slot = base_url_slot_mut(mpd, period, adaptation, representation)?;
    slot.get_or_insert(value);
    Ok(())
}

fn set_base_url_value(
    mpd: &mut Mpd,
    period: Option<usize>,
    adaptation: Option<usize>,
    representation: Option<usize>,
    value: String,
) -> Result<(), String> {
    let slot = base_url_slot_mut(mpd, period, adaptation, representation)?;
    slot.get_or_insert_with(BaseUrl::default)
        .value
        .get_or_insert(value);
    Ok(())
}

fn base_url_slot_mut(
    mpd: &mut Mpd,
    period: Option<usize>,
    adaptation: Option<usize>,
    representation: Option<usize>,
) -> Result<&mut Option<BaseUrl>, String> {
    if let Some(period_index) = period {
        let period = mpd
            .periods
            .get_mut(period_index)
            .ok_or_else(|| "invalid active Period for BaseURL".to_string())?;
        if let Some(adaptation_index) = adaptation {
            let adaptation = period
                .adaptations
                .get_mut(adaptation_index)
                .ok_or_else(|| "invalid active AdaptationSet for BaseURL".to_string())?;
            if let Some(representation_index) = representation {
                let representation = adaptation
                    .representations
                    .get_mut(representation_index)
                    .ok_or_else(|| "invalid active Representation for BaseURL".to_string())?;
                Ok(&mut representation.base_url)
            } else {
                Ok(&mut adaptation.base_url)
            }
        } else {
            Ok(&mut period.base_url)
        }
    } else {
        Ok(&mut mpd.base_url)
    }
}

fn resolved_base_url(
    mpd: &Mpd,
    period: &Period,
    adaptation: &AdaptationSet,
    representation: &Representation,
) -> Option<String> {
    let mut result = None;
    for layer in [
        mpd.base_url.as_ref(),
        period.base_url.as_ref(),
        adaptation.base_url.as_ref(),
        representation.base_url.as_ref(),
    ]
    .into_iter()
    .flatten()
    .filter_map(|base_url| base_url.value.as_deref())
    {
        result = Some(apply_base_url(result.as_deref(), layer));
    }
    result
}

fn effective_availability(
    mpd: &Mpd,
    period: &Period,
    adaptation: &AdaptationSet,
    representation: &Representation,
    template: &SegmentTemplate,
) -> (f64, Option<bool>) {
    effective_base_availability(
        mpd,
        period,
        adaptation,
        representation,
        &SegmentBase {
            availability_time_offset: template.availability_time_offset,
            availability_time_complete: template.availability_time_complete,
            ..SegmentBase::default()
        },
    )
}

fn effective_base_availability(
    mpd: &Mpd,
    period: &Period,
    adaptation: &AdaptationSet,
    representation: &Representation,
    segment_base: &SegmentBase,
) -> (f64, Option<bool>) {
    let mut offset = 0.0;
    let mut complete = None;
    for base_url in [
        mpd.base_url.as_ref(),
        period.base_url.as_ref(),
        adaptation.base_url.as_ref(),
        representation.base_url.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        offset += base_url.availability_time_offset.unwrap_or(0.0);
        if base_url.availability_time_complete.is_some() {
            complete = base_url.availability_time_complete;
        }
    }
    offset += segment_base.availability_time_offset.unwrap_or(0.0);
    if segment_base.availability_time_complete.is_some() {
        complete = segment_base.availability_time_complete;
    }
    (offset, complete)
}

fn availability_offset_declared(
    mpd: &Mpd,
    period: &Period,
    adaptation: &AdaptationSet,
    representation: &Representation,
    segment_base: &SegmentBase,
) -> bool {
    segment_base.availability_time_offset.is_some()
        || [
            mpd.base_url.as_ref(),
            period.base_url.as_ref(),
            adaptation.base_url.as_ref(),
            representation.base_url.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(|item| item.availability_time_offset.is_some())
}

fn apply_base_url(base: Option<&str>, resource: &str) -> String {
    if resource.contains("://") || resource.starts_with("//") || resource.starts_with('/') {
        return resource.to_owned();
    }
    let Some(base) = base else {
        return resource.to_owned();
    };
    if base.ends_with('/') {
        format!("{base}{resource}")
    } else {
        format!("{base}/{resource}")
    }
}

fn substitute_template(
    template: &str,
    representation_id: &str,
    bandwidth: Option<u64>,
    number: u64,
    time: u64,
) -> String {
    let mut output = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('$') {
        output.push_str(&rest[..open]);
        rest = &rest[open + 1..];
        if let Some(next) = rest.strip_prefix('$') {
            output.push('$');
            rest = next;
            continue;
        }
        let Some(close) = rest.find('$') else {
            output.push('$');
            output.push_str(rest);
            return output;
        };
        let token = &rest[..close];
        let replacement = if token == "RepresentationID" {
            Some(representation_id.to_owned())
        } else {
            formatted_template_number(token, "Bandwidth", bandwidth.unwrap_or(0))
                .or_else(|| formatted_template_number(token, "Number", number))
                .or_else(|| formatted_template_number(token, "Time", time))
        };
        if let Some(replacement) = replacement {
            output.push_str(&replacement);
        } else {
            output.push('$');
            output.push_str(token);
            output.push('$');
        }
        rest = &rest[close + 1..];
    }
    output.push_str(rest);
    output
}

fn formatted_template_number(token: &str, identifier: &str, value: u64) -> Option<String> {
    if token == identifier {
        return Some(value.to_string());
    }
    let format = token.strip_prefix(identifier)?.strip_prefix("%0")?;
    let width = format.strip_suffix('d')?.parse::<usize>().ok()?;
    if width == 0 || width > 64 {
        return None;
    }
    Some(format!("{value:0width$}"))
}

fn local_reference(mpd: &Path, uri: &str) -> Option<PathBuf> {
    if uri.contains("://")
        || uri.starts_with("//")
        || uri.starts_with('/')
        || uri.contains('$')
        || uri.split('/').any(|part| part == "..")
    {
        return None;
    }
    let clean = uri.split(['?', '#']).next().unwrap_or(uri);
    Some(mpd.parent().unwrap_or_else(|| Path::new(".")).join(clean))
}

fn parse_duration(value: &str) -> Result<f64, String> {
    let Some(mut rest) = value.strip_prefix('P') else {
        return Err(format!("invalid ISO 8601 duration: {value}"));
    };
    let mut seconds = 0.0;
    let mut in_time = false;
    while !rest.is_empty() {
        if let Some(next) = rest.strip_prefix('T') {
            in_time = true;
            rest = next;
            continue;
        }
        let split = rest
            .find(|character: char| !character.is_ascii_digit() && character != '.')
            .ok_or_else(|| format!("invalid ISO 8601 duration: {value}"))?;
        let number = rest[..split]
            .parse::<f64>()
            .map_err(|_| format!("invalid ISO 8601 duration: {value}"))?;
        let unit = rest[split..]
            .chars()
            .next()
            .ok_or_else(|| format!("invalid ISO 8601 duration: {value}"))?;
        seconds += match (unit, in_time) {
            ('D', false) => number * 86_400.0,
            ('H', true) => number * 3_600.0,
            ('M', true) => number * 60.0,
            ('S', true) => number,
            _ => return Err(format!("unsupported ISO 8601 duration component: {value}")),
        };
        rest = &rest[split + unit.len_utf8()..];
    }
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(format!("invalid ISO 8601 duration: {value}"));
    }
    Ok(seconds)
}

fn looks_like_xs_datetime(value: &str) -> bool {
    if !value.is_ascii() {
        return false;
    }
    let Some(separator) = value.find('T') else {
        return false;
    };
    let (date, time_with_separator) = value.split_at(separator);
    let time = &time_with_separator[1..];
    let mut date_fields = date.split('-');
    let (Some(year), Some(month), Some(day), None) = (
        date_fields.next(),
        date_fields.next(),
        date_fields.next(),
        date_fields.next(),
    ) else {
        return false;
    };
    if year.len() != 4 || month.len() != 2 || day.len() != 2 {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) = (
        year.parse::<u32>(),
        month.parse::<u32>(),
        day.parse::<u32>(),
    ) else {
        return false;
    };
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let maximum_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    if day == 0 || day > maximum_day {
        return false;
    }
    let (clock, zone_valid) = if let Some(clock) = time.strip_suffix('Z') {
        (clock, true)
    } else if time.len() >= 6 && matches!(time.as_bytes().get(time.len() - 6), Some(b'+' | b'-')) {
        let zone_start = time.len() - 6;
        let (clock, zone) = time.split_at(zone_start);
        let bytes = zone.as_bytes();
        let valid = matches!(bytes.first(), Some(b'+' | b'-'))
            && bytes.get(3) == Some(&b':')
            && zone[1..3].parse::<u32>().is_ok_and(|hours| hours <= 14)
            && zone[4..6].parse::<u32>().is_ok_and(|minutes| minutes <= 59)
            && !(&zone[1..3] == "14" && &zone[4..6] != "00");
        (clock, valid)
    } else {
        (time, true)
    };
    if !zone_valid {
        return false;
    }
    let mut clock_fields = clock.split(':');
    let (Some(hour), Some(minute), Some(second), None) = (
        clock_fields.next(),
        clock_fields.next(),
        clock_fields.next(),
        clock_fields.next(),
    ) else {
        return false;
    };
    if hour.len() != 2 || minute.len() != 2 {
        return false;
    }
    let (seconds, fraction) = second.find('.').map_or((second, None), |separator| {
        (&second[..separator], Some(&second[separator + 1..]))
    });
    seconds.len() == 2
        && hour.parse::<u32>().is_ok_and(|hour| hour <= 23)
        && minute.parse::<u32>().is_ok_and(|minute| minute <= 59)
        && seconds.parse::<u32>().is_ok_and(|seconds| seconds <= 59)
        && fraction.is_none_or(|fraction| {
            !fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit())
        })
}

pub(crate) fn parse_xs_datetime_seconds(value: &str) -> Option<f64> {
    if !looks_like_xs_datetime(value) || !has_datetime_zone(value) {
        return None;
    }
    let (date, time) = value.split_once('T')?;
    let mut date_fields = date.split('-');
    let year = date_fields.next()?.parse::<i64>().ok()?;
    let month = date_fields.next()?.parse::<u32>().ok()?;
    let day = date_fields.next()?.parse::<u32>().ok()?;
    if date_fields.next().is_some() {
        return None;
    }
    let (clock, offset_seconds) = if let Some(clock) = time.strip_suffix('Z') {
        (clock, 0_i64)
    } else {
        let split = time.len().checked_sub(6)?;
        let (clock, zone) = time.split_at(split);
        let sign = match zone.as_bytes().first()? {
            b'+' => 1_i64,
            b'-' => -1_i64,
            _ => return None,
        };
        let hours = zone.get(1..3)?.parse::<i64>().ok()?;
        let minutes = zone.get(4..6)?.parse::<i64>().ok()?;
        (clock, sign * (hours * 3_600 + minutes * 60))
    };
    let mut clock_fields = clock.split(':');
    let hours = clock_fields.next()?.parse::<i64>().ok()?;
    let minutes = clock_fields.next()?.parse::<i64>().ok()?;
    let seconds = clock_fields.next()?.parse::<f64>().ok()?;
    if clock_fields.next().is_some() {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(
        days as f64 * 86_400.0 + hours as f64 * 3_600.0 + minutes as f64 * 60.0 + seconds
            - offset_seconds as f64,
    )
}

fn days_from_civil(mut year: i64, month: u32, day: u32) -> i64 {
    year -= i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn has_datetime_zone(value: &str) -> bool {
    value.ends_with('Z')
        || (value.len() >= 6 && matches!(value.as_bytes().get(value.len() - 6), Some(b'+' | b'-')))
}

fn attributes(
    reader: &Reader<&[u8]>,
    element: &BytesStart<'_>,
) -> Result<HashMap<String, String>, String> {
    let mut result = HashMap::new();
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| format!("XML attribute: {error}"))?;
        let key = local_name(attribute.key.as_ref());
        let value = attribute
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
            .map_err(|error| format!("XML attribute value: {error}"))?
            .into_owned();
        if result.insert(key.clone(), value).is_some() {
            return Err(format!("duplicate XML attribute: {key}"));
        }
    }
    Ok(result)
}

fn namespace_attribute(
    reader: &Reader<&[u8]>,
    element: &BytesStart<'_>,
) -> Result<Option<String>, String> {
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| format!("XML attribute: {error}"))?;
        if attribute.key.as_ref() == b"xmlns" {
            return attribute
                .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                .map(|value| Some(value.into_owned()))
                .map_err(|error| format!("XML namespace: {error}"));
        }
    }
    Ok(None)
}

fn parse_optional_u64(
    attributes: &HashMap<String, String>,
    name: &str,
) -> Result<Option<u64>, String> {
    attributes
        .get(name)
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| format!("invalid unsigned integer @{name}={value}"))
        })
        .transpose()
}

fn parse_optional_f64(
    attributes: &HashMap<String, String>,
    name: &str,
) -> Result<Option<f64>, String> {
    attributes
        .get(name)
        .map(|value| {
            let value = value
                .parse::<f64>()
                .map_err(|_| format!("invalid decimal @{name}={value}"))?;
            if value.is_finite() {
                Ok(value)
            } else {
                Err(format!("non-finite decimal @{name}={value}"))
            }
        })
        .transpose()
}

fn parse_optional_availability_offset(
    attributes: &HashMap<String, String>,
    name: &str,
) -> Result<Option<f64>, String> {
    attributes
        .get(name)
        .map(|value| {
            if value == "INF" {
                Ok(f64::INFINITY)
            } else {
                let value = value
                    .parse::<f64>()
                    .map_err(|_| format!("invalid availability offset @{name}={value}"))?;
                if value.is_finite() && value >= 0.0 {
                    Ok(value)
                } else {
                    Err(format!("invalid availability offset @{name}={value}"))
                }
            }
        })
        .transpose()
}

fn parse_optional_bool(
    attributes: &HashMap<String, String>,
    name: &str,
) -> Result<Option<bool>, String> {
    attributes
        .get(name)
        .map(|value| match value.as_str() {
            "true" | "1" => Ok(true),
            "false" | "0" => Ok(false),
            _ => Err(format!("invalid boolean @{name}={value}")),
        })
        .transpose()
}

fn parse_optional_byte_range(
    attributes: &HashMap<String, String>,
    name: &str,
) -> Result<Option<ByteRange>, String> {
    attributes
        .get(name)
        .map(|value| {
            let (start, end) = value
                .split_once('-')
                .ok_or_else(|| format!("invalid byte range @{name}={value}"))?;
            let start = start
                .parse::<u64>()
                .map_err(|_| format!("invalid byte range @{name}={value}"))?;
            let end = end
                .parse::<u64>()
                .map_err(|_| format!("invalid byte range @{name}={value}"))?;
            if start <= end {
                Ok(ByteRange { start, end })
            } else {
                Err(format!("reversed byte range @{name}={value}"))
            }
        })
        .transpose()
}

fn parse_segment_base_attributes(
    attributes: &HashMap<String, String>,
) -> Result<SegmentBase, String> {
    Ok(SegmentBase {
        timescale: parse_optional_u64(attributes, "timescale")?,
        presentation_time_offset: parse_optional_u64(attributes, "presentationTimeOffset")?,
        index_range: parse_optional_byte_range(attributes, "indexRange")?,
        index_range_exact: parse_optional_bool(attributes, "indexRangeExact")?,
        availability_time_offset: parse_optional_availability_offset(
            attributes,
            "availabilityTimeOffset",
        )?,
        availability_time_complete: parse_optional_bool(attributes, "availabilityTimeComplete")?,
        initialization: None,
    })
}

fn current_period_mut(mpd: &mut Mpd, period: Option<usize>) -> Result<&mut Period, String> {
    period
        .and_then(|index| mpd.periods.get_mut(index))
        .ok_or_else(|| "DASH element appears outside Period".into())
}

fn current_adaptation_mut(
    mpd: &mut Mpd,
    period: Option<usize>,
    adaptation: Option<usize>,
) -> Result<&mut AdaptationSet, String> {
    let period = current_period_mut(mpd, period)?;
    adaptation
        .and_then(|index| period.adaptations.get_mut(index))
        .ok_or_else(|| "DASH element appears outside AdaptationSet".into())
}

fn set_segment_base(
    mpd: &mut Mpd,
    period: Option<usize>,
    adaptation: Option<usize>,
    representation: Option<usize>,
    value: SegmentBase,
) -> Result<(), String> {
    let slot = segment_base_slot_mut(mpd, period, adaptation, representation)?;
    if slot.replace(value).is_some() {
        return Err("duplicate SegmentBase at the same MPD hierarchy level".into());
    }
    Ok(())
}

fn set_segment_list(
    mpd: &mut Mpd,
    period: Option<usize>,
    adaptation: Option<usize>,
    representation: Option<usize>,
    value: SegmentList,
) -> Result<(), String> {
    let slot = segment_list_slot_mut(mpd, period, adaptation, representation)?;
    if slot.replace(value).is_some() {
        return Err("duplicate SegmentList at the same MPD hierarchy level".into());
    }
    Ok(())
}

fn segment_base_slot_mut(
    mpd: &mut Mpd,
    period: Option<usize>,
    adaptation: Option<usize>,
    representation: Option<usize>,
) -> Result<&mut Option<SegmentBase>, String> {
    let period = current_period_mut(mpd, period)?;
    if let Some(adaptation_index) = adaptation {
        let adaptation = period
            .adaptations
            .get_mut(adaptation_index)
            .ok_or_else(|| "invalid active AdaptationSet".to_string())?;
        if let Some(representation_index) = representation {
            Ok(&mut adaptation
                .representations
                .get_mut(representation_index)
                .ok_or_else(|| "invalid active Representation".to_string())?
                .segment_base)
        } else {
            Ok(&mut adaptation.segment_base)
        }
    } else {
        Ok(&mut period.segment_base)
    }
}

fn segment_list_slot_mut(
    mpd: &mut Mpd,
    period: Option<usize>,
    adaptation: Option<usize>,
    representation: Option<usize>,
) -> Result<&mut Option<SegmentList>, String> {
    let period = current_period_mut(mpd, period)?;
    if let Some(adaptation_index) = adaptation {
        let adaptation = period
            .adaptations
            .get_mut(adaptation_index)
            .ok_or_else(|| "invalid active AdaptationSet".to_string())?;
        if let Some(representation_index) = representation {
            Ok(&mut adaptation
                .representations
                .get_mut(representation_index)
                .ok_or_else(|| "invalid active Representation".to_string())?
                .segment_list)
        } else {
            Ok(&mut adaptation.segment_list)
        }
    } else {
        Ok(&mut period.segment_list)
    }
}

fn current_segment_list_mut(
    mpd: &mut Mpd,
    period: Option<usize>,
    adaptation: Option<usize>,
    representation: Option<usize>,
) -> Result<&mut SegmentList, String> {
    segment_list_slot_mut(mpd, period, adaptation, representation)?
        .as_mut()
        .ok_or_else(|| "SegmentList child has no enclosing SegmentList".into())
}

fn current_segment_base_like_mut<'a>(
    mpd: &'a mut Mpd,
    period: Option<usize>,
    adaptation: Option<usize>,
    representation: Option<usize>,
    parent: Option<&str>,
) -> Result<&'a mut SegmentBase, String> {
    match parent {
        Some("SegmentBase") => segment_base_slot_mut(mpd, period, adaptation, representation)?
            .as_mut()
            .ok_or_else(|| "Initialization has no enclosing SegmentBase".into()),
        Some("SegmentList") => {
            Ok(&mut current_segment_list_mut(mpd, period, adaptation, representation)?.base)
        }
        _ => Err("Initialization has no supported segment-addressing parent".into()),
    }
}

fn current_timeline_mut<'a>(
    mpd: &'a mut Mpd,
    period: Option<usize>,
    adaptation: Option<usize>,
    representation: Option<usize>,
    addressing: Option<&str>,
) -> Result<&'a mut Vec<TimelineEntry>, String> {
    match addressing {
        Some("SegmentTemplate") => {
            Ok(&mut current_template_mut(mpd, period, adaptation, representation)?.timeline)
        }
        Some("SegmentList") => {
            Ok(&mut current_segment_list_mut(mpd, period, adaptation, representation)?.timeline)
        }
        _ => Err("SegmentTimeline has no supported enclosing addressing element".into()),
    }
}

fn current_content_protection_mut(
    mpd: &mut Mpd,
    period: Option<usize>,
    adaptation: Option<usize>,
    representation: Option<usize>,
) -> Result<&mut ContentProtection, String> {
    let adaptation = current_adaptation_mut(mpd, period, adaptation)?;
    if let Some(representation) = representation {
        adaptation
            .representations
            .get_mut(representation)
            .and_then(|item| item.content_protections.last_mut())
            .ok_or_else(|| "cenc:pssh has no enclosing ContentProtection".into())
    } else {
        adaptation
            .content_protections
            .last_mut()
            .ok_or_else(|| "cenc:pssh has no enclosing ContentProtection".into())
    }
}

fn current_template_mut(
    mpd: &mut Mpd,
    period: Option<usize>,
    adaptation: Option<usize>,
    representation: Option<usize>,
) -> Result<&mut SegmentTemplate, String> {
    let period = current_period_mut(mpd, period)?;
    if let Some(adaptation_index) = adaptation {
        let adaptation = period
            .adaptations
            .get_mut(adaptation_index)
            .ok_or_else(|| "invalid active AdaptationSet".to_string())?;
        if let Some(index) = representation {
            adaptation
                .representations
                .get_mut(index)
                .and_then(|item| item.template.as_mut())
                .ok_or_else(|| "SegmentTimeline has no enclosing SegmentTemplate".into())
        } else {
            adaptation
                .template
                .as_mut()
                .ok_or_else(|| "SegmentTimeline has no enclosing SegmentTemplate".into())
        }
    } else {
        period
            .template
            .as_mut()
            .ok_or_else(|| "SegmentTimeline has no enclosing SegmentTemplate".into())
    }
}

fn local_name(name: &[u8]) -> String {
    String::from_utf8_lossy(name.rsplit(|byte| *byte == b':').next().unwrap_or(name)).into_owned()
}

fn finding(
    rule_id: &'static str,
    severity: Severity,
    passed: bool,
    message: impl Into<String>,
    observed: Option<Value>,
) -> DashFinding {
    DashFinding {
        rule_id,
        severity,
        passed,
        message: message.into(),
        observed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_mpd(directory: &Path, body: &str) -> PathBuf {
        let path = directory.join("stream.mpd");
        fs::write(&path, body).unwrap();
        path
    }

    fn sidx_box(timescale: u32, duration: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&44_u32.to_be_bytes());
        bytes.extend_from_slice(b"sidx");
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(&1_u32.to_be_bytes());
        bytes.extend_from_slice(&timescale.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(&0_u16.to_be_bytes());
        bytes.extend_from_slice(&1_u16.to_be_bytes());
        bytes.extend_from_slice(&100_u32.to_be_bytes());
        bytes.extend_from_slice(&duration.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes
    }

    fn update_mpd(publish_time: &str, timeline: &str, representation_id: &str) -> String {
        format!(
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" id="live"
 type="dynamic" availabilityStartTime="2026-07-29T00:00:00Z"
 publishTime="{publish_time}" minimumUpdatePeriod="PT2S" minBufferTime="PT1S">
 <BaseURL>https://example.invalid/live/</BaseURL>
 <Period id="p0" start="PT0S">
  <AdaptationSet id="audio" contentType="audio" mimeType="audio/mp4"
   codecs="opus" lang="en" audioSamplingRate="48000">
   <SegmentTemplate timescale="10" initialization="init-$RepresentationID$.mp4"
    media="$RepresentationID$-$Time$.m4s">
    <SegmentTimeline>{timeline}</SegmentTimeline>
   </SegmentTemplate>
   <Representation id="{representation_id}" bandwidth="64000"/>
  </AdaptationSet>
 </Period>
</MPD>"#
        )
    }

    #[test]
    fn plans_remote_clock_and_latest_advertised_origin_resource() {
        let mpd = parse_mpd(
            br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic"
 availabilityStartTime="2026-07-29T00:00:00Z" publishTime="2026-07-29T00:00:20Z"
 minimumUpdatePeriod="PT2S" minBufferTime="PT1S">
 <UTCTiming schemeIdUri="urn:mpeg:dash:utc:http-head:2014"
  value="https://clock.example.test/time"/>
 <BaseURL>https://media.example.test/live/</BaseURL>
 <Period start="PT0S"><AdaptationSet contentType="audio" mimeType="audio/mp4">
  <SegmentTemplate timescale="10" initialization="init-$RepresentationID$.mp4"
   media="$RepresentationID$-$Time$.m4s">
   <SegmentTimeline><S t="0" d="10" r="2"/></SegmentTimeline>
  </SegmentTemplate>
  <Representation id="audio" bandwidth="64000"/>
 </AdaptationSet></Period>
</MPD>"#,
        )
        .unwrap();
        let targets = observation_targets_for_mpd(&mpd);
        assert_eq!(targets.len(), 2);
        assert_eq!(
            targets[0].kind,
            dash_observe::DashObservationKind::UtcHttpHead
        );
        assert_eq!(targets[0].uri, "https://clock.example.test/time");
        assert_eq!(
            targets[1].kind,
            dash_observe::DashObservationKind::OriginResource
        );
        assert_eq!(
            targets[1].uri,
            "https://media.example.test/live/audio-20.m4s"
        );
    }

    #[test]
    fn resolves_layered_remote_base_urls_and_templates_without_representation_ids() {
        let mpd = parse_mpd(
            br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
 mediaPresentationDuration="PT2S" minBufferTime="PT1S">
 <BaseURL>https://media.example.test/root/manifest.mpd</BaseURL>
 <Period><BaseURL>/live/</BaseURL>
  <AdaptationSet contentType="audio"><BaseURL>audio/</BaseURL>
   <SegmentTemplate timescale="1" duration="2" media="$Bandwidth$-$Number$.m4s"/>
   <Representation bandwidth="64000"><BaseURL>primary/</BaseURL></Representation>
  </AdaptationSet>
 </Period>
</MPD>"#,
        )
        .unwrap();
        let targets = observation_targets_for_mpd(&mpd);
        assert_eq!(targets.len(), 1);
        assert_eq!(
            targets[0].uri,
            "https://media.example.test/live/audio/primary/64000-1.m4s"
        );
    }

    #[test]
    fn audits_static_audio_segment_template() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_mpd(
            directory.path(),
            r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static"
 mediaPresentationDuration="PT4S" minBufferTime="PT1.5S">
 <Period id="p0">
  <AdaptationSet id="1" contentType="audio" mimeType="audio/mp4"
   codecs="mp4a.40.2" lang="en" audioSamplingRate="48000">
   <SegmentTemplate timescale="48000" duration="96000" startNumber="1"
    initialization="init-$RepresentationID$.mp4" media="$RepresentationID$-$Number$.m4s"/>
   <Representation id="a1" bandwidth="128000"/>
  </AdaptationSet>
 </Period>
</MPD>"#,
        );
        let audit = audit(&path, DashProfile::Iso23009).unwrap();
        assert!(!audit.passed);
        assert!(audit
            .findings
            .iter()
            .any(|item| item.rule_id == "FORGE-DASH-LOCAL-RESOURCE" && !item.passed));
        assert_eq!(audit.properties["representation_count"], 1);
    }

    #[test]
    fn validates_audio_switching_boundaries_across_languages_and_timescales() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_mpd(
            directory.path(),
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static"
 profiles="https://dashif.org/guidelines/dash-if-iop"
 mediaPresentationDuration="PT6S" minBufferTime="PT1S">
 <BaseURL>https://example.invalid/audio/</BaseURL>
 <Period id="p0">
  <AdaptationSet id="1" contentType="audio" mimeType="audio/mp4"
   codecs="mp4a.40.2" lang="en" segmentAlignment="true">
   <SupplementalProperty
    schemeIdUri="urn:mpeg:dash:adaptation-set-switching:2016" value="2"/>
   <SegmentTemplate timescale="48000" presentationTimeOffset="4800"
    initialization="en-init.mp4" media="en-$Time$.m4s">
    <SegmentTimeline><S t="4800" d="96000" r="2"/></SegmentTimeline>
   </SegmentTemplate>
   <Representation id="en" bandwidth="128000"/>
  </AdaptationSet>
  <AdaptationSet id="2" contentType="audio" mimeType="audio/mp4"
   codecs="mp4a.40.2" lang="fr" segmentAlignment="true">
   <SegmentTemplate timescale="1000" presentationTimeOffset="100"
    initialization="fr-init.mp4" media="fr-$Time$.m4s">
    <SegmentTimeline><S t="100" d="2000" r="2"/></SegmentTimeline>
   </SegmentTemplate>
   <Representation id="fr" bandwidth="128000"/>
  </AdaptationSet>
 </Period>
</MPD>"#,
        );
        let audit = audit(&path, DashProfile::DashIfIop).unwrap();
        assert!(
            audit.passed,
            "{:#?}",
            audit
                .findings
                .iter()
                .filter(|finding| finding.severity == Severity::Error && !finding.passed)
                .map(|finding| (&finding.rule_id, &finding.message))
                .collect::<Vec<_>>()
        );
        for rule in [
            "FORGE-DASHIF-ADAPTATION-SWITCHING-DESCRIPTOR",
            "FORGE-DASHIF-ADAPTATION-SWITCHING-REFERENCE",
            "FORGE-DASHIF-ADAPTATION-SWITCHING-TYPE",
            "FORGE-DASHIF-SWITCHING-SEGMENT-ALIGNMENT",
            "FORGE-DASHIF-SWITCHING-ALIGNMENT-EVIDENCE",
            "FORGE-DASHIF-SWITCHING-BOUNDARY-ALIGNMENT",
        ] {
            assert!(
                audit
                    .findings
                    .iter()
                    .any(|finding| finding.rule_id == rule && finding.passed),
                "missing passed rule {rule}"
            );
        }
        assert_eq!(
            audit.properties["adaptation_set_switching_descriptor_count"],
            1
        );
    }

    #[test]
    fn rejects_inconsistent_switching_claims_and_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_mpd(
            directory.path(),
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static"
 mediaPresentationDuration="PT6S" minBufferTime="PT1S">
 <BaseURL>https://example.invalid/audio/</BaseURL>
 <Period>
  <AdaptationSet id="1" contentType="audio" mimeType="audio/mp4"
   codecs="opus" lang="en" segmentAlignment="true">
   <SupplementalProperty
    schemeIdUri="urn:mpeg:dash:adaptation-set-switching:2016" value="2"/>
   <SegmentTemplate timescale="1000" duration="2000"
    initialization="en-init.mp4" media="en-$Number$.m4s"/>
   <Representation id="en" bandwidth="64000"/>
  </AdaptationSet>
  <AdaptationSet id="2" contentType="audio" mimeType="audio/mp4"
   codecs="opus" lang="ja" segmentAlignment="false">
   <SegmentTemplate timescale="1000" duration="1500"
    initialization="ja-init.mp4" media="ja-$Number$.m4s"/>
   <Representation id="ja" bandwidth="64000"/>
  </AdaptationSet>
 </Period>
</MPD>"#,
        );
        let audit = audit(&path, DashProfile::DashIfIop).unwrap();
        assert!(!audit.passed);
        for rule in [
            "FORGE-DASHIF-SWITCHING-SEGMENT-ALIGNMENT",
            "FORGE-DASHIF-SWITCHING-BOUNDARY-ALIGNMENT",
        ] {
            assert!(
                audit
                    .findings
                    .iter()
                    .any(|finding| finding.rule_id == rule && !finding.passed),
                "missing failed rule {rule}"
            );
        }
    }

    #[test]
    fn rejects_malformed_or_dangling_switching_targets() {
        assert!(parse_switching_targets("").is_err());
        assert!(parse_switching_targets("2,2").is_err());
        assert_eq!(
            parse_switching_targets("2, 3").unwrap(),
            vec!["2".to_owned(), "3".to_owned()]
        );

        let directory = tempfile::tempdir().unwrap();
        let path = write_mpd(
            directory.path(),
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static"
 mediaPresentationDuration="PT2S" minBufferTime="PT1S">
 <Period><SegmentTemplate timescale="1" duration="2" media="segment.m4s"/>
  <AdaptationSet id="1" contentType="audio" mimeType="audio/mp4" codecs="opus">
   <SupplementalProperty
    schemeIdUri="urn:mpeg:dash:adaptation-set-switching:2016" value="1"/>
   <SupplementalProperty
    schemeIdUri="urn:mpeg:dash:adaptation-set-switching:2016" value="missing"/>
   <Representation id="audio" bandwidth="64000"/>
  </AdaptationSet>
 </Period>
</MPD>"#,
        );
        let audit = audit(&path, DashProfile::DashIfIop).unwrap();
        assert!(!audit.passed);
        assert!(audit.findings.iter().any(|finding| {
            finding.rule_id == "FORGE-DASHIF-ADAPTATION-SWITCHING-REFERENCE" && !finding.passed
        }));
    }

    #[test]
    fn rejects_duplicate_representation_ids_and_invalid_timeline() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_mpd(
            directory.path(),
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static"
 mediaPresentationDuration="PT2S" minBufferTime="PT1S">
 <Period><AdaptationSet contentType="audio" mimeType="audio/mp4" codecs="opus">
  <SegmentTemplate timescale="48000" initialization="init-$RepresentationID$.mp4"
   media="$RepresentationID$-$Time$.m4s"><SegmentTimeline><S t="0" d="0"/></SegmentTimeline></SegmentTemplate>
  <Representation id="same" bandwidth="64000"/>
  <Representation id="same" bandwidth="96000"/>
 </AdaptationSet></Period></MPD>"#,
        );
        let audit = audit(&path, DashProfile::DashIfIop).unwrap();
        assert!(!audit.passed);
        assert!(audit
            .findings
            .iter()
            .any(|item| { item.rule_id == "FORGE-DASH-REPRESENTATION-ID" && !item.passed }));
        assert!(audit
            .findings
            .iter()
            .any(|item| item.rule_id == "FORGE-DASH-TIMELINE-ENTRY" && !item.passed));
    }

    #[test]
    fn audits_valid_dynamic_low_latency_mpd() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_mpd(
            directory.path(),
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
 xmlns:cenc="urn:mpeg:cenc:2013"
 type="dynamic" profiles="urn:mpeg:dash:profile:isoff-live:2011,http://dashif.org/guidelines/dash-if-uhd#hevc"
 availabilityStartTime="2026-07-29T00:00:00Z"
 publishTime="2026-07-29T00:00:20Z"
 minimumUpdatePeriod="PT2S" minBufferTime="PT1S"
 timeShiftBufferDepth="PT30S" suggestedPresentationDelay="PT3S"
 maxSegmentDuration="PT2S">
 <UTCTiming schemeIdUri="urn:mpeg:dash:utc:direct:2014"
  value="2026-07-29T00:00:20Z"/>
 <ServiceDescription id="0">
  <Latency target="4000" min="2000" max="6000" referenceId="0"/>
  <PlaybackRate min="0.96" max="1.04"/>
 </ServiceDescription>
 <BaseURL availabilityTimeOffset="0.25">https://example.invalid/live/</BaseURL>
 <Period id="p0" start="PT0S" duration="PT10S">
  <EventStream schemeIdUri="urn:example:event" value="live" timescale="1000">
   <Event id="1" presentationTime="1000" duration="500"/>
   <Event id="2" presentationTime="2000" duration="500"/>
  </EventStream>
  <AdaptationSet id="1" contentType="audio" mimeType="audio/mp4"
   codecs="mp4a.40.2" lang="en" audioSamplingRate="48000">
   <AudioChannelConfiguration
    schemeIdUri="urn:mpeg:dash:23003:3:audio_channel_configuration:2011" value="2"/>
   <ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011"
    value="cbcs" cenc:default_KID="34e5db32-8625-47cd-ba06-68fca0655a72"/>
   <ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed">
    <cenc:pssh>AAAAIHBzc2gAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=</cenc:pssh>
   </ContentProtection>
   <ProducerReferenceTime id="0" type="encoder" inband="true"
    wallClockTime="2026-07-29T00:00:00Z" presentationTime="0"/>
   <SegmentTemplate timescale="48000" duration="96000" startNumber="1"
    availabilityTimeOffset="1.25" availabilityTimeComplete="false"
    initialization="init-$RepresentationID$.mp4"
    media="$RepresentationID$-$Number$.m4s"/>
   <Representation id="a1" bandwidth="128000"/>
  </AdaptationSet>
 </Period>
</MPD>"#,
        );
        let audit = audit(&path, DashProfile::DashLive).unwrap();
        assert!(
            audit.passed,
            "{:#?}",
            audit
                .findings
                .iter()
                .filter(|finding| finding.severity == Severity::Error && !finding.passed)
                .map(|finding| (&finding.rule_id, &finding.message))
                .collect::<Vec<_>>()
        );
        assert_eq!(audit.profile, DashProfile::DashLive);
        assert_eq!(audit.properties["utc_timing_count"], 1);
        assert_eq!(audit.properties["event_stream_count"], 1);
        assert!(audit.findings.iter().any(|finding| {
            finding.rule_id == "FORGE-DASH-LL-LATENCY-GEOMETRY" && finding.passed
        }));
    }

    #[test]
    fn rejects_invalid_dynamic_timing_events_protection_and_low_latency() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_mpd(
            directory.path(),
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
 xmlns:cenc="urn:mpeg:cenc:2013" type="dynamic"
 availabilityStartTime="2026-07-29T00:00:00"
 minBufferTime="PT1S" timeShiftBufferDepth="PT2S"
 suggestedPresentationDelay="PT3S">
 <ServiceDescription id="0">
  <Latency target="1000" min="2000" max="500" referenceId="99"/>
 </ServiceDescription>
 <BaseURL>https://example.invalid/live/</BaseURL>
 <Period id="p0" start="PT0S" duration="PT10S">
  <EventStream schemeIdUri="urn:example:event" timescale="1000">
   <Event id="1" presentationTime="10"/>
   <Event id="1" presentationTime="5"/>
  </EventStream>
  <AdaptationSet id="1" contentType="audio" mimeType="audio/mp4" codecs="opus"
   lang="en" audioSamplingRate="48000">
   <ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011"
    value="bogus" cenc:default_KID="bad"><cenc:pssh>AAAA</cenc:pssh></ContentProtection>
   <ProducerReferenceTime id="7" type="bogus" inband="false"
    wallClockTime="not-a-date"/>
   <ProducerReferenceTime id="7" type="encoder" inband="true"/>
   <SegmentTemplate timescale="48000" duration="96000"
    availabilityTimeOffset="0" availabilityTimeComplete="false"
    initialization="init-$RepresentationID$.mp4"
    media="$RepresentationID$-$Number$.m4s"/>
   <Representation id="a1" bandwidth="64000"/>
  </AdaptationSet>
 </Period>
 <Period id="p1" start="PT5S" duration="PT10S">
  <AdaptationSet id="1" contentType="audio" mimeType="audio/mp4" codecs="opus"
   lang="en" audioSamplingRate="48000">
   <SupplementalProperty schemeIdUri="urn:mpeg:dash:period-continuity:2015"
    value="p0"/>
   <SegmentTemplate timescale="48000" duration="96000"
    initialization="init-$RepresentationID$.mp4"
    media="$RepresentationID$-$Number$.m4s"/>
   <Representation id="a2" bandwidth="64000"/>
  </AdaptationSet>
 </Period>
</MPD>"#,
        );
        let audit = audit(&path, DashProfile::DashLive).unwrap();
        assert!(!audit.passed);
        for rule in [
            "FORGE-DASH-LIVE-AVAILABILITY-ANCHOR",
            "FORGE-DASH-LIVE-UPDATE-PERIOD",
            "FORGE-DASH-LIVE-UTC-TIMING",
            "FORGE-DASHIF-PRESENTATION-DELAY",
            "FORGE-DASH-LIVE-SERVICE-DESCRIPTION",
            "FORGE-DASH-LIVE-PRODUCER-REFERENCE-TIME",
            "FORGE-DASH-EVENT-ORDER",
            "FORGE-DASH-EVENT-ID",
            "FORGE-DASH-CENC-DEFAULT-KID",
            "FORGE-DASH-CENC-PSSH",
            "FORGE-DASH-PERIOD-CONTINUITY",
            "FORGE-DASH-LL-AVAILABILITY",
            "FORGE-DASH-LL-LATENCY-GEOMETRY",
        ] {
            assert!(
                audit
                    .findings
                    .iter()
                    .any(|finding| finding.rule_id == rule && !finding.passed),
                "missing failed rule {rule}"
            );
        }
    }

    #[test]
    fn resolves_implicit_period_duration_for_continuity() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_mpd(
            directory.path(),
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic"
 availabilityStartTime="2026-07-29T00:00:00Z"
 publishTime="2026-07-29T00:00:20Z"
 minimumUpdatePeriod="PT2S" minBufferTime="PT1S">
 <UTCTiming schemeIdUri="urn:mpeg:dash:utc:direct:2014"
  value="2026-07-29T00:00:20Z"/>
 <BaseURL>https://example.invalid/</BaseURL>
 <Period id="p0" start="PT0S">
  <AdaptationSet id="1" contentType="audio" mimeType="audio/mp4"
   codecs="opus" lang="en" audioSamplingRate="48000">
   <SegmentTemplate timescale="48000" duration="96000"
    initialization="init-$RepresentationID$.mp4"
    media="$RepresentationID$-$Number$.m4s"/>
   <Representation id="a0" bandwidth="64000"/>
  </AdaptationSet>
 </Period>
 <Period id="p1" start="PT10S" duration="PT10S">
  <AdaptationSet id="1" contentType="audio" mimeType="audio/mp4"
   codecs="opus" lang="en" audioSamplingRate="48000">
   <SupplementalProperty schemeIdUri="urn:mpeg:dash:period-continuity:2015"
    value="p0"/>
   <SegmentTemplate timescale="48000" duration="96000"
    initialization="init-$RepresentationID$.mp4"
    media="$RepresentationID$-$Number$.m4s"/>
   <Representation id="a1" bandwidth="64000"/>
  </AdaptationSet>
 </Period>
</MPD>"#,
        );
        let audit = audit(&path, DashProfile::DashLive).unwrap();
        assert!(audit
            .findings
            .iter()
            .filter(|finding| finding.rule_id == "FORGE-DASH-PERIOD-CONTINUITY")
            .all(|finding| finding.passed));
    }

    #[test]
    fn parses_iso_durations_and_expands_negative_repeat() {
        assert_eq!(parse_duration("P1DT2H3M4.5S").unwrap(), 93_784.5);
        let template = SegmentTemplate {
            timescale: Some(10),
            timeline: vec![
                TimelineEntry {
                    time: Some(0),
                    duration: 10,
                    repeat: -1,
                },
                TimelineEntry {
                    time: Some(30),
                    duration: 10,
                    repeat: 0,
                },
            ],
            ..SegmentTemplate::default()
        };
        assert_eq!(
            expand_timeline(&template, None).unwrap(),
            vec![0, 10, 20, 30]
        );
        let list = SegmentList {
            base: SegmentBase {
                timescale: Some(10),
                ..SegmentBase::default()
            },
            duration: Some(10),
            segment_urls: vec![SegmentUrl::default(), SegmentUrl::default()],
            ..SegmentList::default()
        };
        assert_eq!(expand_segment_list(&list, None).unwrap(), vec![0, 10]);
        assert!(expand_timeline(
            &SegmentTemplate {
                timeline: vec![TimelineEntry {
                    time: None,
                    duration: 0,
                    repeat: -1,
                }],
                ..SegmentTemplate::default()
            },
            Some(1.0),
        )
        .is_err());
        assert_eq!(
            substitute_template(
                "$RepresentationID$-$Number%05d$-$Time$-$$.m4s",
                "audio",
                Some(96_000),
                12,
                24
            ),
            "audio-00012-24-$.m4s"
        );
        assert!(looks_like_xs_datetime("2026-07-27T10:15:30Z"));
        assert!(looks_like_xs_datetime("2024-02-29T23:59:59.123+09:00"));
        assert!(!looks_like_xs_datetime("2025-02-29T10:15:30Z"));
        assert!(!looks_like_xs_datetime("2026-07-27T25:15:30Z"));
        assert!(!looks_like_xs_datetime("2026-07-27T10:15:30+14:01"));
        assert!(!looks_like_xs_datetime("2026-07-27"));
        assert_eq!(
            parse_xs_datetime_seconds("2026-07-29T09:00:00+09:00"),
            parse_xs_datetime_seconds("2026-07-29T00:00:00Z")
        );
        assert!(
            parse_xs_datetime_seconds("2026-07-29T00:00:00.5Z")
                > parse_xs_datetime_seconds("2026-07-29T00:00:00Z")
        );
        assert!(common_order_is_stable(
            &["old".into(), "keep".into(), "tail".into()],
            &["keep".into(), "tail".into(), "new".into()]
        ));
        assert!(!common_order_is_stable(
            &["first".into(), "second".into()],
            &["second".into(), "first".into()]
        ));
        assert!(overlapping_valued_segments_match(
            &[(10, 10, "old")],
            &[(20, 10, "new")]
        ));
        assert!(!overlapping_valued_segments_match(
            &[(10, 10, "old")],
            &[(11, 10, "changed-boundary")]
        ));
        assert!(!overlapping_valued_segments_match(
            &[(10, 10, "old")],
            &[(0, 10, "regressive")]
        ));
    }

    #[test]
    fn audits_segment_list_timing_and_remote_resources() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_mpd(
            directory.path(),
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static"
 profiles="urn:mpeg:dash:profile:isoff-live:2011"
 mediaPresentationDuration="PT4S" minBufferTime="PT1S">
 <BaseURL>https://example.invalid/audio/</BaseURL>
 <Period><AdaptationSet id="1" contentType="audio" mimeType="audio/mp4"
  codecs="mp4a.40.2" lang="en" audioSamplingRate="48000">
  <SegmentList timescale="48000" duration="96000">
   <Initialization sourceURL="init.mp4"/>
   <SegmentURL media="one.m4s"/>
   <SegmentURL media="two.m4s"/>
  </SegmentList>
  <Representation id="audio" bandwidth="128000"/>
 </AdaptationSet></Period>
</MPD>"#,
        );
        let audit = audit(&path, DashProfile::DashIfIop).unwrap();
        assert!(
            audit.passed,
            "{:#?}",
            audit
                .findings
                .iter()
                .filter(|finding| finding.severity == Severity::Error && !finding.passed)
                .map(|finding| (&finding.rule_id, &finding.message))
                .collect::<Vec<_>>()
        );
        assert!(audit.findings.iter().any(|finding| {
            finding.rule_id == "FORGE-DASH-SEGMENT-LIST-COUNT" && finding.passed
        }));
    }

    #[test]
    fn audits_local_segment_base_sidx_and_ranges() {
        let directory = tempfile::tempdir().unwrap();
        let mut media = vec![0_u8; 8];
        media.extend_from_slice(&sidx_box(48_000, 96_000));
        media.extend_from_slice(&[0_u8; 100]);
        fs::write(directory.path().join("audio.mp4"), media).unwrap();
        let path = write_mpd(
            directory.path(),
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static"
 profiles="urn:mpeg:dash:profile:isoff-on-demand:2011"
 mediaPresentationDuration="PT2S" minBufferTime="PT1S">
 <Period><AdaptationSet id="1" contentType="audio" mimeType="audio/mp4"
  codecs="mp4a.40.2" lang="en" audioSamplingRate="48000">
  <Representation id="audio" bandwidth="128000">
   <BaseURL>audio.mp4</BaseURL>
   <SegmentBase timescale="48000" indexRange="8-51" indexRangeExact="true">
    <Initialization range="0-7"/>
   </SegmentBase>
  </Representation>
 </AdaptationSet></Period>
</MPD>"#,
        );
        let audit = audit(&path, DashProfile::DashIfIop).unwrap();
        assert!(
            audit.passed,
            "{:#?}",
            audit
                .findings
                .iter()
                .filter(|finding| finding.severity == Severity::Error && !finding.passed)
                .map(|finding| (&finding.rule_id, &finding.message))
                .collect::<Vec<_>>()
        );
        assert!(audit
            .findings
            .iter()
            .any(|finding| finding.rule_id == "FORGE-DASH-SIDX" && finding.passed));
        let info = parse_sidx(
            &directory.path().join("audio.mp4"),
            ByteRange { start: 8, end: 51 },
        )
        .unwrap();
        assert_eq!(info.timescale, 48_000);
        assert_eq!(info.maximum_subsegment_duration, 96_000);
    }

    #[test]
    fn rejects_mixed_addressing_and_segment_list_count_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_mpd(
            directory.path(),
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static"
 mediaPresentationDuration="PT4S" minBufferTime="PT1S">
 <BaseURL>https://example.invalid/</BaseURL>
 <Period><AdaptationSet id="1" contentType="audio" mimeType="audio/mp4"
  codecs="opus" lang="en" audioSamplingRate="48000">
  <SegmentTemplate timescale="48000" duration="96000"
   initialization="init-$RepresentationID$.mp4"
   media="$RepresentationID$-$Number$.m4s"/>
  <Representation id="template" bandwidth="64000"/>
  <Representation id="list" bandwidth="64000">
   <SegmentList timescale="48000" duration="96000">
    <SegmentURL media="only-one.m4s"/>
   </SegmentList>
  </Representation>
 </AdaptationSet></Period>
</MPD>"#,
        );
        let audit = audit(&path, DashProfile::DashIfIop).unwrap();
        assert!(!audit.passed);
        for rule in [
            "FORGE-DASHIF-SEGMENT-ADDRESSING",
            "FORGE-DASH-SEGMENT-LIST-COUNT",
        ] {
            assert!(audit
                .findings
                .iter()
                .any(|finding| finding.rule_id == rule && !finding.passed));
        }
    }

    #[test]
    fn audits_successive_dynamic_mpd_snapshots() {
        let directory = tempfile::tempdir().unwrap();
        let previous = directory.path().join("previous.mpd");
        let current = directory.path().join("current.mpd");
        fs::write(
            &previous,
            update_mpd("2026-07-29T00:00:20Z", r#"<S t="0" d="10" r="2"/>"#, "a1"),
        )
        .unwrap();
        fs::write(
            &current,
            update_mpd(
                "2026-07-29T09:00:21+09:00",
                r#"<S t="10" d="10" r="2"/>"#,
                "a1",
            ),
        )
        .unwrap();
        let audit = audit_with_previous(&current, &previous, DashProfile::Iso23009).unwrap();
        assert!(
            audit.passed,
            "{:#?}",
            audit
                .findings
                .iter()
                .filter(|finding| finding.severity == Severity::Error && !finding.passed)
                .map(|finding| (&finding.rule_id, &finding.message))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            audit.properties["previous_path"],
            previous.to_string_lossy().as_ref()
        );
        assert!(audit.findings.iter().any(|finding| {
            finding.rule_id == "FORGE-DASH-UPDATE-SEGMENT-EQUIVALENCE" && finding.passed
        }));
    }

    #[test]
    fn rejects_regressive_or_functionally_changed_mpd_update() {
        let directory = tempfile::tempdir().unwrap();
        let previous = directory.path().join("previous.mpd");
        let current = directory.path().join("current.mpd");
        fs::write(
            &previous,
            update_mpd("2026-07-29T00:00:20Z", r#"<S t="0" d="10" r="2"/>"#, "a1"),
        )
        .unwrap();
        fs::write(
            &current,
            update_mpd(
                "2026-07-29T00:00:19Z",
                r#"<S t="10" d="11" r="2"/>"#,
                "replacement",
            ),
        )
        .unwrap();
        let audit = audit_with_previous(&current, &previous, DashProfile::Iso23009).unwrap();
        assert!(!audit.passed);
        for rule in [
            "FORGE-DASH-UPDATE-PUBLISH-TIME",
            "FORGE-DASH-UPDATE-REPRESENTATION-SET",
        ] {
            assert!(audit
                .findings
                .iter()
                .any(|finding| finding.rule_id == rule && !finding.passed));
        }
        fs::write(
            &current,
            update_mpd("2026-07-29T00:00:21Z", r#"<S t="10" d="11" r="2"/>"#, "a1"),
        )
        .unwrap();
        let audit = audit_with_previous(&current, &previous, DashProfile::Iso23009).unwrap();
        assert!(audit.findings.iter().any(|finding| {
            finding.rule_id == "FORGE-DASH-UPDATE-SEGMENT-EQUIVALENCE" && !finding.passed
        }));
    }
}
