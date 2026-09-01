//! Exhaustive, auditable rendering of ADM programme presentations.
//!
//! Forge parses the programme/content/object graph itself, expands every
//! complementary-object choice, and delegates only the normative rendering
//! step to the EBU ADM Renderer (`ear-render`). Every rendered signal is then
//! measured independently with Forge's BS.1770 engine.

use crate::{adm, analysis, decoder, metadata};
use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const SCHEMA_VERSION: u32 = 1;
pub const REPORT_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/adm-presentation-report-v1";
pub const VALIDATOR: &str = "forge-adm-presentation-qc-1";
pub const RENDERER_STANDARD: &str = "ITU-R BS.2127-1";
pub const MEASUREMENT_STANDARD: &str = "ITU-R BS.1770-5";
pub const DEFAULT_MAX_PRESENTATIONS: usize = 256;
pub const HARD_MAX_PRESENTATIONS: usize = 4096;
pub const DEFAULT_MAX_DECODED_SAMPLES: u64 = 500_000_000;
pub const HARD_MAX_DECODED_SAMPLES: u64 = 4_000_000_000;
pub const MAX_TIMEOUT_SECONDS: u64 = 3600;

const TOOL_OUTPUT_LIMIT: usize = 1024 * 1024;
const RENDER_HEADER_ALLOWANCE: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Options {
    pub input: PathBuf,
    pub renderer: PathBuf,
    pub layout: String,
    pub timeout_seconds: u64,
    pub max_presentations: usize,
    pub max_decoded_samples_per_presentation: u64,
    pub loudness_tolerance_lu: f64,
    pub true_peak_tolerance_db: f64,
    pub retained_renders: Option<PathBuf>,
    pub overwrite: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AdmInventory {
    pub programme_count: usize,
    pub content_count: usize,
    pub object_count: usize,
    pub complementary_group_count: usize,
    pub presentation_count: usize,
    pub programmes: Vec<AdmProgramme>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AdmProgramme {
    pub id: String,
    pub name: String,
    pub language: Option<String>,
    pub content_ids: Vec<String>,
    pub referenced_object_ids: Vec<String>,
    pub loudness_metadata: Option<AdmLoudnessMetadata>,
    pub complementary_groups: Vec<ComplementaryGroup>,
    pub presentation_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct AdmLoudnessMetadata {
    pub loudness_method: Option<String>,
    pub integrated_loudness_lufs: Option<f64>,
    pub max_true_peak_dbtp: Option<f64>,
    pub renderer_uri: Option<String>,
    pub renderer_name: Option<String>,
    pub renderer_version: Option<String>,
    pub renderer_audio_object_ids: Vec<String>,
    pub production_profile_passed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComplementaryGroup {
    pub root_object_id: String,
    pub candidates: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Limits {
    pub timeout_seconds_per_presentation: u64,
    pub max_presentations: usize,
    pub max_decoded_samples_per_presentation: u64,
    pub max_render_bytes_per_presentation: u64,
    pub tool_output_bytes_per_stream: usize,
}

#[derive(Debug, Serialize)]
pub struct AdmPresentationReport {
    pub schema: &'static str,
    pub schema_version: u32,
    pub validator: &'static str,
    pub production_profile_standard: &'static str,
    pub renderer_standard: &'static str,
    pub measurement_standard: &'static str,
    pub input_path: String,
    pub input_bytes: u64,
    pub input_sha256: String,
    pub renderer_path: String,
    pub renderer_sha256: String,
    pub output_layout: String,
    pub loudness_tolerance_lu: f64,
    pub true_peak_tolerance_db: f64,
    pub limits: Limits,
    pub production_profile: adm::ProductionProfileResult,
    pub inventory: AdmInventory,
    pub presentation_count: usize,
    pub passed: bool,
    pub presentations: Vec<PresentationResult>,
}

#[derive(Debug, Serialize)]
pub struct PresentationResult {
    pub id: String,
    pub programme_id: String,
    pub programme_name: String,
    pub programme_language: Option<String>,
    pub selected_complementary_object_ids: Vec<String>,
    pub declared_loudness: Option<AdmLoudnessMetadata>,
    pub rendered_sha256: String,
    pub rendered_bytes: u64,
    pub retained_render_path: Option<String>,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub duration_seconds: f64,
    pub measured_integrated_lufs: Option<f64>,
    pub measured_true_peak_dbtp: Option<f64>,
    pub integrated_loudness_drift_lu: Option<f64>,
    pub true_peak_drift_db: Option<f64>,
    pub loudness_metadata_passed: Option<bool>,
    pub true_peak_metadata_passed: Option<bool>,
    pub passed: bool,
}

#[derive(Debug, Default)]
struct ParsedXml {
    nodes: Vec<Node>,
}

#[derive(Debug, Default)]
struct Node {
    name: String,
    parent: Option<usize>,
    attributes: HashMap<String, String>,
    text: String,
}

#[derive(Debug)]
struct Variant {
    programme: usize,
    selected: Vec<String>,
}

pub fn run(options: &Options) -> Result<AdmPresentationReport, String> {
    validate_options(options)?;
    let max_render_bytes = render_byte_limit(options.max_decoded_samples_per_presentation)?;
    let input = fs::canonicalize(&options.input)
        .map_err(|error| format!("resolve ADM input {}: {error}", options.input.display()))?;
    ensure_regular_file(&input, "ADM input")?;
    let renderer = resolve_executable(&options.renderer)?;
    ensure_regular_file(&renderer, "ADM renderer")?;
    let (input_sha256, input_bytes) = sha256_file(&input)?;
    let (renderer_sha256, renderer_bytes) = sha256_file(&renderer)?;
    let axml = metadata::read_wave_chunk(&input, *b"axml")?
        .ok_or_else(|| "ADM presentation QC requires an axml chunk".to_string())?;
    if metadata::read_wave_chunk(&input, *b"chna")?.is_none() {
        return Err("ADM presentation QC requires a chna chunk".into());
    }
    let production_profile =
        adm::validate_production_profile(&input, adm::ProductionProfileMode::Write)?;
    let inventory = inventory_from_axml(&axml, options.max_presentations)?;
    let variants = enumerate_variants(&inventory, options.max_presentations)?;
    let retained_root = prepare_retained_root(options, variants.len())?;

    let work = tempfile::Builder::new()
        .prefix("forge-adm-presentations-")
        .tempdir()
        .map_err(|error| format!("create ADM presentation workspace: {error}"))?;
    let mut presentations = Vec::with_capacity(variants.len());
    for (index, variant) in variants.iter().enumerate() {
        let programme = &inventory.programmes[variant.programme];
        let rendered = work.path().join(format!("render-{:04}.wav", index + 1));
        let mut args = vec![
            OsString::from("-s"),
            OsString::from(&options.layout),
            OsString::from("--programme"),
            OsString::from(&programme.id),
        ];
        for object in &variant.selected {
            args.push(OsString::from("--comp-object"));
            args.push(OsString::from(object));
        }
        args.push(input.as_os_str().to_owned());
        args.push(rendered.as_os_str().to_owned());
        let tool = run_bounded(
            &renderer,
            &args,
            Duration::from_secs(options.timeout_seconds),
            &rendered,
            max_render_bytes,
        )?;
        if !tool.status.success() {
            return Err(format!(
                "ADM renderer failed for {} ({}): {}",
                variant_id(programme, &variant.selected),
                tool.status,
                diagnostic(&tool.stderr)
            ));
        }
        ensure_regular_file(&rendered, "ADM presentation render")?;
        ensure_unchanged(
            &renderer,
            &renderer_sha256,
            renderer_bytes,
            "ADM renderer executable changed while it was running",
        )?;

        let (rendered_sha256, rendered_bytes) = sha256_file(&rendered)?;
        let buffer =
            decoder::decode_limited(&rendered, options.max_decoded_samples_per_presentation)?;
        let measured = analysis::analyze(&buffer);
        ensure_unchanged(
            &rendered,
            &rendered_sha256,
            rendered_bytes,
            "ADM presentation render changed while it was being measured",
        )?;

        let measured_lufs = measured.lufs.is_finite().then_some(measured.lufs);
        let measured_true_peak = measured
            .true_peak_db()
            .is_finite()
            .then_some(measured.true_peak_db());
        let declared = programme.loudness_metadata.clone();
        let loudness_drift = declared
            .as_ref()
            .and_then(|value| value.integrated_loudness_lufs)
            .zip(measured_lufs)
            .map(|(expected, actual)| actual - expected);
        let true_peak_drift = declared
            .as_ref()
            .and_then(|value| value.max_true_peak_dbtp)
            .zip(measured_true_peak)
            .map(|(expected, actual)| actual - expected);
        let loudness_passed = declared
            .as_ref()
            .and_then(|value| value.integrated_loudness_lufs)
            .map(|_| {
                loudness_drift.is_some_and(|drift| drift.abs() <= options.loudness_tolerance_lu)
            });
        let true_peak_passed = declared
            .as_ref()
            .and_then(|value| value.max_true_peak_dbtp)
            .map(|_| {
                true_peak_drift.is_some_and(|drift| drift.abs() <= options.true_peak_tolerance_db)
            });
        let passed = measured_lufs.is_some()
            && measured_true_peak.is_some()
            && declared
                .as_ref()
                .is_none_or(|metadata| metadata.production_profile_passed)
            && loudness_passed != Some(false)
            && true_peak_passed != Some(false);
        let retained_render_path = retained_root
            .as_deref()
            .map(|root| retain_render(root, &rendered, index, options.overwrite))
            .transpose()?
            .map(|path| path.to_string_lossy().into_owned());
        presentations.push(PresentationResult {
            id: variant_id(programme, &variant.selected),
            programme_id: programme.id.clone(),
            programme_name: programme.name.clone(),
            programme_language: programme.language.clone(),
            selected_complementary_object_ids: variant.selected.clone(),
            declared_loudness: declared,
            rendered_sha256,
            rendered_bytes,
            retained_render_path,
            sample_rate_hz: measured.sample_rate,
            channels: measured.channels,
            duration_seconds: measured.duration_secs(),
            measured_integrated_lufs: measured_lufs,
            measured_true_peak_dbtp: measured_true_peak,
            integrated_loudness_drift_lu: loudness_drift,
            true_peak_drift_db: true_peak_drift,
            loudness_metadata_passed: loudness_passed,
            true_peak_metadata_passed: true_peak_passed,
            passed,
        });
    }

    ensure_unchanged(
        &input,
        &input_sha256,
        input_bytes,
        "ADM input changed during presentation QC",
    )?;
    ensure_unchanged(
        &renderer,
        &renderer_sha256,
        renderer_bytes,
        "ADM renderer executable changed during presentation QC",
    )?;
    let passed = production_profile.passed && presentations.iter().all(|item| item.passed);
    Ok(AdmPresentationReport {
        schema: REPORT_SCHEMA,
        schema_version: SCHEMA_VERSION,
        validator: VALIDATOR,
        production_profile_standard: adm::PRODUCTION_PROFILE_STANDARD,
        renderer_standard: RENDERER_STANDARD,
        measurement_standard: MEASUREMENT_STANDARD,
        input_path: input.to_string_lossy().into_owned(),
        input_bytes,
        input_sha256,
        renderer_path: renderer.to_string_lossy().into_owned(),
        renderer_sha256,
        output_layout: options.layout.clone(),
        loudness_tolerance_lu: options.loudness_tolerance_lu,
        true_peak_tolerance_db: options.true_peak_tolerance_db,
        limits: Limits {
            timeout_seconds_per_presentation: options.timeout_seconds,
            max_presentations: options.max_presentations,
            max_decoded_samples_per_presentation: options.max_decoded_samples_per_presentation,
            max_render_bytes_per_presentation: max_render_bytes,
            tool_output_bytes_per_stream: TOOL_OUTPUT_LIMIT,
        },
        production_profile,
        presentation_count: presentations.len(),
        inventory,
        passed,
        presentations,
    })
}

pub fn inventory_from_axml(xml: &[u8], max_presentations: usize) -> Result<AdmInventory, String> {
    if max_presentations == 0 || max_presentations > HARD_MAX_PRESENTATIONS {
        return Err(format!(
            "presentation limit must be 1..={HARD_MAX_PRESENTATIONS}"
        ));
    }
    let parsed = parse_xml(xml)?;
    let programmes = definitions(&parsed, "audioProgramme", "audioProgrammeID")?;
    let contents = definitions(&parsed, "audioContent", "audioContentID")?;
    let objects = definitions(&parsed, "audioObject", "audioObjectID")?;
    if programmes.is_empty() {
        return Err("ADM contains no audioProgramme to enumerate".into());
    }

    let mut object_children = BTreeMap::<String, Vec<String>>::new();
    let mut all_groups = Vec::new();
    let mut group_membership = HashMap::<String, String>::new();
    for (id, index) in &objects {
        let children = direct_child_texts(&parsed, *index, "audioObjectIDRef");
        require_known_references("audioObject", id, &children, &objects)?;
        object_children.insert(id.clone(), children);
        let mut complements = direct_child_texts(&parsed, *index, "audioComplementaryObjectIDRef");
        if complements.is_empty() {
            continue;
        }
        let unique = complements.iter().collect::<BTreeSet<_>>();
        if unique.len() != complements.len() {
            return Err(format!(
                "complementary group {id} contains duplicate object references"
            ));
        }
        complements.sort();
        require_known_references("complementary group", id, &complements, &objects)?;
        let mut candidates = Vec::with_capacity(complements.len() + 1);
        candidates.push(id.clone());
        candidates.extend(complements);
        for member in &candidates {
            if let Some(previous) = group_membership.insert(member.clone(), id.clone()) {
                return Err(format!(
                    "audioObject {member} belongs to complementary groups {previous} and {id}"
                ));
            }
        }
        all_groups.push(ComplementaryGroup {
            root_object_id: id.clone(),
            candidates,
        });
    }
    all_groups.sort_by(|left, right| left.root_object_id.cmp(&right.root_object_id));

    let mut programme_inventory = Vec::with_capacity(programmes.len());
    let mut total = 0_usize;
    for (id, index) in programmes {
        let mut content_ids = direct_child_texts(&parsed, index, "audioContentIDRef");
        content_ids.sort();
        if content_ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(format!(
                "audioProgramme {id} contains duplicate audioContent references"
            ));
        }
        if content_ids.is_empty() {
            return Err(format!("audioProgramme {id} references no audioContent"));
        }
        require_known_references("audioProgramme", &id, &content_ids, &contents)?;
        let mut roots = Vec::new();
        for content in &content_ids {
            let content_index = contents[content];
            let refs = direct_child_texts(&parsed, content_index, "audioObjectIDRef");
            if refs.is_empty() {
                return Err(format!("audioContent {content} references no audioObject"));
            }
            require_known_references("audioContent", content, &refs, &objects)?;
            roots.extend(refs);
        }
        let reachable = reachable_objects(&roots, &object_children);
        let groups = all_groups
            .iter()
            .filter(|group| {
                group
                    .candidates
                    .iter()
                    .any(|candidate| reachable.contains(candidate))
            })
            .cloned()
            .collect::<Vec<_>>();
        let presentation_count = groups.iter().try_fold(1_usize, |count, group| {
            count
                .checked_mul(group.candidates.len())
                .ok_or_else(|| format!("presentation count overflow for {id}"))
        })?;
        total = total
            .checked_add(presentation_count)
            .ok_or_else(|| "total presentation count overflow".to_string())?;
        if total > max_presentations {
            return Err(format!(
                "ADM expands to {total} presentations, exceeding the configured limit {max_presentations}; no renderer was started"
            ));
        }
        let node = &parsed.nodes[index];
        programme_inventory.push(AdmProgramme {
            id: id.clone(),
            name: node
                .attributes
                .get("audioProgrammeName")
                .cloned()
                .unwrap_or_else(|| id.clone()),
            language: node.attributes.get("audioProgrammeLanguage").cloned(),
            content_ids,
            referenced_object_ids: reachable.into_iter().collect(),
            loudness_metadata: parse_loudness(&parsed, index)?,
            complementary_groups: groups,
            presentation_count,
        });
    }
    programme_inventory.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(AdmInventory {
        programme_count: programme_inventory.len(),
        content_count: contents.len(),
        object_count: objects.len(),
        complementary_group_count: all_groups.len(),
        presentation_count: total,
        programmes: programme_inventory,
    })
}

pub fn write_report(
    path: &Path,
    report: &AdmPresentationReport,
    compact: bool,
    overwrite: bool,
) -> Result<(), String> {
    if path.exists() && !overwrite {
        return Err(format!(
            "refusing to replace existing ADM presentation report {}; pass --overwrite",
            path.display()
        ));
    }
    let mut bytes = if compact {
        serde_json::to_vec(report)
    } else {
        serde_json::to_vec_pretty(report)
    }
    .map_err(|error| format!("serialize ADM presentation report: {error}"))?;
    bytes.push(b'\n');
    let mut output = crate::atomic::AtomicOutput::new(path)?;
    output.write_all(&bytes)?;
    output.commit()
}

fn validate_options(options: &Options) -> Result<(), String> {
    if options.layout.trim().is_empty() || options.layout.len() > 64 {
        return Err("ADM output layout must contain 1..=64 characters".into());
    }
    if options.timeout_seconds == 0 || options.timeout_seconds > MAX_TIMEOUT_SECONDS {
        return Err(format!(
            "renderer timeout must be 1..={MAX_TIMEOUT_SECONDS} seconds"
        ));
    }
    if options.max_presentations == 0 || options.max_presentations > HARD_MAX_PRESENTATIONS {
        return Err(format!(
            "presentation limit must be 1..={HARD_MAX_PRESENTATIONS}"
        ));
    }
    if options.max_decoded_samples_per_presentation == 0
        || options.max_decoded_samples_per_presentation > HARD_MAX_DECODED_SAMPLES
    {
        return Err(format!(
            "decoded sample limit must be 1..={HARD_MAX_DECODED_SAMPLES}"
        ));
    }
    for (label, value) in [
        ("loudness tolerance", options.loudness_tolerance_lu),
        ("true-peak tolerance", options.true_peak_tolerance_db),
    ] {
        if !value.is_finite() || !(0.0..=10.0).contains(&value) {
            return Err(format!("{label} must be finite and between 0 and 10"));
        }
    }
    Ok(())
}

fn render_byte_limit(max_decoded_samples: u64) -> Result<u64, String> {
    max_decoded_samples
        .checked_mul(8)
        .and_then(|bytes| bytes.checked_add(RENDER_HEADER_ALLOWANCE))
        .ok_or_else(|| "ADM render byte limit overflow".to_string())
}

fn enumerate_variants(
    inventory: &AdmInventory,
    max_presentations: usize,
) -> Result<Vec<Variant>, String> {
    let mut variants = Vec::with_capacity(inventory.presentation_count);
    for (programme, item) in inventory.programmes.iter().enumerate() {
        let mut combinations = vec![Vec::new()];
        for group in &item.complementary_groups {
            let mut expanded = Vec::new();
            for existing in &combinations {
                for candidate in &group.candidates {
                    let mut selected = existing.clone();
                    selected.push(candidate.clone());
                    expanded.push(selected);
                }
            }
            combinations = expanded;
        }
        for selected in combinations {
            variants.push(Variant {
                programme,
                selected,
            });
        }
        if variants.len() > max_presentations {
            return Err(format!(
                "ADM expands beyond the configured {max_presentations} presentation limit"
            ));
        }
    }
    if variants.len() != inventory.presentation_count {
        return Err("ADM presentation inventory changed during enumeration".into());
    }
    Ok(variants)
}

fn variant_id(programme: &AdmProgramme, selected: &[String]) -> String {
    if selected.is_empty() {
        programme.id.clone()
    } else {
        format!("{}::{}", programme.id, selected.join("+"))
    }
}

fn parse_xml(xml: &[u8]) -> Result<ParsedXml, String> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut parsed = ParsedXml::default();
    let mut stack = Vec::<usize>::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                let index = push_node(&element, stack.last().copied(), &mut parsed)?;
                stack.push(index);
            }
            Ok(Event::Empty(element)) => {
                push_node(&element, stack.last().copied(), &mut parsed)?;
            }
            Ok(Event::Text(text)) => {
                if let Some(index) = stack.last() {
                    parsed.nodes[*index].text.push_str(text.as_ref().trim());
                }
            }
            Ok(Event::CData(text)) => {
                if let Some(index) = stack.last() {
                    parsed.nodes[*index].text.push_str(text.as_ref().trim());
                }
            }
            Ok(Event::End(_)) => {
                if stack.pop().is_none() {
                    return Err("ADM XML contains an unmatched closing element".into());
                }
            }
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(format!(
                    "ADM XML error at byte {}: {error}",
                    reader.error_position()
                ))
            }
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err("ADM XML ended with unclosed elements".into());
    }
    Ok(parsed)
}

