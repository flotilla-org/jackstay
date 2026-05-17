use std::{
    collections::BTreeMap,
    ffi::CString,
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    rc::Rc,
};

use serde::Deserialize;

use crate::{
    control_page::VideoTrackControlPage,
    error::{CaptureTransferError, Result},
    fdpass,
    model::{ClockDomain, ColorSpace, DamageKind, FrameSyncKind, PixelFormat},
    transfer_channel::{CaptureTransferMessage, CaptureTransferRequest},
    video::VideoFrameDesc,
};

#[derive(Debug, Clone)]
pub struct SyntheticSession {
    pub session_id: String,
    pub source_id: u64,
    pub track_id: u64,
    pub fd_socket_path: String,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub source_id: u64,
    pub track_id: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: PixelFormat,
    pub fd_socket_path: String,
}

#[derive(Debug)]
pub struct DaemonFrame {
    pub desc: VideoFrameDesc,
    pub producer_cursor: u64,
    pub len: usize,
    lease_id: u64,
    map_len: usize,
    payload_offset: usize,
    ptr: *mut libc::c_void,
    pool: Option<Rc<RegisteredPoolMapping>>,
}

impl DaemonFrame {
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: ptr is a live read-only mapping of map_len bytes until Drop,
        // and payload_offset/len were validated against that mapping.
        unsafe { std::slice::from_raw_parts(self.ptr.cast::<u8>().add(self.payload_offset), self.len) }
    }
}

impl Drop for DaemonFrame {
    fn drop(&mut self) {
        if self.pool.is_none() {
            // SAFETY: ptr/len describe the mapping created for this immutable frame.
            unsafe {
                libc::munmap(self.ptr, self.map_len);
            }
        }
    }
}

#[derive(Debug)]
struct RegisteredPoolMapping {
    ptr: *mut libc::c_void,
    map_len: usize,
}

