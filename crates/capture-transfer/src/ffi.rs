use std::{
    cell::RefCell,
    ffi::{CStr, c_char, c_void},
    ptr::{self, NonNull},
    rc::Rc,
};

use crate::{
    daemon::{self, DaemonConsumer, DaemonFrame},
    model::{
        ClockDomain, ColorSpace, DamageKind, FrameSyncKind, PayloadKind, PixelFormat, SourceDesc, SourceId, SourceKind, TrackDesc, TrackId,
        VideoTrackDesc,
    },
    state::{Event, EventKind, SessionState},
    video::{AcquiredVideoFrame, ConsumerId, VideoFrameDesc, VideoSlotManager},
};

pub type FtStatus = i32;

pub const FT_STATUS_OK: FtStatus = 0;
pub const FT_STATUS_EMPTY: FtStatus = 1;
pub const FT_STATUS_INVALID_ARGUMENT: FtStatus = 2;
pub const FT_STATUS_ERROR: FtStatus = 3;

pub const FT_SOURCE_KIND_WINDOW: u32 = 1;
pub const FT_SOURCE_KIND_DISPLAY: u32 = 2;
pub const FT_SOURCE_KIND_SURFACE: u32 = 3;

pub const FT_TRACK_TYPE_VIDEO: u32 = 1;

pub const FT_PIXEL_FORMAT_UNKNOWN: u32 = 0;
pub const FT_PIXEL_FORMAT_BGRA8_UNORM: u32 = 1;
pub const FT_PIXEL_FORMAT_RGBA8_UNORM: u32 = 2;

pub const FT_CLOCK_DOMAIN_UNKNOWN: u32 = 0;
pub const FT_CLOCK_DOMAIN_UNIX_TIME: u32 = 1;
pub const FT_CLOCK_DOMAIN_MEDIA_TIME: u32 = 2;
pub const FT_CLOCK_DOMAIN_HOST_TIME: u32 = 3;

pub const FT_COLOR_SPACE_UNKNOWN: u32 = 0;
pub const FT_COLOR_SPACE_SRGB: u32 = 1;

pub const FT_FRAME_SYNC_UNKNOWN: u32 = 0;
pub const FT_FRAME_SYNC_CPU_COPY_COMPLETE: u32 = 1;
pub const FT_FRAME_SYNC_SCK_SAMPLE_READY: u32 = 2;
pub const FT_FRAME_SYNC_NATIVE_TIMELINE: u32 = 3;

pub const FT_DAMAGE_UNKNOWN: u32 = 0;
pub const FT_DAMAGE_FULL_FRAME: u32 = 1;
pub const FT_DAMAGE_NONE: u32 = 2;
pub const FT_DAMAGE_INLINE_RECTS: u32 = 3;
pub const FT_DAMAGE_SIDECAR_RECTS: u32 = 4;

