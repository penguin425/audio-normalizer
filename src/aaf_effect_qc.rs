//! AS-01/AS-05 effect-dictionary profiles for bounded AAF QC.
//!
//! Values carried by ConstantValue and ControlPoint are AAF Indirect values.
//! This module interprets only the standard scalar types needed by the AMWA
//! effect protocols; it does not render effects or load plug-ins.

use crate::aaf_object_qc::{StoredObject, StoredProperty};
use crate::container_qc::{check, AuditCheck};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const MAX_FINDINGS: usize = 100;
const SF_WEAK_REF: u16 = 0x02;
const SF_WEAK_VECTOR: u16 = 0x12;
const SF_WEAK_SET: u16 = 0x1a;

const TYPE_INT32: &str = "01010700-0000-0000-060e-2b3401040101";
const TYPE_BOOLEAN: &str = "01040100-0000-0000-060e-2b3401040101";
const TYPE_RATIONAL: &str = "03010100-0000-0000-060e-2b3401040101";
const TYPE_STRING: &str = "01100200-0000-0000-060e-2b3401040101";
const TYPE_TITLE_ALIGNMENT: &str = "0201012b-0000-0000-060e-2b3401040101";
const EDIT_PROTOCOL: &str = "0d011201-0100-0000-060e-2b3404010105";

const P_LEVEL: &str = "e4962320-2267-11d3-8a4c-0050040ef7d2";
const P_WIPE_NUMBER: &str = "e4962323-2267-11d3-8a4c-0050040ef7d2";
const P_WIPE_REVERSE: &str = "9c894ba0-2277-11d3-8a4c-0050040ef7d2";
const P_SPEED_RATIO: &str = "72559a80-24d7-11d3-8a50-0050040ef7d2";
const P_POSITION_X: &str = "c573a510-071a-454f-b617-ad6ae69054c2";
const P_POSITION_Y: &str = "82e27478-1336-4ea3-bcb9-6b8f17864c42";
const P_CROP_LEFT: &str = "d47b3377-318c-4657-a9d8-75811b6dc3d1";
const P_CROP_RIGHT: &str = "5ecc9dd5-21c1-462b-9fec-c2bd85f14033";
const P_CROP_TOP: &str = "8170a539-9b55-4051-9d4e-46598d01b914";
const P_CROP_BOTTOM: &str = "154ba82b-990a-4c80-9101-3037e28839a1";
const P_SCALE_X: &str = "8d568129-847e-11d5-935a-50f857c10000";
const P_SCALE_Y: &str = "8d56812a-847e-11d5-935a-50f857c10000";
const P_ROTATION: &str = "062cfbd8-f4b1-4a50-b944-f39e2fc73c17";
const P_PIN_TLX: &str = "72a3b4a2-873d-4733-9052-9f83a706ca5b";
const P_PIN_TLY: &str = "29e4d78f-a502-4ebb-8c07-ed5a0320c1b0";
const P_PIN_TRX: &str = "a95296c0-1ed9-4925-8481-2096c72e818d";
const P_PIN_TRY: &str = "ce1757ae-7a0b-45d9-b3f3-3686adff1e2d";
const P_PIN_BLX: &str = "08b2bc81-9b1b-4c01-ba73-bba3554ed029";
const P_PIN_BLY: &str = "c163f2ff-cd83-4655-826e-3724ab7fa092";
const P_PIN_BRX: &str = "53bc5884-897f-479e-b833-191f8692100d";
const P_PIN_BRY: &str = "812fb15b-0b95-4406-878d-efaa1cffc129";
const P_INVERT_ALPHA: &str = "a2667f65-65d8-4abf-a179-0b9b93413949";
const P_LUMA_LEVEL: &str = "21ed5b0f-b7a0-43bc-b779-c47f85bf6c4d";
const P_LUMA_CLIP: &str = "cbd39b25-3ece-441e-ba2c-da473ab5cc7c";
const P_AMPLITUDE: &str = "e4962321-2267-11d3-8a4c-0050040ef7d2";
const P_PAN: &str = "e4962322-2267-11d3-8a4c-0050040ef7d2";
const P_OUTGOING: &str = "9e610007-1be2-41e1-bb11-c95de9964d03";
const P_INCOMING: &str = "48cea642-a8f9-455b-82b3-86c814b797c7";
const P_OPACITY: &str = "cb7c0ec4-f45f-4ee6-aef0-c63ddb134924";
const P_TITLE_TEXT: &str = "7b92827b-5ae3-465e-b5f9-5ee21b070859";
const P_TITLE_FONT: &str = "e8eb7f50-602f-4a2f-8fb2-86c8826ccf24";
const P_TITLE_SIZE: &str = "01c55287-31b3-4f8f-bb87-c92f06eb7f5a";
const P_TITLE_R: &str = "dfe86f24-8a71-4dc5-83a2-988f583af711";
const P_TITLE_G: &str = "f9f41222-36d9-4650-bd5a-a17866cf86b9";
const P_TITLE_B: &str = "f5ba87fa-cf72-4f37-a736-d7096fcb06f1";
const P_TITLE_ALIGN: &str = "47c1733f-6afb-4168-9b6d-476adfbae7ab";
const P_TITLE_BOLD: &str = "8b5732c0-be8e-4332-aa71-5d866add777d";
const P_TITLE_ITALIC: &str = "e4a3c91b-f96a-4dd4-91d8-1ba32000ab72";
const P_TITLE_X: &str = "a25061da-db25-402e-89ff-a6d0efa39444";
const P_TITLE_Y: &str = "6151541f-9d3f-4a0e-a3f9-24cc60eea969";
const P_SLOPE_R: &str = "be2033da-723b-4146-ace0-3299e0ff342e";
const P_SLOPE_G: &str = "7ca8e01b-c6d8-4b3f-b251-28a53e5b958f";
const P_SLOPE_B: &str = "1aeb007b-3cd5-4814-87b5-cbd6a3cdfe8d";
const P_OFFSET_R: &str = "4d1e65e0-85fc-4bb9-a264-13cf320a8539";
const P_OFFSET_G: &str = "76f783e4-0bbd-41d7-b01e-f418c1602a6f";
const P_OFFSET_B: &str = "57110628-522d-4b48-8a28-75477ced984d";
const P_POWER_R: &str = "c2d79c3a-9263-40d9-827d-953ac6b88813";
const P_POWER_G: &str = "524d52e6-86a3-4f41-864b-fb53b15b1d5d";
const P_POWER_B: &str = "5f0cc7dc-907d-4153-bf00-1f3cdf3c05bb";
const P_SATURATION: &str = "0b135705-3312-4d03-ba89-be9ef45e5470";
const P_COLOR_DESC: &str = "f3b9466a-2579-4168-beb5-66b996919a3f";
const P_INPUT_DESC: &str = "b0124dbe-7f97-443c-ae39-c49c1c53d728";
const P_VIEW_DESC: &str = "5a9dfc6f-611f-4db8-8eff-3b9cdb6e1220";

