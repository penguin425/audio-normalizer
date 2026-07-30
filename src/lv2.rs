//! Minimal dependency-free LV2 stereo wrapper around the callback-safe gain
//! processor. The exported ABI follows LV2 Core 1.18.

use crate::realtime::{RealtimeGainConfig, RealtimeGainProcessor};
use std::ffi::{c_char, c_void};
use std::ptr;

const URI: &[u8] = b"https://penguin425.github.io/audio-normalizer/plugins/forge-live\0";

#[repr(C)]
pub struct Lv2Descriptor {
    uri: *const c_char,
    instantiate: Option<
        unsafe extern "C" fn(
            descriptor: *const Lv2Descriptor,
            rate: f64,
            bundle_path: *const c_char,
            features: *const *const c_void,
        ) -> *mut c_void,
    >,
    connect_port: Option<unsafe extern "C" fn(instance: *mut c_void, port: u32, data: *mut c_void)>,
    activate: Option<unsafe extern "C" fn(instance: *mut c_void)>,
    run: Option<unsafe extern "C" fn(instance: *mut c_void, sample_count: u32)>,
    deactivate: Option<unsafe extern "C" fn(instance: *mut c_void)>,
    cleanup: Option<unsafe extern "C" fn(instance: *mut c_void)>,
    extension_data: Option<unsafe extern "C" fn(uri: *const c_char) -> *const c_void>,
}

// LV2 descriptors are immutable process-wide metadata.
unsafe impl Sync for Lv2Descriptor {}

struct Instance {
    processor: RealtimeGainProcessor,
    input_left: *const f32,
    input_right: *const f32,
    output_left: *mut f32,
    output_right: *mut f32,
    gain_db: *const f32,
    latency_frames: *mut f32,
}

unsafe extern "C" fn instantiate(
    _descriptor: *const Lv2Descriptor,
    rate: f64,
    _bundle_path: *const c_char,
    _features: *const *const c_void,
) -> *mut c_void {
    if !rate.is_finite() || !(1.0..=u32::MAX as f64).contains(&rate) {
        return ptr::null_mut();
    }
    let Ok(processor) =
        RealtimeGainProcessor::new(rate.round() as u32, 2, RealtimeGainConfig::default())
    else {
        return ptr::null_mut();
    };
    Box::into_raw(Box::new(Instance {
        processor,
        input_left: ptr::null(),
        input_right: ptr::null(),
        output_left: ptr::null_mut(),
        output_right: ptr::null_mut(),
        gain_db: ptr::null(),
        latency_frames: ptr::null_mut(),
    }))
    .cast()
}

unsafe extern "C" fn connect_port(instance: *mut c_void, port: u32, data: *mut c_void) {
    if instance.is_null() {
        return;
    }
    // SAFETY: LV2 guarantees that instance is the handle returned by instantiate.
    let instance = unsafe { &mut *instance.cast::<Instance>() };
    match port {
        0 => instance.input_left = data.cast_const().cast(),
        1 => instance.input_right = data.cast_const().cast(),
        2 => instance.output_left = data.cast(),
        3 => instance.output_right = data.cast(),
        4 => instance.gain_db = data.cast_const().cast(),
        5 => instance.latency_frames = data.cast(),
        _ => {}
    }
}

unsafe extern "C" fn run(instance: *mut c_void, sample_count: u32) {
    if instance.is_null() {
        return;
    }
    // SAFETY: LV2 guarantees the instance and connected buffers remain valid for run.
    let instance = unsafe { &mut *instance.cast::<Instance>() };
    if !instance.latency_frames.is_null() {
        // SAFETY: output control ports contain one writable f32 for the duration of run.
        unsafe {
            *instance.latency_frames = instance.processor.latency_frames() as f32;
        }
    }
    if instance.input_left.is_null()
        || instance.input_right.is_null()
        || instance.output_left.is_null()
        || instance.output_right.is_null()
    {
        return;
    }
    if !instance.gain_db.is_null() {
        // SAFETY: control ports contain one f32 for the duration of run.
        let gain = unsafe { *instance.gain_db };
        if gain.is_finite() {
            let _ = instance.processor.set_target_gain_db(f64::from(gain));
        }
    }
    for frame in 0..sample_count as usize {
        // SAFETY: audio ports contain sample_count f32 values for this run call.
        let mut stereo = unsafe {
            [
                *instance.input_left.add(frame),
                *instance.input_right.add(frame),
            ]
        };
        if instance.processor.process_interleaved(&mut stereo).is_err() {
            return;
        }
        // SAFETY: output ports contain sample_count writable f32 values.
        unsafe {
            *instance.output_left.add(frame) = stereo[0];
            *instance.output_right.add(frame) = stereo[1];
        }
    }
}

