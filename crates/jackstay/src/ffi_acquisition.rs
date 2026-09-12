//! C ownership boundary for the common acquisition arena.
//!
//! Hosts move an admitted Rust consumer into this API. Frames retain the same
//! Rust lease; neither this boundary nor its callers maintain another lease book.

use std::{
    os::fd::{FromRawFd, OwnedFd},
    ptr,
    time::Duration,
};

use crate::{
    acquisition::arena::{
        AcquireOutcome, ArenaConsumer, ArenaError, Cancellation, ConfigurationDescriptor, ConfigurationGrant, ConfigurationInstall,
        ConsumerGrant, ConsumerReleaseTimeline, FrameDescriptor, FrameLease, GrantDescriptor, WaitEvents, WaitInterest, WaitOutcome,
    },
    ffi::*,
};

pub const FT_ACQUIRE_LATEST: u32 = 1;
pub const FT_ACQUIRE_NEXT: u32 = 2;
pub const FT_ACQUIRE_EXACT: u32 = 3;
pub const FT_WAIT_DATA: u32 = 1;
pub const FT_WAIT_CAPACITY: u32 = 2;
pub const FT_WAIT_ALL: u32 = 3;

pub struct FtAcquisitionConsumer(ArenaConsumer);

impl FtAcquisitionConsumer {
    /// Transfer an admitted consumer to C. Destroy with
    /// `ft_acquisition_consumer_destroy`; outstanding frames remain valid.
    pub fn into_raw(consumer: ArenaConsumer) -> *mut Self {
        Box::into_raw(Box::new(Self(consumer)))
    }
}

pub struct FtAcquiredFrame(Option<FrameLease>);
pub struct FtAcquisitionCancellation(Cancellation);
pub struct FtAcquisitionReleaseTimeline(ConsumerReleaseTimeline);

