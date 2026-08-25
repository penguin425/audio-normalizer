//! Optional CUDA chunk worker for streaming true-peak analysis.
//!
//! The checked-in PTX is loaded through the CUDA Driver API, so Cargo builds
//! and deployed binaries do not require the CUDA toolkit. Each chunk is copied
//! and launched before CPU K-weighting begins; the analyzer synchronizes only
//! after its CPU work, allowing transfer/kernel work to overlap. A single
//! process-wide lease bounds device memory and stream concurrency. Any setup or
//! runtime error hands the retained history and peak back to CPU meters.

use super::truepeak::{oversample_factor, phase_table, TruePeakMeter, MAX_PHASES};
use cudarc::driver::{
    CudaContext, CudaFunction, CudaSlice, CudaStream, LaunchConfig, PinnedHostSlice, PushKernelArg,
};
use cudarc::nvrtc::Ptx;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

const HISTORY: usize = 15;
const CUDA_THREADS: u32 = 256;
const MIN_CUDA_FRAMES: usize = 16_384;
const PTX: &str = include_str!("cuda/truepeak.ptx");

struct CudaShared {
    context: Arc<CudaContext>,
    function: CudaFunction,
    device_name: String,
}

static SHARED: OnceLock<Result<Arc<CudaShared>, String>> = OnceLock::new();
static ACTIVE_WORKER: AtomicBool = AtomicBool::new(false);

fn shared() -> Result<Arc<CudaShared>, String> {
    SHARED
        .get_or_init(|| {
            // cudarc's dynamic loader deliberately panics when asked to call a
            // missing driver. Probe first so a CUDA-enabled Forge binary still
            // has a normal CPU fallback on non-NVIDIA systems.
            if !unsafe { cudarc::driver::sys::is_culib_present() } {
                return Err("NVIDIA CUDA driver library was not found".into());
            }
            let context = CudaContext::new(0)
                .map_err(|error| format!("initialize CUDA device 0: {error}"))?;
            let device_name = context
                .name()
                .unwrap_or_else(|_| "CUDA device 0".to_string());
            let module = context
                .load_module(Ptx::from_src(PTX))
                .map_err(|error| format!("load embedded true-peak PTX: {error}"))?;
            let function = module
                .load_function("forge_true_peak_chunk")
                .map_err(|error| format!("load CUDA true-peak kernel: {error}"))?;
            Ok(Arc::new(CudaShared {
                context,
                function,
                device_name,
            }))
        })
        .clone()
}

/// Validate the optional runtime without acquiring the bounded worker lease.
pub(crate) fn probe() -> Result<String, String> {
    require_matching_cpu_arithmetic()?;
    shared().map(|runtime| runtime.device_name.clone())
}

fn require_matching_cpu_arithmetic() -> Result<(), String> {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") {
        return Ok(());
    }
    #[cfg(target_arch = "aarch64")]
    {
        // Advanced SIMD and fused floating-point multiply-add are mandatory in
        // AArch64 and match the CPU TruePeakMeter implementation.
        return Ok(());
    }
    #[allow(unreachable_code)]
    Err("exact CUDA true-peak requires AVX2/FMA on x86-64 or fused Advanced SIMD on AArch64".into())
}

struct WorkerLease;

impl WorkerLease {
    fn acquire() -> Result<Self, String> {
        ACTIVE_WORKER
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self)
            .map_err(|_| "the bounded CUDA true-peak worker is already in use".into())
    }
}

impl Drop for WorkerLease {
    fn drop(&mut self) {
        ACTIVE_WORKER.store(false, Ordering::Release);
    }
}

pub(crate) struct CudaTruePeakWorker {
    _lease: WorkerLease,
    sample_rate: u32,
    channels: usize,
    factor: usize,
    stream: Arc<CudaStream>,
    function: CudaFunction,
    samples: CudaSlice<f32>,
    coefficients: CudaSlice<f64>,
    device_peaks: CudaSlice<u32>,
    host_chunk_peaks: PinnedHostSlice<u32>,
    capacity_frames: usize,
    recent: Vec<Vec<f32>>,
    prefix: Vec<[f32; HISTORY]>,
    peaks: Vec<f32>,
}

impl CudaTruePeakWorker {
    pub(crate) fn eligible(sample_rate: u32, channels: usize, frames: usize) -> bool {
        require_matching_cpu_arithmetic().is_ok()
            && channels >= 2
            && frames >= MIN_CUDA_FRAMES
            && matches!(oversample_factor(sample_rate), 2 | 4)
            && frames <= u32::MAX as usize
            && channels <= u32::MAX as usize
    }

