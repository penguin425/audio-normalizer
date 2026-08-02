//! Versioned, allocation-free-at-the-boundary C API.
//!
//! The ABI uses fixed-width scalar fields, caller-owned output storage, and
//! integer status codes. Rust-owned strings, vectors, enums, and references
//! never cross the boundary. See `C-API.md` and `include/forge_normalizer.h`
//! for the public contract.

use crate::realtime::{RealtimeGainConfig, RealtimeGainProcessor};
use crate::{decoder, normalize};
use std::ffi::CStr;
use std::os::raw::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::ptr;

/// Current major version of the Forge C ABI.
pub const C_API_VERSION: u32 = 1;

/// Required byte size of [`ForgeAnalysisV1`].
pub const ANALYSIS_V1_SIZE: usize = 80;
/// Required byte size of [`ForgeLiveConfigV1`].
pub const LIVE_CONFIG_V1_SIZE: usize = 48;

static PACKAGE_VERSION: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();

/// Status returned by every fallible C ABI operation.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeStatus {
    Ok = 0,
    NullPointer = 1,
    BufferTooSmall = 2,
    InvalidUtf8 = 3,
    InvalidArgument = 4,
    AnalysisFailed = 5,
}

/// Stable v1 file-analysis result.
///
/// Fields use the units named in the C header. `struct_size` and
/// `api_version` allow callers to audit the exact contract they received.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ForgeAnalysisV1 {
    pub struct_size: u32,
    pub api_version: u32,
    pub sample_rate_hz: u32,
    pub channels: u32,
    pub frames: u64,
    pub integrated_lufs: f64,
    pub max_momentary_lufs: f64,
    pub max_short_term_lufs: f64,
    pub loudness_range_lu: f64,
    pub rms_dbfs: f64,
    pub sample_peak_dbfs: f64,
    pub true_peak_dbtp: f64,
}

impl TryFrom<normalize::Analysis> for ForgeAnalysisV1 {
    type Error = String;

    fn try_from(value: normalize::Analysis) -> Result<Self, Self::Error> {
        Ok(Self {
            struct_size: ANALYSIS_V1_SIZE as u32,
            api_version: C_API_VERSION,
            sample_rate_hz: value.sample_rate,
            channels: u32::from(value.channels),
            frames: value
                .frames
                .try_into()
                .map_err(|_| "decoded frame count does not fit the C ABI".to_string())?,
            integrated_lufs: value.lufs,
            max_momentary_lufs: value.max_momentary_lufs,
            max_short_term_lufs: value.max_short_term_lufs,
            loudness_range_lu: value.loudness_range_lu,
            rms_dbfs: value.rms_db,
            sample_peak_dbfs: value.sample_peak_db(),
            true_peak_dbtp: value.true_peak_db(),
        })
    }
}

/// Configuration for the bounded real-time processor used by FFmpeg,
/// GStreamer, and other streaming hosts.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ForgeLiveConfigV1 {
    pub struct_size: u32,
    pub api_version: u32,
    pub sample_rate_hz: u32,
    pub channels: u32,
    pub initial_gain_db: f64,
    pub ceiling_dbtp: f64,
    pub attack_ms: f64,
    pub release_ms: f64,
}

/// Opaque state for the allocation-free streaming C ABI.
pub struct ForgeLiveV1 {
    processor: RealtimeGainProcessor,
    channels: usize,
    flushed: bool,
}

/// Return the current Forge C ABI major version.
#[no_mangle]
pub extern "C" fn forge_normalizer_c_api_version() -> u32 {
    C_API_VERSION
}

/// Return a process-lifetime, NUL-terminated Forge package-version string.
#[no_mangle]
pub extern "C" fn forge_normalizer_version() -> *const c_char {
    PACKAGE_VERSION.as_ptr().cast()
}

/// Return the required byte size of `ForgeAnalysisV1`.
#[no_mangle]
pub extern "C" fn forge_normalizer_analysis_v1_size() -> usize {
    ANALYSIS_V1_SIZE
}

/// Return the required byte size of `ForgeLiveConfigV1`.
#[no_mangle]
pub extern "C" fn forge_normalizer_live_config_v1_size() -> usize {
    LIVE_CONFIG_V1_SIZE
}

