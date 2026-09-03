use forge_normalizer::analysis_cache::{AnalysisCache, AnalysisCachePolicy, CacheDisposition};
use forge_normalizer::decoder;
use forge_normalizer::normalize::{
    analyze, apply_gain_and_protect, compute_gain, Mode, OutputFormat, Plan,
};
use forge_normalizer::preset::{Preset, ProfileEvidence};
use forge_normalizer::realtime::{RealtimeGainConfig, RealtimeGainProcessor, RealtimeMeter};
use forge_normalizer::stable_input::{StableInput, StableInputOptions};
use forge_normalizer::watch::{WatchCandidate, WatchFolder, WATCH_FOLDER_SCHEMA_V1};
use forge_normalizer::wav::{
    default_channel_roles, AudioBuffer, ChannelRole, PcmKind, WavContainer, WavReader, WavWriter,
};
use std::f32::consts::TAU;
use std::path::Path;
use std::time::Duration;

#[test]
fn documented_public_api_works_from_a_downstream_crate() {
    let sample_rate = 48_000;
    let frames = sample_rate as usize;
    let left: Vec<f32> = (0..frames)
        .map(|frame| 0.1 * (TAU * 997.0 * frame as f32 / sample_rate as f32).sin())
        .collect();
    let right = left.clone();
    let mut audio = AudioBuffer {
        sample_rate,
        channels: 2,
        frames,
        data: vec![left, right],
        channel_roles: default_channel_roles(2),
        source_kind: PcmKind::S16,
    };

    let preset = Preset::named("ebu-r128").expect("built-in preset");
    let analysis = analyze(&audio);
    assert!(analysis.lufs.is_finite());
    assert!(analysis.true_peak_db().is_finite());

    let plan = Plan {
        mode: Mode::Lufs,
        target_lufs: preset.target_lufs,
        target_peak_db: -1.0,
        target_rms_db: -20.0,
        ceiling_db: preset.ceiling_db,
        max_gain_db: Some(12.0),
        dither: false,
        output_kind: Some(PcmKind::S16),
        mp3_bitrate: 192,
        mp3_quality: 2,
        limiter: None,
        wav_container: WavContainer::Auto,
        bwf: false,
        output_sample_rate: None,
        resample_quality: forge_normalizer::dsp::resample::ResampleQuality::Best,
    };
    let gain = compute_gain(&analysis, &plan);
    assert!(gain.is_finite() && gain > 0.0);
    plan.validate_format_request(OutputFormat::Wav)
        .expect("validate public format request");
    apply_gain_and_protect(&mut audio, gain, &plan);

    let temporary = tempfile::tempdir().expect("temporary directory");
    let wave_path = temporary.path().join("public-api.wav");
    WavWriter::write(&wave_path, &audio, PcmKind::S16, false).expect("write public WAV API");
    let probed = WavReader::probe(&wave_path).expect("probe public WAV API");
    assert_eq!((probed.sample_rate, probed.channels), (sample_rate, 2));

    let normalized_path = temporary.path().join("normalized.wav");
    forge_normalizer::normalize::write(&audio, &normalized_path, &plan, OutputFormat::Wav)
        .expect("write public normalization API");
    let normalized = WavReader::probe(&normalized_path).expect("probe normalized output");
    assert_eq!(
        (normalized.sample_rate, normalized.channels),
        (sample_rate, 2)
    );

    let stable_options = StableInputOptions::new(8 * 1024 * 1024).expect("stable input options");
    let stable_input =
        StableInput::from_path(&wave_path, &stable_options).expect("capture stable public input");
    let bound_analysis =
        forge_normalizer::normalize::analyze_stable_input_for_plan(&stable_input, None, &plan)
            .expect("analyze stable public input");
    assert_eq!(
        bound_analysis.input_binding().sha256(),
        stable_input.sha256()
    );
    bound_analysis
        .validate_for_plan(&stable_input, &plan)
        .expect("validate public bound analysis");
    let bound_path = temporary.path().join("bound.wav");
    forge_normalizer::normalize::normalize_one_bound(
        &stable_input,
        &bound_path,
        &plan,
        OutputFormat::Wav,
        &bound_analysis,
    )
    .expect("normalize through public stable-input API");
    assert!(WavReader::probe(&bound_path).is_ok());

    let decode_fn: fn(&Path) -> Result<AudioBuffer, String> = decoder::decode;
    let decoded = decode_fn(&wave_path).expect("decode public API");
    assert_eq!((decoded.frames, decoded.channels), (frames, 2));

    let mut streamed_frames = 0;
    decoder::decode_stream_with_layout(&wave_path, |info, provenance, planar| {
        assert_eq!(info.channels, 2);
        assert_eq!(provenance, decoder::ChannelLayoutProvenance::KnownSpeakers);
        streamed_frames += planar[0].len();
        Ok(())
    })
    .expect("stream through provenance-aware public decoder API");
    assert_eq!(streamed_frames, frames);

    let mut owned_frames = 0;
    decoder::decode_stream_owned_with_layout(&wave_path, |info, provenance, mut planar| {
        assert_eq!(info.channels, 2);
        assert_eq!(provenance, decoder::ChannelLayoutProvenance::KnownSpeakers);
        owned_frames += planar[0].len();
        for channel in &mut planar {
            channel.clear();
        }
        Ok(planar)
    })
    .expect("stream through owned provenance-aware public decoder API");
    assert_eq!(owned_frames, frames);

    let cache = AnalysisCache::new(
        temporary.path().join("analysis-cache"),
        AnalysisCachePolicy::default(),
    )
    .expect("construct public cache API");
    let cached = cache
        .analyze_for_plan(&wave_path, None, &plan)
        .expect("store public cache entry");
    assert_eq!(cached.disposition, CacheDisposition::Stored);
    assert_eq!(
        cache
            .analyze_for_plan(&wave_path, None, &plan)
            .expect("read public cache entry")
            .disposition,
        CacheDisposition::Hit
    );
    let preanalyzed_path = temporary.path().join("preanalyzed.wav");
    forge_normalizer::normalize::normalize_one_preanalyzed_with_roles(
        &wave_path,
        &preanalyzed_path,
        &plan,
        OutputFormat::Wav,
        None,
        &cached.value,
    )
    .expect("normalize with public precomputed-analysis API");
    assert!(preanalyzed_path.is_file());

    let staged_path = temporary.path().join("staged.wav");
    std::fs::write(&staged_path, b"previous destination").expect("seed staged destination");
    let staged = forge_normalizer::normalize::normalize_one_staged_with_roles(
        &wave_path,
        &staged_path,
        &plan,
        OutputFormat::Wav,
        None,
    )
    .expect("render staged public normalization API");
    assert_eq!(
        std::fs::read(&staged_path).expect("read preserved staged destination"),
        b"previous destination"
    );
    let outcome = staged.commit().expect("commit staged normalization");
    assert!(outcome.source.lufs.is_finite());
    assert!(outcome.gain.is_finite());
    assert!(WavReader::probe(&staged_path).is_ok());

    let dropped_path = temporary.path().join("dropped-stage.wav");
    std::fs::write(&dropped_path, b"preserve me").expect("seed dropped destination");
    let dropped = forge_normalizer::normalize::normalize_one_preanalyzed_staged_with_roles(
        &wave_path,
        &dropped_path,
        &plan,
        OutputFormat::Wav,
        None,
        &cached.value,
    )
    .expect("render preanalyzed staged output");
    drop(dropped);
    assert_eq!(
        std::fs::read(&dropped_path).expect("read dropped staged destination"),
        b"preserve me"
    );

    let watch_input = temporary.path().join("watch-input");
    let watch_output = temporary.path().join("watch-output");
    std::fs::create_dir(&watch_input).expect("create public watch input");
    let mut watch = WatchFolder::open(
        temporary.path().join("watch-state.json"),
        &watch_input,
        &watch_output,
        true,
        Duration::from_secs(5),
        serde_json::json!({"profile": "ebu-r128"}),
    )
    .expect("construct public watch API");
    let candidates: Vec<WatchCandidate> = watch.scan().expect("scan public watch API");
    assert!(candidates.is_empty());
    assert_eq!(
        WATCH_FOLDER_SCHEMA_V1,
        "https://penguin425.github.io/audio-normalizer/schema/watch-folder-v1"
    );

    let mut meter = RealtimeMeter::new(sample_rate, vec![ChannelRole::Main, ChannelRole::Main])
        .expect("construct public meter API");
    meter
        .process_planar(&[&decoded.data[0], &decoded.data[1]])
        .expect("process public meter API");
    assert_eq!(meter.measurement().frames, frames as u64);

    let mut processor = RealtimeGainProcessor::new(sample_rate, 2, RealtimeGainConfig::default())
        .expect("construct public processor API");
    processor.set_target_gain_db(-3.0).expect("set gain");
    let mut interleaved = vec![0.0_f32; 512];
    processor
        .process_interleaved(&mut interleaved)
        .expect("process public gain API");
    assert_eq!(processor.latency_frames(), 240);

    let evidence = ProfileEvidence::PublishedPlatformPolicy;
    assert_eq!(evidence.as_str(), "published-platform-policy");
}
