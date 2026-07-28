use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::process::Command;

const PKL_ID: &str = "urn:uuid:11111111-1111-4111-8111-111111111111";
const CPL_ID: &str = "urn:uuid:22222222-2222-4222-8222-222222222222";
const MXF_ID: &str = "urn:uuid:33333333-3333-4333-8333-333333333333";
const TRACK_ID: &str = "urn:uuid:44444444-4444-4444-8444-444444444444";
const DESCRIPTOR_ID: &str = "urn:uuid:55555555-5555-4555-8555-555555555555";

fn sha256_base64(bytes: &[u8]) -> String {
    BASE64.encode(Sha256::digest(bytes))
}

fn write_valid_package(root: &Path) -> bool {
    if !Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        return false;
    }
    let mxf_path = root.join("audio.mxf");
    let generated = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=128x72:rate=25:duration=0.2",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=997:sample_rate=48000:duration=0.2",
            "-c:v",
            "mpeg2video",
            "-pix_fmt",
            "yuv422p",
            "-c:a",
            "pcm_s24le",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-f",
            "mxf",
        ])
        .arg(&mxf_path)
        .output()
        .unwrap();
    assert!(generated.status.success(), "{generated:#?}");

    let cpl = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<CompositionPlaylist xmlns="http://www.smpte-ra.org/schemas/2067-3/2016"
 xmlns:cc="http://www.smpte-ra.org/ns/2067-2/2020">
 <Id>{CPL_ID}</Id><EditRate>25 1</EditRate>
 <ExtensionProperties>
  <cc:ApplicationIdentification>urn:example:forge:test-audio</cc:ApplicationIdentification>
 </ExtensionProperties>
 <EssenceDescriptorList>
  <EssenceDescriptor><Id>{DESCRIPTOR_ID}</Id>
   <AudioSamplingRate>48000 1</AudioSamplingRate><ChannelCount>2</ChannelCount>
   <QuantizationBits>24</QuantizationBits>
   <MCATagSymbol>ST</MCATagSymbol><MCAChannelID>1</MCAChannelID>
   <MCALabelDictionaryID>urn:smpte:ul:060e2b34.0401010d.03020201.00000000</MCALabelDictionaryID>
   <RFC5646SpokenLanguage>en-US</RFC5646SpokenLanguage>
   <SoundfieldGroupLinkID>urn:uuid:66666666-6666-4666-8666-666666666666</SoundfieldGroupLinkID>
  </EssenceDescriptor>
 </EssenceDescriptorList>
 <SegmentList><Segment><SequenceList>
  <MainAudioSequence><TrackId>{TRACK_ID}</TrackId><ResourceList>
   <TrackFileResource><EditRate>48000 1</EditRate><IntrinsicDuration>9600</IntrinsicDuration>
    <EntryPoint>0</EntryPoint><SourceDuration>9600</SourceDuration><RepeatCount>1</RepeatCount>
    <SourceEncoding>{DESCRIPTOR_ID}</SourceEncoding><TrackFileId>{MXF_ID}</TrackFileId>
   </TrackFileResource>
  </ResourceList></MainAudioSequence>
 </SequenceList></Segment></SegmentList>
</CompositionPlaylist>"#
    );
    fs::write(root.join("CPL.xml"), &cpl).unwrap();
    let mxf = fs::read(&mxf_path).unwrap();
    let pkl = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<PackingList xmlns="http://www.smpte-ra.org/schemas/2067-2/2016/PKL">
 <Id>{PKL_ID}</Id><AssetList>
  <Asset><Id>{CPL_ID}</Id><Hash>{}</Hash><Size>{}</Size><Type>text/xml</Type>
   <OriginalFileName>CPL.xml</OriginalFileName>
   <HashAlgorithm Algorithm="http://www.w3.org/2001/04/xmlenc#sha256"/></Asset>
  <Asset><Id>{MXF_ID}</Id><Hash>{}</Hash><Size>{}</Size><Type>application/mxf</Type>
   <OriginalFileName>audio.mxf</OriginalFileName>
   <HashAlgorithm Algorithm="http://www.w3.org/2001/04/xmlenc#sha256"/></Asset>
 </AssetList>