/// Analyze one local audio file into a caller-owned v1 result.
///
/// # Safety
///
/// - `path_utf8` must point to a readable NUL-terminated byte string.
/// - `result` must be aligned for [`ForgeAnalysisV1`] and writable for
///   `result_size` bytes.
/// - when non-null, `error_buffer` must be writable for `error_capacity`
///   bytes and must not overlap `path_utf8` or `result`.
/// - all pointers must remain valid for the duration of the call.
///
/// This uses the non-unwinding C ABI. An unexpected Rust panic therefore
/// never unwinds into the caller.
#[no_mangle]
pub unsafe extern "C" fn forge_normalizer_analyze_file_v1(
    path_utf8: *const c_char,
    max_decoded_samples: u64,
    result: *mut ForgeAnalysisV1,
    result_size: usize,
    error_buffer: *mut c_char,
    error_capacity: usize,
) -> ForgeStatus {
    // SAFETY: the caller owns the optional error buffer according to the
    // function contract.
    unsafe { write_error("", error_buffer, error_capacity) };

    if result.is_null() || path_utf8.is_null() {
        // SAFETY: same caller-owned buffer contract as above.
        unsafe { write_error("a required pointer is null", error_buffer, error_capacity) };
        return ForgeStatus::NullPointer;
    }
    if result_size < ANALYSIS_V1_SIZE {
        // SAFETY: same caller-owned buffer contract as above.
        unsafe {
            write_error(
                "ForgeAnalysisV1 output buffer is too small",
                error_buffer,
                error_capacity,
            )
        };
        return ForgeStatus::BufferTooSmall;
    }
    if max_decoded_samples == 0 {
        // SAFETY: same caller-owned buffer contract as above.
        unsafe {
            write_error(
                "max_decoded_samples must be greater than zero",
                error_buffer,
                error_capacity,
            )
        };
        return ForgeStatus::InvalidArgument;
    }

    // SAFETY: the caller promises a readable NUL-terminated path.
    let path_bytes = unsafe { CStr::from_ptr(path_utf8) };
    let path = match path_bytes.to_str() {
        Ok(path) => path,
        Err(_) => {
            // SAFETY: same caller-owned buffer contract as above.
            unsafe { write_error("path_utf8 is not valid UTF-8", error_buffer, error_capacity) };
            return ForgeStatus::InvalidUtf8;
        }
    };

    let analysis = decoder::decode_limited(Path::new(path), max_decoded_samples)
        .map(|audio| normalize::analyze(&audio))
        .and_then(ForgeAnalysisV1::try_from);
    match analysis {
        Ok(analysis) => {
            // SAFETY: size and null checks above establish writable storage
            // for one complete v1 result.
            unsafe { result.write(analysis) };
            ForgeStatus::Ok
        }
        Err(error) => {
            // SAFETY: same caller-owned buffer contract as above.
            unsafe { write_error(&error, error_buffer, error_capacity) };
            ForgeStatus::AnalysisFailed
        }
    }
}