pub const FT_EVENT_PRODUCER_STARTED: u32 = 1;
pub const FT_EVENT_SOURCE_REGISTERED: u32 = 2;
pub const FT_EVENT_SOURCE_UPDATED: u32 = 3;
pub const FT_EVENT_TRACK_REGISTERED: u32 = 4;
pub const FT_EVENT_TRACK_UPDATED: u32 = 5;
pub const FT_EVENT_SOURCE_UNREGISTERED: u32 = 6;
pub const FT_EVENT_PRODUCER_STOPPED: u32 = 7;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtProducerOptions {
    pub reserved: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtConsumerOptions {
    pub producer: *mut FtProducer,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtSessionDescriptor {
    pub control_socket_path: *const c_char,
    pub session_id: *const c_char,
}

#[repr(C)]
#[derive(Debug)]
pub struct FtSyntheticSession {
    pub session_id: [c_char; 64],
    pub source_id: u64,
    pub track_id: u64,
    pub fd_socket_path: [c_char; 4096],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtSourceDesc {
    pub kind: u32,
    pub label: *const c_char,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtVideoTrackDesc {
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtTrackDesc {
    pub track_type: u32,
    pub video: FtVideoTrackDesc,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FtVideoFrameDesc {
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: u32,
    pub pool_id: u64,
    pub slot_id: u32,
    pub payload_offset: u64,
    pub payload_len: u64,
    pub payload_map_len: u64,
    pub clock_domain: u32,
    pub color_space: u32,
    pub sync_kind: u32,
    pub damage_kind: u32,
    pub damage_base_sequence: u64,
    pub dropped_before_publish: u32,
    pub producer_drop_count: u64,
    pub evicted_count: u64,
    pub consumer_skipped_count: u64,
    pub payload_kind: u32,
    pub modifier: u64,
    pub fence_id: u64,
    pub fence_value: u64,
    pub flags: u32,
}

// ABI contract for FtVideoFrameDesc, mirrored in include/capture_transfer.h.
// Both sides must agree; narrowing a field or appending to the tail without
// updating both is the trap these asserts guard against.
const _: () = {
    assert!(std::mem::size_of::<FtVideoFrameDesc>() == 168);
    assert!(std::mem::offset_of!(FtVideoFrameDesc, pool_id) == 32);
    assert!(std::mem::offset_of!(FtVideoFrameDesc, slot_id) == 40);
    assert!(std::mem::offset_of!(FtVideoFrameDesc, dropped_before_publish) == 96);
    assert!(std::mem::offset_of!(FtVideoFrameDesc, payload_kind) == 128);
    assert!(std::mem::offset_of!(FtVideoFrameDesc, modifier) == 136);
    assert!(std::mem::offset_of!(FtVideoFrameDesc, fence_id) == 144);
    assert!(std::mem::offset_of!(FtVideoFrameDesc, fence_value) == 152);
    assert!(std::mem::offset_of!(FtVideoFrameDesc, flags) == 160);
};

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FtEvent {
    pub kind: u32,
    pub source_id: u64,
    pub track_id: u64,
    pub track_type: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
}

#[repr(C)]
#[derive(Debug)]
pub struct FtVideoFrame {
    pub desc: FtVideoFrameDesc,
    pub data: *const c_void,
    pub len: usize,
    handle: *mut FtFrameHandle,
}

impl Default for FtVideoFrame {
    fn default() -> Self {
        Self {
            desc: FtVideoFrameDesc::default(),
            data: ptr::null(),
            len: 0,
            handle: ptr::null_mut(),
        }
    }
}

#[derive(Debug)]
struct ProducerInner {
    state: SessionState,
    video: VideoSlotManager,
    next_consumer_id: u64,
}

#[derive(Debug)]
pub struct FtProducer {
    inner: Rc<RefCell<ProducerInner>>,
}

#[derive(Debug)]
pub struct FtConsumer {
    kind: FtConsumerKind,
}

#[derive(Debug)]
enum FtConsumerKind {
    InProcess {
        inner: Rc<RefCell<ProducerInner>>,
        consumer_id: ConsumerId,
        event_cursor: usize,
    },
    Daemon {
        consumer: Box<DaemonConsumer>,
        events: Vec<FtEvent>,
        event_cursor: usize,
    },
}

#[derive(Debug)]
enum FtFrameHandle {
    InProcess {
        inner: Rc<RefCell<ProducerInner>>,
        frame: AcquiredVideoFrame,
    },
    Daemon(DaemonFrame),
}

/// # Safety
///
/// `out` must be a valid, non-null pointer to writable storage for one producer
/// pointer. The returned pointer must be destroyed with `ft_producer_destroy`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_producer_create(_options: *const FtProducerOptions, out: *mut *mut FtProducer) -> FtStatus {
    if out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }

    let producer = Box::new(FtProducer {
        inner: Rc::new(RefCell::new(ProducerInner {
            state: SessionState::new(),
            video: VideoSlotManager::new_reusable_pool(3),
            next_consumer_id: 0,
        })),
    });

    // SAFETY: out was checked for null and points to caller-owned storage.
    unsafe {
        *out = Box::into_raw(producer);
    }
    FT_STATUS_OK
}

/// # Safety
///
/// `producer` must be a live pointer returned by `ft_producer_create`.
/// `desc` and `out_source_id` must be valid, non-null pointers for the duration
/// of the call. `desc.label` must point to a NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_producer_register_source(
    producer: *mut FtProducer,
    desc: *const FtSourceDesc,
    out_source_id: *mut u64,
) -> FtStatus {
    let Some(producer) = ptr_as_non_null(producer) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    // SAFETY: producer was checked for null and must be live for this call.
    let producer = unsafe { producer.as_ref() };
    if desc.is_null() || out_source_id.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }

    // SAFETY: desc was checked for null and is only read during this call.
    let Some(desc) = (unsafe { source_desc_from_ffi(&*desc) }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };

    match producer.inner.borrow_mut().state.register_source(desc) {
        Ok(source_id) => {
            // SAFETY: out_source_id was checked for null and points to caller-owned storage.
            unsafe {
                *out_source_id = source_id.get();
            }
            FT_STATUS_OK
        }
        Err(_) => FT_STATUS_ERROR,
    }
}

/// # Safety
///
/// `producer` must be a live pointer returned by `ft_producer_create`.
/// `desc` and `out_track_id` must be valid, non-null pointers for the duration
/// of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_producer_register_track(
    producer: *mut FtProducer,
    source_id: u64,
    desc: *const FtTrackDesc,
    out_track_id: *mut u64,
) -> FtStatus {
    let Some(producer) = ptr_as_non_null(producer) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    // SAFETY: producer was checked for null and must be live for this call.
    let producer = unsafe { producer.as_ref() };
    if desc.is_null() || out_track_id.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }

    // SAFETY: desc was checked for null and is only read during this call.
    let Some(desc) = track_desc_from_ffi(unsafe { &*desc }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };

    match producer.inner.borrow_mut().state.register_track(SourceId::new(source_id), desc) {
        Ok(track_id) => {
            // SAFETY: out_track_id was checked for null and points to caller-owned storage.
            unsafe {
                *out_track_id = track_id.get();
            }
            FT_STATUS_OK
        }
        Err(_) => FT_STATUS_ERROR,
    }
}

