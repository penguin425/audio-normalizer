//! Private resource-control implementation shared by service transports.

#![allow(
    dead_code,
    reason = "some private runtime diagnostics and cancellation hooks are transport/test specific"
)]

use crate::analysis::Analysis;
use crate::channel_layout::ChannelLayoutDescriptor;
use crate::decoder::{self, AnalysisPcmChunk, InputDescriptor, InputDescriptorOptions};
use crate::dsp::lufs;
use crate::stable_input::{
    create_snapshot, StableInput, StableInputError, StableInputOptions, StableInputTransfer,
};
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServiceRuntimeErrorKind {
    InvalidLimit,
    LimitExceeded,
    QuotaExceeded,
    ArithmeticOverflow,
    Cancelled,
    DeadlineExceeded,
    Io,
    IncompleteUpload,
    InvalidSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServiceRuntimeError {
    kind: ServiceRuntimeErrorKind,
    message: String,
}

impl ServiceRuntimeError {
    pub(crate) fn new(kind: ServiceRuntimeErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn io(context: &str, error: io::Error) -> Self {
        Self::new(ServiceRuntimeErrorKind::Io, format!("{context}: {error}"))
    }

    fn snapshot(error: StableInputError) -> Self {
        Self::new(
            ServiceRuntimeErrorKind::InvalidSnapshot,
            format!("adopt completed upload snapshot: {error}"),
        )
    }

    pub(crate) const fn kind(&self) -> ServiceRuntimeErrorKind {
        self.kind
    }
}

impl fmt::Display for ServiceRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ServiceRuntimeError {}

/// One process-wide byte budget.
///
/// A zero-capacity quota is valid and can grant only zero-byte leases. Every
/// non-zero acquisition uses checked arithmetic and a CAS, so an overflow or
/// capacity failure leaves `used` unchanged.
#[derive(Debug)]
pub(crate) struct ByteQuota {
    capacity: u64,
    used: AtomicU64,
}

impl ByteQuota {
    pub(crate) const fn new(capacity: u64) -> Self {
        Self {
            capacity,
            used: AtomicU64::new(0),
        }
    }

    pub(crate) fn try_acquire(
        self: &Arc<Self>,
        bytes: u64,
    ) -> Result<QuotaLease, ServiceRuntimeError> {
        if bytes == 0 {
            return Ok(QuotaLease {
                quota: Some(Arc::clone(self)),
                bytes,
            });
        }

        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let Some(next) = used.checked_add(bytes) else {
                return Err(ServiceRuntimeError::new(
                    ServiceRuntimeErrorKind::ArithmeticOverflow,
                    format!("byte quota usage overflow: {used} + {bytes}"),
                ));
            };
            if next > self.capacity {
                return Err(ServiceRuntimeError::new(
                    ServiceRuntimeErrorKind::QuotaExceeded,
                    format!(
                        "byte quota capacity {} exceeded: {used} used + {bytes} requested",
                        self.capacity
                    ),
                ));
            }
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    return Ok(QuotaLease {
                        quota: Some(Arc::clone(self)),
                        bytes,
                    });
                }
                Err(observed) => used = observed,
            }
        }
    }

    pub(crate) const fn capacity(&self) -> u64 {
        self.capacity
    }

    pub(crate) fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }

    pub(crate) fn available(&self) -> u64 {
        self.capacity.saturating_sub(self.used())
    }
}

/// A non-cloneable RAII claim on a [`ByteQuota`].
#[derive(Debug)]
pub(crate) struct QuotaLease {
    quota: Option<Arc<ByteQuota>>,
    bytes: u64,
}

impl QuotaLease {
    pub(crate) const fn amount(&self) -> u64 {
        self.bytes
    }

    fn release(&mut self) {
        let Some(quota) = self.quota.take() else {
            return;
        };
        if self.bytes == 0 {
            return;
        }

        // Do not use fetch_sub in Drop: even an internal invariant failure must
        // never wrap the global counter or panic while another panic unwinds.
        let mut used = quota.used.load(Ordering::Acquire);
        loop {
            let Some(next) = used.checked_sub(self.bytes) else {
                // Fail closed. Resetting to zero here could erase reservations
                // owned by other leases and admit work above the capacity.
                return;
            };
            match quota
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return,
                Err(observed) => used = observed,
            }
        }
    }
}

impl Drop for QuotaLease {
    fn drop(&mut self) {
        self.release();
    }
}

/// Independent process-wide memory and temporary-storage budgets.
#[derive(Clone, Debug)]
pub(crate) struct ResourceGovernor {
    temporary_storage: Arc<ByteQuota>,
    memory: Arc<ByteQuota>,
}

impl ResourceGovernor {
    pub(crate) fn new(memory_capacity: u64, temporary_storage_capacity: u64) -> Self {
        Self {
            temporary_storage: Arc::new(ByteQuota::new(temporary_storage_capacity)),
            memory: Arc::new(ByteQuota::new(memory_capacity)),
        }
    }

    pub(crate) fn reserve_memory(&self, bytes: u64) -> Result<QuotaLease, ServiceRuntimeError> {
        self.memory.try_acquire(bytes)
    }

    pub(crate) fn reserve_temporary_storage(
        &self,
        bytes: u64,
    ) -> Result<QuotaLease, ServiceRuntimeError> {
        self.temporary_storage.try_acquire(bytes)
    }

    pub(crate) fn memory_used(&self) -> u64 {
        self.memory.used()
    }

    pub(crate) fn temporary_storage_used(&self) -> u64 {
        self.temporary_storage.used()
    }

    pub(crate) fn memory_capacity(&self) -> u64 {
        self.memory.capacity()
    }

    pub(crate) fn temporary_storage_capacity(&self) -> u64 {
        self.temporary_storage.capacity()
    }
}

const REQUEST_RUNNING: u8 = 0;
const REQUEST_CANCELLED: u8 = 1;
const REQUEST_DEADLINE_EXCEEDED: u8 = 2;

#[derive(Debug)]
struct RequestControlInner {
    deadline: Instant,
    state: AtomicU8,
}

/// Cloneable cooperative cancellation and one absolute request deadline.
///
/// Dropping a clone never cancels the request. The first terminal transition is
/// permanent so every transport and worker observes the same reason.
#[derive(Clone, Debug)]
pub(crate) struct RequestControl {
    inner: Arc<RequestControlInner>,
}

impl RequestControl {
    pub(crate) fn with_deadline(deadline: Instant) -> Self {
        Self {
            inner: Arc::new(RequestControlInner {
                deadline,
                state: AtomicU8::new(REQUEST_RUNNING),
            }),
        }
    }

