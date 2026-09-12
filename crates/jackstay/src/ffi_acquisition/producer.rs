//! Single-stream CPU producer boundary. Hosts own source/track selection and
//! serialize producer calls; consumers and frames use the common arena API.

use std::{ptr, time::Duration};

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

pub struct FtCpuProducer(ArenaProducer);

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
            *out = Box::into_raw(Box::new(FtCpuProducer(producer)));
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
    let (Some(producer), Some(out)) = (unsafe { producer.as_mut() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    match producer.0.attach(holding).and_then(ArenaConsumer::from_grant) {
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
    let (Some(producer), Some(descriptor), Some(cursor)) = (unsafe { producer.as_mut() }, unsafe { descriptor.as_ref() }, unsafe {
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
    // SAFETY: caller supplies this readable byte range for the duration of copy.
    match producer.0.publish(descriptor, unsafe { std::slice::from_raw_parts(bytes, len) }) {
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
    let (Some(producer), Some(out)) = (unsafe { producer.as_mut() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Ok(capacity) = usize::try_from(payload_capacity) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    transition(producer.0.reconfigure_cpu(capacity), out)
}

/// Retry a pending replacement while capture is idle. Call after old maps or
/// claims retire; a capacity pause must not depend on another incoming frame.
///
/// # Safety
/// Producer is live/exclusive; output is valid, writable and disjoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_producer_advance(producer: *mut FtCpuProducer, out: *mut FtCpuReconfiguration) -> FtStatus {
    // SAFETY: caller supplies valid exclusive handle/output storage.
    let (Some(producer), Some(out)) = (unsafe { producer.as_mut() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    transition(producer.0.advance_reconfiguration(), out)
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
    let (Some(producer), Some(consumer)) = (unsafe { producer.as_mut() }, unsafe { consumer.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    match producer.0.configure_consumer(&mut consumer.0) {
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
    let Some(producer) = (unsafe { producer.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    match producer.0.poll_cleanup() {
        Ok(_) if producer.0.cleanup_failures().is_empty() => FT_STATUS_OK,
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
    let Some(producer) = (unsafe { handle.as_mut() }) else {
        return FT_STATUS_OK;
    };
    producer.0.stop();
    match producer.0.poll_shutdown_ready() {
        Ok(true) => {
            // SAFETY: successful drainage permits consuming the single owner.
            drop(unsafe { Box::from_raw(std::mem::replace(handle, ptr::null_mut())) });
            FT_STATUS_OK
        }
        Ok(false) => {
            if producer.0.cleanup_failures().is_empty() {
                FT_STATUS_DRAINING
            } else {
                FT_STATUS_RECOVERY_REQUIRED
            }
        }
        Err(error) => status(error),
    }
}