/// # Safety
///
/// `producer` must be a live pointer returned by `ft_producer_create`.
/// `desc` must be a valid, non-null pointer. `pixels` must point to `len`
/// readable bytes for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_producer_publish_video_frame(
    producer: *mut FtProducer,
    track_id: u64,
    desc: *const FtVideoFrameDesc,
    pixels: *const c_void,
    len: usize,
) -> FtStatus {
    let Some(producer) = ptr_as_non_null(producer) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    // SAFETY: producer was checked for null and must be live for this call.
    let producer = unsafe { producer.as_ref() };
    if desc.is_null() || pixels.is_null() || len == 0 {
        return FT_STATUS_INVALID_ARGUMENT;
    }

    // SAFETY: desc and pixels were checked for null and are read only for len bytes during this call.
    let (desc, pixels) = unsafe { (&*desc, std::slice::from_raw_parts(pixels.cast::<u8>(), len)) };
    let Some(desc) = video_frame_desc_from_ffi(desc) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };

    match producer.inner.borrow_mut().video.publish(TrackId::new(track_id), desc, pixels) {
        Ok(()) => FT_STATUS_OK,
        Err(_) => FT_STATUS_ERROR,
    }
}

/// # Safety
///
/// `producer` must be null or a pointer returned by `ft_producer_create` that
/// has not already been destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_producer_destroy(producer: *mut FtProducer) {
    if !producer.is_null() {
        // SAFETY: producer must be a pointer returned by ft_producer_create and not already destroyed.
        unsafe {
            drop(Box::from_raw(producer));
        }
    }
}

/// # Safety
///
/// `options` and `out` must be valid, non-null pointers. `options.producer`
/// must be a live producer pointer. The returned pointer must be destroyed with
/// `ft_consumer_destroy`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_consumer_connect(options: *const FtConsumerOptions, out: *mut *mut FtConsumer) -> FtStatus {
    if options.is_null() || out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }

    // SAFETY: options was checked for null and is only read during this call.
    let producer = unsafe { (*options).producer };
    let Some(producer) = ptr_as_non_null(producer) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    // SAFETY: producer was checked for null and must be live for this call.
    let producer = unsafe { producer.as_ref() };
    let consumer_id = {
        let mut inner = producer.inner.borrow_mut();
        inner.next_consumer_id = inner.next_consumer_id.saturating_add(1).max(1);
        ConsumerId::new(inner.next_consumer_id)
    };

    let consumer = Box::new(FtConsumer {
        kind: FtConsumerKind::InProcess {
            inner: Rc::clone(&producer.inner),
            consumer_id,
            event_cursor: 0,
        },
    });

    // SAFETY: out was checked for null and points to caller-owned storage.
    unsafe {
        *out = Box::into_raw(consumer);
    }
    FT_STATUS_OK
}

/// # Safety
///
/// `control_socket_path` must point to a NUL-terminated string. `out` must
/// point to writable storage. String buffers in `out` are filled on success.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_create_synthetic_session(control_socket_path: *const c_char, out: *mut FtSyntheticSession) -> FtStatus {
    if control_socket_path.is_null() || out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: control_socket_path was checked for null and must be NUL-terminated by caller.
    let Some(control_socket_path) = (unsafe { c_string_to_string(control_socket_path) }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Ok(session) = daemon::create_synthetic_session(&control_socket_path) else {
        return FT_STATUS_ERROR;
    };
    let mut ffi = FtSyntheticSession {
        session_id: [0; 64],
        source_id: session.source_id,
        track_id: session.track_id,
        fd_socket_path: [0; 4096],
    };
    // SAFETY: ffi owns both fixed-size destination buffers.
    if !(unsafe { daemon::copy_string_to_c_buffer(&session.session_id, ffi.session_id.as_mut_ptr(), ffi.session_id.len()) })
        || !(unsafe { daemon::copy_string_to_c_buffer(&session.fd_socket_path, ffi.fd_socket_path.as_mut_ptr(), ffi.fd_socket_path.len()) })
    {
        return FT_STATUS_ERROR;
    }
    // SAFETY: out was checked for null and points to caller-owned storage.
    unsafe {
        *out = ffi;
    }
    FT_STATUS_OK
}