    pub(crate) fn from_timeout(timeout: Duration) -> Result<Self, ServiceRuntimeError> {
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            ServiceRuntimeError::new(
                ServiceRuntimeErrorKind::ArithmeticOverflow,
                "request deadline overflows the platform Instant domain",
            )
        })?;
        Ok(Self::with_deadline(deadline))
    }

    pub(crate) fn cancel(&self) -> bool {
        loop {
            let state = self.inner.state.load(Ordering::Acquire);
            if state != REQUEST_RUNNING {
                return false;
            }
            let terminal = if Instant::now() >= self.inner.deadline {
                REQUEST_DEADLINE_EXCEEDED
            } else {
                REQUEST_CANCELLED
            };
            match self.inner.state.compare_exchange_weak(
                REQUEST_RUNNING,
                terminal,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return terminal == REQUEST_CANCELLED,
                Err(_) => continue,
            }
        }
    }

    pub(crate) fn check(&self) -> Result<(), ServiceRuntimeError> {
        loop {
            match self.inner.state.load(Ordering::Acquire) {
                REQUEST_RUNNING => {
                    if Instant::now() < self.inner.deadline {
                        return Ok(());
                    }
                    match self.inner.state.compare_exchange_weak(
                        REQUEST_RUNNING,
                        REQUEST_DEADLINE_EXCEEDED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return Err(Self::deadline_error()),
                        Err(_) => continue,
                    }
                }
                REQUEST_CANCELLED => return Err(Self::cancelled_error()),
                REQUEST_DEADLINE_EXCEEDED => return Err(Self::deadline_error()),
                _ => {
                    return Err(ServiceRuntimeError::new(
                        ServiceRuntimeErrorKind::Cancelled,
                        "request control entered an invalid terminal state",
                    ));
                }
            }
        }
    }

    pub(crate) fn remaining(&self) -> Result<Duration, ServiceRuntimeError> {
        self.check()?;
        Ok(self
            .inner
            .deadline
            .saturating_duration_since(Instant::now()))
    }

    pub(crate) fn same_request(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Mark a timer-driven request as expired without changing an earlier
    /// terminal cancellation reason.
    pub(crate) fn expire(&self) {
        let _ = self.inner.state.compare_exchange(
            REQUEST_RUNNING,
            REQUEST_DEADLINE_EXCEEDED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn cancelled_error() -> ServiceRuntimeError {
        ServiceRuntimeError::new(ServiceRuntimeErrorKind::Cancelled, "request was cancelled")
    }

    fn deadline_error() -> ServiceRuntimeError {
        ServiceRuntimeError::new(
            ServiceRuntimeErrorKind::DeadlineExceeded,
            "request deadline was exceeded",
        )
    }
}

/// Result of a service-only, quota-bound streaming decode and analysis.
///
/// The private input clone retains the upload file and its temporary-storage
/// lease, while the memory lease remains live until report construction and
/// transport serialization have completed.
#[derive(Debug)]
pub(crate) struct ControlledAnalysis {
    pub(crate) analysis: Analysis,
    pub(crate) channel_layout: ChannelLayoutDescriptor,
    pub(crate) decoded_samples: u64,
    _input: StableInput,
    _decoded_memory_lease: QuotaLease,
}

#[derive(Debug)]
pub(crate) enum ControlledAnalysisError {
    Runtime(ServiceRuntimeError),
    Media(String),
}

impl fmt::Display for ControlledAnalysisError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(error) => error.fmt(formatter),
            Self::Media(message) => formatter.write_str(message),
        }
    }
}

impl From<ServiceRuntimeError> for ControlledAnalysisError {
    fn from(error: ServiceRuntimeError) -> Self {
        Self::Runtime(error)
    }
}

pub(crate) const SERVICE_MAX_CHANNELS: u16 = 64;
const SERVICE_PCM_WORKING_BYTES_PER_SAMPLE: u64 = 16;
const SERVICE_DECODER_FIXED_ALLOWANCE_BYTES: u64 = 64 * 1024 * 1024;
const SERVICE_REPORT_ALLOWANCE_BYTES: u64 = 1024 * 1024;
const SERVICE_CHANNEL_STATE_BYTES: u64 = 4 * 1024;
/// Maximum admission charge retained by a serialized gRPC response body.
///
/// The charge includes both the response message strings and the encoded wire
/// buffer while tonic transitions between them. Actual responses are charged
/// by their checked size and rejected if this conservative envelope is
/// exceeded.
pub(crate) const SERVICE_RESPONSE_WIRE_ALLOWANCE_BYTES: u64 = 4 * 1024 * 1024;

/// Admission charge for the major Forge-owned decode/analysis allocations.
///
/// This includes the immutable encoded input as a worst-case demux packet,
/// two simultaneous eight-byte PCM representations, serial decoder scratch,
/// the maximum K-weighting window and loudness-block vectors, per-channel DSP
/// state, and the bounded exact-layout/report response. Controlled service
/// decoding disables parallel native FLAC, whose multi-decoder batches are not
/// covered by this formula. This is a conservative admission charge rather
/// than a hard RSS/allocator cap: third-party decoder implementation overhead,
/// the async transport stack, thread stacks, and allocator fragmentation are
/// outside the governor and are bounded/configured separately where possible.
pub(crate) fn service_analysis_working_set_reservation_bytes(
    max_input_bytes: u64,
    max_decoded_samples: u64,
) -> Result<u64, ServiceRuntimeError> {
    if max_input_bytes == 0 {
        return Err(ServiceRuntimeError::new(
            ServiceRuntimeErrorKind::InvalidLimit,
            "input byte limit must be greater than zero",
        ));
    }
    if max_decoded_samples == 0 {
        return Err(ServiceRuntimeError::new(
            ServiceRuntimeErrorKind::InvalidLimit,
            "decoded sample limit must be greater than zero",
        ));
    }
    let pcm = max_decoded_samples
        .checked_mul(SERVICE_PCM_WORKING_BYTES_PER_SAMPLE)
        .ok_or_else(|| {
            ServiceRuntimeError::new(
                ServiceRuntimeErrorKind::ArithmeticOverflow,
                "decoded PCM working-set reservation exceeds the byte-count domain",
            )
        })?;
    let loudness_blocks = u64::try_from(lufs::MAX_LOUDNESS_BLOCKS)
        .ok()
        .and_then(|blocks| blocks.checked_mul(std::mem::size_of::<f64>() as u64))
        // Vec capacity may temporarily double while each of the two vectors
        // grows, so charge four maximum f64 vectors in total.
        .and_then(|bytes| bytes.checked_mul(4))
        .ok_or_else(|| {
            ServiceRuntimeError::new(
                ServiceRuntimeErrorKind::ArithmeticOverflow,
                "loudness block reservation exceeds the byte-count domain",
            )
        })?;
    let loudness_window = u64::from(decoder::MAX_DECODE_SAMPLE_RATE_HZ)
        .checked_mul(3)
        .and_then(|frames| frames.checked_mul(std::mem::size_of::<f64>() as u64))
        .ok_or_else(|| {
            ServiceRuntimeError::new(
                ServiceRuntimeErrorKind::ArithmeticOverflow,
                "loudness window reservation exceeds the byte-count domain",
            )
        })?;
    let channel_state = u64::from(SERVICE_MAX_CHANNELS)
        .checked_mul(SERVICE_CHANNEL_STATE_BYTES)
        .ok_or_else(|| {
            ServiceRuntimeError::new(
                ServiceRuntimeErrorKind::ArithmeticOverflow,
                "channel-state reservation exceeds the byte-count domain",
            )
        })?;
    let layout =
        u64::try_from(crate::channel_layout::MAX_CHANNEL_LAYOUT_JSON_BYTES).map_err(|_| {
            ServiceRuntimeError::new(
                ServiceRuntimeErrorKind::ArithmeticOverflow,
                "channel-layout response limit exceeds the byte-count domain",
            )
        })?;
    [
        max_input_bytes,
        pcm,
        SERVICE_DECODER_FIXED_ALLOWANCE_BYTES,
        loudness_blocks,
        loudness_window,
        channel_state,
        layout,
        SERVICE_REPORT_ALLOWANCE_BYTES,
    ]
    .into_iter()
    .try_fold(0_u64, |total, bytes| total.checked_add(bytes))
    .ok_or_else(|| {
        ServiceRuntimeError::new(
            ServiceRuntimeErrorKind::ArithmeticOverflow,
            "service analysis working-set reservation exceeds the byte-count domain",
        )
    })
}

fn validate_service_channel_count(channels: u16) -> Result<(), ServiceRuntimeError> {
    if channels == 0 || channels > SERVICE_MAX_CHANNELS {
        return Err(ServiceRuntimeError::new(
            ServiceRuntimeErrorKind::LimitExceeded,
            format!(
                "decoded channel count {channels} is outside the service range 1..={SERVICE_MAX_CHANNELS}"
            ),
        ));
    }
    Ok(())
}