/// Create an opaque real-time processor from a bounded v1 configuration.
///
/// The returned handle is owned by the caller and must be released with
/// [`forge_normalizer_live_destroy_v1`]. The processor is single-threaded:
/// hosts must serialize calls for one handle, while separate handles may run
/// concurrently.
///
/// # Safety
///
/// `config` must point to a readable [`ForgeLiveConfigV1`]. When non-null,
/// `error_buffer` must be writable for `error_capacity` bytes. All pointers
/// must remain valid for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn forge_normalizer_live_create_v1(
    config: *const ForgeLiveConfigV1,
    error_buffer: *mut c_char,
    error_capacity: usize,
) -> *mut ForgeLiveV1 {
    // SAFETY: the optional error buffer follows the public C ABI contract.
    unsafe { write_error("", error_buffer, error_capacity) };
    if config.is_null() {
        // SAFETY: same caller-owned error buffer contract as above.
        unsafe { write_error("a required pointer is null", error_buffer, error_capacity) };
        return ptr::null_mut();
    }

    let created = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the caller promises a readable ForgeLiveConfigV1 pointer.
        let config = unsafe { *config };
        validate_live_config(&config)?;
        let channels = usize::try_from(config.channels).map_err(|_| {
            (
                ForgeStatus::InvalidArgument,
                "channel count is too large".to_string(),
            )
        })?;
        let processor = RealtimeGainProcessor::new(
            config.sample_rate_hz,
            channels,
            RealtimeGainConfig {
                initial_gain_db: config.initial_gain_db,
                ceiling_dbfs: config.ceiling_dbtp,
                attack_ms: config.attack_ms,
                release_ms: config.release_ms,
            },
        )
        .map_err(|error| (ForgeStatus::InvalidArgument, error))?;
        Ok::<_, (ForgeStatus, String)>(Box::into_raw(Box::new(ForgeLiveV1 {
            processor,
            channels,
            flushed: false,
        })))
    }));

    match created {
        Ok(Ok(handle)) => handle,
        Ok(Err((_status, message))) => {
            // SAFETY: same caller-owned error buffer contract as above.
            unsafe { write_error(&message, error_buffer, error_capacity) };
            ptr::null_mut()
        }
        Err(_) => {
            // SAFETY: same caller-owned error buffer contract as above.
            unsafe {
                write_error(
                    "unexpected panic while creating live processor",
                    error_buffer,
                    error_capacity,
                )
            };
            ptr::null_mut()
        }
    }
}

/// Destroy a live processor. Passing NULL is allowed and is a no-op.
///
/// # Safety
///
/// A non-null `handle` must be a pointer returned by
/// [`forge_normalizer_live_create_v1`] that has not already been destroyed.
#[no_mangle]
pub unsafe extern "C" fn forge_normalizer_live_destroy_v1(handle: *mut ForgeLiveV1) {
    if !handle.is_null() {
        // SAFETY: the handle came from `forge_normalizer_live_create_v1` and
        // ownership is transferred back exactly once.
        unsafe { drop(Box::from_raw(handle)) };
    }
}

/// Return the fixed output latency in frames for a live processor, or zero
/// for a NULL handle.
///
/// # Safety
///
/// A non-null `handle` must remain a valid live handle for the duration of the
/// call.
#[no_mangle]
pub unsafe extern "C" fn forge_normalizer_live_latency_frames_v1(
    handle: *const ForgeLiveV1,
) -> usize {
    if handle.is_null() {
        return 0;
    }
    // SAFETY: the caller promises a live handle for the duration of the call.
    unsafe { (*handle).processor.latency_frames() }
}

/// Process interleaved f32 samples in place. The first `latency_frames`
/// frames are zero while the look-ahead buffer fills.
///
/// # Safety
///
/// `handle` must be a valid, uniquely accessed live handle. When `frames` is
/// non-zero, `samples` must point to `frames * channels` writable `f32` values;
/// it may be null for a zero-frame call. When non-null, `error_buffer` must be
/// writable for `error_capacity` bytes. All pointers must remain valid for the
/// duration of the call.
#[no_mangle]
pub unsafe extern "C" fn forge_normalizer_live_process_interleaved_f32_v1(
    handle: *mut ForgeLiveV1,
    samples: *mut f32,
    frames: usize,
    error_buffer: *mut c_char,
    error_capacity: usize,
) -> ForgeStatus {
    // SAFETY: the optional error buffer follows the public C ABI contract.
    unsafe { write_error("", error_buffer, error_capacity) };
    let Some(handle) = (if handle.is_null() {
        None
    } else {
        // SAFETY: the caller promises a live, uniquely accessed handle.
        Some(unsafe { &mut *handle })
    }) else {
        // SAFETY: same caller-owned error buffer contract as above.
        unsafe { write_error("a required pointer is null", error_buffer, error_capacity) };
        return ForgeStatus::NullPointer;
    };
    if handle.flushed {
        // SAFETY: same caller-owned error buffer contract as above.
        unsafe {
            write_error(
                "live processor was flushed; create a new handle",
                error_buffer,
                error_capacity,
            )
        };
        return ForgeStatus::InvalidArgument;
    }
    let sample_count = match frames.checked_mul(handle.channels) {
        Some(value) => value,
        None => {
            // SAFETY: same caller-owned error buffer contract as above.
            unsafe { write_error("frame count is too large", error_buffer, error_capacity) };
            return ForgeStatus::InvalidArgument;
        }
    };
    if sample_count == 0 {
        return ForgeStatus::Ok;
    }
    if samples.is_null() {
        // SAFETY: same caller-owned error buffer contract as above.
        unsafe { write_error("samples is null", error_buffer, error_capacity) };
        return ForgeStatus::NullPointer;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the caller promises `sample_count` writable f32 values.
        let samples = unsafe { std::slice::from_raw_parts_mut(samples, sample_count) };
        handle
            .processor
            .process_interleaved(samples)
            .map_err(|error| (ForgeStatus::InvalidArgument, error))
    }));
    match result {
        Ok(Ok(())) => ForgeStatus::Ok,
        Ok(Err((status, message))) => {
            // SAFETY: same caller-owned error buffer contract as above.
            unsafe { write_error(&message, error_buffer, error_capacity) };
            status
        }
        Err(_) => {
            // SAFETY: same caller-owned error buffer contract as above.
            unsafe {
                write_error(
                    "unexpected panic while processing live audio",
                    error_buffer,
                    error_capacity,
                )
            };
            ForgeStatus::AnalysisFailed
        }
    }
}