unsafe extern "C" fn cleanup(instance: *mut c_void) {
    if !instance.is_null() {
        // SAFETY: cleanup is called once for the handle returned by instantiate.
        drop(unsafe { Box::from_raw(instance.cast::<Instance>()) });
    }
}

unsafe extern "C" fn extension_data(_uri: *const c_char) -> *const c_void {
    ptr::null()
}

static DESCRIPTOR: Lv2Descriptor = Lv2Descriptor {
    uri: URI.as_ptr().cast(),
    instantiate: Some(instantiate),
    connect_port: Some(connect_port),
    activate: None,
    run: Some(run),
    deactivate: None,
    cleanup: Some(cleanup),
    extension_data: Some(extension_data),
};

/// Return the single Forge LV2 plugin descriptor.
#[no_mangle]
pub extern "C" fn lv2_descriptor(index: u32) -> *const Lv2Descriptor {
    if index == 0 {
        &DESCRIPTOR
    } else {
        ptr::null()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_parity_lv2_output_and_latency_match_realtime_processor() {
        let descriptor = lv2_descriptor(0);
        assert!(!descriptor.is_null());
        let frames = 1_024;
        let left = (0..frames)
            .map(|frame| {
                let carrier = (frame as f32 * 0.071).sin();
                let envelope = if (256..640).contains(&frame) {
                    1.35
                } else {
                    0.23
                };
                carrier * envelope
            })
            .collect::<Vec<_>>();
        let right = left
            .iter()
            .enumerate()
            .map(|(frame, sample)| {
                if frame.is_multiple_of(5) {
                    -*sample * 0.71
                } else {
                    *sample * 0.43
                }
            })
            .collect::<Vec<_>>();
        let mut expected = left
            .iter()
            .zip(&right)
            .flat_map(|(left, right)| [*left, *right])
            .collect::<Vec<_>>();
        let mut reference =
            RealtimeGainProcessor::new(48_000, 2, RealtimeGainConfig::default()).unwrap();
        reference.set_target_gain_db(-6.0).unwrap();
        reference.process_interleaved(&mut expected).unwrap();

        // SAFETY: this test follows the LV2 host lifecycle and buffer contract.
        unsafe {
            let descriptor = &*descriptor;
            let instance =
                descriptor.instantiate.unwrap()(descriptor, 48_000.0, ptr::null(), ptr::null());
            assert!(!instance.is_null());
            let mut out_left = vec![0.0_f32; frames];
            let mut out_right = vec![0.0_f32; frames];
            let mut gain = -6.0_f32;
            let mut latency = -1.0_f32;
            let connect = descriptor.connect_port.unwrap();
            connect(instance, 4, (&mut gain as *mut f32).cast());
            connect(instance, 5, (&mut latency as *mut f32).cast());
            connect(instance, 0, left.as_ptr().cast_mut().cast());
            connect(instance, 1, right.as_ptr().cast_mut().cast());
            connect(instance, 2, out_left.as_mut_ptr().cast());
            connect(instance, 3, out_right.as_mut_ptr().cast());
            descriptor.run.unwrap()(instance, 0);
            assert_eq!(latency, reference.latency_frames() as f32);
            for (start, sample_count) in [(0, 137), (137, 389), (526, frames - 526)] {
                connect(instance, 0, left.as_ptr().add(start).cast_mut().cast());
                connect(instance, 1, right.as_ptr().add(start).cast_mut().cast());
                connect(instance, 2, out_left.as_mut_ptr().add(start).cast());
                connect(instance, 3, out_right.as_mut_ptr().add(start).cast());
                descriptor.run.unwrap()(instance, sample_count as u32);
            }
            let actual = out_left
                .iter()
                .zip(&out_right)
                .flat_map(|(left, right)| [*left, *right])
                .collect::<Vec<_>>();
            assert_eq!(actual, expected);
            assert_eq!(latency, reference.latency_frames() as f32);
            assert_eq!(latency, 240.0);
            descriptor.cleanup.unwrap()(instance);
        }
        assert!(lv2_descriptor(1).is_null());
    }
}