/// # Safety
///
/// `descriptor` and `out` must be valid, non-null pointers. Descriptor string
/// pointers must point to NUL-terminated strings for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_consumer_connect_session(descriptor: *const FtSessionDescriptor, out: *mut *mut FtConsumer) -> FtStatus {
    if descriptor.is_null() || out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: descriptor was checked for null and is only read during this call.
    let descriptor = unsafe { &*descriptor };
    if descriptor.control_socket_path.is_null() || descriptor.session_id.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: descriptor strings were checked for null and must be NUL-terminated by caller.
    let Some(control_socket_path) = (unsafe { c_string_to_string(descriptor.control_socket_path) }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    // SAFETY: descriptor strings were checked for null and must be NUL-terminated by caller.
    let Some(session_id) = (unsafe { c_string_to_string(descriptor.session_id) }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };

    let Ok(info) = daemon::get_session(&control_socket_path, &session_id) else {
        return FT_STATUS_ERROR;
    };
    let events = vec![
        FtEvent {
            kind: FT_EVENT_SOURCE_REGISTERED,
            source_id: info.source_id,
            ..FtEvent::default()
        },
        FtEvent {
            kind: FT_EVENT_TRACK_REGISTERED,
            source_id: info.source_id,
            track_id: info.track_id,
            track_type: FT_TRACK_TYPE_VIDEO,
            width: info.width,
            height: info.height,
            pixel_format: pixel_format_to_ffi(info.pixel_format),
        },
    ];
    let Ok(daemon_consumer) = DaemonConsumer::connect(info) else {
        return FT_STATUS_ERROR;
    };
    let consumer = Box::new(FtConsumer {
        kind: FtConsumerKind::Daemon {
            consumer: Box::new(daemon_consumer),
            events,
            event_cursor: 0,
        },
    });
    // SAFETY: out was checked for null and points to caller-owned storage.
    unsafe {
        *out = Box::into_raw(consumer);
    }
    FT_STATUS_OK
}

/// # Safety
///
/// `consumer` must be a live pointer returned by `ft_consumer_connect`.
/// `out_event` must be a valid, non-null pointer to writable storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_consumer_poll_event(consumer: *mut FtConsumer, out_event: *mut FtEvent) -> FtStatus {
    let Some(mut consumer) = ptr_as_non_null(consumer) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    // SAFETY: consumer was checked for null and must be live and uniquely borrowed for this call.
    let consumer = unsafe { consumer.as_mut() };
    if out_event.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }

    match &mut consumer.kind {
        FtConsumerKind::InProcess { inner, event_cursor, .. } => {
            let events = inner.borrow().state.replay_events();
            let Some(event) = events.get(*event_cursor) else {
                return FT_STATUS_EMPTY;
            };
            *event_cursor += 1;
            // SAFETY: out_event was checked for null and points to caller-owned storage.
            unsafe {
                *out_event = event_to_ffi(event);
            }
        }
        FtConsumerKind::Daemon { events, event_cursor, .. } => {
            let Some(event) = events.get(*event_cursor) else {
                return FT_STATUS_EMPTY;
            };
            *event_cursor += 1;
            // SAFETY: out_event was checked for null and points to caller-owned storage.
            unsafe {
                *out_event = *event;
            }
        }
    }
    FT_STATUS_OK
}

/// # Safety
///
/// `consumer` must be a live pointer returned by `ft_consumer_connect`.
/// `out_frame` must be a valid, non-null pointer to writable storage. A
/// successful frame must be released with `ft_consumer_release_video_frame`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_consumer_acquire_latest_video_frame(
    consumer: *mut FtConsumer,
    track_id: u64,
    out_frame: *mut FtVideoFrame,
) -> FtStatus {
    let Some(mut consumer) = ptr_as_non_null(consumer) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    // SAFETY: consumer was checked for null and must be live and uniquely borrowed for this call.
    let consumer = unsafe { consumer.as_mut() };
    if out_frame.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }

    match &mut consumer.kind {
        FtConsumerKind::InProcess { inner, consumer_id, .. } => {
            match inner.borrow_mut().video.acquire_latest(*consumer_id, TrackId::new(track_id)) {
                Ok(frame) => {
                    let desc = video_frame_desc_to_ffi(&frame.desc);
                    let data = frame.bytes().as_ptr().cast::<c_void>();
                    let len = frame.bytes().len();
                    let handle = Box::into_raw(Box::new(FtFrameHandle::InProcess {
                        inner: Rc::clone(inner),
                        frame,
                    }));
                    // SAFETY: out_frame was checked for null and points to caller-owned storage.
                    unsafe {
                        *out_frame = FtVideoFrame { desc, data, len, handle };
                    }
                    FT_STATUS_OK
                }
                Err(_) => FT_STATUS_ERROR,
            }
        }
        FtConsumerKind::Daemon { consumer, .. } => match consumer.latest_frame(track_id) {
            Ok(frame) => {
                let desc = video_frame_desc_to_ffi(&frame.desc);
                let data = frame.bytes().as_ptr().cast::<c_void>();
                let len = frame.bytes().len();
                let handle = Box::into_raw(Box::new(FtFrameHandle::Daemon(frame)));
                // SAFETY: out_frame was checked for null and points to caller-owned storage.
                unsafe {
                    *out_frame = FtVideoFrame { desc, data, len, handle };
                }
                FT_STATUS_OK
            }
            Err(_) => FT_STATUS_ERROR,
        },
    }
}

