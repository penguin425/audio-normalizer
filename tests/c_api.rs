use forge_normalizer::c_api::{
    forge_normalizer_analysis_v1_size, forge_normalizer_analyze_file_v1,
    forge_normalizer_c_api_version, forge_normalizer_live_config_v1_size, forge_normalizer_version,
    ForgeAnalysisV1, ForgeLiveConfigV1, ForgeStatus, ANALYSIS_V1_SIZE, C_API_VERSION,
    LIVE_CONFIG_V1_SIZE,
};
use forge_normalizer::wav::{default_channel_roles, AudioBuffer, PcmKind, WavWriter};
use std::f32::consts::TAU;
use std::ffi::{CStr, CString};
use std::mem::MaybeUninit;
use std::os::raw::c_char;
use std::ptr;

fn error_text(buffer: &[c_char]) -> &str {
    // SAFETY: every API path under test NUL-terminates this initialized buffer.
    unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_str()
        .expect("UTF-8 error text")
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