impl FtAcquisitionReleaseTimeline {
    /// Transfer an already registered, consumer-bound completion observer to C.
    pub fn into_raw(timeline: ConsumerReleaseTimeline) -> *mut Self {
        Box::into_raw(Box::new(Self(timeline)))
    }
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct FtAcquisitionRange {
    pub first: u64,
    pub last: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct FtAcquisitionEvents {
    pub data_cursor: u64,
    pub capacity_epoch: u64,
    pub reconfiguration_epoch: u64,
    pub closed: u32,
    pub reserved: u32,
}

impl From<WaitEvents> for FtAcquisitionEvents {
    fn from(value: WaitEvents) -> Self {
        Self {
            data_cursor: value.data_cursor,
            capacity_epoch: value.capacity_epoch,
            reconfiguration_epoch: value.reconfiguration_epoch,
            closed: u32::from(value.closed),
            reserved: 0,
        }
    }
}

fn status(error: ArenaError) -> FtStatus {
    match error {
        ArenaError::Closed => FT_STATUS_CLOSED,
        ArenaError::Configuration(_) => FT_STATUS_INVALID_ARGUMENT,
        _ => FT_STATUS_ERROR,
    }
}

/// Import a single-use CPU grant serialized as GrantDescriptor JSON, with its
/// five owned setup FDs. After argument validation FDs are consumed on success
/// OR failure, and their entries become -1. No extra transport copies may remain.
///
/// # Safety
/// The producer must obey ConsumerGrant::from_parts' shared-memory protocol.
/// This process must be the sole intended recipient; never replay, forward or
/// fork its grant/mappings. `json` must reference `len` readable bytes. `fds`
/// must reference five distinct, exclusively owned live FDs; `out` must point
/// to null. All arguments must be non-aliasing and valid throughout this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_import_cpu(
    json: *const u8,
    len: usize,
    fds: *mut i32,
    out: *mut *mut FtAcquisitionConsumer,
) -> FtStatus {
    if json.is_null() || fds.is_null() || len == 0 || len > 1024 * 1024 {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: caller supplies a valid output and a writable array of five FDs.
    let Some(out) = (unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: checked non-null; array size/alignment/lifetime are caller obligations.
    let fds = unsafe { &mut *fds.cast::<[i32; 5]>() };
    if fds.iter().enumerate().any(|(index, fd)| *fd < 0 || fds[..index].contains(fd)) {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let owned = std::mem::replace(fds, [-1; 5]).map(|fd| {
        // SAFETY: each live FD is distinct and its sole ownership is transferred.
        unsafe { OwnedFd::from_raw_fd(fd) }
    });
    // SAFETY: readable range is guaranteed by the caller and length is bounded.
    let bytes = unsafe { std::slice::from_raw_parts(json, len) };
    let descriptor: GrantDescriptor = match serde_json::from_slice(bytes) {
        Ok(descriptor) => descriptor,
        Err(_) => return FT_STATUS_INVALID_ARGUMENT,
    };
    let is_cpu = descriptor.payload_capacity != 0;
    // SAFETY: these are the intended process's single-use mappings from the
    // conforming producer required by this function's caller contract.
    let result = unsafe { ConsumerGrant::from_parts(descriptor, owned) }.and_then(|grant| {
        if !is_cpu {
            return Err(ArenaError::Configuration("CPU import requires inline storage"));
        }
        ArenaConsumer::from_grant(grant)
    });
    match result {
        Ok(consumer) => {
            *out = FtAcquisitionConsumer::into_raw(consumer);
            FT_STATUS_OK
        }
        Err(error) => status(error),
    }
}

/// Install a single-use CPU replacement offer without changing incarnation or
/// holding credit. Stale offers are disposed and return FT_STATUS_STALE.
///
/// # Safety
/// The producer, recipient and mapping lifetime must obey
/// ConfigurationGrant::from_parts. No replay, forwarding, fork or other FD
/// copies are allowed. `consumer` must be live and exclusively accessed; `json`
/// must reference `len` readable bytes and `fd` must point to one exclusively
/// owned live FD. All arguments must be non-aliasing. After basic validation the
/// FD is consumed on every outcome and its caller-visible value becomes -1.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_install_cpu_configuration(
    consumer: *mut FtAcquisitionConsumer,
    json: *const u8,
    len: usize,
    fd: *mut i32,
) -> FtStatus {
    if json.is_null() || len == 0 || len > 1024 * 1024 {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: validity, exclusivity and non-aliasing are caller obligations.
    let (Some(consumer), Some(fd)) = (unsafe { consumer.as_mut() }, unsafe { fd.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if *fd < 0 {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: sole ownership of this live FD is transferred exactly once.
    let owned = unsafe { OwnedFd::from_raw_fd(std::mem::replace(fd, -1)) };
    // SAFETY: caller guarantees the readable byte range; length is bounded above.
    let bytes = unsafe { std::slice::from_raw_parts(json, len) };
    let descriptor: ConfigurationDescriptor = match serde_json::from_slice(bytes) {
        Ok(descriptor) => descriptor,
        Err(_) => return FT_STATUS_INVALID_ARGUMENT,
    };
    let is_cpu = descriptor.payload_capacity != 0;
    // SAFETY: this consumer is the intended recipient of the conforming
    // producer's single-use replacement as required by the caller contract.
    let result = unsafe { ConfigurationGrant::from_parts(&consumer.0, descriptor, owned) }.and_then(|grant| {
        if !is_cpu {
            return Err(ArenaError::Configuration("CPU replacement requires inline storage"));
        }
        consumer.0.install_configuration(grant)
    });
    match result {
        Ok(ConfigurationInstall::Installed) => FT_STATUS_OK,
        Ok(ConfigurationInstall::Stale) => FT_STATUS_STALE,
        Err(error) => status(error),
    }
}

/// Relinquish unleased configuration storage, preserving leases and admission.
///
/// # Safety
/// `consumer` must be null or a live handle with exclusive access.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_relinquish_configuration(consumer: *mut FtAcquisitionConsumer) -> FtStatus {
    // SAFETY: caller guarantees live handle and exclusive access.
    let Some(consumer) = (unsafe { consumer.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    consumer.0.relinquish_configuration();
    FT_STATUS_OK
}

/// Acquire latest/next after `cursor`, or exactly `cursor`. Miss and gap ranges
/// are inclusive. A successful call transfers one independently owned frame.
///
/// # Safety
/// All non-null pointers must be live and correctly aligned. `out` must point
/// to a null handle. Outputs must not alias inputs or each other. Serialize all
/// calls using the same consumer, including destruction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_acquire(
    consumer: *const FtAcquisitionConsumer,
    mode: u32,
    cursor: u64,
    out: *mut *mut FtAcquiredFrame,
    range: *mut FtAcquisitionRange,
) -> FtStatus {
    // SAFETY: caller supplies valid, non-aliasing pointers as documented above.
    let (Some(consumer), Some(out), Some(range)) = (unsafe { consumer.as_ref() }, unsafe { out.as_mut() }, unsafe { range.as_mut() })
    else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    *range = FtAcquisitionRange::default();
    let result = match mode {
        FT_ACQUIRE_LATEST => consumer.0.acquire_latest(cursor),
        FT_ACQUIRE_NEXT => consumer.0.acquire_next(cursor),
        FT_ACQUIRE_EXACT => consumer.0.acquire_exact(cursor),
        _ => return FT_STATUS_INVALID_ARGUMENT,
    };
    match result {
        Ok(AcquireOutcome::Frame(frame)) => {
            *out = Box::into_raw(Box::new(FtAcquiredFrame(Some(frame))));
            FT_STATUS_OK
        }
        Ok(AcquireOutcome::Empty) => FT_STATUS_EMPTY,
        Ok(AcquireOutcome::Miss { cursor }) => {
            *range = FtAcquisitionRange {
                first: cursor,
                last: cursor,
            };
            FT_STATUS_MISS
        }
        Ok(AcquireOutcome::Gap { first, last }) => {
            *range = FtAcquisitionRange { first, last };
            FT_STATUS_GAP
        }
        Ok(AcquireOutcome::HoldingLimit) => FT_STATUS_HOLDING_LIMIT,
        Ok(AcquireOutcome::Reconfiguration) => FT_STATUS_RECONFIGURATION,
        Ok(AcquireOutcome::Closed) => FT_STATUS_CLOSED,
        Err(error) => status(error),
    }
}

/// Copy the full immutable descriptor, including producer readiness.
///
/// # Safety
/// `frame` must be live and not concurrently released. `out` must be a writable,
/// non-aliasing descriptor or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquired_frame_describe(frame: *const FtAcquiredFrame, out: *mut FrameDescriptor) -> FtStatus {
    // SAFETY: pointer validity and exclusive output are caller obligations.
    let (Some(frame), Some(out)) = (unsafe { frame.as_ref() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    *out = *frame.0.as_ref().expect("live C frame").descriptor();
    FT_STATUS_OK
}

/// Borrow CPU bytes until immediate release or the declared deferred completion.
/// Native frames have no inline bytes and return null with length zero.
///
/// # Safety
/// `frame` must be live and not concurrently released; outputs must be writable,
/// non-aliasing pointers or null. Satisfy descriptor readiness before reading.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquired_frame_bytes(frame: *const FtAcquiredFrame, data: *mut *const u8, len: *mut usize) -> FtStatus {
    // SAFETY: caller guarantees validity and non-aliasing of these pointers.
    let (Some(frame), Some(data), Some(len)) = (unsafe { frame.as_ref() }, unsafe { data.as_mut() }, unsafe { len.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let bytes = frame.0.as_ref().expect("live C frame").bytes();
    *data = if bytes.is_empty() { ptr::null() } else { bytes.as_ptr() };
    *len = bytes.len();
    FT_STATUS_OK
}

/// Release completed use and null the handle. Consumer liveness is irrelevant.
///
/// # Safety
/// `frame` must point to an exclusively owned live handle or null. All use of its
/// storage, including GPU work, must have finished before immediate release.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquired_frame_release(frame: *mut *mut FtAcquiredFrame) -> FtStatus {
    // SAFETY: caller guarantees pointer validity and sole ownership.
    let Some(frame) = (unsafe { frame.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if frame.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: this pointer was returned by acquire and is consumed exactly once.
    drop(unsafe { Box::from_raw(std::mem::replace(frame, ptr::null_mut())) });
    FT_STATUS_OK
}

/// Defer release to actual completion. Success nulls the handle; rejection
/// preserves its address, descriptor, storage and holding credit.
///
/// # Safety
/// Pointers must refer to live, non-aliasing handles with exclusive frame access.
/// `value` must cover all outstanding use on the bound completion timeline.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquired_frame_defer_release(
    frame: *mut *mut FtAcquiredFrame,
    timeline: *const FtAcquisitionReleaseTimeline,
    value: u64,
) -> FtStatus {
    // SAFETY: pointer validity and exclusive frame access are caller obligations.
    let (Some(frame), Some(timeline)) = (unsafe { frame.as_mut() }, unsafe { timeline.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    // SAFETY: the caller owns this live frame exclusively.
    let Some(owner) = (unsafe { frame.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let lease = owner.0.take().expect("live C frame");
    match lease.defer_release(&timeline.0, value) {
        Ok(()) => {
            // SAFETY: the retirement owner now retains storage. Consume only
            // the empty C box, once, and invalidate the caller's handle.
            drop(unsafe { Box::from_raw(std::mem::replace(frame, ptr::null_mut())) });
            FT_STATUS_OK
        }
        Err(rejected) => {
            owner.0 = Some(*rejected.frame);
            status(rejected.error)
        }
    }
}

/// Snapshot before checking acquisition, then wait on that snapshot if needed.
///
/// # Safety
/// Consumer must be live with serialized calls; `out` writable and non-aliasing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_snapshot(consumer: *const FtAcquisitionConsumer, out: *mut FtAcquisitionEvents) -> FtStatus {
    // SAFETY: caller guarantees pointer validity and non-aliasing.
    let (Some(consumer), Some(out)) = (unsafe { consumer.as_ref() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    *out = consumer.0.events().into();
    FT_STATUS_OK
}

/// Efficient wait. Reconfiguration and closure always wake; cancellation takes
/// precedence. `u64::MAX` means infinite; zero checks without sleeping.
///
/// # Safety
/// All pointers must be live and non-aliasing, with exclusive consumer access.
/// Cancellation may run concurrently; its handle must outlive this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_wait(
    consumer: *mut FtAcquisitionConsumer,
    observed: *const FtAcquisitionEvents,
    interest: u32,
    cancel: *const FtAcquisitionCancellation,
    timeout_ns: u64,
    out: *mut FtAcquisitionEvents,
) -> FtStatus {
    // SAFETY: caller guarantees valid non-aliasing pointers and exclusive consumer.
    let (Some(consumer), Some(observed), Some(cancel), Some(out)) = (
        unsafe { consumer.as_mut() },
        unsafe { observed.as_ref() },
        unsafe { cancel.as_ref() },
        unsafe { out.as_mut() },
    ) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    *out = FtAcquisitionEvents::default();
    if observed.closed > 1 || observed.reserved != 0 {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let interest = match interest {
        FT_WAIT_DATA => WaitInterest::DATA,
        FT_WAIT_CAPACITY => WaitInterest::CAPACITY,
        FT_WAIT_ALL => WaitInterest::ALL,
        _ => return FT_STATUS_INVALID_ARGUMENT,
    };
    let observed = WaitEvents {
        data_cursor: observed.data_cursor,
        capacity_epoch: observed.capacity_epoch,
        reconfiguration_epoch: observed.reconfiguration_epoch,
        closed: observed.closed != 0,
    };
    let timeout = (timeout_ns != u64::MAX).then(|| Duration::from_nanos(timeout_ns));
    match consumer.0.wait(observed, interest, &cancel.0, timeout) {
        Ok(WaitOutcome::Changed(events)) => {
            *out = events.into();
            if events.closed { FT_STATUS_CLOSED } else { FT_STATUS_OK }
        }
        Ok(WaitOutcome::Cancelled) => FT_STATUS_CANCELLED,
        Ok(WaitOutcome::TimedOut) => FT_STATUS_TIMEOUT,
        Err(error) => status(error),
    }
}

/// Create a one-way cancellation token; cancellation never releases frames.
///
/// # Safety
/// `out` must be writable and contain a null handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_cancellation_create(out: *mut *mut FtAcquisitionCancellation) -> FtStatus {
    // SAFETY: caller guarantees a writable output pointer.
    let Some(out) = (unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    match Cancellation::new() {
        Ok(cancel) => {
            *out = Box::into_raw(Box::new(FtAcquisitionCancellation(cancel)));
            FT_STATUS_OK
        }
        Err(_) => FT_STATUS_ERROR,
    }
}

/// Cancel from any thread. Cancellation is permanent and idempotent.
///
/// # Safety
/// `cancel` must remain live throughout this call and all concurrent waits.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_cancellation_cancel(cancel: *const FtAcquisitionCancellation) -> FtStatus {
    // SAFETY: caller guarantees the token's lifetime; Cancellation is Sync.
    let Some(cancel) = (unsafe { cancel.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    cancel.0.cancel().map_or(FT_STATUS_ERROR, |()| FT_STATUS_OK)
}

macro_rules! destroy {
    ($name:ident, $kind:ty, $description:literal) => {
        #[doc = $description]
        /// Null input/handle is harmless. The handle is nulled after destruction.
        ///
        /// # Safety
        /// Pointer must be writable, with sole ownership of the live handle.
        /// No concurrent operation may still be using this handle.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(handle: *mut *mut $kind) {
            // SAFETY: validity and exclusive ownership are caller obligations.
            if let Some(handle) = unsafe { handle.as_mut() } {
                if !handle.is_null() {
                    // SAFETY: this exact box is consumed once, after clearing it.
                    drop(unsafe { Box::from_raw(std::mem::replace(handle, ptr::null_mut())) });
                }
            }
        }
    };
}

destroy!(
    ft_acquisition_consumer_destroy,
    FtAcquisitionConsumer,
    "Close acquisitions without invalidating existing frames."
);
destroy!(
    ft_acquisition_cancellation_destroy,
    FtAcquisitionCancellation,
    "Destroy a cancellation token after all waits return."
);
destroy!(
    ft_acquisition_release_timeline_destroy,
    FtAcquisitionReleaseTimeline,
    "Destroy a binding handle; pending use keeps its own completion observer."
);