/// # Safety
///
/// `consumer` must be a live pointer returned by `ft_consumer_connect`.
/// `frame` must be null or a pointer previously filled by
/// `ft_consumer_acquire_latest_video_frame`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_consumer_release_video_frame(consumer: *mut FtConsumer, frame: *mut FtVideoFrame) {
    let Some(mut consumer) = ptr_as_non_null(consumer) else {
        return;
    };
    // SAFETY: consumer was checked for null and must be live and uniquely borrowed for this call.
    let consumer = unsafe { consumer.as_mut() };
    if frame.is_null() {
        return;
    }

    // SAFETY: frame was checked for null and is caller-owned storage.
    let frame = unsafe { &mut *frame };
    if frame.handle.is_null() {
        return;
    }

    // SAFETY: handle was produced by ft_consumer_acquire_latest_video_frame and is consumed once here.
    let acquired = unsafe { *Box::from_raw(frame.handle) };
    debug_assert!(
        matches!(
            (&consumer.kind, &acquired),
            (FtConsumerKind::InProcess { .. }, FtFrameHandle::InProcess { .. }) | (FtConsumerKind::Daemon { .. }, FtFrameHandle::Daemon(_))
        ),
        "ft_consumer_release_video_frame called with a frame from a different consumer kind"
    );
    match acquired {
        FtFrameHandle::InProcess { inner, frame } => {
            inner.borrow_mut().video.release(frame);
        }
        FtFrameHandle::Daemon(frame) => {
            if let FtConsumerKind::Daemon { consumer, .. } = &mut consumer.kind {
                // Best effort: the C ABI release hook cannot report I/O errors.
                // If this write fails, connection close releases remaining leases.
                let _ = consumer.release_frame(frame);
            }
        }
    }
    frame.handle = ptr::null_mut();
    frame.data = ptr::null();
    frame.len = 0;
}

/// # Safety
///
/// `consumer` must be null or a pointer returned by `ft_consumer_connect` that
/// has not already been destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_consumer_destroy(consumer: *mut FtConsumer) {
    if !consumer.is_null() {
        // SAFETY: consumer must be a pointer returned by ft_consumer_connect and not already destroyed.
        let consumer = unsafe { Box::from_raw(consumer) };
        if let FtConsumerKind::InProcess { inner, consumer_id, .. } = consumer.kind {
            inner.borrow_mut().video.disconnect_consumer(consumer_id);
        }
    }
}

fn ptr_as_non_null<T>(value: *mut T) -> Option<NonNull<T>> {
    NonNull::new(value)
}

unsafe fn c_string_to_string(value: *const c_char) -> Option<String> {
    if value.is_null() {
        return None;
    }
    // SAFETY: caller guarantees value points to a NUL-terminated C string.
    unsafe { CStr::from_ptr(value) }.to_str().ok().map(ToOwned::to_owned)
}

unsafe fn source_desc_from_ffi(desc: &FtSourceDesc) -> Option<SourceDesc> {
    let kind = match desc.kind {
        FT_SOURCE_KIND_WINDOW => SourceKind::Window,
        FT_SOURCE_KIND_DISPLAY => SourceKind::Display,
        FT_SOURCE_KIND_SURFACE => SourceKind::Surface,
        _ => return None,
    };
    if desc.label.is_null() {
        return None;
    }
    // SAFETY: caller guarantees label points to a NUL-terminated C string for this call.
    let label = unsafe { CStr::from_ptr(desc.label) }.to_str().ok()?.to_string();
    Some(SourceDesc { kind, label })
}

fn track_desc_from_ffi(desc: &FtTrackDesc) -> Option<TrackDesc> {
    match desc.track_type {
        FT_TRACK_TYPE_VIDEO => Some(TrackDesc::Video(VideoTrackDesc {
            width: desc.video.width,
            height: desc.video.height,
            pixel_format: pixel_format_from_ffi(desc.video.pixel_format)?,
        })),
        _ => None,
    }
}

