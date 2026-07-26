//! ISO/IEC 23009-1 DASH MPD validation with bounded local CMAF checks.

use crate::container_qc;
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub const DASH_QC_SCHEMA: &str = "https://penguin425.github.io/audio-normalizer/schema/dash-qc-v1";
const MAX_MPD_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ELEMENTS: usize = 200_000;
const MAX_LOCAL_SEGMENTS: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DashProfile {
    Iso23009,
    DashIfIop,
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
    timeline: Vec<TimelineEntry>,
}

#[derive(Clone, Copy)]
struct TimelineEntry {
    time: Option<u64>,
    duration: u64,
    repeat: i64,
}

#[derive(Default)]
struct Representation {
    id: Option<String>,
    base_url: Option<String>,
    bandwidth: Option<u64>,
    mime_type: Option<String>,
    codecs: Option<String>,
    audio_sampling_rate: Option<u64>,
    audio_channel_configuration: Option<(String, String)>,
    template: Option<SegmentTemplate>,
}

#[derive(Default)]
struct AdaptationSet {
    id: Option<String>,
    base_url: Option<String>,
    content_type: Option<String>,
    mime_type: Option<String>,
    codecs: Option<String>,
    lang: Option<String>,
    audio_sampling_rate: Option<u64>,
    audio_channel_configuration: Option<(String, String)>,
    template: Option<SegmentTemplate>,
    representations: Vec<Representation>,
}

#[derive(Default)]
struct Period {
    id: Option<String>,
    base_url: Option<String>,
    start: Option<f64>,
    duration: Option<f64>,
    template: Option<SegmentTemplate>,
    adaptations: Vec<AdaptationSet>,
}

#[derive(Default)]
struct Mpd {
    root_count: usize,
    namespace: Option<String>,
    base_url: Option<String>,
    profiles: Vec<String>,
    kind: String,
    availability_start_time: Option<String>,
    media_presentation_duration: Option<f64>,
    min_buffer_time: Option<f64>,
    minimum_update_period: Option<f64>,
    periods: Vec<Period>,
    element_count: usize,
}

