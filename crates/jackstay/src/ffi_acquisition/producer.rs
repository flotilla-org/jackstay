//! Single-stream CPU producer boundary. Hosts own source/track selection and
//! serialize producer calls; consumers and frames use the common arena API.

use std::{
    ptr,
    sync::{Arc, Mutex},
    time::Duration,
};

use super::FtAcquisitionConsumer;
use crate::{
    acquisition::{
        AdmissionError,
        arena::{
            ArenaConfig, ArenaConsumer, ArenaError, ArenaProducer, ConfigurationInstall, FrameDescriptor, PublishOutcome,
            ReconfigurationStatus,
        },
    },
    ffi::*,
};

pub struct FtCpuProducer(pub(super) Arc<Mutex<ArenaProducer>>);

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtCpuProducerConfig {
    pub resource_capacity: u32,
    pub retained_history: u32,
    pub producer_reserve: u32,
    pub max_incarnations: u32,
    pub payload_capacity: u64,
    pub memory_budget: u64,
    pub drain_timeout_ns: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct FtCpuReconfiguration {
    pub generation: u64,
    pub requested_bytes: u64,
    pub available_bytes: u64,
}

fn status(error: ArenaError) -> FtStatus {
    match error {
        ArenaError::Admission(
            AdmissionError::InsufficientMemory { .. } | AdmissionError::InsufficientResources { .. } | AdmissionError::IncarnationCapacity,
        ) => FT_STATUS_CAPACITY,
        ArenaError::Admission(AdmissionError::ReconfigurationPending) => FT_STATUS_PAUSED_CAPACITY,
        ArenaError::Admission(AdmissionError::InvalidLimits(_) | AdmissionError::InvalidRequest) | ArenaError::PayloadTooLarge => {
            FT_STATUS_INVALID_ARGUMENT
        }
        ArenaError::RecoveryRequired { .. } => FT_STATUS_RECOVERY_REQUIRED,
        error => super::status(error),
    }
}

/// Create one bounded CPU stream. No source selection or authorization occurs.
///
/// # Safety
/// Config/output must be valid and disjoint, and *out must be null. Serialize
/// calls using a producer, including destruction. Do not abandon a draining
/// producer: keep polling cleanup or retrying destroy until it succeeds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_producer_create(config: *const FtCpuProducerConfig, out: *mut *mut FtCpuProducer) -> FtStatus {
    // SAFETY: validity/exclusivity are caller obligations; nulls are rejected.
    let (Some(config), Some(out)) = (unsafe { config.as_ref() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let Ok(payload_capacity) = usize::try_from(config.payload_capacity) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    match ArenaProducer::new(ArenaConfig {
        resource_capacity: config.resource_capacity,
        retained_history: config.retained_history,
        producer_reserve: config.producer_reserve,
        max_incarnations: config.max_incarnations,
        payload_capacity,
        memory_budget: config.memory_budget,
        drain_timeout: Duration::from_nanos(config.drain_timeout_ns),
    }) {
        Ok(producer) => {
            *out = Box::into_raw(Box::new(FtCpuProducer(Arc::new(Mutex::new(producer)))));
            FT_STATUS_OK
        }
        Err(error) => status(error),
    }
}

/// Admit a fresh in-process incarnation with an explicit holding reservation.
///
/// # Safety
/// Producer is live/exclusive; output is writable, disjoint and starts null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_producer_attach(
    producer: *mut FtCpuProducer,
    holding: u32,
    out: *mut *mut FtAcquisitionConsumer,
) -> FtStatus {
    // SAFETY: caller supplies valid exclusive handles/output storage.
    let (Some(producer), Some(out)) = (unsafe { producer.as_ref() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let Ok(mut arena) = producer.0.lock() else {
        return FT_STATUS_ERROR;
    };
    match arena.attach(holding).and_then(ArenaConsumer::from_grant) {
        Ok(consumer) => {
            *out = FtAcquisitionConsumer::into_raw(consumer);
            FT_STATUS_OK
        }
        Err(error) => status(error),
    }
}

/// Copy one CPU frame. OK writes its publication cursor; DROPPED writes zero.
/// Readiness describes this completed copy; native handles are not accepted.
///
/// # Safety
/// Producer is live/exclusive. Descriptor, byte range and output are valid,
/// disjoint, and do not overlap producer mappings that this call can mutate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_producer_publish(
    producer: *mut FtCpuProducer,
    descriptor: *const FrameDescriptor,
    bytes: *const u8,
    len: usize,
    cursor: *mut u64,
) -> FtStatus {
    // SAFETY: pointer validity and non-aliasing are caller obligations.
    let (Some(producer), Some(descriptor), Some(cursor)) = (unsafe { producer.as_ref() }, unsafe { descriptor.as_ref() }, unsafe {
        cursor.as_mut()
    }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    *cursor = 0;
    if bytes.is_null()
        || len == 0
        || len > isize::MAX as usize
        || descriptor.payload_kind != 0
        || descriptor.width == 0
        || descriptor.height == 0
        || u64::from(descriptor.width) * 4 > u64::from(descriptor.stride)
        || u64::from(descriptor.stride) * u64::from(descriptor.height) != len as u64
        || !matches!(descriptor.pixel_format, FT_PIXEL_FORMAT_BGRA8_UNORM | FT_PIXEL_FORMAT_RGBA8_UNORM)
    {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let mut descriptor = *descriptor;
    descriptor.sync_kind = FT_FRAME_SYNC_CPU_COPY_COMPLETE;
    descriptor.fence_id = 0;
    descriptor.fence_value = 0;
    descriptor.modifier = 0;
    let Ok(mut arena) = producer.0.lock() else {
        return FT_STATUS_ERROR;
    };
    // SAFETY: caller supplies this readable byte range for the duration of copy.
    match arena.publish(descriptor, unsafe { std::slice::from_raw_parts(bytes, len) }) {
        Ok(PublishOutcome::Published { cursor: published }) => {
            *cursor = published;
            FT_STATUS_OK
        }
        Ok(PublishOutcome::Dropped) => FT_STATUS_DROPPED,
        Err(error) => status(error),
    }
}

fn transition(result: Result<ReconfigurationStatus, ArenaError>, out: &mut FtCpuReconfiguration) -> FtStatus {
    *out = FtCpuReconfiguration::default();
    match result {
        Ok(ReconfigurationStatus::Ready { generation }) => {
            out.generation = generation;
            FT_STATUS_OK
        }
        Ok(ReconfigurationStatus::PausedCapacity { requested, available }) => {
            out.requested_bytes = requested;
            out.available_bytes = available;
            FT_STATUS_PAUSED_CAPACITY
        }
        Err(ArenaError::Admission(AdmissionError::InsufficientMemory { requested, available })) => {
            out.requested_bytes = requested;
            out.available_bytes = available;
            FT_STATUS_CAPACITY
        }
        Err(error) => status(error),
    }
}

/// Begin a byte-budgeted replacement, including for a same-size format change.
///
/// # Safety
/// Producer is live/exclusive; output is valid, writable and disjoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_producer_reconfigure(
    producer: *mut FtCpuProducer,
    payload_capacity: u64,
    out: *mut FtCpuReconfiguration,
) -> FtStatus {
    // SAFETY: caller supplies valid exclusive handle/output storage.
    let (Some(producer), Some(out)) = (unsafe { producer.as_ref() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Ok(capacity) = usize::try_from(payload_capacity) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Ok(mut arena) = producer.0.lock() else {
        return FT_STATUS_ERROR;
    };
    transition(arena.reconfigure_cpu(capacity), out)
}

/// Retry a pending replacement while capture is idle. Call after old maps or
/// claims retire; a capacity pause must not depend on another incoming frame.
///
/// # Safety
/// Producer is live/exclusive; output is valid, writable and disjoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_producer_advance(producer: *mut FtCpuProducer, out: *mut FtCpuReconfiguration) -> FtStatus {
    // SAFETY: caller supplies valid exclusive handle/output storage.
    let (Some(producer), Some(out)) = (unsafe { producer.as_ref() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Ok(mut arena) = producer.0.lock() else {
        return FT_STATUS_ERROR;
    };
    transition(arena.advance_reconfiguration(), out)
}

/// Install a replacement on a matching local consumer. Empty means no offer.
///
/// # Safety
/// Both handles are live, exclusive and distinct. Foreign associations are
/// rejected before a current-generation check or an offer is imported.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_producer_configure_consumer(
    producer: *mut FtCpuProducer,
    consumer: *mut FtAcquisitionConsumer,
) -> FtStatus {
    // SAFETY: caller supplies valid exclusive handles.
    let (Some(producer), Some(consumer)) = (unsafe { producer.as_ref() }, unsafe { consumer.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Ok(mut arena) = producer.0.lock() else {
        return FT_STATUS_ERROR;
    };
    match arena.configure_consumer(&mut consumer.0) {
        Ok(Some(ConfigurationInstall::Installed)) => FT_STATUS_OK,
        Ok(Some(ConfigurationInstall::Stale)) => FT_STATUS_STALE,
        Ok(None) => FT_STATUS_EMPTY,
        Err(error) => status(error),
    }
}

/// Run idle cleanup; a recovery failure is distinct from successful maintenance.
///
/// # Safety
/// Producer must be a live exclusively borrowed handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_producer_poll_cleanup(producer: *mut FtCpuProducer) -> FtStatus {
    // SAFETY: caller supplies a valid exclusive handle.
    let Some(producer) = (unsafe { producer.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Ok(mut arena) = producer.0.lock() else {
        return FT_STATUS_ERROR;
    };
    match arena.poll_cleanup() {
        Ok(_) if arena.cleanup_failures().is_empty() => FT_STATUS_OK,
        Ok(_) => FT_STATUS_RECOVERY_REQUIRED,
        Err(error) => status(error),
    }
}

/// Stop publication/admission and destroy only after actual retirement. Returns
/// DRAINING or RECOVERY_REQUIRED with the handle unchanged if owners remain.
/// Destroy consumers/release frames, continue maintenance, then retry. Success
/// clears the handle. Timeout is never permission to force destruction.
///
/// # Safety
/// Pointer is writable and exclusively owns its handle. Null *producer is OK.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_producer_destroy(producer: *mut *mut FtCpuProducer) -> FtStatus {
    // SAFETY: caller supplies exclusive writable handle storage.
    let Some(handle) = (unsafe { producer.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    // SAFETY: non-null pointee is exclusively owned and live.
    let Some(producer) = (unsafe { handle.as_ref() }) else {
        return FT_STATUS_OK;
    };
    let Ok(mut arena) = producer.0.lock() else {
        return FT_STATUS_ERROR;
    };
    arena.stop();
    match arena.poll_shutdown_ready() {
        Ok(true) if Arc::strong_count(&producer.0) == 1 => {
            drop(arena);
            // SAFETY: successful drainage permits consuming the single owner.
            drop(unsafe { Box::from_raw(std::mem::replace(handle, ptr::null_mut())) });
            FT_STATUS_OK
        }
        Ok(_) => {
            if arena.cleanup_failures().is_empty() {
                FT_STATUS_DRAINING
            } else {
                FT_STATUS_RECOVERY_REQUIRED
            }
        }
        Err(error) => status(error),
    }
}

use super::{FT_OS_OBJECT_NONE, FtOsObject};
use crate::acquisition::arena::{CpuReservation, DelegatedWriter, WriterDescriptor, WriterExport, WriterSlot};

pub struct FtCpuReservation {
    reservation: CpuReservation,
    producer: Arc<Mutex<ArenaProducer>>,
}
pub struct FtCpuWriterExport {
    export: WriterExport,
    _producer: Arc<Mutex<ArenaProducer>>,
}
pub struct FtCpuWriter(DelegatedWriter);

/// Reserve an unleased CPU slot. DROPPED clears view/length and leaves *out null.
///
/// # Safety
/// All pointers are live, writable and disjoint; *out starts null. Serialize
/// producer calls. The returned view is exclusive until commit/abandon; cease
/// local access while a delegate writes and finish delegate writes before either.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_producer_reserve(
    producer: *mut FtCpuProducer,
    out: *mut *mut FtCpuReservation,
    bytes: *mut *mut u8,
    len: *mut usize,
    slot: *mut WriterSlot,
) -> FtStatus {
    // SAFETY: pointer validity and exclusivity are caller obligations.
    let (Some(producer), Some(out), Some(bytes), Some(len), Some(slot)) = (
        unsafe { producer.as_ref() },
        unsafe { out.as_mut() },
        unsafe { bytes.as_mut() },
        unsafe { len.as_mut() },
        unsafe { slot.as_mut() },
    ) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    *bytes = ptr::null_mut();
    *len = 0;
    let Ok(mut arena) = producer.0.lock() else { return FT_STATUS_ERROR };
    match arena.reserve() {
        Ok(Some(mut reservation)) => {
            *slot = reservation.slot();
            let view = reservation.bytes_mut();
            *bytes = view.as_mut_ptr();
            *len = view.len();
            *out = Box::into_raw(Box::new(FtCpuReservation {
                reservation,
                producer: producer.0.clone(),
            }));
            FT_STATUS_OK
        }
        Ok(None) => FT_STATUS_DROPPED,
        Err(error) => status(error),
    }
}

/// Commit completed pixels. Consumes/clears reservation on success or error
/// after pointer validation. payload_len must equal stride * height.
///
/// # Safety
/// Reservation is exclusively owned; descriptor/cursor are valid and disjoint.
/// No writer or local view may access the slot during or after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_producer_commit(
    handle: *mut *mut FtCpuReservation,
    descriptor: *const FrameDescriptor,
    cursor: *mut u64,
) -> FtStatus {
    // SAFETY: caller provides valid exclusive pointers.
    let (Some(handle), Some(descriptor), Some(cursor)) = (unsafe { handle.as_mut() }, unsafe { descriptor.as_ref() }, unsafe {
        cursor.as_mut()
    }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if handle.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    *cursor = 0;
    // SAFETY: handle exclusively owns this live reservation.
    let owner = unsafe { Box::from_raw(std::mem::replace(handle, ptr::null_mut())) };
    if descriptor.payload_kind != 0
        || descriptor.width == 0
        || descriptor.height == 0
        || u64::from(descriptor.width) * 4 > u64::from(descriptor.stride)
        || u64::from(descriptor.stride) * u64::from(descriptor.height) != descriptor.payload_len
        || !matches!(descriptor.pixel_format, FT_PIXEL_FORMAT_BGRA8_UNORM | FT_PIXEL_FORMAT_RGBA8_UNORM)
    {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let mut descriptor = *descriptor;
    descriptor.sync_kind = FT_FRAME_SYNC_CPU_COPY_COMPLETE;
    descriptor.fence_id = 0;
    descriptor.fence_value = 0;
    descriptor.modifier = 0;
    let Ok(mut arena) = owner.producer.lock() else {
        return FT_STATUS_ERROR;
    };
    match arena.commit(owner.reservation, descriptor) {
        Ok(PublishOutcome::Published { cursor: value }) => {
            *cursor = value;
            FT_STATUS_OK
        }
        Ok(PublishOutcome::Dropped) => FT_STATUS_DROPPED,
        Err(error) => status(error),
    }
}

/// # Safety
/// Exclusively owns *handle. All delegate writes/local views have finished.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_producer_abandon(handle: *mut *mut FtCpuReservation) -> FtStatus {
    // SAFETY: valid exclusive handle pointer is required.
    let Some(handle) = (unsafe { handle.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !handle.is_null() {
        // SAFETY: caller transfers sole live ownership.
        drop(unsafe { Box::from_raw(std::mem::replace(handle, ptr::null_mut())) });
    }
    FT_STATUS_OK
}

/// Export payload-only storage. Retain owner until every duplicate object and
/// child view is closed. Re-export on each installed allocation generation.
///
/// # Safety
/// Pointers are valid/disjoint, *out is null, *object is NONE. Child is trusted
/// and obeys reservation completion; host tracks all duplicate object lifetimes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_producer_export_writer(
    producer: *mut FtCpuProducer,
    out: *mut *mut FtCpuWriterExport,
    descriptor: *mut WriterDescriptor,
    object: *mut FtOsObject,
) -> FtStatus {
    // SAFETY: caller supplies live exclusive pointers.
    let (Some(producer), Some(out), Some(descriptor), Some(object)) = (
        unsafe { producer.as_ref() },
        unsafe { out.as_mut() },
        unsafe { descriptor.as_mut() },
        unsafe { object.as_mut() },
    ) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() || *object != FT_OS_OBJECT_NONE {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let Ok(mut arena) = producer.0.lock() else { return FT_STATUS_ERROR };
    let export = match arena.export_writer() {
        Ok(Some(export)) => export,
        Ok(None) => return FT_STATUS_DROPPED,
        Err(error) => return status(error),
    };
    // SAFETY: exported lifetime/completion is the caller's obligation.
    let fd = match unsafe { export.duplicate_object() } {
        Ok(fd) => fd,
        Err(error) => return status(error),
    };
    *descriptor = export.descriptor();
    #[cfg(unix)]
    {
        use std::os::fd::IntoRawFd;
        *object = fd.into_raw_fd();
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::IntoRawHandle;
        *object = fd.into_raw_handle();
    }
    *out = Box::into_raw(Box::new(FtCpuWriterExport {
        export,
        _producer: producer.0.clone(),
    }));
    FT_STATUS_OK
}

/// # Safety
/// *handle is exclusively owned. All exported objects/views are already closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_writer_export_destroy(handle: *mut *mut FtCpuWriterExport) -> FtStatus {
    // SAFETY: caller supplies exclusive handle storage.
    let Some(handle) = (unsafe { handle.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !handle.is_null() {
        // SAFETY: caller transfers sole live ownership after remote unmap.
        let owner = unsafe { Box::from_raw(std::mem::replace(handle, ptr::null_mut())) };
        drop(owner.export);
    }
    FT_STATUS_OK
}

/// # Safety
/// Descriptor/object are a conforming producer's export; its lifetime owner
/// outlives this writer. Pointers are valid/disjoint and *out starts null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_writer_import(
    descriptor: *const WriterDescriptor,
    object: *mut FtOsObject,
    out: *mut *mut FtCpuWriter,
) -> FtStatus {
    // SAFETY: caller supplies live exclusive pointers.
    let (Some(descriptor), Some(object), Some(out)) = (unsafe { descriptor.as_ref() }, unsafe { object.as_mut() }, unsafe { out.as_mut() })
    else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() || super::invalid_object(*object) {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: sole live ownership is transferred.
    let fd = unsafe { super::own_object(std::mem::replace(object, FT_OS_OBJECT_NONE)) };
    // SAFETY: trusted producer/lifetime contract is required of the caller.
    match unsafe { DelegatedWriter::from_parts(*descriptor, fd) } {
        Ok(writer) => {
            *out = Box::into_raw(Box::new(FtCpuWriter(writer)));
            FT_STATUS_OK
        }
        Err(error) => status(error),
    }
}

/// # Safety
/// Valid/disjoint pointers and exclusive writer; slot is currently reserved
/// exclusively for this delegate. Finish writes before acknowledging completion.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_writer_slot_view(
    writer: *mut FtCpuWriter,
    slot: *const WriterSlot,
    bytes: *mut *mut u8,
    len: *mut usize,
) -> FtStatus {
    // SAFETY: caller supplies live exclusive pointers.
    let (Some(writer), Some(slot), Some(bytes), Some(len)) = (
        unsafe { writer.as_mut() },
        unsafe { slot.as_ref() },
        unsafe { bytes.as_mut() },
        unsafe { len.as_mut() },
    ) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    *bytes = ptr::null_mut();
    *len = 0;
    // SAFETY: caller obeys the producer's live reservation protocol.
    match unsafe { writer.0.bytes_mut(*slot) } {
        Ok(view) => {
            *bytes = view.as_mut_ptr();
            *len = view.len();
            FT_STATUS_OK
        }
        Err(_) => FT_STATUS_STALE,
    }
}

/// # Safety
/// Exclusive live handle; no outstanding borrowed writer view.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_writer_destroy(handle: *mut *mut FtCpuWriter) -> FtStatus {
    // SAFETY: caller supplies exclusive handle storage.
    let Some(handle) = (unsafe { handle.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !handle.is_null() {
        // SAFETY: sole live ownership is transferred.
        drop(unsafe { Box::from_raw(std::mem::replace(handle, ptr::null_mut())) });
    }
    FT_STATUS_OK
}