fn video_frame_desc_from_ffi(desc: &FtVideoFrameDesc) -> Option<VideoFrameDesc> {
    Some(VideoFrameDesc {
        sequence: desc.sequence,
        timestamp_ns: desc.timestamp_ns,
        width: desc.width,
        height: desc.height,
        stride: desc.stride,
        pixel_format: pixel_format_from_ffi(desc.pixel_format)?,
        pool_id: desc.pool_id,
        slot_id: desc.slot_id,
        payload_offset: desc.payload_offset,
        payload_len: desc.payload_len,
        payload_map_len: desc.payload_map_len,
        clock_domain: clock_domain_from_ffi(desc.clock_domain)?,
        color_space: color_space_from_ffi(desc.color_space)?,
        sync_kind: sync_kind_from_ffi(desc.sync_kind)?,
        damage_kind: damage_kind_from_ffi(desc.damage_kind)?,
        damage_base_sequence: desc.damage_base_sequence,
        dropped_before_publish: desc.dropped_before_publish,
        producer_drop_count: desc.producer_drop_count,
        evicted_count: desc.evicted_count,
        consumer_skipped_count: desc.consumer_skipped_count,
        payload_kind: PayloadKind::from_u32(desc.payload_kind),
        modifier: desc.modifier,
        fence_id: desc.fence_id,
        fence_value: desc.fence_value,
        flags: desc.flags,
    })
}

fn video_frame_desc_to_ffi(desc: &VideoFrameDesc) -> FtVideoFrameDesc {
    FtVideoFrameDesc {
        sequence: desc.sequence,
        timestamp_ns: desc.timestamp_ns,
        width: desc.width,
        height: desc.height,
        stride: desc.stride,
        pixel_format: pixel_format_to_ffi(desc.pixel_format),
        pool_id: desc.pool_id,
        slot_id: desc.slot_id,
        payload_offset: desc.payload_offset,
        payload_len: desc.payload_len,
        payload_map_len: desc.payload_map_len,
        clock_domain: clock_domain_to_ffi(desc.clock_domain),
        color_space: color_space_to_ffi(desc.color_space),
        sync_kind: sync_kind_to_ffi(desc.sync_kind),
        damage_kind: damage_kind_to_ffi(desc.damage_kind),
        damage_base_sequence: desc.damage_base_sequence,
        dropped_before_publish: desc.dropped_before_publish,
        producer_drop_count: desc.producer_drop_count,
        evicted_count: desc.evicted_count,
        consumer_skipped_count: desc.consumer_skipped_count,
        payload_kind: desc.payload_kind as u32,
        modifier: desc.modifier,
        fence_id: desc.fence_id,
        fence_value: desc.fence_value,
        flags: desc.flags,
    }
}

fn event_to_ffi(event: &Event) -> FtEvent {
    let mut ffi = FtEvent {
        kind: event_kind_to_ffi(event.kind()),
        source_id: event.source_id().map_or(0, SourceId::get),
        track_id: event.track_id().map_or(0, TrackId::get),
        ..FtEvent::default()
    };

    if let Some(TrackDesc::Video(video)) = event.track_desc() {
        ffi.track_type = FT_TRACK_TYPE_VIDEO;
        ffi.width = video.width;
        ffi.height = video.height;
        ffi.pixel_format = pixel_format_to_ffi(video.pixel_format);
    }

    ffi
}

fn event_kind_to_ffi(kind: EventKind) -> u32 {
    match kind {
        EventKind::ProducerStarted => FT_EVENT_PRODUCER_STARTED,
        EventKind::SourceRegistered => FT_EVENT_SOURCE_REGISTERED,
        EventKind::SourceUpdated => FT_EVENT_SOURCE_UPDATED,
        EventKind::TrackRegistered => FT_EVENT_TRACK_REGISTERED,
        EventKind::TrackUpdated => FT_EVENT_TRACK_UPDATED,
        EventKind::SourceUnregistered => FT_EVENT_SOURCE_UNREGISTERED,
        EventKind::ProducerStopped => FT_EVENT_PRODUCER_STOPPED,
    }
}

fn pixel_format_from_ffi(format: u32) -> Option<PixelFormat> {
    match format {
        FT_PIXEL_FORMAT_UNKNOWN => Some(PixelFormat::Unknown),
        FT_PIXEL_FORMAT_BGRA8_UNORM => Some(PixelFormat::Bgra8Unorm),
        FT_PIXEL_FORMAT_RGBA8_UNORM => Some(PixelFormat::Rgba8Unorm),
        _ => None,
    }
}

fn pixel_format_to_ffi(format: PixelFormat) -> u32 {
    match format {
        PixelFormat::Unknown => FT_PIXEL_FORMAT_UNKNOWN,
        PixelFormat::Bgra8Unorm => FT_PIXEL_FORMAT_BGRA8_UNORM,
        PixelFormat::Rgba8Unorm => FT_PIXEL_FORMAT_RGBA8_UNORM,
    }
}