impl Drop for RegisteredPoolMapping {
    fn drop(&mut self) {
        // SAFETY: ptr/len describe the mapping created for this registered pool.
        unsafe {
            libc::munmap(self.ptr, self.map_len);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PoolKey {
    track_id: u64,
    pool_id: u64,
    pool_generation: u64,
}

#[derive(Debug)]
pub struct DaemonConsumer {
    info: SessionInfo,
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    // TODO: evict retired pool generations once the capture transfer channel grows pool-retirement messages.
    pools: BTreeMap<PoolKey, Rc<RegisteredPoolMapping>>,
    control_pages: BTreeMap<u64, VideoTrackControlPage>,
}

#[derive(Debug, Deserialize)]
struct SyntheticSessionWire {
    session_id: String,
    source_id: u64,
    track_id: u64,
    fd_socket_path: String,
}

#[derive(Debug, Deserialize)]
struct SessionInfoWire {
    session_id: String,
    source_id: u64,
    track_id: u64,
    width: u32,
    height: u32,
    stride: u32,
    pixel_format: String,
    fd_socket_path: String,
}

#[derive(Debug)]
struct LatestFrameWire {
    track_id: u64,
    sequence: u64,
    lease_id: u64,
    producer_cursor: u64,
    timestamp_ns: u64,
    width: u32,
    height: u32,
    stride: u32,
    pixel_format: String,
    pool_id: u64,
    slot_id: u64,
    slot_generation: u64,
    payload_offset: u64,
    payload_len: u64,
    payload_map_len: u64,
    clock_domain: String,
    color_space: String,
    sync_kind: String,
    damage_kind: String,
    damage_base_sequence: u64,
    dropped_before_publish: u64,
    producer_drop_count: u64,
    evicted_count: u64,
    consumer_skipped_count: u64,
    len: u64,
}

#[derive(Debug)]
struct RegisteredCpuPoolWire {
    track_id: u64,
    pool_id: u64,
    pool_generation: u64,
    payload_map_len: u64,
}

#[derive(Debug)]
struct FrameMapping {
    desc: VideoFrameDesc,
    lease_id: u64,
    producer_cursor: u64,
    payload_offset: usize,
    payload_len: usize,
    payload_map_len: usize,
}

impl DaemonConsumer {
    pub fn connect(info: SessionInfo) -> Result<Self> {
        let stream = UnixStream::connect(&info.fd_socket_path).map_err(|error| daemon_error("connect-fd-socket", error))?;
        let reader_stream = stream.try_clone().map_err(|error| daemon_error("clone-fd-stream", error))?;
        Ok(Self {
            info,
            stream,
            reader: BufReader::new(reader_stream),
            pools: BTreeMap::new(),
            control_pages: BTreeMap::new(),
        })
    }

    pub fn latest_frame(&mut self, track_id: u64) -> Result<DaemonFrame> {
        let request = CaptureTransferRequest::LatestVideoFrame {
            session_id: self.info.session_id.clone(),
            track_id,
        };
        self.write_capture_transfer_request(&request)?;
        self.stream.flush().map_err(|error| daemon_error("flush-latest-request", error))?;

        loop {
            let message = self.read_capture_transfer_message()?;
            // The connection is already scoped to self.info.session_id; session_id
            // remains on the wire for diagnostics and future multiplexing.
            match message {
                CaptureTransferMessage::RegisterVideoControlPage {
                    session_id: _,
                    track_id,
                    map_len,
                } => {
                    let map_len = usize::try_from(map_len).map_err(|_| CaptureTransferError::DaemonTransport {
                        operation: "mmap-control-page",
                        message: "control page map length does not fit usize".to_string(),
                    })?;
                    let fd = fdpass::recv_fd(&self.stream)?;
                    let page = VideoTrackControlPage::map_read_only(fd, map_len)?;
                    page.validate_header()?;
                    self.control_pages.insert(track_id, page);
                }
                CaptureTransferMessage::RegisterCpuPool {
                    session_id: _,
                    track_id,
                    pool_id,
                    pool_generation,
                    payload_map_len,
                    ..
                } => {
                    let pool = RegisteredCpuPoolWire {
                        track_id,
                        pool_id,
                        pool_generation,
                        payload_map_len,
                    };
                    let fd = fdpass::recv_fd(&self.stream)?;
                    self.register_cpu_pool(pool, fd)?;
                }
                CaptureTransferMessage::VideoFrame {
                    session_id: _,
                    track_id,
                    lease_id,
                    producer_cursor,
                    sequence,
                    timestamp_ns,
                    width,
                    height,
                    stride,
                    pixel_format,
                    pool_id,
                    slot_id,
                    slot_generation,
                    payload_offset,
                    payload_len,
                    payload_map_len,
                    clock_domain,
                    color_space,
                    sync_kind,
                    damage_kind,
                    damage_base_sequence,
                    dropped_before_publish,
                    producer_drop_count,
                    evicted_count,
                    consumer_skipped_count,
                    len,
                } => {
                    let frame = LatestFrameWire {
                        track_id,
                        sequence,
                        lease_id,
                        producer_cursor,
                        timestamp_ns,
                        width,
                        height,
                        stride,
                        pixel_format,
                        pool_id,
                        slot_id,
                        slot_generation,
                        payload_offset,
                        payload_len,
                        payload_map_len,
                        clock_domain,
                        color_space,
                        sync_kind,
                        damage_kind,
                        damage_base_sequence,
                        dropped_before_publish,
                        producer_drop_count,
                        evicted_count,
                        consumer_skipped_count,
                        len,
                    };
                    self.compare_control_page_shadow(&frame)?;
                    return self.daemon_frame_from_wire(frame);
                }
            }
        }
    }

    pub fn release_frame(&mut self, frame: DaemonFrame) -> Result<()> {
        let lease_id = frame.lease_id;
        drop(frame);
        self.write_capture_transfer_request(&CaptureTransferRequest::ReleaseVideoFrame { lease_id })?;
        self.stream.flush().map_err(|error| daemon_error("flush-release-request", error))
    }

    fn write_capture_transfer_request(&mut self, request: &CaptureTransferRequest) -> Result<()> {
        let line = serde_json::to_string(request).map_err(|error| daemon_error("encode-capture-transfer-request", error))?;
        writeln!(self.stream, "{line}").map_err(|error| daemon_error("write-capture-transfer-request", error))
    }

    fn read_capture_transfer_message(&mut self) -> Result<CaptureTransferMessage> {
        let mut line = String::new();
        let bytes_read = self
            .reader
            .read_line(&mut line)
            .map_err(|error| daemon_error("read-capture-transfer-channel", error))?;
        if bytes_read == 0 {
            return Err(CaptureTransferError::DaemonTransport {
                operation: "read-capture-transfer-channel",
                message: "daemon closed capture transfer channel".to_string(),
            });
        }
        serde_json::from_str(line.trim_end()).map_err(|error| daemon_error("parse-capture-transfer-message", error))
    }

    fn register_cpu_pool(&mut self, pool: RegisteredCpuPoolWire, fd: OwnedFd) -> Result<()> {
        let map_len = usize::try_from(pool.payload_map_len).map_err(|_| CaptureTransferError::DaemonTransport {
            operation: "mmap-pool",
            message: "pool map length does not fit usize".to_string(),
        })?;
        if map_len == 0 {
            return Err(CaptureTransferError::DaemonTransport {
                operation: "mmap-pool",
                message: "pool map length is zero".to_string(),
            });
        }
        let ptr = mmap_fd(fd.as_raw_fd(), map_len, "mmap-pool")?;
        self.pools.insert(
            PoolKey {
                track_id: pool.track_id,
                pool_id: pool.pool_id,
                pool_generation: pool.pool_generation,
            },
            Rc::new(RegisteredPoolMapping { ptr, map_len }),
        );
        Ok(())
    }

    fn daemon_frame_from_wire(&mut self, frame: LatestFrameWire) -> Result<DaemonFrame> {
        if frame.pool_id == 0 {
            let fd = fdpass::recv_fd(&self.stream)?;
            daemon_frame_from_immutable_fd(fd, frame)
        } else {
            // The current wire shape reuses slot_generation as the pool generation
            // for reusable CPU pools. Slot-level generation is implicit in the
            // frame sequence until the ring protocol grows explicit slot cursors.
            let key = PoolKey {
                track_id: frame.track_id,
                pool_id: frame.pool_id,
                pool_generation: frame.slot_generation,
            };
            let pool = self.pools.get(&key).cloned().ok_or_else(|| CaptureTransferError::DaemonTransport {
                operation: "resolve-frame-pool",
                message: format!(
                    "frame references unregistered pool track={} pool={} generation={}",
                    key.track_id, key.pool_id, key.pool_generation
                ),
            })?;
            daemon_frame_from_registered_pool(pool, frame)
        }
    }

    fn compare_control_page_shadow(&self, frame: &LatestFrameWire) -> Result<()> {
        let Some(page) = self.control_pages.get(&frame.track_id) else {
            return Ok(());
        };
        let entry = page.shadow_read_entry_for_cursor(frame.producer_cursor)?;
        if entry.producer_cursor != frame.producer_cursor
            || entry.sequence != frame.sequence
            || entry.pool_id != frame.pool_id
            || entry.slot_id != frame.slot_id
            || entry.slot_generation != frame.slot_generation
            || entry.payload_offset != frame.payload_offset
            || entry.payload_len != frame.payload_len
        {
            return Err(CaptureTransferError::DaemonTransport {
                operation: "control-page-shadow",
                message: format!(
                    "control page entry does not match frame metadata for track={} cursor={}",
                    frame.track_id, frame.producer_cursor
                ),
            });
        }
        Ok(())
    }
}

pub fn create_synthetic_session(control_socket_path: &str) -> Result<SyntheticSession> {
    let body = http_request(control_socket_path, "POST", "/capture-sessions/synthetic")?;
    let wire: SyntheticSessionWire = serde_json::from_str(&body).map_err(|error| daemon_error("parse-create-synthetic", error))?;
    Ok(SyntheticSession {
        session_id: wire.session_id,
        source_id: wire.source_id,
        track_id: wire.track_id,
        fd_socket_path: wire.fd_socket_path,
    })
}

pub fn get_session(control_socket_path: &str, session_id: &str) -> Result<SessionInfo> {
    let path = format!("/capture-sessions/{session_id}");
    let body = http_request(control_socket_path, "GET", &path)?;
    let wire: SessionInfoWire = serde_json::from_str(&body).map_err(|error| daemon_error("parse-session", error))?;
    Ok(SessionInfo {
        session_id: wire.session_id,
        source_id: wire.source_id,
        track_id: wire.track_id,
        width: wire.width,
        height: wire.height,
        stride: wire.stride,
        pixel_format: parse_pixel_format(&wire.pixel_format)?,
        fd_socket_path: wire.fd_socket_path,
    })
}

fn daemon_frame_from_immutable_fd(fd: OwnedFd, frame: LatestFrameWire) -> Result<DaemonFrame> {
    let mapping = parse_frame_metadata(&frame)?;
    let ptr = mmap_fd(fd.as_raw_fd(), mapping.payload_map_len, "mmap-frame")?;
    Ok(daemon_frame_from_mapping(mapping, ptr, None))
}

fn daemon_frame_from_registered_pool(pool: Rc<RegisteredPoolMapping>, frame: LatestFrameWire) -> Result<DaemonFrame> {
    let mapping = parse_frame_metadata(&frame)?;
    if mapping.payload_map_len != pool.map_len {
        return Err(CaptureTransferError::DaemonTransport {
            operation: "resolve-frame-pool",
            message: "frame map length does not match registered pool".to_string(),
        });
    }
    Ok(daemon_frame_from_mapping(mapping, pool.ptr, Some(pool)))
}

fn parse_frame_metadata(frame: &LatestFrameWire) -> Result<FrameMapping> {
    let pixel_format = parse_pixel_format(&frame.pixel_format)?;
    let clock_domain = parse_clock_domain(&frame.clock_domain)?;
    let color_space = parse_color_space(&frame.color_space)?;
    let sync_kind = parse_sync_kind(&frame.sync_kind)?;
    let damage_kind = parse_damage_kind(&frame.damage_kind)?;
    let payload_offset = usize::try_from(frame.payload_offset).map_err(|_| CaptureTransferError::DaemonTransport {
        operation: "mmap-frame",
        message: "payload offset does not fit usize".to_string(),
    })?;
    let payload_len = usize::try_from(frame.payload_len).map_err(|_| CaptureTransferError::DaemonTransport {
        operation: "mmap-frame",
        message: "payload length does not fit usize".to_string(),
    })?;
    let payload_map_len = usize::try_from(frame.payload_map_len).map_err(|_| CaptureTransferError::DaemonTransport {
        operation: "mmap-frame",
        message: "payload map length does not fit usize".to_string(),
    })?;
    if payload_len == 0 || frame.len != frame.payload_len {
        return Err(CaptureTransferError::DaemonTransport {
            operation: "mmap-frame",
            message: "frame length is zero".to_string(),
        });
    }
    if payload_offset > payload_map_len || payload_len > payload_map_len - payload_offset {
        return Err(CaptureTransferError::DaemonTransport {
            operation: "mmap-frame",
            message: "payload range exceeds mapped length".to_string(),
        });
    }
    Ok(FrameMapping {
        desc: VideoFrameDesc {
            sequence: frame.sequence,
            timestamp_ns: frame.timestamp_ns,
            width: frame.width,
            height: frame.height,
            stride: frame.stride,
            pixel_format,
            pool_id: frame.pool_id,
            slot_id: frame.slot_id,
            slot_generation: frame.slot_generation,
            payload_offset: frame.payload_offset,
            payload_len: frame.payload_len,
            payload_map_len: frame.payload_map_len,
            clock_domain,
            color_space,
            sync_kind,
            damage_kind,
            damage_base_sequence: frame.damage_base_sequence,
            dropped_before_publish: frame.dropped_before_publish,
            producer_drop_count: frame.producer_drop_count,
            evicted_count: frame.evicted_count,
            consumer_skipped_count: frame.consumer_skipped_count,
        },
        lease_id: frame.lease_id,
        producer_cursor: frame.producer_cursor,
        payload_offset,
        payload_len,
        payload_map_len,
    })
}

fn daemon_frame_from_mapping(mapping: FrameMapping, ptr: *mut libc::c_void, pool: Option<Rc<RegisteredPoolMapping>>) -> DaemonFrame {
    DaemonFrame {
        desc: mapping.desc,
        producer_cursor: mapping.producer_cursor,
        len: mapping.payload_len,
        lease_id: mapping.lease_id,
        map_len: mapping.payload_map_len,
        payload_offset: mapping.payload_offset,
        ptr,
        pool,
    }
}

fn mmap_fd(fd: std::os::fd::RawFd, map_len: usize, operation: &'static str) -> Result<*mut libc::c_void> {
    // SAFETY: fd is valid, map length is supplied by daemon metadata, and mapping is read-only.
    // A successful mmap is independent of fd lifetime, so callers may close the fd afterwards.
    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), map_len, libc::PROT_READ, libc::MAP_SHARED, fd, 0) };
    if ptr == libc::MAP_FAILED {
        return Err(CaptureTransferError::DaemonTransport {
            operation,
            message: std::io::Error::last_os_error().to_string(),
        });
    }
    Ok(ptr)
}

