#!/usr/bin/env bash
set -euo pipefail

# AOMedia libiamf v1.1.0 interoperability vectors, pinned to its release commit.
readonly LIBIAMF_COMMIT="f06e919e2ad5502a2adc4bdd4e146f2e7e7ffb63"
readonly BASE_URL="https://raw.githubusercontent.com/AOMediaCodec/libiamf/${LIBIAMF_COMMIT}/tests"
readonly CACHE_DIR="${XDG_CACHE_HOME:-${HOME}/.cache}/forge-normalizer/iamf-${LIBIAMF_COMMIT}"
readonly REPORT_DIR="${CACHE_DIR}/reports"
readonly FIXTURES=(
    "test_000000_3_f.mp4:67b7259727f1d757900ec8196856923c374ba424bf81f4cdb738b8f196d3432c"
    "test_000002_f.mp4:7c3f6231409dc8645e4aa35332b610f92f22b8ed6a48d50176e89013fedca669"
    "test_000005_f.mp4:5566e5d098eca9d70dc177c8d4aff09cabb9347ab6cab54f72379a513e8524bf"
    "test_000006_f.mp4:2289a249a84dd7ea0bed54e004fb2ae40c4ea486a4b9c04e276a340877d174ca"
    "test_000020_f.mp4:706c342239398fb46e6652764b34d51fa1081a93a5dc4e027b55dc43e802b35a"
)

mkdir -p "${CACHE_DIR}" "${REPORT_DIR}"

for fixture in "${FIXTURES[@]}"; do
    name="${fixture%%:*}"
    digest="${fixture##*:}"
    path="${CACHE_DIR}/${name}"
    if [[ ! -f "${path}" ]] ||
        ! printf '%s  %s\n' "${digest}" "${path}" | sha256sum --check --status; then
        curl --fail --location --retry 3 --retry-all-errors \
            --connect-timeout 15 "${BASE_URL}/${name}" --output "${path}"
    fi
    printf '%s  %s\n' "${digest}" "${path}" | sha256sum --check -
done

cargo build --no-default-features --bin forge-container-qc

for name in test_000005_f.mp4 test_000006_f.mp4; do
    report="${REPORT_DIR}/${name}.json"
    target/debug/forge-container-qc "${CACHE_DIR}/${name}" > "${report}"
    jq --exit-status '
        .passed == true
        and .properties.fragmented == true
        and .properties.movie_fragments == 1
        and .properties.iamf_tracks[0].validated_samples == 125
        and ([.layers[].checks[]
            | select(.rule_id == "FORGE-ISOBMFF-IAMF-SAMPLE-DATA"
                or .rule_id == "FORGE-ISOBMFF-IAMF-SAMPLE-TIMING"
                or .rule_id == "FORGE-ISOBMFF-IAMF-ROLL-GROUP"
                or .rule_id == "FORGE-ISOBMFF-IAMF-SYNC-CTS")
            | .passed] == [true, true, true, true])
    ' "${report}" > /dev/null
done

declare -A EXPECTED_FAILURE=(
    ["test_000000_3_f.mp4"]="FORGE-ISOBMFF-IAMF-SAMPLE-TIMING"
    ["test_000002_f.mp4"]="FORGE-ISOBMFF-IAMF-TRIM"
    ["test_000020_f.mp4"]="FORGE-ISOBMFF-IAMF-ROLL-GROUP"
)

for name in "${!EXPECTED_FAILURE[@]}"; do
    report="${REPORT_DIR}/${name}.json"
    if target/debug/forge-container-qc "${CACHE_DIR}/${name}" > "${report}"; then
        echo "expected ${name} to retain its IAMF conformance finding" >&2
        exit 1
    fi
    rule="${EXPECTED_FAILURE[${name}]}"
    jq --exit-status --arg rule "${rule}" '
        .passed == false
        and ([.layers[].checks[] | select(.passed == false) | .rule_id] == [$rule])
        and .properties.fragmented == true
        and .properties.iamf_tracks[0].validated_samples > 0
    ' "${report}" > /dev/null
done

echo "AOMedia libiamf v1.1.0 fragmented ISO-BMFF checks passed"