/// Flush the look-ahead tail after end-of-stream. The caller provides storage
/// for at least `forge_normalizer_live_latency_frames_v1(handle)` frames;
/// exactly that many frames are written and reported through `written_frames`.
/// Flush is one-shot and invalidates further processing on the handle.
///
/// # Safety
///
/// `handle` must be a valid, uniquely accessed live handle and `written_frames`
/// must point to writable storage for one `usize`. When the reported latency
/// is non-zero, `output` must point to `capacity_frames * channels` writable
/// `f32` values. When non-null, `error_buffer` must be writable for
/// `error_capacity` bytes. All pointers must remain valid for the duration of
/// the call.
#[no_mangle]
pub unsafe extern "C" fn forge_normalizer_live_flush_interleaved_f32_v1(
    handle: *mut ForgeLiveV1,
    output: *mut f32,
    capacity_frames: usize,
    written_frames: *mut usize,
    error_buffer: *mut c_char,
    error_capacity: usize,
) -> ForgeStatus {
    // SAFETY: the optional error buffer follows the public C ABI contract.
    unsafe { write_error("", error_buffer, error_capacity) };
    if written_frames.is_null() {
        // SAFETY: same caller-owned error buffer contract as above.
        unsafe { write_error("written_frames is null", error_buffer, error_capacity) };
        return ForgeStatus::NullPointer;
    }
    // SAFETY: the caller provided a writable scalar for the result count.
    unsafe { written_frames.write(0) };
    let Some(handle) = (if handle.is_null() {
        None
    } else {
        // SAFETY: the caller promises a live, uniquely accessed handle.
        Some(unsafe { &mut *handle })
    }) else {
        // SAFETY: same caller-owned error buffer contract as above.
        unsafe { write_error("a required pointer is null", error_buffer, error_capacity) };
        return ForgeStatus::NullPointer;
    };
    if handle.flushed {
        // SAFETY: same caller-owned error buffer contract as above.
        unsafe {
            write_error(
                "live processor was already flushed",
                error_buffer,
                error_capacity,
            )
        };
        return ForgeStatus::InvalidArgument;
    }
    let latency_frames = handle.processor.latency_frames();
    if capacity_frames < latency_frames {
        // SAFETY: same caller-owned error buffer contract as above.
        unsafe {
            write_error(
                "flush output buffer is smaller than the processor latency",
                error_buffer,
                error_capacity,
            )
        };
        return ForgeStatus::BufferTooSmall;
    }
    let sample_count = match latency_frames.checked_mul(handle.channels) {
        Some(value) => value,
        None => {
            // SAFETY: same caller-owned error buffer contract as above.
            unsafe {
                write_error(
                    "latency sample count is too large",
                    error_buffer,
                    error_capacity,
                )
            };
            return ForgeStatus::InvalidArgument;
        }
    };
    if sample_count != 0 && output.is_null() {
        // SAFETY: same caller-owned error buffer contract as above.
        unsafe { write_error("output is null", error_buffer, error_capacity) };
        return ForgeStatus::NullPointer;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the capacity check establishes writable storage for the
        // exact latency-sized interleaved tail.
        let output = unsafe { std::slice::from_raw_parts_mut(output, sample_count) };
        output.fill(0.0);
        handle
            .processor
            .process_interleaved(output)
            .map_err(|error| (ForgeStatus::InvalidArgument, error))
    }));
    match result {
        Ok(Ok(())) => {
            handle.flushed = true;
            // SAFETY: the caller provided a writable scalar for the result count.
            unsafe { written_frames.write(latency_frames) };
            ForgeStatus::Ok
        }
        Ok(Err((status, message))) => {
            // SAFETY: same caller-owned error buffer contract as above.
            unsafe { write_error(&message, error_buffer, error_capacity) };
            status
        }
        Err(_) => {
            // SAFETY: same caller-owned error buffer contract as above.
            unsafe {
                write_error(
                    "unexpected panic while flushing live audio",
                    error_buffer,
                    error_capacity,
                )
            };
            ForgeStatus::AnalysisFailed
        }
    }
}

