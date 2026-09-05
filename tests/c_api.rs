use forge_normalizer::c_api::{
    forge_normalizer_analysis_v1_size, forge_normalizer_analyze_file_v1,
    forge_normalizer_analyze_file_with_layout_v1, forge_normalizer_c_api_version,
    forge_normalizer_live_config_v1_size, forge_normalizer_version, ForgeAnalysisV1,
    ForgeLiveConfigV1, ForgeStatus, ANALYSIS_V1_SIZE, C_API_VERSION, LIVE_CONFIG_V1_SIZE,
};
use forge_normalizer::channel_layout::{ChannelLayoutDescriptor, ChannelLayoutOrigin};
use forge_normalizer::wav::{default_channel_roles, AudioBuffer, ChannelRole, PcmKind, WavWriter};
use std::f32::consts::TAU;
use std::ffi::{CStr, CString};
use std::fs;
use std::mem::MaybeUninit;
use std::os::raw::c_char;
use std::path::Path;
use std::ptr;

fn error_text(buffer: &[c_char]) -> &str {
    // SAFETY: every API path under test NUL-terminates this initialized buffer.
    unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_str()
        .expect("UTF-8 error text")
}

fn write_f32_wave(path: &Path, sample: f32) {
    let mut bytes = Vec::with_capacity(48);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&40_u32.to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&3_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&48_000_u32.to_le_bytes());
    bytes.extend_from_slice(&192_000_u32.to_le_bytes());
    bytes.extend_from_slice(&4_u16.to_le_bytes());
    bytes.extend_from_slice(&32_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&4_u32.to_le_bytes());
    bytes.extend_from_slice(&sample.to_le_bytes());
    fs::write(path, bytes).unwrap();
}

#[test]
fn version_and_size_queries_are_stable() {
    assert_eq!(forge_normalizer_c_api_version(), C_API_VERSION);
    assert_eq!(forge_normalizer_analysis_v1_size(), ANALYSIS_V1_SIZE);
    assert_eq!(
        forge_normalizer_live_config_v1_size(),
        std::mem::size_of::<ForgeLiveConfigV1>()
    );
    assert_eq!(forge_normalizer_live_config_v1_size(), LIVE_CONFIG_V1_SIZE);
    // SAFETY: the version API returns a process-lifetime NUL-terminated string.
    let version = unsafe { CStr::from_ptr(forge_normalizer_version()) }
        .to_str()
        .expect("UTF-8 package version");
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
}

#[test]
fn invalid_inputs_return_bounded_status_and_error_text() {
    let mut output = MaybeUninit::<ForgeAnalysisV1>::uninit();
    let mut error = [1_i8; 128];
    // SAFETY: output and error buffers are valid and non-overlapping.
    let status = unsafe {
        forge_normalizer_analyze_file_v1(
            ptr::null(),
            1,
            output.as_mut_ptr(),
            ANALYSIS_V1_SIZE,
            error.as_mut_ptr(),
            error.len(),
        )
    };
    assert_eq!(status, ForgeStatus::NullPointer);
    assert!(error_text(&error).contains("null"));

    let path = CString::new("missing.wav").unwrap();
    // SAFETY: pointers reference live caller-owned buffers.
    let status = unsafe {
        forge_normalizer_analyze_file_v1(
            path.as_ptr(),
            1,
            output.as_mut_ptr(),
            ANALYSIS_V1_SIZE - 1,
            error.as_mut_ptr(),
            error.len(),
        )
    };
    assert_eq!(status, ForgeStatus::BufferTooSmall);
    assert!(error_text(&error).contains("output buffer is too small"));

    // SAFETY: pointers reference live caller-owned buffers.
    let status = unsafe {
        forge_normalizer_analyze_file_v1(
            path.as_ptr(),
            0,
            output.as_mut_ptr(),
            ANALYSIS_V1_SIZE,
            error.as_mut_ptr(),
            error.len(),
        )
    };
    assert_eq!(status, ForgeStatus::InvalidArgument);

    let invalid_utf8 = [0xff_u8, 0];
    // SAFETY: `invalid_utf8` is NUL-terminated and all output buffers are valid.
    let status = unsafe {
        forge_normalizer_analyze_file_v1(
            invalid_utf8.as_ptr().cast(),
            1,
            output.as_mut_ptr(),
            ANALYSIS_V1_SIZE,
            error.as_mut_ptr(),
            error.len(),
        )
    };
    assert_eq!(status, ForgeStatus::InvalidUtf8);
}

