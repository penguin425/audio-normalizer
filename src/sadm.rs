//! ITU-R BS.2125-1 S-ADM frame and flow validation.

use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const STANDARD: &str = "ITU-R BS.2125-1";
pub const VERSION: &str = "05/2022";
pub const VALIDATOR: &str = "forge-bs2125-1-flow-1";

#[derive(Debug, Serialize)]
pub struct SadmAudit {
    pub standard: &'static str,
    pub standard_version: &'static str,
    pub validator: &'static str,
    pub frame_count: usize,
    pub flow_id: Option<String>,
    pub time_reference: String,
    pub passed: bool,
    pub flow_rules: Vec<SadmRule>,
    pub frames: Vec<SadmFrameAudit>,
}

#[derive(Debug, Serialize)]
pub struct SadmFrameAudit {
    pub index: usize,
    pub path: String,
    pub frame_format_id: Option<String>,
    pub frame_type: Option<String>,
    pub start: Option<String>,
    pub duration: Option<String>,
    pub passed: bool,
    pub rules: Vec<SadmRule>,
}

#[derive(Debug, Serialize)]
pub struct SadmRule {
    pub rule_id: &'static str,
    pub path: String,
    pub requirement: String,
    pub observed: String,
    pub passed: bool,
}

#[derive(Debug, Default)]
struct ParsedFrame {
    roots: usize,
    frame_headers: usize,
    frame_formats: usize,
    transport_track_formats: usize,
    audio_format_extended: usize,
    attributes: HashMap<String, String>,
    changed_statuses: Vec<String>,
}

pub fn audit(paths: &[PathBuf]) -> Result<SadmAudit, String> {
    if paths.is_empty() {
        return Err("at least one S-ADM frame XML file is required".into());
    }
    let mut frames = Vec::with_capacity(paths.len());
    let mut parsed_frames = Vec::with_capacity(paths.len());
    for (offset, path) in paths.iter().enumerate() {
        let xml = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
        let parsed =
            parse_frame(&xml).map_err(|error| format!("parse {}: {error}", path.display()))?;
        let frame = validate_frame(offset + 1, path, &parsed);
        frames.push(frame);
        parsed_frames.push(parsed);
    }

    let mut flow_rules = Vec::new();
    let first_type = attribute(&parsed_frames[0], "type");
    flow_rules.push(rule(
        "BS2125-FIRST-FRAME",
        "/flow/frame[1]/frameHeader/frameFormat/@type",
        "the first frame type shall be header, full, or all",
        first_type.unwrap_or("missing"),
        matches!(first_type, Some("header" | "full" | "all")),
    ));

    let flow_id = attribute(&parsed_frames[0], "flowID").map(str::to_owned);
    let flow_ids_fixed = parsed_frames
        .iter()
        .all(|frame| attribute(frame, "flowID").map(str::to_owned) == flow_id);
    flow_rules.push(rule(
        "BS2125-FLOW-ID-FIXED",
        "/flow/frame/frameHeader/frameFormat/@flowID",
        "flowID, when present, shall be a fixed RFC 4122 UUID for the flow",
        flow_id.as_deref().unwrap_or("not present"),
        flow_ids_fixed && flow_id.as_deref().is_none_or(valid_uuid),
    ));

    let time_reference = attribute(&parsed_frames[0], "timeReference").unwrap_or("total");
    let time_reference_fixed = parsed_frames
        .iter()
        .all(|frame| attribute(frame, "timeReference").unwrap_or("total") == time_reference);
    flow_rules.push(rule(
        "BS2125-TIME-REFERENCE-FIXED",
        "/flow/frame/frameHeader/frameFormat/@timeReference",
        "timeReference shall be total or local and fixed for the entire flow",
        time_reference,
        time_reference_fixed && matches!(time_reference, "total" | "local"),
    ));

    let indices = parsed_frames
        .iter()
        .map(|frame| attribute(frame, "frameFormatID").and_then(frame_index))
        .collect::<Vec<_>>();
    let sequential = indices
        .iter()
        .enumerate()
        .all(|(offset, value)| *value == Some((offset + 1) as u64));
    flow_rules.push(rule(
        "BS2125-FRAME-SEQUENCE",
        "/flow/frame/frameHeader/frameFormat/@frameFormatID",
        "the hexadecimal frame index shall start at 1 and increment by 1",
        indices
            .iter()
            .map(|value| value.map_or_else(|| "invalid".into(), |value| value.to_string()))
            .collect::<Vec<_>>()
            .join(", "),
        sequential,
    ));

    let times = parsed_frames
        .iter()
        .map(|frame| {
            attribute(frame, "start")
                .and_then(parse_time)
                .zip(attribute(frame, "duration").and_then(parse_time))
        })
        .collect::<Vec<_>>();
    let contiguous = times.windows(2).all(|pair| match (pair[0], pair[1]) {
        (Some((start, duration)), Some((next, _))) => (start + duration - next).abs() <= 1e-9,
        _ => false,
    });
    flow_rules.push(rule(
        "BS2125-FRAME-CONTIGUITY",
        "/flow/frame/frameHeader/frameFormat",
        "S-ADM frames shall be non-overlapping and contiguous",
        times
            .iter()
            .map(|value| {
                value.map_or_else(
                    || "invalid".into(),
                    |(start, duration)| format!("{start:.9}+{duration:.9}"),
                )
            })
            .collect::<Vec<_>>()
            .join(", "),
        times.iter().all(Option::is_some) && contiguous,
    ));

    let passed =
        frames.iter().all(|frame| frame.passed) && flow_rules.iter().all(|item| item.passed);
    Ok(SadmAudit {
        standard: STANDARD,
        standard_version: VERSION,
        validator: VALIDATOR,
        frame_count: frames.len(),
        flow_id,
        time_reference: time_reference.into(),
        passed,
        flow_rules,
        frames,
    })
}

