use std::{
    cell::RefCell,
    ffi::{CStr, c_char, c_void},
    ptr,
    rc::Rc,
};

use crate::{
    model::{PixelFormat, SourceDesc, SourceId, SourceKind, TrackDesc, TrackId, VideoTrackDesc},
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

pub const FT_PIXEL_FORMAT_BGRA8_UNORM: u32 = 1;
pub const FT_PIXEL_FORMAT_RGBA8_UNORM: u32 = 2;

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
}

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
    handle: *mut AcquiredVideoFrame,
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
}

#[derive(Debug)]
pub struct FtProducer {
    inner: Rc<RefCell<ProducerInner>>,
}

#[derive(Debug)]
pub struct FtConsumer {
    inner: Rc<RefCell<ProducerInner>>,
    consumer_id: ConsumerId,
    event_cursor: usize,
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
            video: VideoSlotManager::new(3),
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
    let Some(producer) = producer_as_ref(producer) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
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
    let Some(producer) = producer_as_ref(producer) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
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
    let Some(producer) = producer_as_ref(producer) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
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
    let Some(producer) = producer_as_ref(producer) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };

    let consumer = Box::new(FtConsumer {
        inner: Rc::clone(&producer.inner),
        consumer_id: ConsumerId::new(1),
        event_cursor: 0,
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
    let Some(consumer) = consumer_as_mut(consumer) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if out_event.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }

    let events = consumer.inner.borrow().state.replay_events();
    let Some(event) = events.get(consumer.event_cursor) else {
        return FT_STATUS_EMPTY;
    };
    consumer.event_cursor += 1;

    // SAFETY: out_event was checked for null and points to caller-owned storage.
    unsafe {
        *out_event = event_to_ffi(event);
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
    let Some(consumer) = consumer_as_mut(consumer) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if out_frame.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }

    match consumer
        .inner
        .borrow_mut()
        .video
        .acquire_latest(consumer.consumer_id, TrackId::new(track_id))
    {
        Ok(frame) => {
            let desc = video_frame_desc_to_ffi(&frame.desc);
            let data = frame.bytes().as_ptr().cast::<c_void>();
            let len = frame.bytes().len();
            let handle = Box::into_raw(Box::new(frame));
            // SAFETY: out_frame was checked for null and points to caller-owned storage.
            unsafe {
                *out_frame = FtVideoFrame { desc, data, len, handle };
            }
            FT_STATUS_OK
        }
        Err(_) => FT_STATUS_ERROR,
    }
}

/// # Safety
///
/// `consumer` must be a live pointer returned by `ft_consumer_connect`.
/// `frame` must be null or a pointer previously filled by
/// `ft_consumer_acquire_latest_video_frame`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_consumer_release_video_frame(consumer: *mut FtConsumer, frame: *mut FtVideoFrame) {
    let Some(consumer) = consumer_as_mut(consumer) else {
        return;
    };
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
    consumer.inner.borrow_mut().video.release(acquired);
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
        consumer.inner.borrow_mut().video.disconnect_consumer(consumer.consumer_id);
    }
}

fn producer_as_ref(producer: *mut FtProducer) -> Option<&'static FtProducer> {
    if producer.is_null() {
        None
    } else {
        // SAFETY: caller-provided pointer is assumed to come from ft_producer_create.
        Some(unsafe { &*producer })
    }
}

fn consumer_as_mut(consumer: *mut FtConsumer) -> Option<&'static mut FtConsumer> {
    if consumer.is_null() {
        None
    } else {
        // SAFETY: caller-provided pointer is assumed to come from ft_consumer_connect.
        Some(unsafe { &mut *consumer })
    }
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
        FT_PIXEL_FORMAT_BGRA8_UNORM => Some(PixelFormat::Bgra8Unorm),
        FT_PIXEL_FORMAT_RGBA8_UNORM => Some(PixelFormat::Rgba8Unorm),
        _ => None,
    }
}

fn pixel_format_to_ffi(format: PixelFormat) -> u32 {
    match format {
        PixelFormat::Bgra8Unorm => FT_PIXEL_FORMAT_BGRA8_UNORM,
        PixelFormat::Rgba8Unorm => FT_PIXEL_FORMAT_RGBA8_UNORM,
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::CString, ptr};

    use crate::ffi::{
        FT_EVENT_SOURCE_REGISTERED, FT_EVENT_TRACK_REGISTERED, FT_PIXEL_FORMAT_BGRA8_UNORM, FT_SOURCE_KIND_WINDOW, FT_STATUS_EMPTY,
        FT_STATUS_OK, FT_TRACK_TYPE_VIDEO, FtConsumer, FtConsumerOptions, FtEvent, FtProducer, FtProducerOptions, FtSourceDesc,
        FtTrackDesc, FtVideoFrame, FtVideoFrameDesc, FtVideoTrackDesc, ft_consumer_acquire_latest_video_frame, ft_consumer_connect,
        ft_consumer_destroy, ft_consumer_poll_event, ft_consumer_release_video_frame, ft_producer_create, ft_producer_destroy,
        ft_producer_publish_video_frame, ft_producer_register_source, ft_producer_register_track,
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
