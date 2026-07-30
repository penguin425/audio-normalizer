#!/usr/bin/env bash
set -euo pipefail

# AOMedia libiamf v1.1.0 interoperability vectors, pinned to its release commit.
readonly LIBIAMF_COMMIT="f06e919e2ad5502a2adc4bdd4e146f2e7e7ffb63"
readonly BASE_URL="https://raw.githubusercontent.com/AOMediaCodec/libiamf/${LIBIAMF_COMMIT}/tests"
readonly CACHE_DIR="${XDG_CACHE_HOME:-${HOME}/.cache}/forge-normalizer/iamf-${LIBIAMF_COMMIT}"
readonly REPORT_DIR="${CACHE_DIR}/reports"
readonly FIXTURES=(
    "test_000000_3.iamf:4b7483a09118b747f7f161c4fbcee5a73c1077ce1b0a1a5e2a37a2151c38f740"
    "test_000000_3_s.mp4:1e187382403edf4d9ac9c5c407384180079aa615ff984de684e9929d51f06e9d"
    "test_000000_3_f.mp4:67b7259727f1d757900ec8196856923c374ba424bf81f4cdb738b8f196d3432c"
    "test_000002.iamf:f14f7a55a4e6f3919c67364dfbcfc2a2f754b869438182477d4e8fc6acb69f2a"
    "test_000002_s.mp4:a49155419f9998bd736d16ddf68fcae4e6877b342697fed635b6d69242add4b2"
    "test_000002_f.mp4:7c3f6231409dc8645e4aa35332b610f92f22b8ed6a48d50176e89013fedca669"
    "test_000003.iamf:6f455cca4198118da2eafcc8212edd46f7ec9d5a88ca189429582ea554b5cc0f"
    "test_000003_s.mp4:00382e5fb8bf106b933a4800a21ed861633c08280f961adfaa2e220a75d3eece"
    "test_000003_f.mp4:a4dc98a2b40755798ba20ecd6e3e60f81f1ebff30e9d769e71c84f696f44a1d9"
    "test_000005.iamf:2889b9d1fc4bc33d4591e4097d7e1d4da6df445cba42f746b03dc9cf239acd9c"
    "test_000005_s.mp4:79a2f64fc09eead2de12ce10ecc08172c4ed54a12c8b2a0fbd9e89aad55dd81c"
    "test_000005_f.mp4:5566e5d098eca9d70dc177c8d4aff09cabb9347ab6cab54f72379a513e8524bf"
    "test_000006.iamf:38e8f49a107287372bf089951aba0199e0f9de56e2cb41bf9511027051f2d7dc"
    "test_000006_s.mp4:514b1a71e5c7820e9fe26bd27cd8c6ff7a3c06ccf44a57b51e74979b0850781a"
    "test_000006_f.mp4:2289a249a84dd7ea0bed54e004fb2ae40c4ea486a4b9c04e276a340877d174ca"
    "test_000020.iamf:ac94884120f48413a9f972cddd3ea1f6f777441f32a0b4ef5f52492dfcb5f42d"
    "test_000020_s.mp4:cde61995e666b4d893fbbd87d5612a125606b5cb9c2e5cb9cbd0ef3fe7fa7155"
    "test_000020_f.mp4:706c342239398fb46e6652764b34d51fa1081a93a5dc4e027b55dc43e802b35a"
    "test_000038.iamf:35021bf384efc481f0306d7375d664309ea4ff4dac8b62c88b76a008d30f4555"
    "test_000038_s.mp4:544c0aca3cd8ee5211b326792d83f3928c38dbd402d3f4906e729c4612d5efa0"
    "test_000038_f.mp4:5ef06fe33874c9f68d6c858890c76c9d8b3e7b69ca3eefe4382e4dba38e38ebb"
    "test_000042.iamf:764cc54d5004894f5b708cc3a78a59bf4043c90496b706892c69dda5a98b780a"
    "test_000042_s.mp4:7c6fef1d2a2a4f1b02142606b2a91b9a4dc627d8b4f9e70c0a2969ea868478f8"
    "test_000042_f.mp4:d6f625eea98ca3474d05635fa1c8b2fca03cf1a213513464bfbba34314860d6c"
    "test_000060.iamf:11f8bd9078139ab1709d4fb8eae8365b7b6074b1b02beebb4202e54b5011f341"
    "test_000060_s.mp4:c642ca4705424591c512e8deb720864568c15964092666fdd846c0603238fca6"
    "test_000060_f.mp4:49ef093dec07fe364151a1f86310846545de40a4b478fb519f5d8bed3410fff0"
    "test_000062.iamf:7be1c71c8e2df4a2113f737eb18b05384c1712994567b03b8ce4766ea16b890c"
    "test_000062_s.mp4:5d7e606c28b44b7396ad5684d51eef7d089a6d8fd35fcc005838b87bbdba517d"
    "test_000062_f.mp4:edef009b806d4a8f18316ae03566d71ade7f77f4cf2bb2037b7c540814b3b47d"
    "test_000063.iamf:74aac487486cd2c9ebe3eecc437a36df0931b06cca0366d3af633c8b9c34bf21"
    "test_000063_s.mp4:43fabbe931d520d31563088573b34849fa6f8942dfe5f4f7868b7d56d1b8a739"
    "test_000063_f.mp4:bab71d4c39976e2954b813f4ffb183019b8f64de73e1394dc279c2535b191676"
    "test_000066.iamf:0735d0e1eaa17bff3016ecff6298f970e0f55ccca8fee71ee53f061b562301f7"
    "test_000066_s.mp4:eccdbbe1e3c48cd8a48304d940a7faae2d8b5bec5e3172b04a3823d012ee32f3"
    "test_000066_f.mp4:156d27c61ac41c9a7efefcced6e9308f64d9564efecdeb0b2f65c8798fa0948b"
    "test_000071.iamf:960b8abbeebf23a9815897ce79645d201c782ffc4e1805d222b6d3b8c59c7130"
    "test_000071_s.mp4:c10b6ba99d8abce300b5de72c0687bacb9b7d0aeae091acfa5392568f5841d83"
    "test_000071_f.mp4:685a0eb600220114513063f57721c75256158c0ff56b5a0ee142028b4f43f6c4"
    "test_000088.iamf:653a3fd323cb2260922e9650253608380c71242e0561d9850d99a18b50e4ea54"
    "test_000088_s.mp4:6078c1ff0bd28c45fb471c66433a885ed69a3fff278540c05b3cfab6656a5cae"
    "test_000088_f.mp4:a2ad854658b4a199f295df82a9a619a37099846250741911bb72daa25443c928"
)
readonly VALID_STANDALONE=(
    test_000000_3.iamf test_000002.iamf test_000003.iamf test_000005.iamf
    test_000006.iamf test_000020.iamf test_000038.iamf test_000042.iamf
    test_000060.iamf test_000062.iamf test_000066.iamf test_000071.iamf
    test_000088.iamf
)
readonly VALID_ISOBMFF=(
    test_000005_s.mp4 test_000005_f.mp4
    test_000006_s.mp4 test_000006_f.mp4
    test_000038_s.mp4 test_000038_f.mp4
    test_000042_s.mp4 test_000042_f.mp4
    test_000060_s.mp4 test_000060_f.mp4
    test_000062_s.mp4 test_000062_f.mp4
    test_000066_s.mp4 test_000066_f.mp4
    test_000071_s.mp4 test_000071_f.mp4
    test_000088_s.mp4 test_000088_f.mp4
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

declare -A STATUS=()
for fixture in "${FIXTURES[@]}"; do
    name="${fixture%%:*}"
    report="${REPORT_DIR}/${name}.json"
    set +e
    target/debug/forge-container-qc "${CACHE_DIR}/${name}" > "${report}"
    status=$?
    set -e
    if (( status > 1 )); then
        printf 'malformed-input or I/O failure for %s (exit %d)\n' "${name}" "${status}" >&2
        exit 1
    fi
    STATUS["${name}"]="${status}"
done

for name in "${VALID_STANDALONE[@]}"; do
    report="${REPORT_DIR}/${name}.json"
    if (( STATUS["${name}"] != 0 )); then
        printf 'expected valid standalone vector %s to pass\n' "${name}" >&2
        exit 1
    fi
    jq --exit-status '
        .passed == true
        and .format == "iamf"
        and .properties.descriptor_sets >= 1
        and .properties.temporal_units >= 1
        and ([.layers[].checks[]
            | select(.rule_id == "FORGE-IAMF-OBU-BOUNDS"
                or .rule_id == "FORGE-IAMF-PROFILE-CONSTRAINTS"
                or .rule_id == "FORGE-IAMF-DESCRIPTOR-LINKS"
                or .rule_id == "FORGE-IAMF-AUDIO-FRAME-LINKS"
                or .rule_id == "FORGE-IAMF-PARAMETER-BLOCK"
                or .rule_id == "FORGE-IAMF-TIMELINE")
            | .passed] | length == 6 and all)
    ' "${report}" > /dev/null
done

for name in "${VALID_ISOBMFF[@]}"; do
    report="${REPORT_DIR}/${name}.json"
    if (( STATUS["${name}"] != 0 )); then
        printf 'expected valid ISO-BMFF vector %s to pass\n' "${name}" >&2
        exit 1
    fi
    if [[ "${name}" == *_f.mp4 ]]; then
        fragmented=true
    else
        fragmented=false
    fi
    jq --exit-status --argjson fragmented "${fragmented}" '
        .passed == true
        and .format == "isobmff"
        and .properties.fragmented == $fragmented
        and .properties.iamf_tracks[0].validated_samples > 0
        and ([.layers[].checks[]
            | select(.rule_id == "FORGE-ISOBMFF-IAMF-SAMPLE-DATA"
                or .rule_id == "FORGE-ISOBMFF-IAMF-SAMPLE-TIMING"
                or .rule_id == "FORGE-ISOBMFF-IAMF-ROLL-GROUP"
                or .rule_id == "FORGE-ISOBMFF-IAMF-TRIM"
                or .rule_id == "FORGE-ISOBMFF-IAMF-SYNC-CTS")
            | .passed] | length == 5 and all)
    ' "${report}" > /dev/null
done

# The pinned upstream set intentionally includes an invalid duplicate-anchor
# vector. Four otherwise-valid upstream vectors also retain packaging findings:
# their MP4 variants omit required edit-list/roll evidence or expose an
# inconsistent sample duration. Keep those distinctions exact and auditable.
declare -A EXPECTED_FAILURE=(
    ["test_000000_3_s.mp4"]='["FORGE-ISOBMFF-IAMF-SAMPLE-TIMING"]'
    ["test_000000_3_f.mp4"]='["FORGE-ISOBMFF-IAMF-SAMPLE-TIMING"]'
    ["test_000002_s.mp4"]='["FORGE-ISOBMFF-IAMF-TRIM"]'
    ["test_000002_f.mp4"]='["FORGE-ISOBMFF-IAMF-TRIM"]'
    ["test_000003_s.mp4"]='["FORGE-ISOBMFF-IAMF-TRIM"]'
    ["test_000003_f.mp4"]='["FORGE-ISOBMFF-IAMF-TRIM"]'
    ["test_000020_s.mp4"]='["FORGE-ISOBMFF-DURATION-XCHECK","FORGE-ISOBMFF-IAMF-ROLL-GROUP"]'
    ["test_000020_f.mp4"]='["FORGE-ISOBMFF-IAMF-ROLL-GROUP"]'
    ["test_000063.iamf"]='["FORGE-IAMF-DESCRIPTOR-LINKS","FORGE-IAMF-MIX-PRESENTATION","FORGE-IAMF-PROFILE-CONSTRAINTS"]'
    ["test_000063_s.mp4"]='["FORGE-IAMF-DESCRIPTOR-LINKS","FORGE-IAMF-MIX-PRESENTATION","FORGE-IAMF-PROFILE-CONSTRAINTS"]'
    ["test_000063_f.mp4"]='["FORGE-IAMF-DESCRIPTOR-LINKS","FORGE-IAMF-MIX-PRESENTATION","FORGE-IAMF-PROFILE-CONSTRAINTS"]'
)

classified=$((
    ${#VALID_STANDALONE[@]} +
    ${#VALID_ISOBMFF[@]} +
    ${#EXPECTED_FAILURE[@]}
))
if (( ${#STATUS[@]} != ${#FIXTURES[@]} || classified != ${#FIXTURES[@]} )); then
    printf 'fixture names must be unique and every artifact must have one expectation\n' >&2
    exit 1
fi

for name in "${!EXPECTED_FAILURE[@]}"; do
    report="${REPORT_DIR}/${name}.json"
    if (( STATUS["${name}"] != 1 )); then
        printf 'expected %s to retain its exact IAMF finding\n' "${name}" >&2
        exit 1
    fi
    jq --exit-status --argjson expected "${EXPECTED_FAILURE[${name}]}" '
        .passed == false
        and ([.layers[].checks[] | select(.passed == false) | .rule_id] | unique) == $expected
    ' "${report}" > /dev/null
done

# Pin observable coverage for the newly added OAR/IAMF verification vectors:
# LPCM, mono/projection Ambisonics, localized annotations, anchored loudness,
# and STEP/LINEAR/BEZIER parameter animation with variable subblock counts.
jq --exit-status '
    .properties.codec_configs[0].codec_id == "ipcm"
    and .properties.audio_elements[0].output_channels == 2
' "${REPORT_DIR}/test_000003.iamf.json" > /dev/null
jq --exit-status '
    .properties.audio_elements[0].ambisonics_mode == "mono"
    and .properties.audio_elements[0].output_channels == 4
' "${REPORT_DIR}/test_000038.iamf.json" > /dev/null
jq --exit-status '
    .properties.audio_elements[0].ambisonics_mode == "projection"
    and .properties.audio_elements[0].output_channels == 4
' "${REPORT_DIR}/test_000042.iamf.json" > /dev/null
jq --exit-status '
    .properties.mix_presentations[0].annotation_languages == ["en-us", "es-mx"]
' "${REPORT_DIR}/test_000060.iamf.json" > /dev/null
jq --exit-status '
    .properties.mix_presentations[0].sub_mixes[0].layouts[0].info_type == 2
    and (.properties.mix_presentations[0].sub_mixes[0].layouts[0].anchored_loudness | length) == 2
' "${REPORT_DIR}/test_000062.iamf.json" > /dev/null
jq --exit-status '
    ([.properties.parameter_blocks[].animation_types[]] | unique) == [0, 2]
' "${REPORT_DIR}/test_000066.iamf.json" > /dev/null
jq --exit-status '
    ([.properties.parameter_blocks[].subblocks] | unique) == [1, 2, 3]
' "${REPORT_DIR}/test_000071.iamf.json" > /dev/null
jq --exit-status '
    ([.properties.parameter_blocks[].animation_types[]] | unique) == [0, 1, 2]
    and ([.properties.parameter_blocks[].subblocks] | unique) == [3]
' "${REPORT_DIR}/test_000088.iamf.json" > /dev/null

printf 'AOMedia libiamf v1.1.0 OAR/IAMF matrix passed: 42 artifacts across 14 vectors\n'