const PINS: &[&str] = &[
    P_PIN_TLX, P_PIN_TLY, P_PIN_TRX, P_PIN_TRY, P_PIN_BLX, P_PIN_BLY, P_PIN_BRX, P_PIN_BRY,
];
const COLOR_REQUIRED: &[&str] = &[
    P_SLOPE_R, P_SLOPE_G, P_SLOPE_B, P_OFFSET_R, P_OFFSET_G, P_OFFSET_B, P_POWER_R, P_POWER_G,
    P_POWER_B,
];
const COLOR_ALL: &[&str] = &[
    P_SLOPE_R,
    P_SLOPE_G,
    P_SLOPE_B,
    P_OFFSET_R,
    P_OFFSET_G,
    P_OFFSET_B,
    P_POWER_R,
    P_POWER_G,
    P_POWER_B,
    P_SATURATION,
    P_COLOR_DESC,
    P_INPUT_DESC,
    P_VIEW_DESC,
];
const TITLE_ALL: &[&str] = &[
    P_TITLE_TEXT,
    P_TITLE_FONT,
    P_TITLE_SIZE,
    P_TITLE_R,
    P_TITLE_G,
    P_TITLE_B,
    P_TITLE_ALIGN,
    P_TITLE_BOLD,
    P_TITLE_ITALIC,
    P_TITLE_X,
    P_TITLE_Y,
];

const PICTURE_DEFS: &[&str] = &[
    "01030202-0100-0000-060e-2b3404010101",
    "6f3c8ce1-6cef-11d2-807d-006008143e6f",
];
const SOUND_DEFS: &[&str] = &[
    "01030202-0200-0000-060e-2b3404010101",
    "78e1ebe1-6cef-11d2-807d-006008143e6f",
];

#[derive(Clone, Copy)]
enum Protocol {
    As01,
    As05,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Placement {
    Segment,
    Transition,
}

#[derive(Clone, Copy)]
enum ValueRule {
    Int32,
    Boolean,
    String,
    Alignment,
    Rational {
        minimum: i64,
        maximum: i64,
        exclude_zero: bool,
    },
}

#[derive(Clone, Copy)]
struct ParameterProfile {
    name: &'static str,
    type_id: &'static str,
    value: ValueRule,
}

#[derive(Clone, Copy)]
struct OperationProfile {
    id: &'static str,
    name: &'static str,
    protocol: Protocol,
    data_definitions: &'static [&'static str],
    inputs: i32,
    time_warp: bool,
    bypass: Option<u32>,
    placement: Placement,
    required: &'static [&'static str],
    allowed: &'static [&'static str],
    allow_extensions: bool,
}

#[allow(clippy::too_many_arguments)]
const fn operation(
    id: &'static str,
    name: &'static str,
    protocol: Protocol,
    data_definitions: &'static [&'static str],
    inputs: i32,
    time_warp: bool,
    bypass: Option<u32>,
    placement: Placement,
    required: &'static [&'static str],
    allowed: &'static [&'static str],
) -> OperationProfile {
    OperationProfile {
        id,
        name,
        protocol,
        data_definitions,
        inputs,
        time_warp,
        bypass,
        placement,
        required,
        allowed,
        allow_extensions: false,
    }
}