    pub(crate) fn new(
        sample_rate: u32,
        channels: usize,
        initial_frames: usize,
    ) -> Result<Self, String> {
        if !Self::eligible(sample_rate, channels, initial_frames) {
            return Err("audio chunk is not eligible for CUDA true-peak processing".into());
        }
        let lease = WorkerLease::acquire()?;
        let shared = shared()?;
        let stream = shared
            .context
            .new_stream()
            .map_err(|error| format!("create CUDA true-peak stream: {error}"))?;
        let factor = oversample_factor(sample_rate);
        let table = phase_table(factor);
        let mut flattened = [0.0f64; 16 * MAX_PHASES];
        for tap in 0..16 {
            for phase in 0..MAX_PHASES {
                flattened[tap * MAX_PHASES + phase] = table[tap][phase];
            }
        }
        let coefficients = stream
            .clone_htod(&flattened)
            .map_err(|error| format!("upload CUDA true-peak coefficients: {error}"))?;
        let device_peaks = stream
            .alloc_zeros::<u32>(channels)
            .map_err(|error| format!("allocate CUDA true-peak result: {error}"))?;
        let sample_count = allocation_len(channels, initial_frames)?;
        let samples = stream
            .alloc_zeros::<f32>(sample_count)
            .map_err(|error| format!("allocate CUDA true-peak input: {error}"))?;
        let host_chunk_peaks = unsafe { shared.context.alloc_pinned::<u32>(channels) }
            .map_err(|error| format!("allocate pinned CUDA true-peak result: {error}"))?;
        stream
            .synchronize()
            .map_err(|error| format!("initialize CUDA true-peak worker: {error}"))?;
        Ok(Self {
            _lease: lease,
            sample_rate,
            channels,
            factor,
            stream,
            function: shared.function.clone(),
            samples,
            coefficients,
            device_peaks,
            host_chunk_peaks,
            capacity_frames: initial_frames,
            recent: (0..channels).map(|_| Vec::with_capacity(HISTORY)).collect(),
            prefix: vec![[0.0; HISTORY]; channels],
            peaks: vec![0.0; channels],
        })
    }

    /// Queue transfers, interpolation, reduction, and the tiny result copy.
    /// The caller performs CPU K-weighting before calling [`Self::finish_chunk`].
    pub(crate) fn begin_chunk<C>(&mut self, planar: &[C]) -> Result<(), String>
    where
        C: AsRef<[f32]>,
    {
        let frames = validate_planar(planar, self.channels)?;
        if frames == 0 {
            return Ok(());
        }
        self.ensure_capacity(frames)?;
        self.prepare_prefix(planar);
        let stride = HISTORY + self.capacity_frames;
        self.stream
            .memset_zeros(&mut self.device_peaks)
            .map_err(|error| format!("clear CUDA true-peak result: {error}"))?;
        for (channel_index, channel) in planar.iter().enumerate() {
            let start = channel_index * stride;
            {
                let mut destination = self.samples.slice_mut(start..start + HISTORY);
                self.stream
                    .memcpy_htod(&self.prefix[channel_index], &mut destination)
                    .map_err(|error| format!("upload CUDA true-peak history: {error}"))?;
            }
            {
                let mut destination = self
                    .samples
                    .slice_mut(start + HISTORY..start + HISTORY + frames);
                self.stream
                    .memcpy_htod(channel.as_ref(), &mut destination)
                    .map_err(|error| format!("upload CUDA true-peak samples: {error}"))?;
            }
        }

        let stride = stride as u64;
        let frames = frames as u32;
        let channels = self.channels as u32;
        let factor = self.factor as u32;
        let config = LaunchConfig {
            grid_dim: (frames.div_ceil(CUDA_THREADS), channels, 1),
            block_dim: (CUDA_THREADS, 1, 1),
            shared_mem_bytes: 0,
        };
        let mut launch = self.stream.launch_builder(&self.function);
        launch
            .arg(&self.samples)
            .arg(&self.coefficients)
            .arg(&stride)
            .arg(&frames)
            .arg(&channels)
            .arg(&factor)
            .arg(&mut self.device_peaks);
        unsafe { launch.launch(config) }
            .map_err(|error| format!("launch CUDA true-peak kernel: {error}"))?;
        self.stream
            .memcpy_dtoh(&self.device_peaks, &mut self.host_chunk_peaks)
            .map_err(|error| format!("download CUDA true-peak result: {error}"))?;
        Ok(())
    }