/// # Safety
///
/// `out` must point to writable storage of `len` bytes.
pub unsafe fn copy_string_to_c_buffer(value: &str, out: *mut libc::c_char, len: usize) -> bool {
    if out.is_null() || len == 0 {
        return false;
    }
    let Ok(c_string) = CString::new(value) else {
        return false;
    };
    let bytes = c_string.as_bytes_with_nul();
    if bytes.len() > len {
        return false;
    }
    // SAFETY: guaranteed by the caller.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<libc::c_char>(), out, bytes.len());
    }
    true
}

fn http_request(control_socket_path: &str, method: &str, path: &str) -> Result<String> {
    let mut stream = UnixStream::connect(control_socket_path).map_err(|error| daemon_error("connect-control-socket", error))?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: porthole\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .map_err(|error| daemon_error("write-http-request", error))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| daemon_error("read-http-response", error))?;
    parse_http_response(&response)
}

fn parse_http_response(response: &str) -> Result<String> {
    let Some((headers, body)) = response.split_once("\r\n\r\n") else {
        return Err(CaptureTransferError::DaemonTransport {
            operation: "parse-http",
            message: "missing header/body separator".to_string(),
        });
    };
    let Some(status_line) = headers.lines().next() else {
        return Err(CaptureTransferError::DaemonTransport {
            operation: "parse-http",
            message: "missing status line".to_string(),
        });
    };
    let ok_status = status_line.split_whitespace().nth(1).map(|status| status == "200").unwrap_or(false);
    if !ok_status {
        return Err(CaptureTransferError::DaemonTransport {
            operation: "http-status",
            message: status_line.to_string(),
        });
    }
    Ok(body.to_string())
}