const OPERATIONS: &[OperationProfile] = &[
    operation(
        "0c3bea40-fc05-11d2-8a29-0050040ef7d2",
        "Video Dissolve",
        Protocol::As01,
        PICTURE_DEFS,
        2,
        false,
        None,
        Placement::Transition,
        &[],
        &[P_LEVEL],
    ),
    operation(
        "0c3bea44-fc05-11d2-8a29-0050040ef7d2",
        "SMPTE Video Wipe",
        Protocol::As01,
        PICTURE_DEFS,
        2,
        false,
        None,
        Placement::Transition,
        &[P_WIPE_NUMBER],
        &[P_WIPE_NUMBER, P_WIPE_REVERSE, P_LEVEL],
    ),
    operation(
        "9d2ea890-0968-11d3-8a38-0050040ef7d2",
        "Video Speed Control",
        Protocol::As01,
        PICTURE_DEFS,
        1,
        true,
        Some(1),
        Placement::Segment,
        &[P_SPEED_RATIO],
        &[P_SPEED_RATIO],
    ),
    operation(
        "9d2ea891-0968-11d3-8a38-0050040ef7d2",
        "Video Repeat",
        Protocol::As01,
        PICTURE_DEFS,
        1,
        true,
        Some(1),
        Placement::Segment,
        &[],
        &[],
    ),
    operation(
        "f1db0f32-8d64-11d3-80df-006008143e6f",
        "Video Flip",
        Protocol::As01,
        PICTURE_DEFS,
        1,
        false,
        Some(1),
        Placement::Segment,
        &[],
        &[],
    ),
    operation(
        "f1db0f34-8d64-11d3-80df-006008143e6f",
        "Video Flop",
        Protocol::As01,
        PICTURE_DEFS,
        1,
        false,
        Some(1),
        Placement::Segment,
        &[],
        &[],
    ),
    operation(
        "f1db0f33-8d64-11d3-80df-006008143e6f",
        "Video Flip-Flop",
        Protocol::As01,
        PICTURE_DEFS,
        1,
        false,
        Some(1),
        Placement::Segment,
        &[],
        &[],
    ),
    operation(
        "86f5711e-ee72-450c-a118-17cf3b175dff",
        "Video Position",
        Protocol::As01,
        PICTURE_DEFS,
        1,
        false,
        None,
        Placement::Segment,
        &[],
        &[P_POSITION_X, P_POSITION_Y],
    ),
    operation(
        "f5826680-26c5-4149-8554-43d3c7a3bc09",
        "Video Crop",
        Protocol::As01,
        PICTURE_DEFS,
        1,
        false,
        None,
        Placement::Segment,
        &[],
        &[P_CROP_LEFT, P_CROP_RIGHT, P_CROP_TOP, P_CROP_BOTTOM],
    ),
    operation(
        "2e0a119d-e6f7-4bee-b5dc-6dd42988687e",
        "Video Scale",
        Protocol::As01,
        PICTURE_DEFS,
        1,
        false,
        None,
        Placement::Segment,
        &[],
        &[P_SCALE_X, P_SCALE_Y],
    ),
    operation(
        "f2ca330d-8d45-4db4-b1b5-136ab055586f",
        "Video Rotate",
        Protocol::As01,
        PICTURE_DEFS,
        1,
        false,
        None,
        Placement::Segment,
        &[P_ROTATION],
        &[P_ROTATION],
    ),
    operation(
        "21d5c51a-8acb-46d5-9392-5cae640c8836",
        "Video Corner Pinning",
        Protocol::As01,
        PICTURE_DEFS,
        1,
        false,
        None,
        Placement::Segment,
        &[],
        PINS,
    ),
    operation(
        "14db900e-d537-49f6-889b-012568fcc234",
        "Alpha With Video Key",
        Protocol::As01,
        PICTURE_DEFS,
        2,
        false,
        Some(1),
        Placement::Segment,
        &[],
        &[P_INVERT_ALPHA],
    ),
    operation(
        "e599cb0f-ba5f-4192-9356-51eb19c08589",
        "Separate-Alpha Key",
        Protocol::As01,
        PICTURE_DEFS,
        // AS-01's table says two, but immediately specifies background,
        // foreground, and alpha inputs; the AAF SDK analyzer requires three.
        3,
        false,
        Some(1),
        Placement::Segment,
        &[],
        &[P_INVERT_ALPHA],
    ),
    operation(
        "38ff7903-69e5-476b-be5a-eafc2000f011",
        "Luminance Key",
        Protocol::As01,
        PICTURE_DEFS,
        2,
        false,
        Some(1),
        Placement::Segment,
        &[P_LUMA_LEVEL, P_LUMA_CLIP],
        &[P_LUMA_LEVEL, P_LUMA_CLIP],
    ),
    OperationProfile {
        allow_extensions: true,
        ..operation(
            "30a315c2-71e5-4e82-a4ef-0513ee056b65",
            "Chroma Key",
            Protocol::As01,
            PICTURE_DEFS,
            2,
            false,
            Some(1),
            Placement::Segment,
            &[],
            &[],
        )
    },
    operation(
        "9d2ea894-0968-11d3-8a38-0050040ef7d2",
        "Mono Audio Gain",
        Protocol::As01,
        SOUND_DEFS,
        1,
        false,
        Some(1),
        Placement::Segment,
        &[P_AMPLITUDE],
        &[P_AMPLITUDE],
    ),
    operation(
        "9d2ea893-0968-11d3-8a38-0050040ef7d2",
        "Mono Audio Pan",
        Protocol::As01,
        SOUND_DEFS,
        1,
        false,
        Some(1),
        Placement::Segment,
        &[P_PAN],
        &[P_PAN],
    ),
    operation(
        "0c3bea41-fc05-11d2-8a29-0050040ef7d2",
        "Mono Audio Dissolve",
        Protocol::As01,
        SOUND_DEFS,
        2,
        false,
        None,
        Placement::Transition,
        &[],
        &[P_LEVEL],
    ),
    operation(
        "2311bd90-b5da-4285-aa3a-8552848779b3",
        "Two-Parameter Mono Audio Dissolve",
        Protocol::As01,
        SOUND_DEFS,
        2,
        false,
        None,
        Placement::Transition,
        &[],
        &[P_OUTGOING, P_INCOMING],
    ),
    operation(
        "5aba98f8-f389-471f-8fee-dfde7ec7f9bb",
        "Video Color",
        Protocol::As05,
        PICTURE_DEFS,
        1,
        false,
        None,
        Placement::Segment,
        COLOR_REQUIRED,
        COLOR_ALL,
    ),
    operation(
        "2c50831c-572e-4042-b1dd-55ed0b7c49df",
        "Video Title",
        Protocol::As05,
        PICTURE_DEFS,
        1,
        false,
        None,
        Placement::Segment,
        &[P_TITLE_TEXT],
        TITLE_ALL,
    ),
    operation(
        "9bb90dfd-2aad-49af-b09c-8ba6cd5281d1",
        "Video Opacity",
        Protocol::As05,
        PICTURE_DEFS,
        1,
        false,
        None,
        Placement::Segment,
        &[P_OPACITY],
        &[P_OPACITY],
    ),
];

#[derive(Default)]
struct Findings {
    values: Vec<String>,
    total: usize,
}

impl Findings {
    fn push(&mut self, value: impl Into<String>) {
        self.total += 1;
        if self.values.len() < MAX_FINDINGS {
            self.values.push(value.into());
        }
    }
}

#[derive(Default)]
struct Stats {
    profiled_operations: usize,
    as01_operations: usize,
    as05_operations: usize,
    parameters: usize,
    constant_values: usize,
    varying_values: usize,
    control_points: usize,
    unsupported_operations: usize,
}

pub(crate) struct EffectAudit {
    pub(crate) check: AuditCheck,
    pub(crate) properties: Value,
}