fn clock_domain_from_ffi(domain: u32) -> Option<ClockDomain> {
    match domain {
        FT_CLOCK_DOMAIN_UNKNOWN => Some(ClockDomain::Unknown),
        FT_CLOCK_DOMAIN_UNIX_TIME => Some(ClockDomain::UnixTime),
        FT_CLOCK_DOMAIN_MEDIA_TIME => Some(ClockDomain::MediaTime),
        FT_CLOCK_DOMAIN_HOST_TIME => Some(ClockDomain::HostTime),
        _ => None,
    }
}

fn clock_domain_to_ffi(domain: ClockDomain) -> u32 {
    match domain {
        ClockDomain::Unknown => FT_CLOCK_DOMAIN_UNKNOWN,
        ClockDomain::UnixTime => FT_CLOCK_DOMAIN_UNIX_TIME,
        ClockDomain::MediaTime => FT_CLOCK_DOMAIN_MEDIA_TIME,
        ClockDomain::HostTime => FT_CLOCK_DOMAIN_HOST_TIME,
    }
}

fn color_space_from_ffi(color_space: u32) -> Option<ColorSpace> {
    match color_space {
        FT_COLOR_SPACE_UNKNOWN => Some(ColorSpace::Unknown),
        FT_COLOR_SPACE_SRGB => Some(ColorSpace::Srgb),
        _ => None,
    }
}

fn color_space_to_ffi(color_space: ColorSpace) -> u32 {
    match color_space {
        ColorSpace::Unknown => FT_COLOR_SPACE_UNKNOWN,
        ColorSpace::Srgb => FT_COLOR_SPACE_SRGB,
    }
}

fn sync_kind_from_ffi(sync_kind: u32) -> Option<FrameSyncKind> {
    match sync_kind {
        FT_FRAME_SYNC_UNKNOWN => Some(FrameSyncKind::Unknown),
        FT_FRAME_SYNC_CPU_COPY_COMPLETE => Some(FrameSyncKind::CpuCopyComplete),
        FT_FRAME_SYNC_SCK_SAMPLE_READY => Some(FrameSyncKind::SckSampleReady),
        FT_FRAME_SYNC_NATIVE_TIMELINE => Some(FrameSyncKind::NativeTimeline),
        _ => None,
    }
}

fn sync_kind_to_ffi(sync_kind: FrameSyncKind) -> u32 {
    match sync_kind {
        FrameSyncKind::Unknown => FT_FRAME_SYNC_UNKNOWN,
        FrameSyncKind::CpuCopyComplete => FT_FRAME_SYNC_CPU_COPY_COMPLETE,
        FrameSyncKind::SckSampleReady => FT_FRAME_SYNC_SCK_SAMPLE_READY,
        FrameSyncKind::NativeTimeline => FT_FRAME_SYNC_NATIVE_TIMELINE,
    }
}

fn damage_kind_from_ffi(damage_kind: u32) -> Option<DamageKind> {
    match damage_kind {
        FT_DAMAGE_UNKNOWN => Some(DamageKind::Unknown),
        FT_DAMAGE_FULL_FRAME => Some(DamageKind::FullFrame),
        FT_DAMAGE_NONE => Some(DamageKind::None),
        FT_DAMAGE_INLINE_RECTS => Some(DamageKind::InlineRects),
        FT_DAMAGE_SIDECAR_RECTS => Some(DamageKind::SidecarRects),
        _ => None,
    }
}

