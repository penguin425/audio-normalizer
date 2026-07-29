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
         #EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:6.2,\nhttps://example.invalid/a.ts\n\
         #EXT-X-ENDLIST\n",
    )
    .unwrap();
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
fn streaming_qc_cli_selects_low_latency_profile() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("live.m3u8");
    fs::write(
        &path,
        "#EXTM3U\n\
         #EXT-X-VERSION:9\n\
         #EXT-X-TARGETDURATION:2\n\
         #EXT-X-PART-INF:PART-TARGET=0.5\n\
         #EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.5\n\
         #EXT-X-PROGRAM-DATE-TIME:2026-07-29T00:00:00Z\n\
         #EXT-X-PART:DURATION=0.5,INDEPENDENT=YES,URI=\"https://example.invalid/0.0.m4s\"\n\
         #EXTINF:0.5,\n\
         https://example.invalid/0.ts\n\
         #EXT-X-PART:DURATION=0.5,INDEPENDENT=YES,URI=\"https://example.invalid/1.0.m4s\"\n\
         #EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"https://example.invalid/1.1.m4s\"\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-streaming-qc"))
        .arg(&path)
        .args(["--profile", "ll-hls"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["profile"], "ll-hls");
    assert!(audit["findings"].as_array().unwrap().iter().any(|finding| {
        finding["rule_id"] == "FORGE-LL-HLS-BLOCKING-RELOAD" && finding["passed"] == true
    }));
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

#[test]
fn streaming_qc_cli_selects_dash_live_profile() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("live.mpd");
    std::fs::write(
        &input,
        r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic"
 profiles="http://dashif.org/guidelines/dash-if-uhd#hevc"
 availabilityStartTime="2026-07-29T00:00:00Z"
 publishTime="2026-07-29T00:00:20Z"
 minimumUpdatePeriod="PT2S" minBufferTime="PT1S"
 timeShiftBufferDepth="PT30S" suggestedPresentationDelay="PT3S">
 <UTCTiming schemeIdUri="urn:mpeg:dash:utc:direct:2014"
  value="2026-07-29T00:00:20Z"/>
 <ServiceDescription id="0"><Latency target="4000"/></ServiceDescription>
 <BaseURL>https://example.invalid/live/</BaseURL>
 <Period id="p0" start="PT0S" duration="PT10S">
  <AdaptationSet id="1" contentType="audio" mimeType="audio/mp4"
   codecs="opus" lang="en" audioSamplingRate="48000">
   <SegmentTemplate timescale="48000" duration="96000"
    initialization="init-$RepresentationID$.mp4"
    media="$RepresentationID$-$Number$.m4s"/>
   <Representation id="a1" bandwidth="96000"/>
  </AdaptationSet>
 </Period>
</MPD>"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-streaming-qc"))
        .arg(&input)
        .args(["--profile", "dash-live"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["profile"], "dash-live");
    assert!(audit["findings"].as_array().unwrap().iter().any(|finding| {
        finding["rule_id"] == "FORGE-DASH-LIVE-UTC-TIMING" && finding["passed"] == true
    }));
}

#[test]
fn streaming_qc_cli_compares_successive_dash_snapshots() {
    let directory = tempfile::tempdir().unwrap();
    let previous = directory.path().join("previous.mpd");
    let current = directory.path().join("current.mpd");
    let snapshot = |publish_time: &str, timeline: &str| {
        format!(
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" id="live"
 type="dynamic" availabilityStartTime="2026-07-29T00:00:00Z"
 publishTime="{publish_time}" minimumUpdatePeriod="PT2S" minBufferTime="PT1S">
 <BaseURL>https://example.invalid/live/</BaseURL>
 <Period id="p0" start="PT0S">
  <AdaptationSet id="audio" contentType="audio" mimeType="audio/mp4"
   codecs="opus" lang="en" audioSamplingRate="48000">
   <SegmentTemplate timescale="10" initialization="init-$RepresentationID$.mp4"
    media="$RepresentationID$-$Time$.m4s">
    <SegmentTimeline>{timeline}</SegmentTimeline>
   </SegmentTemplate>
   <Representation id="a1" bandwidth="64000"/>
  </AdaptationSet>
 </Period>
</MPD>"#
        )
    };
    fs::write(
        &previous,
        snapshot("2026-07-29T00:00:20Z", r#"<S t="0" d="10" r="2"/>"#),
    )
    .unwrap();
    fs::write(
        &current,
        snapshot("2026-07-29T00:00:21Z", r#"<S t="10" d="10" r="2"/>"#),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-streaming-qc"))
        .arg(&current)
        .args(["--profile", "iso23009", "--previous-mpd"])
        .arg(&previous)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        audit["properties"]["previous_path"],
        previous.to_string_lossy().as_ref()
    );
    assert_eq!(audit["properties"]["previous_passed"], true);
    assert!(audit["findings"].as_array().unwrap().iter().any(|finding| {
        finding["rule_id"] == "FORGE-DASH-UPDATE-SEGMENT-EQUIVALENCE" && finding["passed"] == true
    }));
}

#[test]
fn streaming_qc_cli_applies_and_audits_dash_mpd_patch() {
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path().join("base.mpd");
    let patch = directory.path().join("update.mpp");
    fs::write(
        &base,
        r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" id="live"
 type="dynamic" availabilityStartTime="2026-07-29T00:00:00Z"
 publishTime="2026-07-29T00:00:20Z" minimumUpdatePeriod="PT2S"
 minBufferTime="PT1S">
 <BaseURL>https://example.invalid/live/</BaseURL>
 <PatchLocation ttl="60">update.mpp</PatchLocation>
 <Period id="p0" start="PT0S">
  <AdaptationSet id="audio" contentType="audio" mimeType="audio/mp4"
   codecs="opus" lang="en" audioSamplingRate="48000">
   <SegmentTemplate timescale="10" initialization="init-$RepresentationID$.mp4"
    media="$RepresentationID$-$Time$.m4s">
    <SegmentTimeline><S t="0" d="10"/><S t="10" d="10"/><S t="20" d="10"/></SegmentTimeline>
   </SegmentTemplate>
   <Representation id="a1" bandwidth="64000"/>
  </AdaptationSet>
 </Period>
</MPD>"#,
    )
    .unwrap();
    fs::write(
        &patch,
        r#"<Patch xmlns="urn:mpeg:dash:schema:mpd-patch:2020"
 mpdId="live" originalPublishTime="2026-07-29T00:00:20Z"
 publishTime="2026-07-29T00:00:22Z">
 <replace sel="/MPD/@publishTime">2026-07-29T00:00:22Z</replace>
 <replace sel="/MPD/PatchLocation[1]"><PatchLocation ttl="60">next.mpp</PatchLocation></replace>
 <remove sel="/MPD/Period[@id='p0']/AdaptationSet[@id='audio']/SegmentTemplate/SegmentTimeline/S[1]"/>
 <add sel="/MPD/Period[@id='p0']/AdaptationSet[@id='audio']/SegmentTemplate/SegmentTimeline/S[2]" pos="after"><S t="30" d="10"/></add>
</Patch>"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-streaming-qc"))
        .arg(&base)
        .args(["--profile", "iso23009", "--mpd-patch"])
        .arg(&patch)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        audit["properties"]["patch_path"],
        patch.to_string_lossy().as_ref()
    );
    assert_eq!(audit["properties"]["patch_operation_count"], 4);
    assert_eq!(audit["properties"]["publish_time"], "2026-07-29T00:00:22Z");
    assert!(audit["findings"].as_array().unwrap().iter().any(|finding| {
        finding["rule_id"] == "FORGE-DASH-PATCH-RESULT-PUBLISH-TIME" && finding["passed"] == true
    }));

    let mismatched = fs::read_to_string(&patch)
        .unwrap()
        .replace(r#"mpdId="live""#, r#"mpdId="different""#);
    fs::write(&patch, mismatched).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-streaming-qc"))
        .arg(&base)
        .args(["--profile", "iso23009", "--mpd-patch"])
        .arg(&patch)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:#?}");
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(audit["findings"].as_array().unwrap().iter().any(|finding| {
        finding["rule_id"] == "FORGE-DASH-PATCH-MPD-ID" && finding["passed"] == false
    }));
}

#[test]
fn streaming_qc_cli_cross_checks_mpegts_segment_boundaries() {
    if !Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping MPEG-TS HLS test because FFmpeg is unavailable");
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let playlist = directory.path().join("audio.m3u8");
    let segment_pattern = directory.path().join("seg%03d.ts");
    let generated = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=48000:duration=3",
            "-c:a",
            "aac",
            "-b:a",
            "96k",
            "-f",
            "hls",
            "-hls_time",
            "1",
            "-hls_list_size",
            "0",
            "-hls_segment_type",
            "mpegts",
            "-hls_segment_filename",
        ])
        .arg(&segment_pattern)
        .arg(&playlist)
        .status()
        .unwrap();
    assert!(generated.success());

    let output = Command::new(env!("CARGO_BIN_EXE_forge-streaming-qc"))
        .arg(&playlist)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(audit["findings"].as_array().unwrap().iter().any(|finding| {
        finding["rule_id"] == "FORGE-HLS-TS-PTS-CONTINUITY" && finding["passed"] == true
    }));

    fs::copy(
        directory.path().join("seg000.ts"),
        directory.path().join("seg001.ts"),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-streaming-qc"))
        .arg(&playlist)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(audit["findings"].as_array().unwrap().iter().any(|finding| {
        finding["rule_id"] == "FORGE-HLS-TS-PTS-CONTINUITY" && finding["passed"] == false
    }));

    let text = fs::read_to_string(&playlist).unwrap();
    let mut lines = text.lines().collect::<Vec<_>>();
    let segment = lines.iter().position(|line| *line == "seg001.ts").unwrap();
    assert!(segment > 0 && lines[segment - 1].starts_with("#EXTINF:"));
    lines.insert(segment - 1, "#EXT-X-DISCONTINUITY");
    fs::write(&playlist, format!("{}\n", lines.join("\n"))).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-streaming-qc"))
        .arg(&playlist)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
}