pub(crate) fn audit(
    objects: &[StoredObject],
    streams: &HashMap<PathBuf, Vec<u8>>,
    references: &HashMap<(PathBuf, u16), Vec<PathBuf>>,
) -> EffectAudit {
    let by_path: HashMap<&Path, &StoredObject> = objects
        .iter()
        .map(|object| (object.path.as_path(), object))
        .collect();
    let operation_definitions = definitions(objects, 0x001c);
    let parameter_definitions = definitions(objects, 0x001d);
    let protocol_claimed = objects
        .iter()
        .find(|object| class_code(&object.class_id) == Some(0x002f))
        .and_then(|header| property(header, 0x3b09))
        .and_then(direct_auid)
        .is_some_and(|value| value == EDIT_PROTOCOL);
    let mut findings = Findings::default();
    let mut stats = Stats::default();
    let mut fallbacks = Vec::new();
    let mut validated_definitions = HashSet::new();

    for group in objects
        .iter()
        .filter(|object| class_code(&object.class_id) == Some(0x000a))
    {
        let Some(operation_id) = property(group, 0x0b01)
            .and_then(weak_key)
            .and_then(|value| auid_string(value).ok())
        else {
            continue;
        };
        let Some(profile) = OPERATIONS.iter().find(|profile| profile.id == operation_id) else {
            stats.unsupported_operations += 1;
            let (name, description) = operation_definitions
                .get(&operation_id)
                .copied()
                .map(definition_identity)
                .unwrap_or_else(|| (operation_id.clone(), None));
            fallbacks.push(json!({
                "object": group.path,
                "operation_id": operation_id,
                "name": name,
                "description": description,
                "action": "ignore unsupported effect while preserving its data",
                "log": "DefinitionObject Name and Description"
            }));
            continue;
        };
        stats.profiled_operations += 1;
        match profile.protocol {
            Protocol::As01 => stats.as01_operations += 1,
            Protocol::As05 => stats.as05_operations += 1,
        }
        if validated_definitions.insert(profile.id) {
            validate_operation_definition(
                group,
                profile,
                operation_definitions.get(profile.id).copied(),
                streams,
                protocol_claimed,
                &mut findings,
            );
        }
        validate_operation_parameters(
            group,
            profile,
            &parameter_definitions,
            &by_path,
            references,
            &mut findings,
            &mut stats,
            &mut fallbacks,
        );
    }

    let passed = findings.total == 0;
    let observed = (!passed).then(|| {
        json!({
            "total": findings.total,
            "reported": findings.values,
        })
    });
    let fallback_candidate_count = fallbacks.len();
    EffectAudit {
        check: check(
            "FORGE-AAF-EFFECT-PROFILES",
            passed,
            if passed {
                "AMWA AS-01/AS-05 effect dictionaries, parameters, values, and interpolation profiles conform"
            } else {
                "one or more AMWA AS-01/AS-05 effect-profile constraints fail"
            },
            observed,
        ),
        properties: json!({
            "supported_profiles": OPERATIONS.len(),
            "as01_profiles": OPERATIONS.iter().filter(|profile| matches!(profile.protocol, Protocol::As01)).count(),
            "as05_profiles": OPERATIONS.iter().filter(|profile| matches!(profile.protocol, Protocol::As05)).count(),
            "profiled_operations": stats.profiled_operations,
            "as01_operations": stats.as01_operations,
            "as05_operations": stats.as05_operations,
            "parameters": stats.parameters,
            "constant_values": stats.constant_values,
            "varying_values": stats.varying_values,
            "control_points": stats.control_points,
            "unsupported_operations": stats.unsupported_operations,
            "edit_protocol_claimed": protocol_claimed,
            "fallback_candidate_count": fallback_candidate_count,
            "fallback_profiles": {
                "unsupported_effect_or_parameter": "ignore while preserving data; log DefinitionObject Name and Description",
                "unsupported_interpolation": "use linear interpolation and log the fallback",
                "unsupported_time_variation": "use the average ControlPoint value and log the fallback",
                "missing_or_unknown_title_font": "use a suitable available default and warn the user"
            },
            "fallback_candidates": fallbacks,
        }),
    }
}

fn definitions(objects: &[StoredObject], code: u16) -> HashMap<String, &StoredObject> {
    objects
        .iter()
        .filter(|object| class_code(&object.class_id) == Some(code))
        .filter_map(|object| {
            property(object, 0x1b01)
                .and_then(direct_auid)
                .map(|id| (id, object))
        })
        .collect()
}