fn damage_kind_to_ffi(damage_kind: DamageKind) -> u32 {
    match damage_kind {
        DamageKind::Unknown => FT_DAMAGE_UNKNOWN,
        DamageKind::FullFrame => FT_DAMAGE_FULL_FRAME,
        DamageKind::None => FT_DAMAGE_NONE,
        DamageKind::InlineRects => FT_DAMAGE_INLINE_RECTS,
        DamageKind::SidecarRects => FT_DAMAGE_SIDECAR_RECTS,
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::CString, ptr};

    use crate::ffi::{
        FT_CLOCK_DOMAIN_MEDIA_TIME, FT_COLOR_SPACE_UNKNOWN, FT_DAMAGE_FULL_FRAME, FT_EVENT_SOURCE_REGISTERED, FT_EVENT_TRACK_REGISTERED,
        FT_FRAME_SYNC_CPU_COPY_COMPLETE, FT_PIXEL_FORMAT_BGRA8_UNORM, FT_SOURCE_KIND_WINDOW, FT_STATUS_EMPTY, FT_STATUS_OK,
        FT_TRACK_TYPE_VIDEO, FtConsumer, FtConsumerOptions, FtEvent, FtProducer, FtProducerOptions, FtSourceDesc, FtTrackDesc,
        FtVideoFrame, FtVideoFrameDesc, FtVideoTrackDesc, ft_consumer_acquire_latest_video_frame, ft_consumer_connect, ft_consumer_destroy,
        ft_consumer_poll_event, ft_consumer_release_video_frame, ft_producer_create, ft_producer_destroy, ft_producer_publish_video_frame,
        ft_producer_register_source, ft_producer_register_track,
    };

    #[test]
    fn producer_consumer_smoke_through_c_abi() {
        let mut producer: *mut FtProducer = ptr::null_mut();
        let producer_options = FtProducerOptions { reserved: 0 };

        unsafe {
            assert_eq!(ft_producer_create(&producer_options, &mut producer), FT_STATUS_OK);
            assert!(!producer.is_null());

            let label = CString::new("Terminal").unwrap();
            let source_desc = FtSourceDesc {
                kind: FT_SOURCE_KIND_WINDOW,
                label: label.as_ptr(),
            };
            let mut source_id = 0;

            assert_eq!(ft_producer_register_source(producer, &source_desc, &mut source_id), FT_STATUS_OK);
            assert_eq!(source_id, 1);

            let track_desc = FtTrackDesc {
                track_type: FT_TRACK_TYPE_VIDEO,
                video: FtVideoTrackDesc {
                    width: 2,
                    height: 1,
                    pixel_format: FT_PIXEL_FORMAT_BGRA8_UNORM,
                },
            };
            let mut track_id = 0;

            assert_eq!(
                ft_producer_register_track(producer, source_id, &track_desc, &mut track_id),
                FT_STATUS_OK
            );
            assert_eq!(track_id, 1);

            let frame_desc = FtVideoFrameDesc {
                sequence: 1,
                timestamp_ns: 100,
                width: 2,
                height: 1,
                stride: 8,
                pixel_format: FT_PIXEL_FORMAT_BGRA8_UNORM,
                pool_id: 0,
                slot_id: 0,
                payload_offset: 0,
                payload_len: 0,
                payload_map_len: 0,
                clock_domain: FT_CLOCK_DOMAIN_MEDIA_TIME,
                color_space: FT_COLOR_SPACE_UNKNOWN,
                sync_kind: FT_FRAME_SYNC_CPU_COPY_COMPLETE,
                damage_kind: FT_DAMAGE_FULL_FRAME,
                damage_base_sequence: 1,
                dropped_before_publish: 0,
                producer_drop_count: 0,
                evicted_count: 0,
                consumer_skipped_count: 0,
                payload_kind: 0,
                modifier: 0,
                fence_id: 0,
                fence_value: 0,
                flags: 0,
            };
            let pixels = [1_u8, 2, 3, 4];

            assert_eq!(
                ft_producer_publish_video_frame(producer, track_id, &frame_desc, pixels.as_ptr().cast(), pixels.len()),
                FT_STATUS_OK
            );

            let mut consumer: *mut FtConsumer = ptr::null_mut();
            let consumer_options = FtConsumerOptions { producer };
            assert_eq!(ft_consumer_connect(&consumer_options, &mut consumer), FT_STATUS_OK);
            assert!(!consumer.is_null());

            let mut event = FtEvent::default();
            assert_eq!(ft_consumer_poll_event(consumer, &mut event), FT_STATUS_OK);
            assert_eq!(event.kind, FT_EVENT_SOURCE_REGISTERED);
            assert_eq!(event.source_id, source_id);

            assert_eq!(ft_consumer_poll_event(consumer, &mut event), FT_STATUS_OK);
            assert_eq!(event.kind, FT_EVENT_TRACK_REGISTERED);
            assert_eq!(event.source_id, source_id);
            assert_eq!(event.track_id, track_id);

            assert_eq!(ft_consumer_poll_event(consumer, &mut event), FT_STATUS_EMPTY);

            let mut frame = FtVideoFrame::default();
            assert_eq!(ft_consumer_acquire_latest_video_frame(consumer, track_id, &mut frame), FT_STATUS_OK);
            assert_eq!(frame.desc.sequence, 1);
            assert_eq!(frame.desc.clock_domain, FT_CLOCK_DOMAIN_MEDIA_TIME);
            assert_eq!(frame.desc.color_space, FT_COLOR_SPACE_UNKNOWN);
            assert_eq!(frame.desc.sync_kind, FT_FRAME_SYNC_CPU_COPY_COMPLETE);
            assert_eq!(frame.desc.damage_kind, FT_DAMAGE_FULL_FRAME);
            assert_eq!(frame.desc.damage_base_sequence, 1);
            assert_ne!(frame.desc.pool_id, 0);
            assert_eq!(frame.desc.slot_id, 0);
            assert_eq!(frame.desc.payload_len as usize, pixels.len());
            assert!(frame.desc.payload_offset + frame.desc.payload_len <= frame.desc.payload_map_len);
            assert_eq!(frame.len, pixels.len());
            assert!(!frame.data.is_null());

            ft_consumer_release_video_frame(consumer, &mut frame);
            assert!(frame.data.is_null());
            assert_eq!(frame.len, 0);

            ft_consumer_destroy(consumer);
            ft_producer_destroy(producer);
        }
    }
}