fn push_node(
    element: &quick_xml::events::BytesStart<'_>,
    parent: Option<usize>,
    parsed: &mut ParsedXml,
) -> Result<usize, String> {
    let name = local_name(element.name().as_ref());
    let mut attributes = HashMap::new();
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| format!("ADM XML attribute: {error}"))?;
        let key = local_name(attribute.key.as_ref());
        let value = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|error| format!("ADM XML attribute {key}: {error}"))?
            .into_owned();
        attributes.insert(key, value);
    }
    let index = parsed.nodes.len();
    parsed.nodes.push(Node {
        name,
        parent,
        attributes,
        text: String::new(),
    });
    Ok(index)
}

fn local_name(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_owned()
}

fn definitions(
    parsed: &ParsedXml,
    element: &str,
    attribute: &str,
) -> Result<BTreeMap<String, usize>, String> {
    let mut values = BTreeMap::new();
    for (index, node) in parsed
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| node.name == element)
    {
        let id = node
            .attributes
            .get(attribute)
            .ok_or_else(|| format!("{element} is missing {attribute}"))?
            .trim();
        if id.is_empty() || values.insert(id.to_owned(), index).is_some() {
            return Err(format!("{element} IDs must be non-empty and unique: {id}"));
        }
    }
    Ok(values)
}