</PackingList>"#,
        sha256_base64(cpl.as_bytes()),
        cpl.len(),
        sha256_base64(&mxf),
        mxf.len()
    );
    fs::write(root.join("PKL.xml"), &pkl).unwrap();
    let assetmap = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<AssetMap xmlns="http://www.smpte-ra.org/schemas/429-9/2007/AM"><AssetList>
 <Asset><Id>{PKL_ID}</Id><PackingList>true</PackingList><ChunkList><Chunk><Path>PKL.xml</Path><VolumeIndex>1</VolumeIndex><Offset>0</Offset><Length>{}</Length></Chunk></ChunkList></Asset>
 <Asset><Id>{CPL_ID}</Id><ChunkList><Chunk><Path>CPL.xml</Path><Length>{}</Length></Chunk></ChunkList></Asset>
 <Asset><Id>{MXF_ID}</Id><ChunkList><Chunk><Path>audio.mxf</Path><Length>{}</Length></Chunk></ChunkList></Asset>
</AssetList></AssetMap>"#,
        pkl.len(),
        cpl.len(),
        mxf.len()
    );
    fs::write(root.join("ASSETMAP"), assetmap).unwrap();
    true
}

#[test]
fn cli_audits_hashes_references_timing_mca_and_application() {
    let directory = tempfile::tempdir().unwrap();
    if !write_valid_package(directory.path()) {
        return;
    }
    let output = Command::new(env!("CARGO_BIN_EXE_forge-imf-qc"))
        .arg(directory.path())
        .arg("--compact")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["passed"], true);
    assert_eq!(value["properties"]["hash_verified_asset_count"], 2);
    assert_eq!(value["properties"]["composition_playlist_count"], 1);
    assert_eq!(value["properties"]["mca_label_count"], 1);
    assert!(value["findings"].as_array().unwrap().iter().any(|finding| {
        finding["rule_id"] == "FORGE-IMF-CPL-VIRTUAL-TRACK-TIMING" && finding["passed"] == true
    }));
}

#[test]
fn cli_reports_hash_mismatch_and_path_escape_as_qc_failures() {
    let directory = tempfile::tempdir().unwrap();
    if !write_valid_package(directory.path()) {
        return;
    }
    let cpl_path = directory.path().join("CPL.xml");
    let cpl = fs::read_to_string(&cpl_path).unwrap();
    fs::write(&cpl_path, cpl.replacen("test-audio", "best-audio", 1)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-imf-qc"))
        .arg(directory.path())
        .arg("--compact")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["findings"].as_array().unwrap().iter().any(|finding| {
        finding["rule_id"] == "FORGE-IMF-PKL-HASH" && finding["passed"] == false
    }));

    let unsafe_directory = tempfile::tempdir().unwrap();
    fs::write(
        unsafe_directory.path().join("ASSETMAP"),
        format!(
            r#"<AssetMap xmlns="http://www.smpte-ra.org/schemas/429-9/2007/AM"><AssetList>
<Asset><Id>{PKL_ID}</Id><ChunkList><Chunk><Path>../outside.xml</Path></Chunk></ChunkList></Asset>
</AssetList></AssetMap>"#
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-imf-qc"))
        .arg(unsafe_directory.path())
        .arg("--compact")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["findings"].as_array().unwrap().iter().any(|finding| {
        finding["rule_id"] == "FORGE-IMF-ASSETMAP-LOCAL-PATH" && finding["passed"] == false
    }));
}

#[test]
fn cli_uses_exit_two_for_malformed_xml() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("ASSETMAP"),
        b"<!DOCTYPE x><AssetMap></AssetMap>",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-imf-qc"))
        .arg(directory.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("must not contain a DTD"));
}