fn validate_frame(index: usize, path: &Path, parsed: &ParsedFrame) -> SadmFrameAudit {
    let mut rules = vec![
        rule(
            "BS2125-FRAME-ROOT",
            "/frame",
            "exactly one frame root element",
            format!("{} frame element(s)", parsed.roots),
            parsed.roots == 1,
        ),
        rule(
            "BS2125-FRAME-HEADER",
            "/frame/frameHeader",
            "exactly one frameHeader",
            format!("{} frameHeader element(s)", parsed.frame_headers),
            parsed.frame_headers == 1,
        ),
        rule(
            "BS2125-FRAME-FORMAT",
            "/frame/frameHeader/frameFormat",
            "exactly one frameFormat",
            format!("{} frameFormat element(s)", parsed.frame_formats),
            parsed.frame_formats == 1,
        ),
        rule(
            "BS2125-TRANSPORT-TRACK-FORMAT",
            "/frame/frameHeader/transportTrackFormat",
            "one or more transportTrackFormat elements",
            format!(
                "{} transportTrackFormat element(s)",
                parsed.transport_track_formats
            ),
            parsed.transport_track_formats >= 1,
        ),
        rule(
            "BS2125-AUDIO-FORMAT-EXTENDED",
            "/frame/audioFormatExtended",
            "exactly one audioFormatExtended payload",
            format!(
                "{} audioFormatExtended element(s)",
                parsed.audio_format_extended
            ),
            parsed.audio_format_extended == 1,
        ),
    ];
    let id = attribute(parsed, "frameFormatID");
    rules.push(rule(
        "BS2125-FRAME-FORMAT-ID",
        "/frame/frameHeader/frameFormat/@frameFormatID",
        "FF_ plus an 8-digit (or legacy 11-digit) hexadecimal index and optional divided-frame chunk",
        id.unwrap_or("missing"),
        id.is_some_and(|value| frame_index(value).is_some() && valid_frame_id(value)),
    ));
    let frame_type = attribute(parsed, "type");
    rules.push(rule(
        "BS2125-FRAME-TYPE",
        "/frame/frameHeader/frameFormat/@type",
        "header, full, divided, intermediate, or all",
        frame_type.unwrap_or("missing"),
        matches!(
            frame_type,
            Some("header" | "full" | "divided" | "intermediate" | "all")
        ),
    ));
    for name in ["start", "duration"] {
        let value = attribute(parsed, name);
        let parsed_time = value.and_then(parse_time);
        rules.push(rule(
            if name == "start" {
                "BS2125-FRAME-START"
            } else {
                "BS2125-FRAME-DURATION"
            },
            format!("/frame/frameHeader/frameFormat/@{name}"),
            if name == "start" {
                "a valid BS.2125 time value"
            } else {
                "a positive BS.2125 time value"
            },
            value.unwrap_or("missing"),
            parsed_time.is_some_and(|seconds| name == "start" || seconds > 0.0),
        ));
    }
    let invalid_statuses = parsed
        .changed_statuses
        .iter()
        .filter(|status| !matches!(status.as_str(), "new" | "changed" | "expired" | "extended"))
        .cloned()
        .collect::<Vec<_>>();
    rules.push(rule(
        "BS2125-CHANGED-IDS-STATUS",
        "/frame/frameHeader/frameFormat/changedIDs/*/@status",
        "changed ID status shall be new, changed, expired, or extended",
        if invalid_statuses.is_empty() {
            "all statuses valid"
        } else {
            "invalid status present"
        },
        invalid_statuses.is_empty(),
    ));
    SadmFrameAudit {
        index,
        path: path.to_string_lossy().into_owned(),
        frame_format_id: id.map(str::to_owned),
        frame_type: frame_type.map(str::to_owned),
        start: attribute(parsed, "start").map(str::to_owned),
        duration: attribute(parsed, "duration").map(str::to_owned),
        passed: rules.iter().all(|item| item.passed),
        rules,
    }
}