/// Update the smoothed target gain for a live processor.
///
/// # Safety
///
/// `handle` must be a valid, uniquely accessed live handle. When non-null,
/// `error_buffer` must be writable for `error_capacity` bytes.
#[no_mangle]
pub unsafe extern "C" fn forge_normalizer_live_set_target_gain_db_v1(
    handle: *mut ForgeLiveV1,
    gain_db: f64,
    error_buffer: *mut c_char,
    error_capacity: usize,
) -> ForgeStatus {
    live_mutate(handle, error_buffer, error_capacity, |processor| {
        if !gain_db.is_finite() || !(-120.0..=120.0).contains(&gain_db) {
            return Err("real-time target gain must be finite and within -120..120 dB".into());
        }
        processor.set_target_gain_db(gain_db)
    })
}

/// Update the true-peak ceiling for a live processor.
///
/// # Safety
///
/// `handle` must be a valid, uniquely accessed live handle. When non-null,
/// `error_buffer` must be writable for `error_capacity` bytes.
#[no_mangle]
pub unsafe extern "C" fn forge_normalizer_live_set_ceiling_dbtp_v1(
    handle: *mut ForgeLiveV1,
    ceiling_dbtp: f64,
    error_buffer: *mut c_char,
    error_capacity: usize,
) -> ForgeStatus {
    live_mutate(handle, error_buffer, error_capacity, |processor| {
        if !ceiling_dbtp.is_finite() || !(-120.0..=0.0).contains(&ceiling_dbtp) {
            return Err("real-time ceiling must be finite and within -120..0 dBTP".into());
        }
        processor.set_ceiling_dbfs(ceiling_dbtp)
    })
}

fn validate_live_config(config: &ForgeLiveConfigV1) -> Result<(), (ForgeStatus, String)> {
    if usize::try_from(config.struct_size).ok() < Some(LIVE_CONFIG_V1_SIZE) {
        return Err((
            ForgeStatus::BufferTooSmall,
            "ForgeLiveConfigV1 is smaller than the required size".into(),
        ));
    }
    if config.api_version != C_API_VERSION {
        return Err((
            ForgeStatus::InvalidArgument,
            "ForgeLiveConfigV1 api_version is unsupported".into(),
        ));
    }
    if !(8_000..=384_000).contains(&config.sample_rate_hz)
        || config.channels == 0
        || config.channels > 64
        || !config.initial_gain_db.is_finite()
        || !(-120.0..=120.0).contains(&config.initial_gain_db)
        || !config.ceiling_dbtp.is_finite()
        || !(-120.0..=0.0).contains(&config.ceiling_dbtp)
        || !config.attack_ms.is_finite()
        || !(0.01..=10_000.0).contains(&config.attack_ms)
        || !config.release_ms.is_finite()
        || !(0.01..=10_000.0).contains(&config.release_ms)
    {
        return Err((
            ForgeStatus::InvalidArgument,
            "live configuration is outside the bounded v1 limits".into(),
        ));
    }
    Ok(())
}