fn parse_pixel_format(value: &str) -> Result<PixelFormat> {
    match value {
        "bgra8_unorm" => Ok(PixelFormat::Bgra8Unorm),
        "rgba8_unorm" => Ok(PixelFormat::Rgba8Unorm),
        other => Err(CaptureTransferError::DaemonTransport {
            operation: "parse-pixel-format",
            message: other.to_string(),
        }),
    }
}

fn parse_clock_domain(value: &str) -> Result<ClockDomain> {
    match value {
        "unknown" => Ok(ClockDomain::Unknown),
        "unix_time" => Ok(ClockDomain::UnixTime),
        "media_time" => Ok(ClockDomain::MediaTime),
        "host_time" => Ok(ClockDomain::HostTime),
        other => Err(CaptureTransferError::DaemonTransport {
            operation: "parse-clock-domain",
            message: other.to_string(),
        }),
    }
}

fn parse_color_space(value: &str) -> Result<ColorSpace> {
    match value {
        "unknown" => Ok(ColorSpace::Unknown),
        "srgb" => Ok(ColorSpace::Srgb),
        other => Err(CaptureTransferError::DaemonTransport {
            operation: "parse-color-space",
            message: other.to_string(),
        }),
    }
}

fn parse_sync_kind(value: &str) -> Result<FrameSyncKind> {
    match value {
        "unknown" => Ok(FrameSyncKind::Unknown),
        "cpu_copy_complete" => Ok(FrameSyncKind::CpuCopyComplete),
        "sck_sample_ready" => Ok(FrameSyncKind::SckSampleReady),
        "native_timeline" => Ok(FrameSyncKind::NativeTimeline),
        other => Err(CaptureTransferError::DaemonTransport {
            operation: "parse-sync-kind",
            message: other.to_string(),
        }),
    }
}