fn validate_operation_definition(
    group: &StoredObject,
    profile: &OperationProfile,
    definition: Option<&StoredObject>,
    streams: &HashMap<PathBuf, Vec<u8>>,
    protocol_claimed: bool,
    findings: &mut Findings,
) {
    let Some(definition) = definition else {
        findings.push(format!(
            "{} uses {} without its OperationDefinition",
            group.path.display(),
            profile.name
        ));
        return;
    };
    let data_definition = property(definition, 0x1e01)
        .and_then(weak_key)
        .and_then(|value| auid_string(value).ok());
    if data_definition
        .as_deref()
        .is_none_or(|value| !profile.data_definitions.contains(&value))
    {
        findings.push(format!(
            "{} {} has a non-profile DataDefinition",
            definition.path.display(),
            profile.name
        ));
    }
    if protocol_claimed && property(definition, 0x1e07).and_then(signed_i32) != Some(profile.inputs)
    {
        findings.push(format!(
            "{} {} must declare NumberInputs {}",
            definition.path.display(),
            profile.name,
            profile.inputs
        ));
    }
    if protocol_claimed && property(definition, 0x1e02).and_then(boolean) != Some(profile.time_warp)
    {
        findings.push(format!(
            "{} {} has a missing or incorrect IsTimeWarp",
            definition.path.display(),
            profile.name
        ));
    }
    if protocol_claimed {
        if let Some(expected) = profile.bypass {
            if property(definition, 0x1e08).and_then(unsigned_u32) != Some(expected) {
                findings.push(format!(
                    "{} {} must declare Bypass {expected}",
                    definition.path.display(),
                    profile.name
                ));
            }
        }
    }
    if protocol_claimed && !profile.allow_extensions {
        match weak_set_keys(definition, 0x1e09, streams) {
            Ok(declared) => {
                let expected: HashSet<String> = profile
                    .allowed
                    .iter()
                    .map(|value| (*value).to_owned())
                    .collect();
                if !expected.is_subset(&declared) {
                    findings.push(format!(
                        "{} {} ParametersDefined omits one or more standard parameters",
                        definition.path.display(),
                        profile.name
                    ));
                }
            }
            Err(error) => findings.push(format!(
                "{} {} ParametersDefined: {error}",
                definition.path.display(),
                profile.name
            )),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_operation_parameters(
    group: &StoredObject,
    profile: &OperationProfile,
    definitions: &HashMap<String, &StoredObject>,
    by_path: &HashMap<&Path, &StoredObject>,
    references: &HashMap<(PathBuf, u16), Vec<PathBuf>>,
    findings: &mut Findings,
    stats: &mut Stats,
    fallbacks: &mut Vec<Value>,
) {
    let transition = ancestor_with_class(group, by_path, 0x0017);
    if (profile.placement == Placement::Transition) != transition {
        findings.push(format!(
            "{} {} is used in the wrong {} context",
            group.path.display(),
            profile.name,
            if transition { "Transition" } else { "Segment" }
        ));
    }
    let parameter_paths = references
        .get(&(group.path.clone(), 0x0b03))
        .cloned()
        .unwrap_or_default();
    let mut actual = HashSet::new();
    for path in parameter_paths {
        let Some(parameter) = by_path.get(path.as_path()).copied() else {
            continue;
        };
        stats.parameters += 1;
        let Some(parameter_id) = property(parameter, 0x4c01).and_then(direct_auid) else {
            findings.push(format!(
                "{} has a malformed Parameter::Definition",
                parameter.path.display()
            ));
            continue;
        };
        actual.insert(parameter_id.clone());
        if !profile.allowed.contains(&parameter_id.as_str()) {
            let definition = definitions.get(&parameter_id).copied();
            if definition.is_none() {
                findings.push(format!(
                    "{} uses parameter {parameter_id} absent from Dictionary::ParameterDefinitions",
                    parameter.path.display()
                ));
            }
            let (name, description) = definition
                .map(definition_identity)
                .unwrap_or_else(|| (parameter_id.clone(), None));
            fallbacks.push(json!({
                "object": parameter.path,
                "parameter_id": parameter_id,
                "name": name,
                "description": description,
                "action": "ignore unsupported parameter while preserving its data",
                "log": "DefinitionObject Name and Description"
            }));
            continue;
        }
        let Some(parameter_profile) = parameter_profile(&parameter_id) else {
            let (name, description) = definitions
                .get(&parameter_id)
                .copied()
                .map(definition_identity)
                .unwrap_or_else(|| (parameter_id.clone(), None));
            fallbacks.push(json!({
                "object": parameter.path,
                "parameter_id": parameter_id,
                "name": name,
                "description": description,
                "action": "ignore unsupported parameter while preserving its data",
                "log": "DefinitionObject Name and Description"
            }));
            continue;
        };
        validate_parameter_definition(
            parameter,
            &parameter_id,
            parameter_profile,
            definitions.get(&parameter_id).copied(),
            findings,
        );
        validate_parameter_value(
            parameter,
            &parameter_id,
            parameter_profile,
            by_path,
            references,
            findings,
            stats,
            fallbacks,
        );
    }
    for required in profile.required {
        if !actual.contains(*required) {
            findings.push(format!(
                "{} {} omits required parameter {}",
                group.path.display(),
                profile.name,
                parameter_profile(required).map_or(*required, |value| value.name)
            ));
        }
    }
    if profile.id == "2c50831c-572e-4042-b1dd-55ed0b7c49df" && !actual.contains(P_TITLE_FONT) {
        fallbacks.push(json!({
            "object": group.path,
            "parameter": "Font Name",
            "action": "use a suitable available default font",
            "log": "warn the user"
        }));
    }
}

fn validate_parameter_definition(
    parameter: &StoredObject,
    parameter_id: &str,
    profile: ParameterProfile,
    definition: Option<&StoredObject>,
    findings: &mut Findings,
) {
    let Some(definition) = definition else {
        findings.push(format!(
            "{} references {} absent from Dictionary::ParameterDefinitions",
            parameter.path.display(),
            profile.name
        ));
        return;
    };
    let declared_type = property(definition, 0x1f01)
        .and_then(weak_key)
        .and_then(|value| auid_string(value).ok());
    if declared_type.as_deref() != Some(profile.type_id) {
        findings.push(format!(
            "{} {} ({parameter_id}) has an incorrect ParameterDefinition type",
            definition.path.display(),
            profile.name
        ));
    }
}

fn definition_identity(definition: &StoredObject) -> (String, Option<String>) {
    let name = property(definition, 0x1b02)
        .and_then(|value| utf16_string(&value.data))
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "<unnamed definition>".to_owned());
    let description = property(definition, 0x1b03)
        .and_then(|value| utf16_string(&value.data))
        .filter(|value| !value.trim().is_empty());
    (name, description)
}

#[allow(clippy::too_many_arguments)]
fn validate_parameter_value(
    parameter: &StoredObject,
    parameter_id: &str,
    profile: ParameterProfile,
    by_path: &HashMap<&Path, &StoredObject>,
    references: &HashMap<(PathBuf, u16), Vec<PathBuf>>,
    findings: &mut Findings,
    stats: &mut Stats,
    fallbacks: &mut Vec<Value>,
) {
    match class_code(&parameter.class_id) {
        Some(0x003d) => {
            stats.constant_values += 1;
            if let Some(value) = property(parameter, 0x4d01) {
                validate_indirect(parameter, parameter_id, profile, value, findings);
            }
        }
        Some(0x003e) => {
            stats.varying_values += 1;
            let interpolation = property(parameter, 0x4e01)
                .and_then(weak_key)
                .and_then(|value| auid_string(value).ok());
            if interpolation
                .as_deref()
                .is_none_or(|value| !allowed_interpolation(value))
            {
                findings.push(format!(
                    "{} {} uses an interpolation outside the AS-01 profile",
                    parameter.path.display(),
                    profile.name
                ));
                fallbacks.push(json!({
                    "object": parameter.path,
                    "parameter_id": parameter_id,
                    "action": "use linear interpolation",
                    "log": "record unsupported interpolation fallback"
                }));
            }
            let points = references
                .get(&(parameter.path.clone(), 0x4e02))
                .cloned()
                .unwrap_or_default();
            stats.control_points += points.len();
            for path in points {
                let Some(point) = by_path.get(path.as_path()).copied() else {
                    continue;
                };
                if let Some(value) = property(point, 0x1a02) {
                    validate_indirect(point, parameter_id, profile, value, findings);
                }
            }
        }
        _ => findings.push(format!(
            "{} {} must use ConstantValue or VaryingValue",
            parameter.path.display(),
            profile.name
        )),
    }
}

fn validate_indirect(
    owner: &StoredObject,
    parameter_id: &str,
    profile: ParameterProfile,
    property: &StoredProperty,
    findings: &mut Findings,
) {
    let Some((type_id, payload)) = indirect(&property.data) else {
        findings.push(format!(
            "{} {} has a malformed AAF Indirect value",
            owner.path.display(),
            profile.name
        ));
        return;
    };
    if type_id != profile.type_id {
        findings.push(format!(
            "{} {} ({parameter_id}) Indirect value has type {type_id}, expected {}",
            owner.path.display(),
            profile.name,
            profile.type_id
        ));
        return;
    }
    let valid = match profile.value {
        ValueRule::Int32 => payload.len() == 4,
        ValueRule::Boolean => matches!(payload, [0] | [1]),
        ValueRule::String => utf16_string(payload).is_some(),
        ValueRule::Alignment => matches!(payload, [0] | [1] | [2]),
        ValueRule::Rational {
            minimum,
            maximum,
            exclude_zero,
        } => rational(payload).is_some_and(|value| {
            rational_at_least(value, minimum)
                && rational_at_most(value, maximum)
                && (!exclude_zero || value.0 != 0)
        }),
    };
    if !valid {
        findings.push(format!(
            "{} {} has a malformed or out-of-range value",
            owner.path.display(),
            profile.name
        ));
    }
}

fn parameter_profile(id: &str) -> Option<ParameterProfile> {
    let rational = |name, minimum, maximum, exclude_zero| ParameterProfile {
        name,
        type_id: TYPE_RATIONAL,
        value: ValueRule::Rational {
            minimum,
            maximum,
            exclude_zero,
        },
    };
    Some(match id {
        P_WIPE_NUMBER => ParameterProfile {
            name: "SMPTE Wipe Number",
            type_id: TYPE_INT32,
            value: ValueRule::Int32,
        },
        P_WIPE_REVERSE => ParameterProfile {
            name: "SMPTE Reverse",
            type_id: TYPE_BOOLEAN,
            value: ValueRule::Boolean,
        },
        P_INVERT_ALPHA | P_TITLE_BOLD | P_TITLE_ITALIC => ParameterProfile {
            name: match id {
                P_INVERT_ALPHA => "Invert Alpha",
                P_TITLE_BOLD => "Title Bold",
                _ => "Title Italic",
            },
            type_id: TYPE_BOOLEAN,
            value: ValueRule::Boolean,
        },
        P_TITLE_TEXT | P_TITLE_FONT | P_COLOR_DESC | P_INPUT_DESC | P_VIEW_DESC => {
            ParameterProfile {
                name: match id {
                    P_TITLE_TEXT => "Title Text",
                    P_TITLE_FONT => "Font Name",
                    P_COLOR_DESC => "Color Correction Description",
                    P_INPUT_DESC => "Color Input Description",
                    _ => "Color Viewing Description",
                },
                type_id: TYPE_STRING,
                value: ValueRule::String,
            }
        }
        P_TITLE_ALIGN => ParameterProfile {
            name: "Title Alignment",
            type_id: TYPE_TITLE_ALIGNMENT,
            value: ValueRule::Alignment,
        },
        P_LEVEL | P_CROP_LEFT | P_CROP_RIGHT | P_CROP_TOP | P_CROP_BOTTOM | P_ROTATION
        | P_PIN_TLX | P_PIN_TLY | P_PIN_TRX | P_PIN_TRY | P_PIN_BLX | P_PIN_BLY | P_PIN_BRX
        | P_PIN_BRY | P_LUMA_CLIP | P_PAN | P_OUTGOING | P_INCOMING | P_OPACITY | P_TITLE_R
        | P_TITLE_G | P_TITLE_B => rational(parameter_name(id), 0, 1, false),
        P_SPEED_RATIO => rational(
            "Speed Ratio",
            i64::from(i32::MIN),
            i64::from(i32::MAX),
            true,
        ),
        P_POSITION_X | P_POSITION_Y | P_TITLE_X | P_TITLE_Y | P_OFFSET_R | P_OFFSET_G
        | P_OFFSET_B => rational(
            parameter_name(id),
            i64::from(i32::MIN),
            i64::from(i32::MAX),
            false,
        ),
        P_SCALE_X | P_SCALE_Y | P_LUMA_LEVEL | P_AMPLITUDE | P_TITLE_SIZE | P_SLOPE_R
        | P_SLOPE_G | P_SLOPE_B | P_POWER_R | P_POWER_G | P_POWER_B | P_SATURATION => {
            rational(parameter_name(id), 0, i64::from(i32::MAX), false)
        }
        _ => return None,
    })
}

fn parameter_name(id: &str) -> &'static str {
    match id {
        P_LEVEL => "Level",
        P_CROP_LEFT => "Crop Left",
        P_CROP_RIGHT => "Crop Right",
        P_CROP_TOP => "Crop Top",
        P_CROP_BOTTOM => "Crop Bottom",
        P_ROTATION => "Rotation",
        P_PIN_TLX => "Pin Top Left X",
        P_PIN_TLY => "Pin Top Left Y",
        P_PIN_TRX => "Pin Top Right X",
        P_PIN_TRY => "Pin Top Right Y",
        P_PIN_BLX => "Pin Bottom Left X",
        P_PIN_BLY => "Pin Bottom Left Y",
        P_PIN_BRX => "Pin Bottom Right X",
        P_PIN_BRY => "Pin Bottom Right Y",
        P_LUMA_CLIP => "Luminance Key Clip",
        P_PAN => "Pan",
        P_OUTGOING => "Outgoing Level",
        P_INCOMING => "Incoming Level",
        P_OPACITY => "Opacity Level",
        P_TITLE_R => "Title Font Color R",
        P_TITLE_G => "Title Font Color G",
        P_TITLE_B => "Title Font Color B",
        P_POSITION_X => "Position Offset X",
        P_POSITION_Y => "Position Offset Y",
        P_TITLE_X => "Title Position X",
        P_TITLE_Y => "Title Position Y",
        P_OFFSET_R => "Color Offset R",
        P_OFFSET_G => "Color Offset G",
        P_OFFSET_B => "Color Offset B",
        P_SCALE_X => "Scale X",
        P_SCALE_Y => "Scale Y",
        P_LUMA_LEVEL => "Luminance Key Level",
        P_AMPLITUDE => "Amplitude",
        P_TITLE_SIZE => "Title Font Size",
        P_SLOPE_R => "Color Slope R",
        P_SLOPE_G => "Color Slope G",
        P_SLOPE_B => "Color Slope B",
        P_POWER_R => "Color Power R",
        P_POWER_G => "Color Power G",
        P_POWER_B => "Color Power B",
        P_SATURATION => "Color Saturation",
        _ => "Effect Parameter",
    }
}

fn allowed_interpolation(id: &str) -> bool {
    matches!(
        id,
        "5b6c85a4-0ede-11d3-80a9-006008143e6f"
            | "5b6c85a5-0ede-11d3-80a9-006008143e6f"
            | "5b6c85a6-0ede-11d3-80a9-006008143e6f"
            | "15829ec3-1f24-458a-960d-c65bb23c2aa1"
            | "c09153f7-bd18-4e5a-ad09-cbdd654fa001"
    )
}

fn weak_set_keys(
    object: &StoredObject,
    pid: u16,
    streams: &HashMap<PathBuf, Vec<u8>>,
) -> Result<HashSet<String>, String> {
    let Some(property) = property(object, pid) else {
        return Ok(HashSet::new());
    };
    if property.format != SF_WEAK_SET {
        return Err("property is not a weak-reference set".to_owned());
    }
    let name = utf16_string(&property.data)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "invalid weak-set index name".to_owned())?;
    let bytes = streams
        .get(&object.path.join(format!("{name} index")))
        .ok_or_else(|| "missing weak-set index".to_owned())?;
    if bytes.len() < 9 {
        return Err("truncated weak-set index".to_owned());
    }
    let count = read_u32(bytes, 0)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| "invalid weak-set count".to_owned())?;
    let key_size = usize::from(bytes[8]);
    if key_size != 16 {
        return Err(format!("unsupported weak-set key size {key_size}"));
    }
    let expected = count
        .checked_mul(key_size)
        .and_then(|value| value.checked_add(9))
        .ok_or_else(|| "weak-set length overflow".to_owned())?;
    if bytes.len() != expected {
        return Err(format!(
            "weak-set declares {count} entries but has {} bytes",
            bytes.len()
        ));
    }
    let mut result = HashSet::with_capacity(count);
    for key in bytes[9..].chunks_exact(16) {
        let id = auid_string(key).map_err(|()| "invalid weak-set AUID".to_owned())?;
        result.insert(id);
    }
    Ok(result)
}

