#!/usr/bin/env bash
set -euo pipefail

readonly ARCHIVE_NAME="ebu-loudness-test-setv05.zip"
readonly ARCHIVE_SHA256="9cc500b4df83f7c21855c74dce795ef5209a752bf884253ae57d0ce512efb062"
readonly SOURCE_URLS=(
    "https://mirror.iro.umontreal.ca/gentoo/gentoo/distfiles/de/${ARCHIVE_NAME}"
    "https://mirrors.nju.edu.cn/gentoo/distfiles/de/${ARCHIVE_NAME}"
    "https://ftp.jaist.ac.jp/pub/Linux/Gentoo/distfiles/de/${ARCHIVE_NAME}"
    "https://ftp.lysator.liu.se/pub/gentoo/distfiles/de/${ARCHIVE_NAME}"
    "https://mirrors.mit.edu/gentoo-distfiles/distfiles/de/${ARCHIVE_NAME}"
)
readonly CACHE_DIR="${XDG_CACHE_HOME:-${HOME}/.cache}/forge-normalizer"
readonly ARCHIVE_PATH="${CACHE_DIR}/${ARCHIVE_NAME}"
readonly FIXTURE_DIR="${CACHE_DIR}/ebu-loudness-test-setv05"

mkdir -p "${CACHE_DIR}" "${FIXTURE_DIR}"

if [[ ! -f "${ARCHIVE_PATH}" ]]; then
    for source_url in "${SOURCE_URLS[@]}"; do
        if curl --fail --location --retry 3 --retry-all-errors \
            --connect-timeout 15 "${source_url}" --output "${ARCHIVE_PATH}"; then
            break
        fi
        rm -f "${ARCHIVE_PATH}"
    done
fi

test -f "${ARCHIVE_PATH}"
printf '%s  %s\n' "${ARCHIVE_SHA256}" "${ARCHIVE_PATH}" | sha256sum --check -

unzip -joq "${ARCHIVE_PATH}" \
    "seq-3341-1-16bit.wav" \
    "seq-3341-2-16bit.wav" \
    "seq-3341-3-16bit-v02.wav" \
    "seq-3341-4-16bit-v02.wav" \
    "seq-3341-5-16bit-v02.wav" \
    "seq-3341-6-5channels-16bit.wav" \
    "seq-3341-6-6channels-WAVEEX-16bit.wav" \
    "seq-3341-7_seq-3342-5-24bit.wav" \
    "seq-3341-2011-8_seq-3342-6-24bit-v02.wav" \
    "seq-3341-9-24bit.wav" \
    "seq-3341-10-*-24bit.wav" \
    "seq-3341-11-24bit.wav" \
    "seq-3341-12-24bit.wav" \
    "seq-3341-13-*-24bit.wav*" \
    "seq-3341-14-24bit.wav.wav" \
    "seq-3341-1[5-9]-24bit.wav.wav" \
    "seq-3341-2[0-3]-24bit.wav.wav" \
    "seq-3342-1-16bit.wav" \
    "seq-3342-2-16bit.wav" \
    "seq-3342-3-16bit.wav" \
    "seq-3342-4-16bit.wav" \
    -d "${FIXTURE_DIR}"

EBU_TEST_SET="${FIXTURE_DIR}" \
    cargo test --test ebu_conformance -- --ignored