fn direct_children(parsed: &ParsedXml, parent: usize, name: &str) -> Vec<usize> {
    parsed
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| node.parent == Some(parent) && node.name == name)
        .map(|(index, _)| index)
        .collect()
}

fn direct_child_texts(parsed: &ParsedXml, parent: usize, name: &str) -> Vec<String> {
    direct_children(parsed, parent, name)
        .into_iter()
        .map(|index| parsed.nodes[index].text.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect()
}

fn require_known_references(
    owner_kind: &str,
    owner_id: &str,
    references: &[String],
    definitions: &BTreeMap<String, usize>,
) -> Result<(), String> {
    if let Some(reference) = references
        .iter()
        .find(|reference| !definitions.contains_key(reference.as_str()))
    {
        return Err(format!(
            "{owner_kind} {owner_id} references unknown element {reference}"
        ));
    }
    Ok(())
}

fn reachable_objects(
    roots: &[String],
    children: &BTreeMap<String, Vec<String>>,
) -> BTreeSet<String> {
    let mut reachable = BTreeSet::new();
    let mut pending = roots.to_vec();
    while let Some(id) = pending.pop() {
        if !reachable.insert(id.clone()) {
            continue;
        }
        if let Some(next) = children.get(&id) {
            pending.extend(next.iter().cloned());
        }
    }
    reachable
}

fn parse_loudness(
    parsed: &ParsedXml,
    programme: usize,
) -> Result<Option<AdmLoudnessMetadata>, String> {
    let values = direct_children(parsed, programme, "loudnessMetadata");
    if values.len() > 1 {
        return Err("audioProgramme contains multiple loudnessMetadata elements".into());
    }
    let Some(index) = values.first().copied() else {
        return Ok(None);
    };
    let renderer = direct_children(parsed, index, "renderer");
    if renderer.len() > 1 {
        return Err("loudnessMetadata contains multiple renderer elements".into());
    }
    let renderer_node = renderer.first().map(|index| &parsed.nodes[*index]);
    let renderer_audio_object_ids = renderer
        .first()
        .map(|index| direct_child_texts(parsed, *index, "audioObjectIDRef"))
        .unwrap_or_default();
    if renderer_audio_object_ids
        .iter()
        .collect::<HashSet<_>>()
        .len()
        != renderer_audio_object_ids.len()
    {
        return Err("loudness renderer contains duplicate audioObject references".into());
    }
    let loudness_method = parsed.nodes[index]
        .attributes
        .get("loudnessMethod")
        .cloned();
    let integrated_loudness_lufs = optional_f64(parsed, index, "integratedLoudness")?;
    let production_profile_passed = loudness_method.as_deref().is_some_and(bs1770_5_or_later)
        && integrated_loudness_lufs.is_some();
    Ok(Some(AdmLoudnessMetadata {
        loudness_method,
        integrated_loudness_lufs,
        max_true_peak_dbtp: optional_f64(parsed, index, "maxTruePeak")?,
        renderer_uri: renderer_node.and_then(|node| node.attributes.get("uri").cloned()),
        renderer_name: renderer_node.and_then(|node| node.attributes.get("name").cloned()),
        renderer_version: renderer_node.and_then(|node| node.attributes.get("version").cloned()),
        renderer_audio_object_ids,
        production_profile_passed,
    }))
}

fn bs1770_5_or_later(value: &str) -> bool {
    value
        .trim()
        .strip_prefix("ITU-R BS.1770-")
        .and_then(|version| version.parse::<u32>().ok())
        .is_some_and(|version| version >= 5)
}

fn optional_f64(parsed: &ParsedXml, parent: usize, name: &str) -> Result<Option<f64>, String> {
    let values = direct_child_texts(parsed, parent, name);
    if values.len() > 1 {
        return Err(format!("loudnessMetadata contains multiple {name} values"));
    }
    values
        .first()
        .map(|value| {
            let parsed = value
                .parse::<f64>()
                .map_err(|error| format!("invalid {name} value {value}: {error}"))?;
            if !parsed.is_finite() {
                return Err(format!("{name} must be finite"));
            }
            Ok(parsed)
        })
        .transpose()
}

fn resolve_executable(command: &Path) -> Result<PathBuf, String> {
    if command.is_absolute() || command.components().count() > 1 {
        return fs::canonicalize(command)
            .map_err(|error| format!("resolve ADM renderer {}: {error}", command.display()));
    }
    let path = std::env::var_os("PATH")
        .ok_or_else(|| "PATH is unavailable while resolving the ADM renderer".to_string())?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(command);
        if candidate.is_file() {
            return fs::canonicalize(&candidate)
                .map_err(|error| format!("resolve ADM renderer {}: {error}", candidate.display()));
        }
        #[cfg(windows)]
        {
            let candidate = directory.join(format!("{}.exe", command.to_string_lossy()));
            if candidate.is_file() {
                return fs::canonicalize(&candidate).map_err(|error| {
                    format!("resolve ADM renderer {}: {error}", candidate.display())
                });
            }
        }
    }
    Err(format!(
        "ADM renderer {} was not found in PATH; install the EBU ADM Renderer or pass --renderer",
        command.display()
    ))
}