#[cfg(test)]
fn all_parameter_ids() -> &'static [&'static str] {
    &[
        P_LEVEL,
        P_WIPE_NUMBER,
        P_WIPE_REVERSE,
        P_SPEED_RATIO,
        P_POSITION_X,
        P_POSITION_Y,
        P_CROP_LEFT,
        P_CROP_RIGHT,
        P_CROP_TOP,
        P_CROP_BOTTOM,
        P_SCALE_X,
        P_SCALE_Y,
        P_ROTATION,
        P_PIN_TLX,
        P_PIN_TLY,
        P_PIN_TRX,
        P_PIN_TRY,
        P_PIN_BLX,
        P_PIN_BLY,
        P_PIN_BRX,
        P_PIN_BRY,
        P_INVERT_ALPHA,
        P_LUMA_LEVEL,
        P_LUMA_CLIP,
        P_AMPLITUDE,
        P_PAN,
        P_OUTGOING,
        P_INCOMING,
        P_OPACITY,
        P_TITLE_TEXT,
        P_TITLE_FONT,
        P_TITLE_SIZE,
        P_TITLE_R,
        P_TITLE_G,
        P_TITLE_B,
        P_TITLE_ALIGN,
        P_TITLE_BOLD,
        P_TITLE_ITALIC,
        P_TITLE_X,
        P_TITLE_Y,
        P_SLOPE_R,
        P_SLOPE_G,
        P_SLOPE_B,
        P_OFFSET_R,
        P_OFFSET_G,
        P_OFFSET_B,
        P_POWER_R,
        P_POWER_G,
        P_POWER_B,
        P_SATURATION,
        P_COLOR_DESC,
        P_INPUT_DESC,
        P_VIEW_DESC,
    ]
}