#[test]
fn c_api_analyzes_a_bounded_file_into_the_fixed_v1_layout() {
    let sample_rate = 48_000_u32;
    let frames = sample_rate as usize;
    let signal = (0..frames)
        .map(|frame| 0.1 * (TAU * 997.0 * frame as f32 / sample_rate as f32).sin())
        .collect::<Vec<_>>();
    let audio = AudioBuffer {
        sample_rate,
        channels: 2,
        frames,
        data: vec![signal.clone(), signal],
        channel_roles: default_channel_roles(2),
        source_kind: PcmKind::S16,
    };
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("c-api.wav");
    WavWriter::write(&path, &audio, PcmKind::S16, false).unwrap();
    let path = CString::new(path.to_str().unwrap()).unwrap();
    let mut result = MaybeUninit::<ForgeAnalysisV1>::uninit();
    let mut error = [1_i8; 256];

    // SAFETY: every pointer references live, sufficiently sized,
    // non-overlapping caller-owned storage.
    let status = unsafe {
        forge_normalizer_analyze_file_v1(
            path.as_ptr(),
            (frames * 2) as u64,
            result.as_mut_ptr(),
            ANALYSIS_V1_SIZE,
            error.as_mut_ptr(),
            error.len(),
        )
    };
    assert_eq!(status, ForgeStatus::Ok, "{}", error_text(&error));
    // SAFETY: a successful call initializes the entire fixed-size result.
    let result = unsafe { result.assume_init() };
    assert_eq!(result.struct_size as usize, ANALYSIS_V1_SIZE);
    assert_eq!(result.api_version, C_API_VERSION);
    assert_eq!(
        (result.sample_rate_hz, result.channels, result.frames),
        (sample_rate, 2, frames as u64)
    );
    assert!(result.integrated_lufs.is_finite());
    assert!(result.true_peak_dbtp.is_finite());
    assert_eq!(error_text(&error), "");

    let mut limited = MaybeUninit::<ForgeAnalysisV1>::uninit();
    // SAFETY: the same pointer contract holds for the deliberately bounded call.
    let status = unsafe {
        forge_normalizer_analyze_file_v1(
            path.as_ptr(),
            (frames * 2 - 1) as u64,
            limited.as_mut_ptr(),
            ANALYSIS_V1_SIZE,
            error.as_mut_ptr(),
            error.len(),
        )
    };
    assert_eq!(status, ForgeStatus::AnalysisFailed);
    assert!(error_text(&error).contains("decoded sample count"));
    assert!(error_text(&error).contains("safety limit"));
}

#[test]
fn c_api_rejects_non_finite_ieee_float_wave_samples() {
    let temporary = tempfile::tempdir().unwrap();

    for (name, sample) in [
        ("nan", f32::NAN),
        ("positive-infinity", f32::INFINITY),
        ("negative-infinity", f32::NEG_INFINITY),
    ] {
        let path = temporary.path().join(format!("{name}.wav"));
        write_f32_wave(&path, sample);
        let path = CString::new(path.to_str().unwrap()).unwrap();
        let mut result = MaybeUninit::<ForgeAnalysisV1>::uninit();
        let mut error = [1_i8; 256];

        // SAFETY: every pointer references live, sufficiently sized,
        // non-overlapping caller-owned storage.
        let status = unsafe {
            forge_normalizer_analyze_file_v1(
                path.as_ptr(),
                1,
                result.as_mut_ptr(),
                ANALYSIS_V1_SIZE,
                error.as_mut_ptr(),
                error.len(),
            )
        };

        assert_eq!(status, ForgeStatus::AnalysisFailed, "{name}");
        assert_eq!(
            error_text(&error),
            "non-finite sample at frame 0, channel 0",
            "{name}"
        );
    }
}

