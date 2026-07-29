#!/usr/bin/env bash
set -euo pipefail

# Public pyaaf2 interoperability fixtures (MIT License), pinned to one commit.
readonly PYAAF2_COMMIT="08dcc3dfe823ea5781db1cb657d9c2606557fcdc"
readonly BASE_URL="https://raw.githubusercontent.com/markreidvfx/pyaaf2/${PYAAF2_COMMIT}/tests/test_files"
readonly CACHE_DIR="${XDG_CACHE_HOME:-${HOME}/.cache}/forge-normalizer/aaf-${PYAAF2_COMMIT}"
readonly REPORT_DIR="${CACHE_DIR}/reports"
readonly SDK_FIXTURE="tests/fixtures/aaf-sdk-export-as05-effects.aaf.xz"
readonly SDK_AAF_SHA256="6c57cad838a466c6fc98b30dfc7144f66b0895079bacc09232663668a460d453"

readonly FIXTURES=(
    "empty.aaf:0a83da0fc7e2de89391c101c4b668dac6f857fbd721504e6d8f6b408b6e84a05"
    "sector_size_512.aaf:e4991b3e1272a8d23efb16a6fa11d560184e4024ddee41cbe32277345ce9af3b"
    "test_file_01.aaf:be513199eb84f554421da2523b202183a43c8a68e9703f9d0e8541f6e831b9e9"
)

mkdir -p "${CACHE_DIR}" "${REPORT_DIR}"

for fixture in "${FIXTURES[@]}"; do
    name="${fixture%%:*}"
    digest="${fixture##*:}"
    path="${CACHE_DIR}/${name}"
    if [[ ! -f "${path}" ]]; then
        curl --fail --location --retry 3 --retry-all-errors \
            --connect-timeout 15 "${BASE_URL}/${name}" --output "${path}"
    fi
    printf '%s  %s\n' "${digest}" "${path}" | sha256sum --check -
done

cargo build --no-default-features --bin forge-container-qc

for name in empty.aaf sector_size_512.aaf; do
    report="${REPORT_DIR}/${name}.json"
    target/debug/forge-container-qc "${CACHE_DIR}/${name}" > "${report}"
    jq --exit-status '
        .passed == true
        and .properties.method == "forge-aaf-effect-profiles-metadictionary-object-model-edit-protocol-v3"
        and ([.layers[].checks[] | select(.passed == false)] | length) == 0
        and .properties.object_model.objects > 0
        and .properties.object_model.strong_references
            == .properties.object_model.objects
    ' "${report}" > /dev/null
done

# This fixture labels itself OpEditProtocol but contains unnamed essence
# tracks. The core object graph must remain valid while the protocol layer
# reports the normative MobSlot::Name violation.
nonconformant_report="${REPORT_DIR}/test_file_01.aaf.json"
if target/debug/forge-container-qc "${CACHE_DIR}/test_file_01.aaf" \
    > "${nonconformant_report}"; then
    echo "expected test_file_01.aaf to fail Edit Protocol validation" >&2
    exit 1
fi
jq --exit-status '
    .passed == false
    and ([.layers[].checks[]
        | select(.passed == false)
        | .rule_id] == [
            "FORGE-AAF-EFFECT-PROFILES",
            "FORGE-AAF-EDIT-PROTOCOL"
        ])
    and ([.layers[].checks[]
        | select(.rule_id == "FORGE-AAF-OBJECT-OWNERSHIP")
        | .passed] == [true])
    and ([.layers[].checks[]
        | select(.rule_id == "FORGE-AAF-STRONG-REFERENCES")
        | .passed] == [true])
    and ([.layers[].checks[]
        | select(.rule_id == "FORGE-AAF-METADICTIONARY-DEFINITIONS")
        | .passed] == [true])
    and ([.layers[].checks[]
        | select(.rule_id == "FORGE-AAF-EXTENSION-PROPERTY-TYPES")
        | .passed] == [true])
    and .properties.object_model.meta_dictionary.class_definitions == 79
    and .properties.object_model.meta_dictionary.type_definitions == 146
    and .properties.object_model.meta_dictionary.extension_property_definitions == 71
    and .properties.object_model.meta_dictionary.interpreted_extension_values == 1116
    and .properties.object_model.effect_profiles.profiled_operations == 20
    and .properties.object_model.effect_profiles.as01_operations == 20
    and .properties.object_model.effect_profiles.unsupported_operations == 8
    and (.properties.object_model.effect_profiles.fallback_candidates | length) > 0
' "${nonconformant_report}" > /dev/null

# Reference output from the official AAF SDK ExportAS05Effects example. The
# example itself omits Edit Protocol SlotName values, but its complete AS-05
# operation and parameter dictionaries must pass the dedicated effect profile.
sdk_aaf="${CACHE_DIR}/aaf-sdk-export-as05-effects.aaf"
xz --decompress --stdout "${SDK_FIXTURE}" > "${sdk_aaf}"
printf '%s  %s\n' "${SDK_AAF_SHA256}" "${sdk_aaf}" | sha256sum --check -
sdk_report="${REPORT_DIR}/aaf-sdk-export-as05-effects.aaf.json"
if target/debug/forge-container-qc "${sdk_aaf}" > "${sdk_report}"; then
    echo "expected AAF SDK example to retain its known SlotName finding" >&2
    exit 1
fi
jq --exit-status '
    .passed == false
    and ([.layers[].checks[]
        | select(.passed == false)
        | .rule_id] == ["FORGE-AAF-EDIT-PROTOCOL"])
    and ([.layers[].checks[]
        | select(.rule_id == "FORGE-AAF-EFFECT-PROFILES")
        | .passed] == [true])
    and .properties.object_model.effect_profiles.supported_profiles == 23
    and .properties.object_model.effect_profiles.as05_profiles == 3
    and .properties.object_model.effect_profiles.profiled_operations == 3
    and .properties.object_model.effect_profiles.as05_operations == 3
    and .properties.object_model.effect_profiles.parameters == 25
    and .properties.object_model.effect_profiles.constant_values == 25
    and .properties.object_model.effect_profiles.varying_values == 0
    and .properties.object_model.effect_profiles.unsupported_operations == 0
' "${sdk_report}" > /dev/null

echo "AAF public-fixture interoperability checks passed"