unsafe fn live_mutate(
    handle: *mut ForgeLiveV1,
    error_buffer: *mut c_char,
    error_capacity: usize,
    operation: impl FnOnce(&mut RealtimeGainProcessor) -> Result<(), String>,
) -> ForgeStatus {
    // SAFETY: the optional error buffer follows the public C ABI contract.
    unsafe { write_error("", error_buffer, error_capacity) };
    if handle.is_null() {
        // SAFETY: same caller-owned error buffer contract as above.
        unsafe { write_error("a required pointer is null", error_buffer, error_capacity) };
        return ForgeStatus::NullPointer;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the caller promises a live, uniquely accessed handle.
        let handle = unsafe { &mut *handle };
        if handle.flushed {
            return Err("live processor was flushed; create a new handle".into());
        }
        operation(&mut handle.processor)
    }));
    match result {
        Ok(Ok(())) => ForgeStatus::Ok,
        Ok(Err(message)) => {
            // SAFETY: same caller-owned error buffer contract as above.
            unsafe { write_error(&message, error_buffer, error_capacity) };
            ForgeStatus::InvalidArgument
        }
        Err(_) => {
            // SAFETY: same caller-owned error buffer contract as above.
            unsafe {
                write_error(
                    "unexpected panic while updating live processor",
                    error_buffer,
                    error_capacity,
                )
            };
            ForgeStatus::AnalysisFailed
        }
    }
}