fn prepare_retained_root(
    options: &Options,
    presentation_count: usize,
) -> Result<Option<PathBuf>, String> {
    let Some(path) = options.retained_renders.as_deref() else {
        return Ok(None);
    };
    if path.exists() && !path.is_dir() {
        return Err(format!(
            "retained-render path is not a directory: {}",
            path.display()
        ));
    }
    fs::create_dir_all(path).map_err(|error| {
        format!(
            "create retained-render directory {}: {error}",
            path.display()
        )
    })?;
    let root = fs::canonicalize(path)
        .map_err(|error| format!("resolve retained-render directory: {error}"))?;
    if !options.overwrite {
        for index in 0..presentation_count {
            let destination = root.join(format!("presentation-{:04}.wav", index + 1));
            if destination.exists() {
                return Err(format!(
                    "retained render already exists: {}; pass --overwrite",
                    destination.display()
                ));
            }
        }
    }
    Ok(Some(root))
}

fn retain_render(
    root: &Path,
    source: &Path,
    index: usize,
    overwrite: bool,
) -> Result<PathBuf, String> {
    let destination = root.join(format!("presentation-{:04}.wav", index + 1));
    if destination.exists() && !overwrite {
        return Err(format!(
            "retained render already exists: {}; pass --overwrite",
            destination.display()
        ));
    }
    let mut output = crate::atomic::AtomicOutput::new(&destination)?;
    output.copy_from_path(source)?;
    output.commit()?;
    Ok(destination)
}