fn ancestor_with_class(
    object: &StoredObject,
    by_path: &HashMap<&Path, &StoredObject>,
    code: u16,
) -> bool {
    let mut path = object.path.parent();
    while let Some(candidate) = path {
        if by_path
            .get(candidate)
            .is_some_and(|value| class_code(&value.class_id) == Some(code))
        {
            return true;
        }
        path = candidate.parent();
    }
    false
}

fn indirect(bytes: &[u8]) -> Option<(String, &[u8])> {
    if bytes.len() < 17 || bytes[0] != 0x4c {
        return None;
    }
    Some((auid_string(&bytes[1..17]).ok()?, &bytes[17..]))
}

fn rational(bytes: &[u8]) -> Option<(i32, i32)> {
    let numerator = i32::from_le_bytes(bytes.get(..4)?.try_into().ok()?);
    let denominator = i32::from_le_bytes(bytes.get(4..8)?.try_into().ok()?);
    (bytes.len() == 8 && denominator != 0).then_some((numerator, denominator))
}

fn rational_at_least(value: (i32, i32), bound: i64) -> bool {
    let (numerator, denominator) = normalize_rational(value);
    i128::from(numerator) >= i128::from(bound) * i128::from(denominator)
}

fn rational_at_most(value: (i32, i32), bound: i64) -> bool {
    let (numerator, denominator) = normalize_rational(value);
    i128::from(numerator) <= i128::from(bound) * i128::from(denominator)
}

fn normalize_rational((numerator, denominator): (i32, i32)) -> (i64, i64) {
    if denominator < 0 {
        (-i64::from(numerator), -i64::from(denominator))
    } else {
        (i64::from(numerator), i64::from(denominator))
    }
}