/// Decode and analyze one immutable upload with cooperative checkpoints.
///
/// Existing library decode and analysis entry points are untouched. The
/// service path uses the descriptor streaming decoder so cancellation and the
/// absolute deadline are observed between bounded codec packets/chunks and
/// before and after each DSP chunk.
pub(crate) fn analyze_stable_input(
    input: StableInput,
    requested_layout: Option<ChannelLayoutDescriptor>,
    max_decoded_samples: u64,
    governor: &ResourceGovernor,
    control: &RequestControl,
) -> Result<ControlledAnalysis, ControlledAnalysisError> {
    analyze_stable_input_impl(
        input,
        requested_layout,
        max_decoded_samples,
        governor,
        control,
        |_| {},
        |_, _| {},
    )
}

#[cfg(test)]
fn analyze_stable_input_with_checkpoint<F>(
    input: StableInput,
    requested_layout: Option<ChannelLayoutDescriptor>,
    max_decoded_samples: u64,
    governor: &ResourceGovernor,
    control: &RequestControl,
    checkpoint: F,
) -> Result<ControlledAnalysis, ControlledAnalysisError>
where
    F: FnMut(&RequestControl, u64),
{
    analyze_stable_input_impl(
        input,
        requested_layout,
        max_decoded_samples,
        governor,
        control,
        |_| {},
        checkpoint,
    )
}

#[cfg(test)]
fn analyze_stable_input_with_probe_checkpoint<P>(
    input: StableInput,
    requested_layout: Option<ChannelLayoutDescriptor>,
    max_decoded_samples: u64,
    governor: &ResourceGovernor,
    control: &RequestControl,
    probe_checkpoint: P,
) -> Result<ControlledAnalysis, ControlledAnalysisError>
where
    P: FnMut(&RequestControl) + Send,
{
    analyze_stable_input_impl(
        input,
        requested_layout,
        max_decoded_samples,
        governor,
        control,
        probe_checkpoint,
        |_, _| {},
    )
}

fn analyze_stable_input_impl<P, F>(
    input: StableInput,
    requested_layout: Option<ChannelLayoutDescriptor>,
    max_decoded_samples: u64,
    governor: &ResourceGovernor,
    control: &RequestControl,
    mut probe_checkpoint: P,
    mut checkpoint: F,
) -> Result<ControlledAnalysis, ControlledAnalysisError>
where
    P: FnMut(&RequestControl) + Send,
    F: FnMut(&RequestControl, u64),
{
    control.check()?;
    let reservation =
        service_analysis_working_set_reservation_bytes(input.byte_len(), max_decoded_samples)?;
    let decoded_memory_lease = governor.reserve_memory(reservation)?;

    let retained_input = input.clone();
    let descriptor_options = requested_layout
        .map_or_else(InputDescriptorOptions::default, |layout| {
            InputDescriptorOptions::default().with_channel_layout(layout)
        });
    const CONTROLLED_STOP: &str = "__forge_service_controlled_analysis_stop__";
    let descriptor_result =
        InputDescriptor::probe_with_control(input, descriptor_options, max_decoded_samples, || {
            probe_checkpoint(control);
            match control.check() {
                Ok(()) => Ok(()),
                Err(_) => Err(CONTROLLED_STOP.into()),
            }
        });
    // A terminal request reason takes precedence over a decoder error that
    // happened concurrently with cancellation/deadline expiry.
    control.check()?;
    let descriptor = match descriptor_result {
        Ok(descriptor) => descriptor,
        Err(error) if error == decoder::SERVICE_PACKET_SAMPLE_LIMIT_EXCEEDED => {
            return Err(ControlledAnalysisError::Runtime(ServiceRuntimeError::new(
                ServiceRuntimeErrorKind::LimitExceeded,
                format!("decoded audio contains more than {max_decoded_samples} samples"),
            )));
        }
        Err(error) if error == decoder::SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED => {
            return Err(ControlledAnalysisError::Runtime(ServiceRuntimeError::new(
                ServiceRuntimeErrorKind::LimitExceeded,
                "encoded container packet or metadata exceeds the service safety limit",
            )));
        }
        Err(error) => return Err(ControlledAnalysisError::Media(error)),
    };
    let info = descriptor.stream_info().clone();
    validate_service_channel_count(info.channels)?;
    let channel_layout = descriptor.channel_layout().clone();
    let override_roles = descriptor
        .uses_explicit_channel_layout()
        .then(|| channel_layout.channel_roles());
    let channel_roles = crate::normalize::resolve_decoded_channel_roles(
        descriptor.stable_input().stable_path(),
        info.channels,
        &info.channel_roles,
        channel_layout.provenance(),
        override_roles.as_deref(),
    )
    .map_err(ControlledAnalysisError::Media)?;
    let mut analyzer = lufs::StreamingAnalyzer::new(info.sample_rate, channel_roles.clone());
    let mut decoded_frames = 0_u64;
    let mut runtime_failure = None;
    let decode_result = decoder::decode_descriptor_analysis_stream_with_control(
        &descriptor,
        max_decoded_samples,
        || match control.check() {
            Ok(()) => Ok(()),
            Err(_) => Err(CONTROLLED_STOP.into()),
        },
        |_, _, chunk| {
            if let Err(error) = control.check() {
                runtime_failure = Some(error);
                return Err(CONTROLLED_STOP.into());
            }
            let chunk_frames = chunk.frames();
            let chunk_frames = match u64::try_from(chunk_frames) {
                Ok(frames) => frames,
                Err(_) => {
                    runtime_failure = Some(ServiceRuntimeError::new(
                        ServiceRuntimeErrorKind::ArithmeticOverflow,
                        "decoded chunk frame count exceeds the service byte-count domain",
                    ));
                    return Err(CONTROLLED_STOP.into());
                }
            };
            let next_frames = match decoded_frames.checked_add(chunk_frames) {
                Some(frames) => frames,
                None => {
                    runtime_failure = Some(ServiceRuntimeError::new(
                        ServiceRuntimeErrorKind::ArithmeticOverflow,
                        "decoded frame count overflow",
                    ));
                    return Err(CONTROLLED_STOP.into());
                }
            };
            let next_samples = match next_frames.checked_mul(u64::from(info.channels)) {
                Some(samples) => samples,
                None => {
                    runtime_failure = Some(ServiceRuntimeError::new(
                        ServiceRuntimeErrorKind::ArithmeticOverflow,
                        "decoded sample count overflow",
                    ));
                    return Err(CONTROLLED_STOP.into());
                }
            };
            if next_samples > max_decoded_samples {
                runtime_failure = Some(ServiceRuntimeError::new(
                    ServiceRuntimeErrorKind::LimitExceeded,
                    format!("decoded audio contains more than {max_decoded_samples} samples"),
                ));
                return Err(CONTROLLED_STOP.into());
            }
            let dsp_checkpoint = || match control.check() {
                Ok(()) => Ok(()),
                Err(_) => Err(CONTROLLED_STOP.into()),
            };
            let process_result = match chunk {
                AnalysisPcmChunk::F32(planar) => {
                    analyzer.process_with_control(planar, dsp_checkpoint)
                }
                AnalysisPcmChunk::S32(planar) => {
                    analyzer.process_i32_with_control(planar, dsp_checkpoint)
                }
                AnalysisPcmChunk::F64(planar) => {
                    analyzer.process_f64_with_control(planar, dsp_checkpoint)
                }
            };
            process_result?;
            decoded_frames = next_frames;
            checkpoint(control, decoded_frames);
            if let Err(error) = control.check() {
                runtime_failure = Some(error);
                return Err(CONTROLLED_STOP.into());
            }
            Ok(())
        },
    );
    if let Some(error) = runtime_failure.take() {
        return Err(ControlledAnalysisError::Runtime(error));
    }
    if let Err(error) = decode_result {
        if let Err(terminal) = control.check() {
            return Err(ControlledAnalysisError::Runtime(terminal));
        }
        if error == decoder::SERVICE_PACKET_SAMPLE_LIMIT_EXCEEDED {
            return Err(ControlledAnalysisError::Runtime(ServiceRuntimeError::new(
                ServiceRuntimeErrorKind::LimitExceeded,
                format!(
                    "one decoded packet exceeds the {max_decoded_samples}-sample service limit"
                ),
            )));
        }
        if error == decoder::SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED {
            return Err(ControlledAnalysisError::Runtime(ServiceRuntimeError::new(
                ServiceRuntimeErrorKind::LimitExceeded,
                "encoded container packet or metadata exceeds the service safety limit",
            )));
        }
        return Err(ControlledAnalysisError::Media(error));
    }
    control.check()?;
    let finish_result = analyzer.finish_with_control(|| match control.check() {
        Ok(()) => Ok(()),
        Err(error) => {
            runtime_failure = Some(error);
            Err(CONTROLLED_STOP.into())
        }
    });
    if let Some(error) = runtime_failure {
        return Err(ControlledAnalysisError::Runtime(error));
    }
    let measured = finish_result.map_err(ControlledAnalysisError::Media)?;
    control.check()?;
    let decoded_samples = decoded_frames
        .checked_mul(u64::from(info.channels))
        .ok_or_else(|| {
            ControlledAnalysisError::Runtime(ServiceRuntimeError::new(
                ServiceRuntimeErrorKind::ArithmeticOverflow,
                "decoded sample count overflow",
            ))
        })?;
    Ok(ControlledAnalysis {
        analysis: Analysis {
            sample_rate: info.sample_rate,
            channels: info.channels,
            channel_roles,
            frames: measured.frames,
            kind: info.source_kind,
            lufs: measured.ebu.integrated_lufs,
            max_momentary_lufs: measured.ebu.max_momentary_lufs,
            max_short_term_lufs: measured.ebu.max_short_term_lufs,
            loudness_range_lu: measured.ebu.loudness_range_lu,
            rms_db: measured.rms_db,
            sample_peak: measured.sample_peak,
            true_peak: measured.true_peak,
            loudness_blocks: measured.ebu.gating_blocks,
        },
        channel_layout,
        decoded_samples,
        _input: retained_input,
        _decoded_memory_lease: decoded_memory_lease,
    })
}