fn ensure_regular_file(path: &Path, label: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| format!("stat {label}: {error}"))?;
    if !metadata.file_type().is_file() {
        return Err(format!("{label} must be a regular file"));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<(String, u64), String> {
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
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
        hex.push_str(&format!("{byte:02x}"));
    }
    Ok((hex, bytes))
}

fn ensure_unchanged(
    path: &Path,
    expected_sha256: &str,
    expected_bytes: u64,
    message: &str,
) -> Result<(), String> {
    let (sha256, bytes) = sha256_file(path)?;
    if sha256 != expected_sha256 || bytes != expected_bytes {
        return Err(message.into());
    }
    Ok(())
}

struct ToolOutput {
    status: ExitStatus,
    stderr: Vec<u8>,
}

fn run_bounded(
    executable: &Path,
    args: &[OsString],
    timeout: Duration,
    rendered: &Path,
    max_render_bytes: u64,
) -> Result<ToolOutput, String> {
    let mut stdout_file =
        tempfile::tempfile().map_err(|error| format!("create renderer stdout spool: {error}"))?;
    let mut stderr_file =
        tempfile::tempfile().map_err(|error| format!("create renderer stderr spool: {error}"))?;
    let mut child = Command::new(executable)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file.try_clone().map_err(|error| {
            format!("clone renderer stdout spool: {error}")
        })?))
        .stderr(Stdio::from(stderr_file.try_clone().map_err(|error| {
            format!("clone renderer stderr spool: {error}")
        })?))
        .spawn()
        .map_err(|error| format!("start ADM renderer {}: {error}", executable.display()))?;
    let started = Instant::now();
    let status = loop {
        let stdout_len = stdout_file
            .metadata()
            .map_err(|error| format!("stat renderer stdout: {error}"))?
            .len();
        let stderr_len = stderr_file
            .metadata()
            .map_err(|error| format!("stat renderer stderr: {error}"))?
            .len();
        if stdout_len > TOOL_OUTPUT_LIMIT as u64 || stderr_len > TOOL_OUTPUT_LIMIT as u64 {
            let _ = child.kill();
            let _ = child.wait();
            return Err("ADM renderer output exceeded its 1 MiB per-stream safety limit".into());
        }
        match fs::symlink_metadata(rendered) {
            Ok(metadata) if metadata.len() > max_render_bytes => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "ADM render exceeded its {max_render_bytes} byte safety limit"
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "stat ADM render while renderer is running: {error}"
                ));
            }
        }
        match child
            .try_wait()
            .map_err(|error| format!("wait for ADM renderer: {error}"))?
        {
            Some(status) => break status,
            None if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "ADM renderer exceeded the {} second per-presentation timeout",
                    timeout.as_secs()
                ));
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    };
    let _ = read_bounded(&mut stdout_file, TOOL_OUTPUT_LIMIT, "stdout")?;
    let stderr = read_bounded(&mut stderr_file, TOOL_OUTPUT_LIMIT, "stderr")?;
    Ok(ToolOutput { status, stderr })
}