fn parse_frame(xml: &[u8]) -> Result<ParsedFrame, String> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut parsed = ParsedFrame::default();
    let mut depth = 0_usize;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                observe_element(&element, depth, &mut parsed)?;
                depth += 1;
            }
            Ok(Event::Empty(element)) => {
                observe_element(&element, depth, &mut parsed)?;
            }
            Ok(Event::End(_)) => depth = depth.saturating_sub(1),
            Ok(Event::Eof) => break,
            Err(error) => return Err(format!("XML: {error}")),
            _ => {}
        }
    }
    Ok(parsed)
}

fn observe_element(
    element: &quick_xml::events::BytesStart<'_>,
    depth: usize,
    parsed: &mut ParsedFrame,
) -> Result<(), String> {
    let name = local_name(element.name().as_ref());
    if name == "frame" && depth == 0 {
        parsed.roots += 1;
    } else if name == "frameHeader" {
        parsed.frame_headers += 1;
    } else if name == "frameFormat" {
        parsed.frame_formats += 1;
        for attribute in element.attributes() {
            let attribute = attribute.map_err(|error| format!("XML attribute: {error}"))?;
            let key = local_name(attribute.key.as_ref());
            let value = attribute
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(|error| format!("XML attribute value: {error}"))?
                .into_owned();
            parsed.attributes.insert(key, value);
        }
    } else if name == "transportTrackFormat" {
        parsed.transport_track_formats += 1;
    } else if name == "audioFormatExtended" {
        parsed.audio_format_extended += 1;
    } else if name.ends_with("IDRef") {
        for attribute in element.attributes() {
            let attribute = attribute.map_err(|error| format!("XML attribute: {error}"))?;
            if local_name(attribute.key.as_ref()) == "status" {
                parsed.changed_statuses.push(
                    attribute
                        .normalized_value(XmlVersion::Implicit1_0)
                        .map_err(|error| format!("XML status: {error}"))?
                        .into_owned(),
                );
            }
        }
    }
    Ok(())
}

fn attribute<'a>(frame: &'a ParsedFrame, name: &str) -> Option<&'a str> {
    frame.attributes.get(name).map(String::as_str)
}

fn rule(
    rule_id: &'static str,
    path: impl Into<String>,
    requirement: impl Into<String>,
    observed: impl Into<String>,
    passed: bool,
) -> SadmRule {
    SadmRule {
        rule_id,
        path: path.into(),
        requirement: requirement.into(),
        observed: observed.into(),
        passed,
    }
}

fn local_name(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_owned()
}

