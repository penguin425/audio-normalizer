use serde_json::Value;
use std::fs;
use std::process::Command;

#[test]
fn streaming_qc_cli_separates_errors_from_apple_warnings() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("audio.m3u8");
    fs::write(
        &path,
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:7\n\
         #EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:6.2,\na.ts\n#EXT-X-ENDLIST\n",
    )
    .unwrap();
    fs::write(directory.path().join("a.ts"), []).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-streaming-qc"))
        .arg(&path)
        .args(["--profile", "apple-hls"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("PASS"));
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["passed"], true);
    assert!(audit["warning_count"].as_u64().unwrap() > 0);

    fs::write(&path, "#EXTM3U\n#EXTINF:6,\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-streaming-qc"))
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["passed"], false);
}

#[test]
fn streaming_qc_cli_audits_dash_and_selects_profile() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("audio.mpd");
    let output = directory.path().join("audit.json");
    std::fs::write(
        &input,
        r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static"
 mediaPresentationDuration="PT2S" minBufferTime="PT1S">
 <Period><AdaptationSet contentType="audio" mimeType="audio/mp4" codecs="opus">
  <SegmentTemplate timescale="48000" duration="96000"
   initialization="init-$RepresentationID$.mp4" media="$RepresentationID$-$Number$.m4s"/>
  <Representation id="a1" bandwidth="96000"/>
 </AdaptationSet></Period></MPD>"#,
    )
    .unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_forge-streaming-qc"))
        .arg(&input)
        .args(["--profile", "dash-if-iop", "--output"])
        .arg(&output)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(1));
    let audit: serde_json::Value = serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
    assert_eq!(audit["profile"], "dash-if-iop");
    assert_eq!(
        audit["schema"],
        "https://penguin425.github.io/audio-normalizer/schema/dash-qc-v1"
    );
}
