use std::{
    ffi::CString,
    io::{BufRead, BufReader, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
};

use serde::Deserialize;

use crate::{
    error::{CaptureTransferError, Result},
    fdpass,
    model::PixelFormat,
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
    pub len: usize,
    ptr: *mut libc::c_void,
}

impl DaemonFrame {
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: ptr is a live read-only mapping of len bytes until Drop.
        unsafe { std::slice::from_raw_parts(self.ptr.cast::<u8>(), self.len) }
    }
}

impl Drop for DaemonFrame {
    fn drop(&mut self) {
        // SAFETY: ptr/len describe the mapping created in latest_frame.
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
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

#[derive(Debug, Deserialize)]
struct LatestFrameWire {
    sequence: u64,
    timestamp_ns: u64,
    width: u32,
    height: u32,
    stride: u32,
    pixel_format: String,
    len: usize,
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

pub fn latest_frame(info: &SessionInfo, track_id: u64) -> Result<DaemonFrame> {
    let mut stream = UnixStream::connect(&info.fd_socket_path).map_err(|error| daemon_error("connect-fd-socket", error))?;
    writeln!(
        stream,
        "{}",
        serde_json::json!({
            "session_id": info.session_id,
            "track_id": track_id
        })
    )
    .map_err(|error| daemon_error("write-latest-request", error))?;
    let fd = fdpass::recv_fd(&stream)?;

    let reader_stream = stream.try_clone().map_err(|error| daemon_error("clone-fd-stream", error))?;
    let mut reader = BufReader::new(reader_stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|error| daemon_error("read-latest-response", error))?;
    let frame: LatestFrameWire = serde_json::from_str(line.trim_end()).map_err(|error| daemon_error("parse-latest-frame", error))?;
    let pixel_format = parse_pixel_format(&frame.pixel_format)?;
    if frame.len == 0 {
        return Err(CaptureTransferError::DaemonTransport {
            operation: "mmap-frame",
            message: "frame length is zero".to_string(),
        });
    }

    // SAFETY: fd is valid, len is supplied by daemon metadata, and mapping is read-only.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            frame.len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(CaptureTransferError::DaemonTransport {
            operation: "mmap-frame",
            message: std::io::Error::last_os_error().to_string(),
        });
    }

    Ok(DaemonFrame {
        desc: VideoFrameDesc {
            sequence: frame.sequence,
            timestamp_ns: frame.timestamp_ns,
            width: frame.width,
            height: frame.height,
            stride: frame.stride,
            pixel_format,
        },
        len: frame.len,
        ptr,
    })
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

fn daemon_error(operation: &'static str, error: impl std::fmt::Display) -> CaptureTransferError {
    CaptureTransferError::DaemonTransport {
        operation,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_http_response;

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
}