    pub(crate) fn finish_chunk<C>(&mut self, planar: &[C]) -> Result<(), String>
    where
        C: AsRef<[f32]>,
    {
        self.stream
            .synchronize()
            .map_err(|error| format!("synchronize CUDA true-peak worker: {error}"))?;
        let chunk_peaks = self
            .host_chunk_peaks
            .as_slice()
            .map_err(|error| format!("read CUDA true-peak result: {error}"))?;
        for (peak, bits) in self.peaks.iter_mut().zip(chunk_peaks.iter().copied()) {
            *peak = peak.max(f32::from_bits(bits));
        }
        for (recent, channel) in self.recent.iter_mut().zip(planar) {
            update_recent(recent, channel.as_ref());
        }
        Ok(())
    }

    pub(crate) fn peak(&self) -> f32 {
        self.peaks.iter().copied().fold(0.0, f32::max)
    }

    /// Recover exact CPU stream state after a driver failure. `recent` only
    /// contains chunks that completed successfully, so the failing chunk can
    /// be replayed once by the caller without double-counting.
    pub(crate) fn into_cpu_meters(self) -> Vec<TruePeakMeter> {
        let _ = self.stream.synchronize();
        self.recent
            .iter()
            .zip(self.peaks.iter().copied())
            .map(|(recent, peak)| {
                TruePeakMeter::from_recent_samples(self.sample_rate, recent, peak)
            })
            .collect()
    }

    fn ensure_capacity(&mut self, frames: usize) -> Result<(), String> {
        if frames <= self.capacity_frames {
            return Ok(());
        }
        self.stream
            .synchronize()
            .map_err(|error| format!("resize CUDA true-peak input: {error}"))?;
        let capacity = frames.next_power_of_two();
        let sample_count = allocation_len(self.channels, capacity)?;
        let replacement = self
            .stream
            .alloc_zeros::<f32>(sample_count)
            .map_err(|error| format!("resize CUDA true-peak input: {error}"))?;
        self.samples = replacement;
        self.capacity_frames = capacity;
        Ok(())
    }

    fn prepare_prefix<C>(&mut self, planar: &[C])
    where
        C: AsRef<[f32]>,
    {
        for ((destination, recent), channel) in self.prefix.iter_mut().zip(&self.recent).zip(planar)
        {
            let channel = channel.as_ref();
            if recent.is_empty() {
                destination.fill(channel[0]);
            } else {
                destination.fill(recent[0]);
                destination[HISTORY - recent.len()..].copy_from_slice(recent);
            }
        }
    }
}

fn allocation_len(channels: usize, frames: usize) -> Result<usize, String> {
    frames
        .checked_add(HISTORY)
        .and_then(|stride| stride.checked_mul(channels))
        .ok_or_else(|| "CUDA true-peak allocation size overflow".into())
}

fn validate_planar<C>(planar: &[C], channels: usize) -> Result<usize, String>
where
    C: AsRef<[f32]>,
{
    if planar.len() != channels {
        return Err("CUDA true-peak channel count changed".into());
    }
    let frames = planar.first().map_or(0, |channel| channel.as_ref().len());
    if planar
        .iter()
        .any(|channel| channel.as_ref().len() != frames)
    {
        return Err("CUDA true-peak channel length mismatch".into());
    }
    Ok(frames)
}