/// Bounded, incrementally hashed upload retained in a private temporary file.
///
/// The declared length is reserved from the global temporary quota before the
/// file is created. This is deliberately conservative: a partial-write failure
/// keeps the full reservation until the poisoned spool is dropped. The
/// snapshot field stays first and the lease last so close/unlink precedes quota
/// release on Windows as well as Unix.
///
/// This quota accounts for active in-process reservations. Drop ordering makes
/// normal close-and-unlink happen before a reservation is returned, but it is
/// not durable storage accounting: an OS unlink failure or abrupt process
/// termination can leave physical temporary files outside the governor's
/// knowledge. Startup scavenging and filesystem quotas remain deployment
/// responsibilities.
pub(crate) struct UploadSpool {
    snapshot: NamedTempFile,
    options: StableInputOptions,
    declared_bytes: u64,
    written_bytes: u64,
    hasher: Sha256,
    control: RequestControl,
    failed: Option<ServiceRuntimeError>,
    temporary_storage_lease: Option<QuotaLease>,
}

impl fmt::Debug for UploadSpool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UploadSpool")
            .field("declared_bytes", &self.declared_bytes)
            .field("written_bytes", &self.written_bytes)
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

impl UploadSpool {
    pub(crate) fn create(
        governor: &ResourceGovernor,
        control: RequestControl,
        declared_bytes: u64,
        options: StableInputOptions,
    ) -> Result<Self, ServiceRuntimeError> {
        if declared_bytes == 0 {
            return Err(ServiceRuntimeError::new(
                ServiceRuntimeErrorKind::InvalidLimit,
                "declared upload length must be greater than zero",
            ));
        }
        if declared_bytes > options.max_input_bytes().get() {
            return Err(ServiceRuntimeError::new(
                ServiceRuntimeErrorKind::LimitExceeded,
                format!(
                    "declared upload length {declared_bytes} exceeds the per-request limit {}",
                    options.max_input_bytes()
                ),
            ));
        }
        control.check()?;
        let temporary_storage_lease = governor.reserve_temporary_storage(declared_bytes)?;
        let snapshot =
            create_snapshot(options.source_name_hint()).map_err(ServiceRuntimeError::snapshot)?;

        Ok(Self {
            snapshot,
            options,
            declared_bytes,
            written_bytes: 0,
            hasher: Sha256::new(),
            control,
            failed: None,
            temporary_storage_lease: Some(temporary_storage_lease),
        })
    }

    pub(crate) const fn declared_bytes(&self) -> u64 {
        self.declared_bytes
    }

    pub(crate) const fn written_bytes(&self) -> u64 {
        self.written_bytes
    }

    pub(crate) fn write_chunk(&mut self, bytes: &[u8]) -> Result<(), ServiceRuntimeError> {
        self.write_chunk_using(bytes, |destination, remaining| destination.write(remaining))
    }

