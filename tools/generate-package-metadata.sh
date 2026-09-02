#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 VERSION DIST_DIR" >&2
  exit 2
fi

version="$1"
dist_dir="$2"
base_url="https://github.com/penguin425/audio-normalizer/releases/download/v${version}"

hash_for() {
  sha256sum "${dist_dir}/$1" | awk '{print $1}'
}

linux_hash="$(hash_for "forge-v${version}-linux-x86_64.tar.gz")"
macos_x86_hash="$(hash_for "forge-v${version}-macos-x86_64.tar.gz")"
macos_arm_hash="$(hash_for "forge-v${version}-macos-aarch64.tar.gz")"
windows_hash="$(hash_for "forge-v${version}-windows-x86_64.zip")"

cat >"${dist_dir}/forge.rb" <<EOF
class Forge < Formula
  desc "EBU R128 and ITU-R BS.1770-5 loudness normalizer"
  homepage "https://github.com/penguin425/audio-normalizer"
  version "${version}"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "${base_url}/forge-v${version}-macos-aarch64.tar.gz"
      sha256 "${macos_arm_hash}"
    else
      url "${base_url}/forge-v${version}-macos-x86_64.tar.gz"
      sha256 "${macos_x86_hash}"
    end
  end

  on_linux do
    url "${base_url}/forge-v${version}-linux-x86_64.tar.gz"
    sha256 "${linux_hash}"
  end

  def install
    bin.install Dir["forge", "forge-*"].select { |path| File.file?(path) && File.executable?(path) }
    include.install "include/forge_normalizer.h"
    lib.install Dir["libforge_normalizer.*"]
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/forge --version")
  end
end
EOF

cat >"${dist_dir}/forge-scoop.json" <<EOF
{
  "version": "${version}",
  "description": "EBU R128 and ITU-R BS.1770-5 loudness normalizer",
  "homepage": "https://github.com/penguin425/audio-normalizer",
  "license": "MIT",
  "architecture": {
    "64bit": {
      "url": "${base_url}/forge-v${version}-windows-x86_64.zip",
      "hash": "${windows_hash}",
      "extract_dir": "forge-v${version}-windows-x86_64"
    }
  },
  "bin": [
    "forge.exe",
    "forge-live.exe",
    "forge-doctor.exe",
    "forge-compare.exe",
    "forge-audio-compare.exe",
    "forge-container-qc.exe",
    "forge-streaming-qc.exe",
    "forge-presentation-qc.exe",
    "forge-adm-presentation-qc.exe",
    "forge-adm-interactivity-qc.exe",
    "forge-adm-semantics-qc.exe",
    "forge-adm-emission-qc.exe",
    "forge-downmix-qc.exe",
    "forge-binaural-qc.exe",
    "forge-remediate.exe",
    "forge-metadata-repair.exe",
    "forge-sadm-qc.exe",
    "forge-dialogue-provider.exe",
    "forge-anomaly-provider.exe",
    "forge-provenance-qc.exe",
    "forge-imf-qc.exe",
    "forge-aes31-qc.exe",
    "forge-rtp-qc.exe",
    "forge-nmos-qc.exe",
    "forge-st2022-7-qc.exe",
    "forge-report.exe",
    "forge-multi-delivery.exe",
    "forge-segment-normalize.exe",
    "forge-ac4-qc.exe",
    "forge-mpegh-qc.exe",
    "forge-dts-qc.exe"
  ]
}
EOF

cat >"${dist_dir}/Penguin425.Forge.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.version.1.10.0.schema.json
PackageIdentifier: Penguin425.Forge
PackageVersion: ${version}
DefaultLocale: en-US
ManifestType: version
ManifestVersion: 1.10.0
EOF

cat >"${dist_dir}/Penguin425.Forge.locale.en-US.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.defaultLocale.1.10.0.schema.json
PackageIdentifier: Penguin425.Forge
PackageVersion: ${version}
PackageLocale: en-US
Publisher: penguin425
PackageName: Forge
License: MIT
ShortDescription: EBU R128 and ITU-R BS.1770-5 loudness normalizer
PackageUrl: https://github.com/penguin425/audio-normalizer
ManifestType: defaultLocale
ManifestVersion: 1.10.0
EOF

cat >"${dist_dir}/Penguin425.Forge.installer.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.installer.1.10.0.schema.json
PackageIdentifier: Penguin425.Forge
PackageVersion: ${version}
InstallerType: zip
Installers:
  - Architecture: x64
    InstallerUrl: ${base_url}/forge-v${version}-windows-x86_64.zip
    InstallerSha256: ${windows_hash^^}
    NestedInstallerType: portable
    NestedInstallerFiles:
      - RelativeFilePath: forge-v${version}-windows-x86_64/forge.exe
        PortableCommandAlias: forge
ManifestType: installer
ManifestVersion: 1.10.0
EOF

jq -e . "${dist_dir}/forge-scoop.json" >/dev/null
if command -v ruby >/dev/null 2>&1; then
  ruby -c "${dist_dir}/forge.rb" >/dev/null
fi