fn valid_frame_id(value: &str) -> bool {
    let Some(value) = value.strip_prefix("FF_") else {
        return false;
    };
    let parts = value.split('_').collect::<Vec<_>>();
    matches!(parts[0].len(), 8 | 11)
        && parts[0].bytes().all(|byte| byte.is_ascii_hexdigit())
        && (parts.len() == 1
            || (parts.len() == 2
                && parts[1].len() == 2
                && parts[1].bytes().all(|byte| byte.is_ascii_hexdigit())))
}

fn frame_index(value: &str) -> Option<u64> {
    let index = value.strip_prefix("FF_")?.split('_').next()?;
    matches!(index.len(), 8 | 11)
        .then(|| u64::from_str_radix(index, 16).ok())
        .flatten()
}

fn valid_uuid(value: &str) -> bool {
    let lengths = [8, 4, 4, 4, 12];
    let parts = value.split('-').collect::<Vec<_>>();
    parts.len() == lengths.len()
        && parts.iter().zip(lengths).all(|(part, length)| {
            part.len() == length && part.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

fn parse_time(value: &str) -> Option<f64> {
    let value = value
        .split_once('T')
        .map(|(_, time)| time.trim_end_matches('Z'))
        .unwrap_or(value);
    if let Some((time, rate)) = value.split_once('S') {
        let rate: f64 = rate.parse().ok()?;
        if rate <= 0.0 {
            return None;
        }
        if time.contains(':') {
            let (whole, samples) = time.rsplit_once('.')?;
            let samples: f64 = samples.parse().ok()?;
            return parse_clock(whole).map(|seconds| seconds + samples / rate);
        }
        return Some(time.parse::<f64>().ok()? / rate);
    }
    if value.contains(':') {
        parse_clock(value)
    } else {
        value.parse().ok()
    }
}

fn parse_clock(value: &str) -> Option<f64> {
    let parts = value.split(':').collect::<Vec<_>>();
    if parts.len() != 3 {
        return None;
    }
    let hours: f64 = parts[0].parse().ok()?;
    let minutes: f64 = parts[1].parse().ok()?;
    let seconds: f64 = parts[2].parse().ok()?;
    (hours >= 0.0 && (0.0..60.0).contains(&minutes) && (0.0..60.0).contains(&seconds))
        .then_some(hours * 3600.0 + minutes * 60.0 + seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_a_contiguous_bs2125_flow() {
        let work = tempfile::tempdir().unwrap();
        let paths = [("frame1.xml", "00000001", "00:00:00.00000", "header"),
            ("frame2.xml", "00000002", "00:00:00.50000", "full")]
            .into_iter()
            .map(|(name, id, start, kind)| {
                let path = work.path().join(name);
                fs::write(
                    &path,
                    format!(
                        r#"<frame><frameHeader><frameFormat frameFormatID="FF_{id}" start="{start}" duration="00:00:00.50000" flowID="12345678-abcd-4000-a000-112233445566" type="{kind}"/><transportTrackFormat/></frameHeader><audioFormatExtended/></frame>"#
                    ),
                )
                .unwrap();
                path
            })
            .collect::<Vec<_>>();
        let audit = audit(&paths).unwrap();
        assert!(audit.passed, "{:#?}", audit.flow_rules);
        assert_eq!(audit.frame_count, 2);
    }

    #[test]
    fn rejects_gaps_and_invalid_changed_id_status() {
        let work = tempfile::tempdir().unwrap();
        let first = work.path().join("first.xml");
        let second = work.path().join("second.xml");
        fs::write(&first, r#"<frame><frameHeader><frameFormat frameFormatID="FF_00000001" start="0S48000" duration="24000S48000" type="header"/><transportTrackFormat/></frameHeader><audioFormatExtended/></frame>"#).unwrap();
        fs::write(&second, r#"<frame><frameHeader><frameFormat frameFormatID="FF_00000003" start="30000S48000" duration="24000S48000" type="intermediate"><changedIDs><audioObjectIDRef status="bad">AO_1001</audioObjectIDRef></changedIDs></frameFormat><transportTrackFormat/></frameHeader><audioFormatExtended/></frame>"#).unwrap();
        let audit = audit(&[first, second]).unwrap();
        assert!(!audit.passed);
    }
}