    fn write_chunk_using(
        &mut self,
        bytes: &[u8],
        write: impl FnMut(&mut File, &[u8]) -> io::Result<usize>,
    ) -> Result<(), ServiceRuntimeError> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        match self.write_chunk_attempt(bytes, write) {
            Ok(()) => Ok(()),
            Err(error) => Err(self.poison(error)),
        }
    }

    fn write_chunk_attempt(
        &mut self,
        bytes: &[u8],
        mut write: impl FnMut(&mut File, &[u8]) -> io::Result<usize>,
    ) -> Result<(), ServiceRuntimeError> {
        self.control.check()?;

        let chunk_bytes = u64::try_from(bytes.len()).map_err(|_| {
            ServiceRuntimeError::new(
                ServiceRuntimeErrorKind::ArithmeticOverflow,
                "upload chunk length does not fit the byte-count domain",
            )
        })?;
        let next_written = self.written_bytes.checked_add(chunk_bytes).ok_or_else(|| {
            ServiceRuntimeError::new(
                ServiceRuntimeErrorKind::ArithmeticOverflow,
                "upload byte count overflow",
            )
        })?;
        if next_written > self.declared_bytes {
            return Err(ServiceRuntimeError::new(
                ServiceRuntimeErrorKind::LimitExceeded,
                format!(
                    "upload would contain {next_written} bytes, above its declared length {}",
                    self.declared_bytes
                ),
            ));
        }

        let mut offset = 0_usize;
        while offset < bytes.len() {
            self.control.check()?;
            let remaining = &bytes[offset..];
            let written = match write(self.snapshot.as_file_mut(), remaining) {
                Ok(0) => {
                    let error = ServiceRuntimeError::io(
                        "write upload spool",
                        io::Error::new(
                            io::ErrorKind::WriteZero,
                            "writer accepted no bytes from a non-empty upload chunk",
                        ),
                    );
                    return Err(error);
                }
                Ok(written) if written <= remaining.len() => written,
                Ok(_) => {
                    let error = ServiceRuntimeError::io(
                        "write upload spool",
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "writer reported more bytes than it was given",
                        ),
                    );
                    return Err(error);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let error = ServiceRuntimeError::io("write upload spool", error);
                    return Err(error);
                }
            };
            let written_u64 = u64::try_from(written).map_err(|_| {
                ServiceRuntimeError::new(
                    ServiceRuntimeErrorKind::ArithmeticOverflow,
                    "written upload prefix does not fit the byte-count domain",
                )
            })?;
            let committed_bytes = self.written_bytes.checked_add(written_u64).ok_or_else(|| {
                ServiceRuntimeError::new(
                    ServiceRuntimeErrorKind::ArithmeticOverflow,
                    "upload byte count overflow after a partial write",
                )
            })?;
            self.hasher.update(&remaining[..written]);
            self.written_bytes = committed_bytes;
            offset += written;
        }
        debug_assert_eq!(self.written_bytes, next_written);
        Ok(())
    }

    pub(crate) fn finish_into_stable_input(mut self) -> Result<StableInput, ServiceRuntimeError> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        self.control.check()?;
        if self.written_bytes != self.declared_bytes {
            return Err(ServiceRuntimeError::new(
                ServiceRuntimeErrorKind::IncompleteUpload,
                format!(
                    "upload ended at {} bytes, expected {}",
                    self.written_bytes, self.declared_bytes
                ),
            ));
        }
        self.snapshot
            .as_file_mut()
            .flush()
            .map_err(|error| ServiceRuntimeError::io("flush upload spool", error))?;
        let actual_len = self
            .snapshot
            .as_file()
            .metadata()
            .map_err(|error| ServiceRuntimeError::io("inspect upload spool", error))?
            .len();
        if actual_len != self.written_bytes {
            return Err(ServiceRuntimeError::new(
                ServiceRuntimeErrorKind::InvalidSnapshot,
                format!(
                    "upload spool contains {actual_len} bytes, expected {}",
                    self.written_bytes
                ),
            ));
        }
        self.snapshot
            .as_file_mut()
            .seek(SeekFrom::Start(0))
            .map_err(|error| ServiceRuntimeError::io("rewind upload spool", error))?;

        let sha256 = self.hasher.finalize().into();
        let Some(temporary_storage_lease) = self.temporary_storage_lease.take() else {
            return Err(ServiceRuntimeError::new(
                ServiceRuntimeErrorKind::InvalidSnapshot,
                "upload spool lost its temporary-storage lease",
            ));
        };
        let transfer = StableInputTransfer::new(
            self.snapshot,
            self.written_bytes,
            sha256,
            self.options,
            temporary_storage_lease,
        );
        StableInput::from_owned_snapshot(transfer).map_err(ServiceRuntimeError::snapshot)
    }

    fn poison(&mut self, error: ServiceRuntimeError) -> ServiceRuntimeError {
        self.failed.get_or_insert(error).clone()
    }

    #[cfg(test)]
    fn snapshot_path(&self) -> &std::path::Path {
        self.snapshot.path()
    }

    #[cfg(test)]
    fn snapshot_len(&self) -> u64 {
        self.snapshot.as_file().metadata().unwrap().len()
    }

    #[cfg(test)]
    fn incremental_sha256(&self) -> [u8; 32] {
        self.hasher.clone().finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe, RefUnwindSafe, UnwindSafe};
    use std::sync::{mpsc, Barrier};
    use std::thread;

    fn options(limit: u64, hint: &str) -> StableInputOptions {
        StableInputOptions::new(limit)
            .unwrap()
            .with_source_name_hint(hint)
    }

    fn control() -> RequestControl {
        RequestControl::from_timeout(Duration::from_secs(60)).unwrap()
    }

    fn digest(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }

    fn mono_s16_wave(frames: usize) -> Vec<u8> {
        let sample_rate = 48_000_u32;
        let data_bytes = u32::try_from(frames.checked_mul(2).unwrap()).unwrap();
        let mut audio = Vec::with_capacity(44 + data_bytes as usize);
        audio.extend_from_slice(b"RIFF");
        audio.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        audio.extend_from_slice(b"WAVEfmt ");
        audio.extend_from_slice(&16_u32.to_le_bytes());
        audio.extend_from_slice(&1_u16.to_le_bytes());
        audio.extend_from_slice(&1_u16.to_le_bytes());
        audio.extend_from_slice(&sample_rate.to_le_bytes());
        audio.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        audio.extend_from_slice(&2_u16.to_le_bytes());
        audio.extend_from_slice(&16_u16.to_le_bytes());
        audio.extend_from_slice(b"data");
        audio.extend_from_slice(&data_bytes.to_le_bytes());
        for frame in 0..frames {
            let sample = ((frame % 97) as i16).saturating_mul(100);
            audio.extend_from_slice(&sample.to_le_bytes());
        }
        audio
    }

    fn mono_s16_wave_with_junk_chunks(junk_chunks: usize) -> Vec<u8> {
        let sample_rate = 48_000_u32;
        let mut audio = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        audio.extend_from_slice(&16_u32.to_le_bytes());
        audio.extend_from_slice(&1_u16.to_le_bytes());
        audio.extend_from_slice(&1_u16.to_le_bytes());
        audio.extend_from_slice(&sample_rate.to_le_bytes());
        audio.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        audio.extend_from_slice(&2_u16.to_le_bytes());
        audio.extend_from_slice(&16_u16.to_le_bytes());
        for _ in 0..junk_chunks {
            audio.extend_from_slice(b"JUNK");
            audio.extend_from_slice(&0_u32.to_le_bytes());
        }
        audio.extend_from_slice(b"data");
        audio.extend_from_slice(&2_u32.to_le_bytes());
        audio.extend_from_slice(&1_i16.to_le_bytes());
        let riff_size = u32::try_from(audio.len() - 8).unwrap();
        audio[4..8].copy_from_slice(&riff_size.to_le_bytes());
        audio
    }

    fn analysis_reservation(input_bytes: usize, max_samples: u64) -> u64 {
        service_analysis_working_set_reservation_bytes(input_bytes as u64, max_samples).unwrap()
    }

    fn spool_bytes(
        governor: &ResourceGovernor,
        request_control: &RequestControl,
        bytes: &[u8],
    ) -> StableInput {
        let length = bytes.len() as u64;
        let mut spool = UploadSpool::create(
            governor,
            request_control.clone(),
            length,
            options(length, "analysis.wav"),
        )
        .unwrap();
        for chunk in bytes.chunks(997) {
            spool.write_chunk(chunk).unwrap();
        }
        spool.finish_into_stable_input().unwrap()
    }

    #[test]
    fn byte_quota_is_checked_and_zero_capacity_is_well_defined() {
        let quota = Arc::new(ByteQuota::new(4));
        let zero = quota.try_acquire(0).unwrap();
        assert_eq!(zero.amount(), 0);
        assert_eq!(quota.used(), 0);
        assert_eq!(quota.available(), 4);

        let exact = quota.try_acquire(4).unwrap();
        assert_eq!(quota.used(), 4);
        assert_eq!(quota.available(), 0);
        assert_eq!(
            quota.try_acquire(1).unwrap_err().kind(),
            ServiceRuntimeErrorKind::QuotaExceeded
        );
        assert_eq!(quota.used(), 4);
        drop(exact);
        drop(zero);
        assert_eq!(quota.used(), 0);

        let disabled = Arc::new(ByteQuota::new(0));
        disabled.try_acquire(0).unwrap();
        assert_eq!(
            disabled.try_acquire(1).unwrap_err().kind(),
            ServiceRuntimeErrorKind::QuotaExceeded
        );

        let maximum = Arc::new(ByteQuota::new(u64::MAX));
        let maximum_lease = maximum.try_acquire(u64::MAX).unwrap();
        assert_eq!(
            maximum.try_acquire(1).unwrap_err().kind(),
            ServiceRuntimeErrorKind::ArithmeticOverflow
        );
        assert_eq!(maximum.used(), u64::MAX);
        drop(maximum_lease);
        assert_eq!(maximum.used(), 0);
    }

    #[test]
    fn concurrent_quota_acquisition_never_exceeds_capacity() {
        const THREADS: usize = 24;
        const CAPACITY: usize = 7;
        let quota = Arc::new(ByteQuota::new(CAPACITY as u64));
        let start = Arc::new(Barrier::new(THREADS + 1));
        let release = Arc::new(Barrier::new(THREADS + 1));
        let (send, receive) = mpsc::channel();
        let mut workers = Vec::new();
        for _ in 0..THREADS {
            let quota = Arc::clone(&quota);
            let start = Arc::clone(&start);
            let release = Arc::clone(&release);
            let send = send.clone();
            workers.push(thread::spawn(move || {
                start.wait();
                let lease = quota.try_acquire(1);
                send.send(lease.is_ok()).unwrap();
                release.wait();
                drop(lease);
            }));
        }
        drop(send);
        start.wait();
        let acquired = (0..THREADS)
            .map(|_| receive.recv().unwrap())
            .filter(|acquired| *acquired)
            .count();
        assert_eq!(acquired, CAPACITY);
        assert_eq!(quota.used(), CAPACITY as u64);
        release.wait();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(quota.used(), 0);
    }

    #[test]
    fn quota_leases_release_once_during_explicit_release_and_unwind() {
        let quota = Arc::new(ByteQuota::new(16));
        let mut lease = quota.try_acquire(7).unwrap();
        lease.release();
        assert_eq!(quota.used(), 0);
        drop(lease);
        assert_eq!(quota.used(), 0);

        let unwind = catch_unwind(AssertUnwindSafe({
            let quota = Arc::clone(&quota);
            move || {
                let _lease = quota.try_acquire(11).unwrap();
                panic!("exercise lease unwind");
            }
        }));
        assert!(unwind.is_err());
        assert_eq!(quota.used(), 0);

        let inconsistent = Arc::new(ByteQuota::new(10));
        let other_lease = inconsistent.try_acquire(3).unwrap();
        let damaged_lease = inconsistent.try_acquire(7).unwrap();
        inconsistent.used.store(3, Ordering::Release);
        let damaged_drop = catch_unwind(AssertUnwindSafe(|| drop(damaged_lease)));
        assert!(damaged_drop.is_ok());
        assert_eq!(
            inconsistent.used(),
            3,
            "an underflowing release must not erase another lease"
        );
        drop(other_lease);
        assert_eq!(inconsistent.used(), 0);
    }

    #[test]
    fn resource_governor_clones_share_independent_memory_and_temp_budgets() {
        let governor = ResourceGovernor::new(8, 16);
        let clone = governor.clone();
        let memory = clone.reserve_memory(8).unwrap();
        let temporary = governor.reserve_temporary_storage(16).unwrap();
        assert_eq!(governor.memory_used(), 8);
        assert_eq!(clone.temporary_storage_used(), 16);
        assert_eq!(
            governor.reserve_memory(1).unwrap_err().kind(),
            ServiceRuntimeErrorKind::QuotaExceeded
        );
        assert_eq!(
            clone.reserve_temporary_storage(1).unwrap_err().kind(),
            ServiceRuntimeErrorKind::QuotaExceeded
        );
        drop(memory);
        assert_eq!(governor.memory_used(), 0);
        assert_eq!(governor.temporary_storage_used(), 16);
        drop(temporary);
        assert_eq!(governor.temporary_storage_used(), 0);
    }

    #[test]
    fn request_control_clones_keep_one_terminal_reason() {
        let running = control();
        let clone = running.clone();
        drop(clone);
        running.check().unwrap();
        assert!(running.remaining().unwrap() <= Duration::from_secs(60));

        let cancelled = running.clone();
        assert!(cancelled.cancel());
        assert!(!running.cancel());
        assert_eq!(
            running.check().unwrap_err().kind(),
            ServiceRuntimeErrorKind::Cancelled
        );
        running.expire();
        assert_eq!(
            running.check().unwrap_err().kind(),
            ServiceRuntimeErrorKind::Cancelled
        );

        let expired = RequestControl::with_deadline(Instant::now());
        assert_eq!(
            expired.check().unwrap_err().kind(),
            ServiceRuntimeErrorKind::DeadlineExceeded
        );
        assert!(!expired.cancel());
        assert_eq!(
            expired.check().unwrap_err().kind(),
            ServiceRuntimeErrorKind::DeadlineExceeded
        );
        assert_eq!(
            RequestControl::from_timeout(Duration::MAX)
                .unwrap_err()
                .kind(),
            ServiceRuntimeErrorKind::ArithmeticOverflow
        );
    }

    #[test]
    fn completed_spool_is_adopted_without_copy_and_retains_quota_for_all_clones() {
        let governor = ResourceGovernor::new(0, 64);
        let mut spool =
            UploadSpool::create(&governor, control(), 6, options(64, "incoming.WAV")).unwrap();
        let snapshot_path = spool.snapshot_path().to_owned();
        assert_eq!(snapshot_path.extension().unwrap(), "WAV");
        assert_eq!(governor.temporary_storage_used(), 6);
        spool.write_chunk(b"abc").unwrap();
        spool.write_chunk(b"def").unwrap();
        assert_eq!(spool.incremental_sha256(), digest(b"abcdef"));

        let input = spool.finish_into_stable_input().unwrap();
        assert_eq!(input.stable_path(), snapshot_path);
        assert_eq!(input.byte_len(), 6);
        assert_eq!(input.sha256(), &digest(b"abcdef"));
        assert_eq!(
            input.source_name_hint().unwrap(),
            std::path::Path::new("incoming.WAV")
        );
        assert!(input.source_path().is_none());
        assert_eq!(governor.temporary_storage_used(), 6);

        let clone = input.clone();
        drop(input);
        assert!(snapshot_path.exists());
        assert_eq!(governor.temporary_storage_used(), 6);
        drop(clone);
        assert!(!snapshot_path.exists());
        assert_eq!(governor.temporary_storage_used(), 0);
    }

    #[test]
    fn upload_limits_fail_before_writing_and_incomplete_finish_cleans_up() {
        let governor = ResourceGovernor::new(0, 4);
        assert_eq!(
            UploadSpool::create(&governor, control(), 0, options(4, "zero.bin"))
                .unwrap_err()
                .kind(),
            ServiceRuntimeErrorKind::InvalidLimit
        );
        assert_eq!(
            UploadSpool::create(&governor, control(), 5, options(4, "large.bin"))
                .unwrap_err()
                .kind(),
            ServiceRuntimeErrorKind::LimitExceeded
        );
        assert_eq!(
            UploadSpool::create(&governor, control(), 5, options(8, "quota.bin"))
                .unwrap_err()
                .kind(),
            ServiceRuntimeErrorKind::QuotaExceeded
        );
        assert_eq!(governor.temporary_storage_used(), 0);

        let mut spool =
            UploadSpool::create(&governor, control(), 4, options(4, "excess.bin")).unwrap();
        let path = spool.snapshot_path().to_owned();
        spool.write_chunk(b"abc").unwrap();
        let excess = spool.write_chunk(b"de").unwrap_err();
        assert_eq!(excess.kind(), ServiceRuntimeErrorKind::LimitExceeded);
        assert_eq!(spool.written_bytes(), 3);
        assert_eq!(spool.snapshot_len(), 3);
        assert_eq!(spool.write_chunk(b"d").unwrap_err(), excess);
        assert_eq!(spool.finish_into_stable_input().unwrap_err(), excess);
        assert!(!path.exists());
        assert_eq!(governor.temporary_storage_used(), 0);

        let mut exact =
            UploadSpool::create(&governor, control(), 4, options(4, "exact.bin")).unwrap();
        exact.write_chunk(b"abcd").unwrap();
        drop(exact.finish_into_stable_input().unwrap());
        assert_eq!(governor.temporary_storage_used(), 0);

        let mut incomplete =
            UploadSpool::create(&governor, control(), 4, options(4, "short.bin")).unwrap();
        let path = incomplete.snapshot_path().to_owned();
        incomplete.write_chunk(b"abc").unwrap();
        assert_eq!(
            incomplete.finish_into_stable_input().unwrap_err().kind(),
            ServiceRuntimeErrorKind::IncompleteUpload
        );
        assert!(!path.exists());
        assert_eq!(governor.temporary_storage_used(), 0);
    }

    #[test]
    fn partial_io_and_write_zero_poison_the_spool_with_the_original_failure() {
        let governor = ResourceGovernor::new(0, 32);
        let mut partial =
            UploadSpool::create(&governor, control(), 6, options(6, "partial.bin")).unwrap();
        let partial_path = partial.snapshot_path().to_owned();
        let mut calls = 0;
        let first_error = partial
            .write_chunk_using(b"abcdef", |destination, remaining| {
                calls += 1;
                if calls == 1 {
                    destination.write(&remaining[..2])
                } else {
                    Err(io::Error::other("injected failure"))
                }
            })
            .unwrap_err();
        assert_eq!(first_error.kind(), ServiceRuntimeErrorKind::Io);
        assert_eq!(partial.written_bytes(), 2);
        assert_eq!(partial.snapshot_len(), 2);
        assert_eq!(partial.incremental_sha256(), digest(b"ab"));
        assert_eq!(partial.write_chunk(b"x").unwrap_err(), first_error);
        assert_eq!(partial.finish_into_stable_input().unwrap_err(), first_error);
        assert!(!partial_path.exists());
        assert_eq!(governor.temporary_storage_used(), 0);

        let mut zero =
            UploadSpool::create(&governor, control(), 1, options(1, "zero-write.bin")).unwrap();
        let zero_path = zero.snapshot_path().to_owned();
        let zero_error = zero.write_chunk_using(b"x", |_, _| Ok(0)).unwrap_err();
        assert_eq!(zero_error.kind(), ServiceRuntimeErrorKind::Io);
        assert_eq!(zero.write_chunk(b"x").unwrap_err(), zero_error);
        drop(zero);
        assert!(!zero_path.exists());
        assert_eq!(governor.temporary_storage_used(), 0);
    }

    #[test]
    fn interrupted_and_short_writes_retry_and_commit_each_successful_prefix() {
        let governor = ResourceGovernor::new(0, 16);
        let mut spool =
            UploadSpool::create(&governor, control(), 6, options(6, "retry.bin")).unwrap();
        let mut calls = 0;
        spool
            .write_chunk_using(b"abcdef", |destination, remaining| {
                calls += 1;
                match calls {
                    1 => Err(io::Error::from(io::ErrorKind::Interrupted)),
                    2 => destination.write(&remaining[..2]),
                    _ => destination.write(remaining),
                }
            })
            .unwrap();
        assert_eq!(calls, 3);
        assert_eq!(spool.written_bytes(), 6);
        assert_eq!(spool.snapshot_len(), 6);
        assert_eq!(spool.incremental_sha256(), digest(b"abcdef"));
        let input = spool.finish_into_stable_input().unwrap();
        assert_eq!(input.sha256(), &digest(b"abcdef"));
        drop(input);
        assert_eq!(governor.temporary_storage_used(), 0);
    }

    #[test]
    fn cancellation_and_deadline_after_create_fail_and_release_the_spool() {
        let governor = ResourceGovernor::new(0, 16);
        let cancel_control = control();
        let mut cancelled = UploadSpool::create(
            &governor,
            cancel_control.clone(),
            4,
            options(4, "cancel-after-create.bin"),
        )
        .unwrap();
        let cancelled_path = cancelled.snapshot_path().to_owned();
        cancel_control.cancel();
        let cancelled_error = cancelled.write_chunk(b"data").unwrap_err();
        assert_eq!(cancelled_error.kind(), ServiceRuntimeErrorKind::Cancelled);
        assert_eq!(
            cancelled.finish_into_stable_input().unwrap_err(),
            cancelled_error
        );
        assert!(!cancelled_path.exists());
        assert_eq!(governor.temporary_storage_used(), 0);

        let deadline_control = control();
        let deadline = UploadSpool::create(
            &governor,
            deadline_control.clone(),
            4,
            options(4, "deadline-after-create.bin"),
        )
        .unwrap();
        let deadline_path = deadline.snapshot_path().to_owned();
        deadline_control
            .inner
            .state
            .store(REQUEST_DEADLINE_EXCEEDED, Ordering::Release);
        assert_eq!(
            deadline.finish_into_stable_input().unwrap_err().kind(),
            ServiceRuntimeErrorKind::DeadlineExceeded
        );
        assert!(!deadline_path.exists());
        assert_eq!(governor.temporary_storage_used(), 0);
    }

    #[test]
    fn cancellation_overflow_and_unwind_leave_no_spool_or_quota() {
        let governor = ResourceGovernor::new(8, u64::MAX);
        let cancelled = control();
        cancelled.cancel();
        assert_eq!(
            UploadSpool::create(&governor, cancelled, 4, options(4, "cancelled.bin"))
                .unwrap_err()
                .kind(),
            ServiceRuntimeErrorKind::Cancelled
        );
        assert_eq!(governor.temporary_storage_used(), 0);

        let mut overflow = UploadSpool::create(
            &governor,
            control(),
            u64::MAX,
            options(u64::MAX, "overflow.bin"),
        )
        .unwrap();
        overflow.written_bytes = u64::MAX;
        assert_eq!(
            overflow.write_chunk(b"x").unwrap_err().kind(),
            ServiceRuntimeErrorKind::ArithmeticOverflow
        );
        assert_eq!(
            overflow.write_chunk(&[]).unwrap_err().kind(),
            ServiceRuntimeErrorKind::ArithmeticOverflow
        );
        drop(overflow);
        assert_eq!(governor.temporary_storage_used(), 0);

        let observed_path = Arc::new(std::sync::Mutex::new(None));
        let unwind = catch_unwind(AssertUnwindSafe({
            let governor = governor.clone();
            let observed_path = Arc::clone(&observed_path);
            move || {
                let _memory = governor.reserve_memory(8).unwrap();
                let spool =
                    UploadSpool::create(&governor, control(), 8, options(8, "unwind.bin")).unwrap();
                *observed_path.lock().unwrap() = Some(spool.snapshot_path().to_owned());
                panic!("exercise spool unwind");
            }
        }));
        assert!(unwind.is_err());
        let path = observed_path.lock().unwrap().clone().unwrap();
        assert!(!path.exists());
        assert_eq!(governor.memory_used(), 0);
        assert_eq!(governor.temporary_storage_used(), 0);
    }

    #[test]
    fn controlled_streaming_analysis_holds_both_quotas_through_result_lifetime() {
        let audio = mono_s16_wave(24_000);
        let decoded_limit = 24_000_u64;
        let reservation = analysis_reservation(audio.len(), decoded_limit);
        let governor = ResourceGovernor::new(reservation, audio.len() as u64);
        let request_control = control();
        let input = spool_bytes(&governor, &request_control, &audio);
        assert_eq!(governor.temporary_storage_used(), audio.len() as u64);

        let result =
            analyze_stable_input(input, None, decoded_limit, &governor, &request_control).unwrap();
        assert_eq!(result.analysis.frames, 24_000);
        assert_eq!(result.decoded_samples, decoded_limit);
        let offline = crate::analysis::analyze(
            &crate::decoder::decode_limited(result._input.stable_path(), decoded_limit).unwrap(),
        );
        assert!((result.analysis.lufs - offline.lufs).abs() <= 1e-9);
        assert!((result.analysis.rms_db - offline.rms_db).abs() <= 1e-9);
        assert_eq!(
            result.analysis.true_peak.to_bits(),
            offline.true_peak.to_bits()
        );
        assert_eq!(governor.memory_used(), reservation);
        assert_eq!(governor.temporary_storage_used(), audio.len() as u64);

        drop(result);
        assert_eq!(governor.memory_used(), 0);
        assert_eq!(governor.temporary_storage_used(), 0);
    }

    #[test]
    fn decoded_expansion_and_cooperative_cancellation_release_all_resources() {
        let audio = mono_s16_wave(24_000);

        let limited_reservation = analysis_reservation(audio.len(), 23_999);
        let limited_governor = ResourceGovernor::new(limited_reservation, audio.len() as u64);
        let limited_control = control();
        let limited_input = spool_bytes(&limited_governor, &limited_control, &audio);
        let error = analyze_stable_input(
            limited_input,
            None,
            23_999,
            &limited_governor,
            &limited_control,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ControlledAnalysisError::Runtime(ref runtime)
                if runtime.kind() == ServiceRuntimeErrorKind::LimitExceeded
        ));
        assert_eq!(limited_governor.memory_used(), 0);
        assert_eq!(limited_governor.temporary_storage_used(), 0);

        let cancelled_governor = ResourceGovernor::new(
            analysis_reservation(audio.len(), 24_000),
            audio.len() as u64,
        );
        let cancelled_control = control();
        let cancelled_input = spool_bytes(&cancelled_governor, &cancelled_control, &audio);
        let mut checkpoints = 0;
        let error = analyze_stable_input_with_checkpoint(
            cancelled_input,
            None,
            24_000,
            &cancelled_governor,
            &cancelled_control,
            |control, processed_frames| {
                checkpoints += 1;
                assert!(processed_frames > 0);
                control.cancel();
            },
        )
        .unwrap_err();
        assert_eq!(checkpoints, 1);
        assert!(matches!(
            error,
            ControlledAnalysisError::Runtime(ref runtime)
                if runtime.kind() == ServiceRuntimeErrorKind::Cancelled
        ));
        assert_eq!(cancelled_governor.memory_used(), 0);
        assert_eq!(cancelled_governor.temporary_storage_used(), 0);
    }

    #[test]
    fn junk_heavy_wave_probe_honors_cancel_and_deadline_and_releases_quotas() {
        let audio = mono_s16_wave_with_junk_chunks(1_024);
        let reservation = analysis_reservation(audio.len(), 1);

        for (deadline, expected) in [
            (false, ServiceRuntimeErrorKind::Cancelled),
            (true, ServiceRuntimeErrorKind::DeadlineExceeded),
        ] {
            let governor = ResourceGovernor::new(reservation, audio.len() as u64);
            let request_control = control();
            let input = spool_bytes(&governor, &request_control, &audio);
            let mut checkpoints = 0;
            let error = analyze_stable_input_with_probe_checkpoint(
                input,
                None,
                1,
                &governor,
                &request_control,
                |control| {
                    checkpoints += 1;
                    if checkpoints == 8 {
                        if deadline {
                            control.expire();
                        } else {
                            control.cancel();
                        }
                    }
                },
            )
            .unwrap_err();
            assert!(matches!(
                error,
                ControlledAnalysisError::Runtime(ref runtime) if runtime.kind() == expected
            ));
            assert_eq!(checkpoints, 8);
            assert_eq!(governor.memory_used(), 0);
            assert_eq!(governor.temporary_storage_used(), 0);
        }
    }

    #[cfg(feature = "opus-encoding")]
    #[test]
    fn ogg_page_scan_cancellation_releases_memory_and_temp_quotas() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("controlled.opus");
        let roles = crate::wav::default_channel_roles(1);
        let mut writer =
            crate::opus::OpusStreamWriter::create(&path, 48_000, 960, 1, &roles, 64, -18.0, None)
                .unwrap();
        writer.write_chunk(&[vec![0.0; 960]]).unwrap();
        writer.finish().unwrap();
        let audio = std::fs::read(&path).unwrap();
        let reservation = analysis_reservation(audio.len(), 960);
        let governor = ResourceGovernor::new(reservation, audio.len() as u64);
        let request_control = control();
        let input = spool_bytes(&governor, &request_control, &audio);

        let mut checkpoints = 0;
        let error = analyze_stable_input_with_probe_checkpoint(
            input,
            None,
            960,
            &governor,
            &request_control,
            |control| {
                checkpoints += 1;
                if checkpoints == 4 {
                    control.cancel();
                }
            },
        )
        .unwrap_err();
        assert_eq!(checkpoints, 4);
        assert!(matches!(
            error,
            ControlledAnalysisError::Runtime(ref runtime)
                if runtime.kind() == ServiceRuntimeErrorKind::Cancelled
        ));
        assert_eq!(governor.memory_used(), 0);
        assert_eq!(governor.temporary_storage_used(), 0);
    }

    #[test]
    fn analysis_reservation_is_checked_and_covers_named_working_sets() {
        assert_eq!(
            service_analysis_working_set_reservation_bytes(1, 0)
                .unwrap_err()
                .kind(),
            ServiceRuntimeErrorKind::InvalidLimit
        );
        assert_eq!(
            service_analysis_working_set_reservation_bytes(1, u64::MAX)
                .unwrap_err()
                .kind(),
            ServiceRuntimeErrorKind::ArithmeticOverflow
        );
        assert_eq!(
            service_analysis_working_set_reservation_bytes(0, 7)
                .unwrap_err()
                .kind(),
            ServiceRuntimeErrorKind::InvalidLimit
        );
        let reservation = service_analysis_working_set_reservation_bytes(7, 7).unwrap();
        assert!(reservation > 7 + 7 * 16);

        let governor = ResourceGovernor::new(reservation, 1);
        let exact = governor.reserve_memory(reservation).unwrap();
        assert_eq!(governor.memory_used(), reservation);
        assert_eq!(
            governor.reserve_memory(reservation).unwrap_err().kind(),
            ServiceRuntimeErrorKind::QuotaExceeded,
            "a concurrent maximum analysis must not over-admit"
        );
        drop(exact);
        assert!(governor.reserve_memory(reservation).is_ok());
        let below = ResourceGovernor::new(reservation - 1, 1);
        assert_eq!(
            below.reserve_memory(reservation).unwrap_err().kind(),
            ServiceRuntimeErrorKind::QuotaExceeded
        );
        assert!(validate_service_channel_count(SERVICE_MAX_CHANNELS).is_ok());
        assert_eq!(
            validate_service_channel_count(SERVICE_MAX_CHANNELS + 1)
                .unwrap_err()
                .kind(),
            ServiceRuntimeErrorKind::LimitExceeded
        );
    }

    #[test]
    fn runtime_primitives_have_the_required_thread_safety() {
        fn assert_shared<T: Send + Sync + UnwindSafe + RefUnwindSafe>() {}
        fn assert_worker<T: Send + UnwindSafe + RefUnwindSafe>() {}

        assert_shared::<ByteQuota>();
        assert_shared::<QuotaLease>();
        assert_shared::<ResourceGovernor>();
        assert_shared::<RequestControl>();
        assert_worker::<UploadSpool>();
        assert_shared::<StableInputTransfer>();
        assert_shared::<StableInput>();
    }
}