fn read_bounded(file: &mut File, limit: usize, label: &str) -> Result<Vec<u8>, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek renderer {label}: {error}"))?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read renderer {label}: {error}"))?;
    if bytes.len() > limit {
        return Err(format!("ADM renderer {label} exceeded its safety limit"));
    }
    Ok(bytes)
}

fn diagnostic(stderr: &[u8]) -> String {
    let value = String::from_utf8_lossy(stderr);
    let value = value.trim();
    if value.is_empty() {
        "no diagnostic output".into()
    } else {
        value.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AXML: &[u8] = br#"<audioFormatExtended version="ITU-R_BS.2076-3">
  <audioProgramme audioProgrammeID="APR_1002" audioProgrammeName="Second">
    <audioContentIDRef>ACO_1002</audioContentIDRef>
  </audioProgramme>
  <audioProgramme audioProgrammeID="APR_1001" audioProgrammeName="Main" audioProgrammeLanguage="en">
    <audioContentIDRef>ACO_1001</audioContentIDRef>
    <loudnessMetadata loudnessMethod="ITU-R BS.1770-5">
      <integratedLoudness>-23.0</integratedLoudness><maxTruePeak>-1.0</maxTruePeak>
      <renderer uri="urn:itu:bs:2127:0:itu_adm_renderer"><audioObjectIDRef>AO_1001</audioObjectIDRef></renderer>
    </loudnessMetadata>
  </audioProgramme>
  <audioContent audioContentID="ACO_1001"><audioObjectIDRef>AO_1001</audioObjectIDRef></audioContent>
  <audioContent audioContentID="ACO_1002"><audioObjectIDRef>AO_2001</audioObjectIDRef></audioContent>
  <audioObject audioObjectID="AO_1001"><audioObjectIDRef>AO_1100</audioObjectIDRef><audioComplementaryObjectIDRef>AO_1002</audioComplementaryObjectIDRef><audioComplementaryObjectIDRef>AO_1003</audioComplementaryObjectIDRef></audioObject>
  <audioObject audioObjectID="AO_1002"/><audioObject audioObjectID="AO_1003"/><audioObject audioObjectID="AO_1100"/>
  <audioObject audioObjectID="AO_2001"/>
</audioFormatExtended>"#;

    #[test]
    fn inventories_every_programme_and_complementary_choice_deterministically() {
        let inventory = inventory_from_axml(AXML, 16).unwrap();
        assert_eq!(inventory.programme_count, 2);
        assert_eq!(inventory.presentation_count, 4);
        assert_eq!(inventory.programmes[0].id, "APR_1001");
        assert_eq!(inventory.programmes[0].presentation_count, 3);
        assert_eq!(
            inventory.programmes[0].complementary_groups[0].candidates,
            ["AO_1001", "AO_1002", "AO_1003"]
        );
        assert_eq!(
            inventory.programmes[0]
                .loudness_metadata
                .as_ref()
                .unwrap()
                .integrated_loudness_lufs,
            Some(-23.0)
        );
        let variants = enumerate_variants(&inventory, 16).unwrap();
        assert_eq!(variants.len(), 4);
        assert_eq!(variants[0].selected, ["AO_1001"]);
        assert_eq!(variants[2].selected, ["AO_1003"]);
        assert!(variants[3].selected.is_empty());
    }

    #[test]
    fn rejects_combinatorial_expansion_before_rendering() {
        let error = inventory_from_axml(AXML, 3).unwrap_err();
        assert!(error.contains("no renderer was started"), "{error}");
    }

    #[test]
    fn expands_the_cartesian_product_of_independent_groups() {
        let xml = br#"<audioFormatExtended>
          <audioProgramme audioProgrammeID="APR_1"><audioContentIDRef>ACO_1</audioContentIDRef></audioProgramme>
          <audioContent audioContentID="ACO_1"><audioObjectIDRef>AO_1</audioObjectIDRef><audioObjectIDRef>AO_3</audioObjectIDRef></audioContent>
          <audioObject audioObjectID="AO_1"><audioComplementaryObjectIDRef>AO_2</audioComplementaryObjectIDRef></audioObject>
          <audioObject audioObjectID="AO_2"/>
          <audioObject audioObjectID="AO_3"><audioComplementaryObjectIDRef>AO_4</audioComplementaryObjectIDRef></audioObject>
          <audioObject audioObjectID="AO_4"/>
        </audioFormatExtended>"#;
        let inventory = inventory_from_axml(xml, 4).unwrap();
        assert_eq!(inventory.presentation_count, 4);
        let variants = enumerate_variants(&inventory, 4).unwrap();
        assert_eq!(variants[0].selected, ["AO_1", "AO_3"]);
        assert_eq!(variants[1].selected, ["AO_1", "AO_4"]);
        assert_eq!(variants[2].selected, ["AO_2", "AO_3"]);
        assert_eq!(variants[3].selected, ["AO_2", "AO_4"]);
    }

    #[test]
    fn rejects_pre_bs1770_5_loudness_metadata_for_production() {
        let xml = br#"<audioFormatExtended>
          <audioProgramme audioProgrammeID="APR_1"><audioContentIDRef>ACO_1</audioContentIDRef><loudnessMetadata loudnessMethod="ITU-R BS.1770-4"><integratedLoudness>-23</integratedLoudness></loudnessMetadata></audioProgramme>
          <audioContent audioContentID="ACO_1"><audioObjectIDRef>AO_1</audioObjectIDRef></audioContent>
          <audioObject audioObjectID="AO_1"/>
        </audioFormatExtended>"#;
        let inventory = inventory_from_axml(xml, 1).unwrap();
        assert!(
            !inventory.programmes[0]
                .loudness_metadata
                .as_ref()
                .unwrap()
                .production_profile_passed
        );
    }

    #[test]
    fn rejects_objects_in_multiple_complementary_groups() {
        let xml = br#"<audioFormatExtended>
          <audioProgramme audioProgrammeID="APR_1"><audioContentIDRef>ACO_1</audioContentIDRef></audioProgramme>
          <audioContent audioContentID="ACO_1"><audioObjectIDRef>AO_1</audioObjectIDRef></audioContent>
          <audioObject audioObjectID="AO_1"><audioComplementaryObjectIDRef>AO_3</audioComplementaryObjectIDRef></audioObject>
          <audioObject audioObjectID="AO_2"><audioComplementaryObjectIDRef>AO_3</audioComplementaryObjectIDRef></audioObject>
          <audioObject audioObjectID="AO_3"/>
        </audioFormatExtended>"#;
        assert!(inventory_from_axml(xml, 16)
            .unwrap_err()
            .contains("belongs to complementary groups"));
    }
}
