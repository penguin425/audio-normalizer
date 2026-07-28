//! ISO/IEC 23009-1 DASH MPD validation with bounded local CMAF checks.

use crate::container_qc;
use base64::Engine as _;
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
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

#[derive(Clone, Copy)]
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
}

#[derive(Default)]
struct AdaptationSet {
    id: Option<String>,
    base_url: Option<BaseUrl>,
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
    adaptations: Vec<AdaptationSet>,
}

#[derive(Default)]
struct Mpd {
    root_count: usize,
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
                resolve_template(
                    representation.template.as_ref(),
                    adaptation.template.as_ref(),
                    period.template.as_ref(),
                )
                .is_some_and(|template| {
                    effective_availability(mpd, period, adaptation, representation, &template).1
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
            let template = resolve_template(
                representation.template.as_ref(),
                adaptation.template.as_ref(),
                period.template.as_ref(),
            );
            match template {
                Some(template) => {
                    let base_url = resolved_base_url(mpd, period, adaptation, representation);
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
        }
    }
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
    offset += template.availability_time_offset.unwrap_or(0.0);
    if template.availability_time_complete.is_some() {
        complete = template.availability_time_complete;
    }
    (offset, complete)
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
    }
}