pub fn audit(path: &Path, profile: DashProfile) -> Result<DashAudit, String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if metadata.len() > MAX_MPD_BYTES {
        return Err(format!(
            "{} exceeds the {} byte MPD safety limit",
            path.display(),
            MAX_MPD_BYTES
        ));
    }
    let xml = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let mpd = parse_mpd(&xml)?;
    let mut findings = Vec::new();
    validate_mpd(path, &mpd, profile, &mut findings);
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
            "namespace": mpd.namespace,
            "base_url": mpd.base_url,
            "profiles": mpd.profiles,
            "availability_start_time": mpd.availability_start_time,
            "media_presentation_duration_seconds": mpd.media_presentation_duration,
            "minimum_buffer_time_seconds": mpd.min_buffer_time,
            "minimum_update_period_seconds": mpd.minimum_update_period,
            "period_count": mpd.periods.len(),
            "adaptation_set_count": adaptation_count,
            "representation_count": representation_count,
            "element_count": mpd.element_count,
        }),
    })
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
            Ok(Event::Text(text)) if stack.last().map(String::as_str) == Some("BaseURL") => {
                let value = String::from_utf8_lossy(text.as_ref()).trim().to_owned();
                if !value.is_empty() {
                    set_base_url(
                        &mut mpd,
                        active_period,
                        active_adaptation,
                        active_representation,
                        value,
                    )?;
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
            current_template_mut(
                mpd,
                *active_period,
                *active_adaptation,
                *active_representation,
            )?
            .timeline
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
    if profile == DashProfile::DashIfIop {
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
    let period_start = period.start.unwrap_or(0.0);
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
        if profile == DashProfile::DashIfIop && adaptation_audio {
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
        for (representation_index, representation) in adaptation.representations.iter().enumerate()
        {
            let label = format!(
                "Period {period_index} AdaptationSet {adaptation_index} Representation {representation_index}"
            );
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
            let template = resolve_template(
                representation.template.as_ref(),
                adaptation.template.as_ref(),
                period.template.as_ref(),
            );
            match template {
                Some(template) => {
                    let base_url = resolved_base_url(mpd, period, adaptation, representation);
                    validate_template(&label, &template, period_duration, findings);
                    if let Ok(timeline) = expand_timeline(&template, period_duration) {
                        timelines.push(timeline);
                    }
                    audit_local_resources(
                        path,
                        representation,
                        &template,
                        base_url.as_deref(),
                        period_duration,
                        findings,
                    );
                }
                None => findings.push(finding(
                    "FORGE-DASH-SEGMENT-ADDRESSING",
                    Severity::Warning,
                    false,
                    format!(
                        "{label} does not use SegmentTemplate; local segments were not expanded"
                    ),
                    None,
                )),
            }
        }
        if profile == DashProfile::DashIfIop && timelines.len() > 1 {
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
    let has_addressing = template.duration.is_some() || !template.timeline.is_empty();
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

fn audit_local_resources(
    mpd_path: &Path,
    representation: &Representation,
    template: &SegmentTemplate,
    base_url: Option<&str>,
    period_duration: Option<f64>,
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
    if !template.timeline.is_empty() {
        let mut result = Vec::new();
        let mut current = 0_u64;
        for (entry_index, entry) in template.timeline.iter().enumerate() {
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
                result.push(current);
                current = current.saturating_add(entry.duration);
            }
        }
        return Ok(result);
    }
    let Some(duration) = template.duration else {
        return Err("SegmentTemplate has neither duration nor timeline".into());
    };
    let Some(period_duration) = period_duration else {
        return Err("duration-addressed SegmentTemplate has no bounded Period duration".into());
    };
    let count = ((period_duration * effective_timescale(template) as f64) / duration as f64).ceil()
        as usize;
    Ok((0..count.min(MAX_LOCAL_SEGMENTS))
        .map(|index| index as u64 * duration)
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
        if !layer.timeline.is_empty() {
            resolved.timeline.clone_from(&layer.timeline);
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
    value: String,
) -> Result<(), String> {
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
                representation.base_url.get_or_insert(value);
            } else {
                adaptation.base_url.get_or_insert(value);
            }
        } else {
            period.base_url.get_or_insert(value);
        }
    } else {
        mpd.base_url.get_or_insert(value);
    }
    Ok(())
}

fn resolved_base_url(
    mpd: &Mpd,
    period: &Period,
    adaptation: &AdaptationSet,
    representation: &Representation,
) -> Option<String> {
    let mut result = None;
    for layer in [
        mpd.base_url.as_deref(),
        period.base_url.as_deref(),
        adaptation.base_url.as_deref(),
        representation.base_url.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        result = Some(apply_base_url(result.as_deref(), layer));
    }
    result
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
    let Some((date, time)) = value.split_once('T') else {
        return false;
    };
    let date_parts = date.split('-').collect::<Vec<_>>();
    if date_parts.len() != 3
        || date_parts[0].len() != 4
        || date_parts[1].len() != 2
        || date_parts[2].len() != 2
        || date_parts
            .iter()
            .any(|part| !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return false;
    }
    let time = time
        .strip_suffix('Z')
        .or_else(|| time.rsplit_once(['+', '-']).map(|(head, _)| head))
        .unwrap_or(time);
    let fields = time.split(':').collect::<Vec<_>>();
    fields.len() == 3
        && fields[0].len() == 2
        && fields[1].len() == 2
        && fields[0].bytes().all(|byte| byte.is_ascii_digit())
        && fields[1].bytes().all(|byte| byte.is_ascii_digit())
        && fields[2]
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
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
            .decode_and_unescape_value(reader.decoder())
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
                .decode_and_unescape_value(reader.decoder())
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
        assert!(!looks_like_xs_datetime("2026-07-27"));
    }
}
