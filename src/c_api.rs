//! Versioned, allocation-free-at-the-boundary C API.
//!
//! The ABI uses fixed-width scalar fields, caller-owned output storage, and
//! integer status codes. Rust-owned strings, vectors, enums, and references
//! never cross the boundary. See `C-API.md` and `include/forge_normalizer.h`
//! for the public contract.

use crate::{decoder, normalize};
use std::ffi::CStr;
use std::os::raw::c_char;
use std::path::Path;
use std::ptr;

/// Current major version of the Forge C ABI.
pub const C_API_VERSION: u32 = 1;

/// Required byte size of [`ForgeAnalysisV1`].
pub const ANALYSIS_V1_SIZE: usize = 80;

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
}
