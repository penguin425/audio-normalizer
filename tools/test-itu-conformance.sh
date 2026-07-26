#!/usr/bin/env bash
set -euo pipefail

readonly BASE_URL="https://www.itu.int/dms_pub/itu-r/oth/11/02"
readonly CACHE_DIR="${XDG_CACHE_HOME:-${HOME}/.cache}/forge-normalizer/bs2217-2"
readonly ARCHIVE_DIR="${CACHE_DIR}/archives"
readonly FIXTURE_DIR="${CACHE_DIR}/files"
readonly CHECKSUMS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/bs2217-sha256.txt"

mkdir -p "${ARCHIVE_DIR}" "${FIXTURE_DIR}"

while read -r expected archive; do
    archive_path="${ARCHIVE_DIR}/${archive}"
    sequence="${archive%.zip}"
    if [[ ! -f "${archive_path}" ]] ||
        ! printf '%s  %s\n' "${expected}" "${archive_path}" | sha256sum --check --status; then
        curl --fail --location --retry 3 --retry-all-errors --connect-timeout 15 \
            "${BASE_URL}/R110200000100${sequence}ZIPM.zip" \
            --output "${archive_path}"
    fi
    printf '%s  %s\n' "${expected}" "${archive_path}" | sha256sum --check -
    unzip -oq "${archive_path}" -d "${FIXTURE_DIR}"
done < "${CHECKSUMS}"

ITU_BS2217_TEST_SET="${FIXTURE_DIR}" \
    cargo test --test itu_conformance -- --ignored