fn property(object: &StoredObject, pid: u16) -> Option<&StoredProperty> {
    object
        .properties
        .iter()
        .find(|property| property.pid == pid)
}

fn weak_key(property: &StoredProperty) -> Option<&[u8]> {
    if !matches!(property.format, SF_WEAK_REF | SF_WEAK_VECTOR | SF_WEAK_SET)
        || property.data.len() < 5
    {
        return None;
    }
    let size = usize::from(property.data[4]);
    matches!(size, 16 | 32)
        .then_some(())
        .and_then(|()| property.data.get(5..5 + size))
        .filter(|_| property.data.len() == 5 + size)
}

fn direct_auid(property: &StoredProperty) -> Option<String> {
    (property.data.len() == 16)
        .then(|| auid_string(&property.data).ok())
        .flatten()
}

fn auid_string(bytes: &[u8]) -> Result<String, ()> {
    let value: [u8; 16] = bytes.try_into().map_err(|_| ())?;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        value[3], value[2], value[1], value[0], value[5], value[4], value[7], value[6],
        value[8], value[9], value[10], value[11], value[12], value[13], value[14], value[15]
    ))
}

fn class_code(class_id: &str) -> Option<u16> {
    const PREFIX: &str = "0d010101-0101-";
    const SUFFIX: &str = "-060e-2b3402060101";
    let value = class_id.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    (value.len() == 4)
        .then(|| u16::from_str_radix(&value[..2], 16).ok())
        .flatten()
}

fn signed_i32(property: &StoredProperty) -> Option<i32> {
    let bytes: [u8; 4] = property.data.as_slice().try_into().ok()?;
    Some(i32::from_le_bytes(bytes))
}

fn unsigned_u32(property: &StoredProperty) -> Option<u32> {
    let bytes: [u8; 4] = property.data.as_slice().try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn boolean(property: &StoredProperty) -> Option<bool> {
    match property.data.as_slice() {
        [0] => Some(false),
        [1] => Some(true),
        _ => None,
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn utf16_string(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 || !bytes.len().is_multiple_of(2) || !bytes.ends_with(&[0, 0]) {
        return None;
    }
    let mut units = Vec::with_capacity(bytes.len() / 2 - 1);
    for pair in bytes.chunks_exact(2) {
        let unit = u16::from_le_bytes([pair[0], pair[1]]);
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    String::from_utf16(&units).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn little_endian_auid(value: &str) -> Vec<u8> {
        let hex: String = value
            .chars()
            .filter(|character| *character != '-')
            .collect();
        let mut bytes = (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
            .collect::<Vec<_>>();
        bytes[..4].reverse();
        bytes[4..6].reverse();
        bytes[6..8].reverse();
        bytes
    }

    fn indirect_rational(type_id: &str, numerator: i32, denominator: i32) -> Vec<u8> {
        let mut value = vec![0x4c];
        value.extend(little_endian_auid(type_id));
        value.extend(numerator.to_le_bytes());
        value.extend(denominator.to_le_bytes());
        value
    }

    #[test]
    fn validates_indirect_rational_boundaries() {
        let valid = indirect_rational(TYPE_RATIONAL, 1, 1);
        let (type_id, payload) = indirect(&valid).unwrap();
        assert_eq!(type_id, TYPE_RATIONAL);
        assert_eq!(rational(payload), Some((1, 1)));
        assert!(rational_at_least((0, -1), 0));
        assert!(rational_at_most((-1, -1), 1));
        assert!(!rational_at_most((2, 1), 1));
    }

    #[test]
    fn exposes_complete_as01_and_as05_profile_sets() {
        assert_eq!(
            OPERATIONS
                .iter()
                .filter(|value| matches!(value.protocol, Protocol::As01))
                .count(),
            20
        );
        assert_eq!(
            OPERATIONS
                .iter()
                .filter(|value| matches!(value.protocol, Protocol::As05))
                .count(),
            3
        );
        assert_eq!(all_parameter_ids().len(), 53);
    }

    #[test]
    fn rejects_out_of_range_effect_scalars() {
        let owner = StoredObject {
            path: PathBuf::from("/value"),
            class_id: String::new(),
            properties: Vec::new(),
        };
        for (id, bytes) in [
            (P_LEVEL, indirect_rational(TYPE_RATIONAL, 2, 1)),
            (P_SPEED_RATIO, indirect_rational(TYPE_RATIONAL, 0, 1)),
        ] {
            let profile = parameter_profile(id).unwrap();
            let property = StoredProperty {
                pid: 0,
                format: 0x82,
                data: bytes,
            };
            let mut findings = Findings::default();
            validate_indirect(&owner, id, profile, &property, &mut findings);
            assert_eq!(findings.total, 1, "{id}");
        }

        for (id, type_id, payload) in [
            (P_WIPE_REVERSE, TYPE_BOOLEAN, vec![2]),
            (P_TITLE_ALIGN, TYPE_TITLE_ALIGNMENT, vec![3]),
            (P_TITLE_TEXT, TYPE_STRING, vec![b'A', 0]),
        ] {
            let mut data = vec![0x4c];
            data.extend(little_endian_auid(type_id));
            data.extend(payload);
            let property = StoredProperty {
                pid: 0,
                format: 0x82,
                data,
            };
            let mut findings = Findings::default();
            validate_indirect(
                &owner,
                id,
                parameter_profile(id).unwrap(),
                &property,
                &mut findings,
            );
            assert_eq!(findings.total, 1, "{id}");
        }
    }

    #[test]
    fn permits_only_the_five_as01_interpolators() {
        for id in [
            "5b6c85a4-0ede-11d3-80a9-006008143e6f",
            "5b6c85a5-0ede-11d3-80a9-006008143e6f",
            "5b6c85a6-0ede-11d3-80a9-006008143e6f",
            "15829ec3-1f24-458a-960d-c65bb23c2aa1",
            "c09153f7-bd18-4e5a-ad09-cbdd654fa001",
        ] {
            assert!(allowed_interpolation(id), "{id}");
        }
        assert!(!allowed_interpolation(
            "5b6c85a3-0ede-11d3-80a9-006008143e6f"
        ));
    }
}