unsafe fn write_error(message: &str, buffer: *mut c_char, capacity: usize) {
    if buffer.is_null() || capacity == 0 {
        return;
    }
    let mut length = message.len().min(capacity - 1);
    while !message.is_char_boundary(length) {
        length -= 1;
    }
    // SAFETY: the caller promises `capacity` writable bytes. `length` is at
    // most `capacity - 1`, leaving one byte for the terminator.
    unsafe {
        ptr::copy_nonoverlapping(message.as_ptr(), buffer.cast::<u8>(), length);
        buffer.add(length).write(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    #[test]
    fn v1_layout_and_discriminants_are_fixed() {
        assert_eq!(size_of::<ForgeStatus>(), 4);
        assert_eq!(size_of::<ForgeAnalysisV1>(), ANALYSIS_V1_SIZE);
        assert_eq!(align_of::<ForgeAnalysisV1>(), 8);
        assert_eq!(offset_of!(ForgeAnalysisV1, struct_size), 0);
        assert_eq!(offset_of!(ForgeAnalysisV1, frames), 16);
        assert_eq!(offset_of!(ForgeAnalysisV1, integrated_lufs), 24);
        assert_eq!(offset_of!(ForgeAnalysisV1, true_peak_dbtp), 72);
        assert_eq!(ForgeStatus::Ok as i32, 0);
        assert_eq!(ForgeStatus::AnalysisFailed as i32, 5);
    }

    #[test]
    fn error_writer_truncates_on_a_utf8_boundary_and_terminates() {
        let mut bytes = [1_i8; 5];
        // SAFETY: `bytes` is a writable five-byte buffer.
        unsafe { write_error("ééé", bytes.as_mut_ptr(), bytes.len()) };
        assert_eq!(bytes.map(|byte| byte as u8), [0xc3, 0xa9, 0xc3, 0xa9, 0]);
    }

    #[test]
    fn live_c_api_preserves_latency_and_flushes_exact_tail() {
        let config = ForgeLiveConfigV1 {
            struct_size: LIVE_CONFIG_V1_SIZE as u32,
            api_version: C_API_VERSION,
            sample_rate_hz: 48_000,
            channels: 2,
            initial_gain_db: 0.0,
            ceiling_dbtp: -1.0,
            attack_ms: 10.0,
            release_ms: 100.0,
        };
        let mut error = [0_i8; 256];
        // SAFETY: config and error are valid caller-owned storage.
        let handle =
            unsafe { forge_normalizer_live_create_v1(&config, error.as_mut_ptr(), error.len()) };
        assert!(!handle.is_null(), "{}", error_text(&error));
        // SAFETY: handle was returned by the create function.
        let latency = unsafe { forge_normalizer_live_latency_frames_v1(handle) };
        assert_eq!(latency, 240);

        let frames = latency + 32;
        let mut samples = vec![0.0_f32; frames * 2];
        samples[(frames - 1) * 2] = 0.25;
        // SAFETY: samples and error provide the declared writable storage.
        let status = unsafe {
            forge_normalizer_live_process_interleaved_f32_v1(
                handle,
                samples.as_mut_ptr(),
                frames,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        assert_eq!(status, ForgeStatus::Ok, "{}", error_text(&error));
        assert!(samples[..latency * 2].iter().all(|sample| *sample == 0.0));

        let mut too_small = vec![0.0_f32; (latency - 1) * 2];
        let mut written = usize::MAX;
        // SAFETY: the handle and output count are valid; the deliberately
        // short output is covered by the ABI's buffer-size contract.
        let status = unsafe {
            forge_normalizer_live_flush_interleaved_f32_v1(
                handle,
                too_small.as_mut_ptr(),
                latency - 1,
                &mut written,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        assert_eq!(status, ForgeStatus::BufferTooSmall);
        assert_eq!(written, 0);

        let mut tail = vec![0.0_f32; latency * 2];
        // SAFETY: tail has exactly the required latency-sized capacity.
        let status = unsafe {
            forge_normalizer_live_flush_interleaved_f32_v1(
                handle,
                tail.as_mut_ptr(),
                latency,
                &mut written,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        assert_eq!(status, ForgeStatus::Ok, "{}", error_text(&error));
        assert_eq!(written, latency);
        assert!(tail.iter().any(|sample| sample.abs() > 0.0));

        // SAFETY: the handle is still valid, and a zero-frame call does not
        // dereference a samples pointer.
        let status = unsafe {
            forge_normalizer_live_process_interleaved_f32_v1(
                handle,
                ptr::null_mut(),
                0,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        assert_eq!(status, ForgeStatus::InvalidArgument);
        // SAFETY: release the owned handle exactly once.
        unsafe { forge_normalizer_live_destroy_v1(handle) };
    }

    #[test]
    fn live_c_api_rejects_unbounded_configuration_and_setters() {
        let mut config = ForgeLiveConfigV1 {
            struct_size: LIVE_CONFIG_V1_SIZE as u32,
            api_version: C_API_VERSION,
            sample_rate_hz: 48_000,
            channels: 2,
            initial_gain_db: 0.0,
            ceiling_dbtp: 1.0,
            attack_ms: 10.0,
            release_ms: 100.0,
        };
        let mut error = [0_i8; 128];
        // SAFETY: config and error are valid caller-owned storage.
        let handle =
            unsafe { forge_normalizer_live_create_v1(&config, error.as_mut_ptr(), error.len()) };
        assert!(handle.is_null());
        assert!(!error_text(&error).is_empty());

        config.ceiling_dbtp = -1.0;
        // SAFETY: config and error are valid caller-owned storage.
        let handle =
            unsafe { forge_normalizer_live_create_v1(&config, error.as_mut_ptr(), error.len()) };
        assert!(!handle.is_null(), "{}", error_text(&error));
        // SAFETY: handle and error are valid for this call.
        let status = unsafe {
            forge_normalizer_live_set_target_gain_db_v1(
                handle,
                121.0,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        assert_eq!(status, ForgeStatus::InvalidArgument);
        // SAFETY: handle and error are valid for this call.
        let status = unsafe {
            forge_normalizer_live_set_ceiling_dbtp_v1(
                handle,
                -121.0,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        assert_eq!(status, ForgeStatus::InvalidArgument);
        // SAFETY: release the owned handle exactly once.
        unsafe { forge_normalizer_live_destroy_v1(handle) };
    }

    fn error_text(buffer: &[c_char]) -> &str {
        // SAFETY: all tested API paths NUL-terminate this initialized buffer.
        unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_str()
            .expect("UTF-8 error text")
    }
}