fn update_recent(recent: &mut Vec<f32>, samples: &[f32]) {
    if samples.len() >= HISTORY {
        recent.clear();
        recent.extend_from_slice(&samples[samples.len() - HISTORY..]);
        return;
    }
    let keep = HISTORY - samples.len();
    if recent.len() > keep {
        recent.drain(..recent.len() - keep);
    }
    recent.extend_from_slice(samples);
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn embedded_ptx_identifies_current_cuda_source() {
        let source = include_bytes!("cuda/truepeak.cu");
        let digest = Sha256::digest(source)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert!(
            PTX.lines()
                .any(|line| line == format!("// Source SHA-256: {digest}")),
            "regenerate truepeak.ptx after changing truepeak.cu"
        );
    }

    #[test]
    fn recent_history_is_bounded_and_ordered() {
        let mut recent = Vec::with_capacity(HISTORY);
        update_recent(&mut recent, &[1.0, 2.0, 3.0]);
        update_recent(&mut recent, &[4.0, 5.0]);
        assert_eq!(recent, [1.0, 2.0, 3.0, 4.0, 5.0]);
        let long = (10..40).map(|value| value as f32).collect::<Vec<_>>();
        update_recent(&mut recent, &long);
        assert_eq!(
            recent,
            (25..40).map(|value| value as f32).collect::<Vec<_>>()
        );
        update_recent(&mut recent, &[40.0, 41.0]);
        assert_eq!(
            recent,
            (27..42).map(|value| value as f32).collect::<Vec<_>>()
        );
    }

    #[test]
    fn cuda_chunks_match_cpu_true_peak_bits_and_recover_state() {
        let channels = 8;
        let first_frames = 20_003;
        let second_frames = 17_011;
        let mut first = vec![vec![0.0f32; first_frames]; channels];
        let mut second = vec![vec![0.0f32; second_frames]; channels];
        for (channel_index, channel) in first.iter_mut().enumerate() {
            for (frame, sample) in channel.iter_mut().enumerate() {
                *sample = fixture_sample(channel_index, frame);
            }
        }
        for (channel_index, channel) in second.iter_mut().enumerate() {
            for (frame, sample) in channel.iter_mut().enumerate() {
                *sample = fixture_sample(channel_index, first_frames + frame);
            }
        }

        let Ok(mut worker) = CudaTruePeakWorker::new(48_000, channels, first_frames) else {
            eprintln!("CUDA runtime unavailable; exact accelerator test skipped");
            return;
        };
        worker.begin_chunk(&first).unwrap();
        worker.finish_chunk(&first).unwrap();

        let mut reference = (0..channels)
            .map(|_| TruePeakMeter::for_sample_rate(48_000))
            .collect::<Vec<_>>();
        for (meter, channel) in reference.iter_mut().zip(&first) {
            meter.process(channel);
        }
        let expected_first = reference
            .iter()
            .map(TruePeakMeter::peak)
            .fold(0.0, f32::max);
        assert_eq!(worker.peak().to_bits(), expected_first.to_bits());

        let mut recovered = worker.into_cpu_meters();
        for (meter, channel) in recovered.iter_mut().zip(&second) {
            meter.process(channel);
        }
        for (meter, channel) in reference.iter_mut().zip(&second) {
            meter.process(channel);
        }
        let recovered_peak = recovered
            .iter()
            .map(TruePeakMeter::peak)
            .fold(0.0, f32::max);
        let reference_peak = reference
            .iter()
            .map(TruePeakMeter::peak)
            .fold(0.0, f32::max);
        assert_eq!(recovered_peak.to_bits(), reference_peak.to_bits());

        // Exercise the 2x table plus exceptional input behavior without a
        // second parallel test competing for the single bounded GPU lease.
        let frames = 18_019;
        let mut factor_two = vec![vec![0.0f32; frames]; 2];
        for (channel_index, channel) in factor_two.iter_mut().enumerate() {
            for (frame, sample) in channel.iter_mut().enumerate() {
                *sample = fixture_sample(channel_index, frame);
            }
        }
        factor_two[0][101] = f32::from_bits(1);
        factor_two[1][307] = f32::NAN;
        let mut factor_two_reference = (0..2)
            .map(|_| TruePeakMeter::for_sample_rate(96_000))
            .collect::<Vec<_>>();
        for (meter, channel) in factor_two_reference.iter_mut().zip(&factor_two) {
            meter.process(channel);
        }
        let mut factor_two_worker =
            CudaTruePeakWorker::new(96_000, 2, frames).expect("CUDA lease was released");
        factor_two_worker.begin_chunk(&factor_two).unwrap();
        factor_two_worker.finish_chunk(&factor_two).unwrap();
        let expected = factor_two_reference
            .iter()
            .map(TruePeakMeter::peak)
            .fold(0.0, f32::max);
        assert_eq!(factor_two_worker.peak().to_bits(), expected.to_bits());

        factor_two[0][frames - 19] = f32::INFINITY;
        factor_two_worker.begin_chunk(&factor_two).unwrap();
        factor_two_worker.finish_chunk(&factor_two).unwrap();
        assert!(factor_two_worker.peak().is_infinite());
        assert!(!CudaTruePeakWorker::eligible(192_000, 2, frames));
    }

    fn fixture_sample(channel: usize, frame: usize) -> f32 {
        let phase = frame as f64 * (0.017 + channel as f64 * 0.0013);
        let carrier = phase.sin() * 0.73;
        let transient = if (frame + channel * 97).is_multiple_of(4093) {
            0.98
        } else {
            0.0
        };
        (carrier + transient) as f32
    }
}