fn parse_damage_kind(value: &str) -> Result<DamageKind> {
    match value {
        "unknown" => Ok(DamageKind::Unknown),
        "full_frame" => Ok(DamageKind::FullFrame),
        "none" => Ok(DamageKind::None),
        "inline_rects" => Ok(DamageKind::InlineRects),
        "sidecar_rects" => Ok(DamageKind::SidecarRects),
        other => Err(CaptureTransferError::DaemonTransport {
            operation: "parse-damage-kind",
            message: other.to_string(),
        }),
    }
}

fn daemon_error(operation: &'static str, error: impl std::fmt::Display) -> CaptureTransferError {
    CaptureTransferError::DaemonTransport {
        operation,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::File,
        io::{BufRead, BufReader, Seek, SeekFrom, Write},
        os::{fd::AsRawFd, unix::net::UnixListener},
        thread,
    };

    use super::{DaemonConsumer, SessionInfo, parse_http_response};
    use crate::{
        control_page::{PendingVideoRingEntry, VideoTrackControlPage},
        fdpass,
        model::PixelFormat,
    };

    #[test]
    fn parses_http_body() {
        let body = parse_http_response("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}").unwrap();
        assert_eq!(body, "{}");
    }

    #[test]
    fn rejects_non_200_status_without_substring_match() {
        let error = parse_http_response("HTTP/1.1 1200 Weird\r\ncontent-length: 2\r\n\r\n{}").unwrap_err();
        assert!(error.to_string().contains("1200 Weird"));
    }

    #[test]
    fn daemon_consumer_reuses_connection_and_releases_by_lease_id() {
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("capture-fd.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());

            assert_latest_request(&mut reader, "session-1", 7);
            let first_file = tempfile_file_with_contents(b"abcd");
            writeln!(stream.try_clone().unwrap(), "{}", latest_frame_json(11, 1, 4)).unwrap();
            fdpass::send_fd(&stream, first_file.as_raw_fd()).unwrap();

            assert_release_request(&mut reader, 11);

            assert_latest_request(&mut reader, "session-1", 7);
            let second_file = tempfile_file_with_contents(b"wxyz");
            writeln!(stream.try_clone().unwrap(), "{}", latest_frame_json(12, 2, 4)).unwrap();
            fdpass::send_fd(&stream, second_file.as_raw_fd()).unwrap();

            assert_release_request(&mut reader, 12);
        });

        let info = SessionInfo {
            session_id: "session-1".to_string(),
            source_id: 1,
            track_id: 7,
            width: 2,
            height: 1,
            stride: 8,
            pixel_format: PixelFormat::Bgra8Unorm,
            fd_socket_path: socket_path.to_string_lossy().into_owned(),
        };
        let mut consumer = DaemonConsumer::connect(info).unwrap();

        let first = consumer.latest_frame(7).unwrap();
        assert_eq!(first.desc.sequence, 1);
        assert_eq!(first.producer_cursor, 1);
        assert_eq!(first.bytes(), b"abcd");
        consumer.release_frame(first).unwrap();

        let second = consumer.latest_frame(7).unwrap();
        assert_eq!(second.desc.sequence, 2);
        assert_eq!(second.producer_cursor, 2);
        assert_eq!(second.bytes(), b"wxyz");
        consumer.release_frame(second).unwrap();

        server.join().unwrap();
    }

    #[test]
    fn daemon_consumer_caches_registered_cpu_pool_frames() {
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("capture-fd.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());

            assert_latest_request(&mut reader, "session-1", 7);
            let pool_file = tempfile_file_with_contents(b"abcdwxyz");
            writeln!(stream.try_clone().unwrap(), "{}", register_pool_json(7, 1, 1, 8, 4, 2)).unwrap();
            fdpass::send_fd(&stream, pool_file.as_raw_fd()).unwrap();
            writeln!(
                stream.try_clone().unwrap(),
                "{}",
                latest_frame_json_with_offset(11, 1, 1, 1, 0, 4, 8)
            )
            .unwrap();

            assert_release_request(&mut reader, 11);

            assert_latest_request(&mut reader, "session-1", 7);
            writeln!(
                stream.try_clone().unwrap(),
                "{}",
                latest_frame_json_with_offset(12, 2, 1, 1, 4, 4, 8)
            )
            .unwrap();

            assert_release_request(&mut reader, 12);
        });

        let info = SessionInfo {
            session_id: "session-1".to_string(),
            source_id: 1,
            track_id: 7,
            width: 2,
            height: 1,
            stride: 8,
            pixel_format: PixelFormat::Bgra8Unorm,
            fd_socket_path: socket_path.to_string_lossy().into_owned(),
        };
        let mut consumer = DaemonConsumer::connect(info).unwrap();

        let first = consumer.latest_frame(7).unwrap();
        assert_eq!(first.desc.sequence, 1);
        assert_eq!(first.producer_cursor, 1);
        assert_eq!(first.bytes(), b"abcd");
        consumer.release_frame(first).unwrap();

        let second = consumer.latest_frame(7).unwrap();
        assert_eq!(second.desc.sequence, 2);
        assert_eq!(second.producer_cursor, 2);
        assert_eq!(second.bytes(), b"wxyz");
        consumer.release_frame(second).unwrap();

        server.join().unwrap();
    }

    #[test]
    fn daemon_consumer_maps_control_page_shadow() {
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("capture-fd.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());

            assert_latest_request(&mut reader, "session-1", 7);
            let mut control_page = VideoTrackControlPage::new(2);
            control_page.push(PendingVideoRingEntry {
                sequence: 1,
                frame_key: 1,
                pool_id: 1,
                slot_id: 0,
                slot_generation: 1,
                payload_offset: 0,
                payload_len: 4,
            });
            let control_fd = control_page.try_clone_fd().unwrap();
            writeln!(
                stream.try_clone().unwrap(),
                "{}",
                register_control_page_json(7, control_page.mapped_len() as u64)
            )
            .unwrap();
            fdpass::send_fd(&stream, control_fd.as_raw_fd()).unwrap();

            let pool_file = tempfile_file_with_contents(b"abcd");
            writeln!(stream.try_clone().unwrap(), "{}", register_pool_json(7, 1, 1, 4, 4, 1)).unwrap();
            fdpass::send_fd(&stream, pool_file.as_raw_fd()).unwrap();
            writeln!(
                stream.try_clone().unwrap(),
                "{}",
                latest_frame_json_with_offset(11, 1, 1, 1, 0, 4, 4)
            )
            .unwrap();

            assert_release_request(&mut reader, 11);
        });

        let info = SessionInfo {
            session_id: "session-1".to_string(),
            source_id: 1,
            track_id: 7,
            width: 2,
            height: 1,
            stride: 8,
            pixel_format: PixelFormat::Bgra8Unorm,
            fd_socket_path: socket_path.to_string_lossy().into_owned(),
        };
        let mut consumer = DaemonConsumer::connect(info).unwrap();

        let frame = consumer.latest_frame(7).unwrap();
        assert_eq!(frame.desc.sequence, 1);
        assert_eq!(frame.producer_cursor, 1);
        assert_eq!(frame.bytes(), b"abcd");
        consumer.release_frame(frame).unwrap();

        server.join().unwrap();
    }

    fn assert_latest_request(reader: &mut BufReader<std::os::unix::net::UnixStream>, session_id: &str, track_id: u64) {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let request: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(request["op"], "latest_video_frame");
        assert_eq!(request["session_id"], session_id);
        assert_eq!(request["track_id"], track_id);
    }

    fn assert_release_request(reader: &mut BufReader<std::os::unix::net::UnixStream>, lease_id: u64) {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let request: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(request["op"], "release_video_frame");
        assert_eq!(request["lease_id"], lease_id);
    }

    fn latest_frame_json(lease_id: u64, sequence: u64, payload_len: u64) -> serde_json::Value {
        latest_frame_json_with_offset(lease_id, sequence, 0, 0, 0, payload_len, payload_len)
    }

    fn latest_frame_json_with_offset(
        lease_id: u64,
        sequence: u64,
        pool_id: u64,
        slot_generation: u64,
        payload_offset: u64,
        payload_len: u64,
        payload_map_len: u64,
    ) -> serde_json::Value {
        serde_json::json!({
            "op": "video_frame",
            "session_id": "session-1",
            "track_id": 7,
            "sequence": sequence,
            "producer_cursor": sequence,
            "lease_id": lease_id,
            "timestamp_ns": 100,
            "width": 2,
            "height": 1,
            "stride": 8,
            "pixel_format": "bgra8_unorm",
            "pool_id": pool_id,
            "slot_id": 0,
            "slot_generation": slot_generation,
            "payload_offset": payload_offset,
            "payload_len": payload_len,
            "payload_map_len": payload_map_len,
            "clock_domain": "media_time",
            "color_space": "srgb",
            "sync_kind": "cpu_copy_complete",
            "damage_kind": "full_frame",
            "damage_base_sequence": sequence,
            "dropped_before_publish": 0,
            "producer_drop_count": 0,
            "evicted_count": 0,
            "consumer_skipped_count": 0,
            "len": payload_len
        })
    }

    fn register_pool_json(
        track_id: u64,
        pool_id: u64,
        pool_generation: u64,
        payload_map_len: u64,
        slot_stride: u64,
        slot_count: u64,
    ) -> serde_json::Value {
        serde_json::json!({
            "op": "register_cpu_pool",
            "session_id": "session-1",
            "track_id": track_id,
            "pool_id": pool_id,
            "pool_generation": pool_generation,
            "payload_map_len": payload_map_len,
            "slot_stride": slot_stride,
            "slot_count": slot_count
        })
    }

    fn register_control_page_json(track_id: u64, map_len: u64) -> serde_json::Value {
        serde_json::json!({
            "op": "register_video_control_page",
            "session_id": "session-1",
            "track_id": track_id,
            "map_len": map_len
        })
    }

    fn tempfile_file_with_contents(contents: &[u8]) -> File {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(contents).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file
    }
}