#[test]
fn c_api_rejects_maskless_multichannel_without_a_layout_override() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("maskless.wav");
    let audio = AudioBuffer {
        sample_rate: 48_000,
        channels: 6,
        frames: 8,
        data: vec![vec![0.0; 8]; 6],
        channel_roles: vec![ChannelRole::Main; 6],
        source_kind: PcmKind::F32,
    };
    WavWriter::write(&path, &audio, PcmKind::F32, false).unwrap();
    let path = CString::new(path.to_str().unwrap()).unwrap();
    let mut result = MaybeUninit::<ForgeAnalysisV1>::uninit();
    let mut error = [1_i8; 256];

    // SAFETY: every pointer references live, sufficiently sized,
    // non-overlapping caller-owned storage.
    let status = unsafe {
        forge_normalizer_analyze_file_v1(
            path.as_ptr(),
            48,
            result.as_mut_ptr(),
            ANALYSIS_V1_SIZE,
            error.as_mut_ptr(),
            error.len(),
        )
    };

    assert_eq!(status, ForgeStatus::AnalysisFailed);
    assert!(error_text(&error).contains("ambiguous 6-channel layout"));
}

#[test]
fn c_api_exact_layout_override_returns_effective_descriptor() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("maskless.wav");
    let audio = AudioBuffer {
        sample_rate: 48_000,
        channels: 6,
        frames: 48_000,
        data: vec![vec![0.01; 48_000]; 6],
        channel_roles: vec![ChannelRole::Main; 6],
        source_kind: PcmKind::F32,
    };
    WavWriter::write(&path, &audio, PcmKind::F32, false).unwrap();
    let path = CString::new(path.to_str().unwrap()).unwrap();
    let layout = ChannelLayoutDescriptor::from_channel_roles(default_channel_roles(6)).unwrap();
    let layout = CString::new(layout.to_json().unwrap()).unwrap();
    let mut result = MaybeUninit::<ForgeAnalysisV1>::uninit();
    let mut required = 0_usize;
    let mut error = [1_i8; 256];

    // A null output buffer performs bounded size negotiation while still
    // returning the fixed analysis result.
    // SAFETY: every non-null pointer references live, non-overlapping storage.
    let status = unsafe {
        forge_normalizer_analyze_file_with_layout_v1(
            path.as_ptr(),
            6 * 48_000,
            layout.as_ptr(),
            result.as_mut_ptr(),
            ANALYSIS_V1_SIZE,
            ptr::null_mut(),
            0,
            &mut required,
            error.as_mut_ptr(),
            error.len(),
        )
    };
    assert_eq!(status, ForgeStatus::BufferTooSmall);
    assert!(required > 1);
    // SAFETY: the size-negotiation result is documented as initialized.
    assert_eq!(unsafe { result.assume_init() }.channels, 6);

    let mut result = MaybeUninit::<ForgeAnalysisV1>::uninit();
    let mut effective = vec![0_i8; required];
    // SAFETY: every pointer references live, sufficiently sized,
    // non-overlapping caller-owned storage.
    let status = unsafe {
        forge_normalizer_analyze_file_with_layout_v1(
            path.as_ptr(),
            6 * 48_000,
            layout.as_ptr(),
            result.as_mut_ptr(),
            ANALYSIS_V1_SIZE,
            effective.as_mut_ptr(),
            effective.len(),
            &mut required,
            error.as_mut_ptr(),
            error.len(),
        )
    };
    assert_eq!(status, ForgeStatus::Ok, "{}", error_text(&error));
    assert_eq!(required, effective.len());
    // SAFETY: successful analysis NUL-terminates the returned JSON buffer.
    let effective = unsafe { CStr::from_ptr(effective.as_ptr()) }
        .to_str()
        .unwrap();
    let effective = ChannelLayoutDescriptor::from_json(effective).unwrap();
    assert_eq!(effective.channel_count(), 6);
    assert_eq!(effective.origin(), ChannelLayoutOrigin::ExplicitOverride);
    // SAFETY: a successful call initialized the fixed result.
    assert_eq!(unsafe { result.assume_init() }.channels, 6);
}
