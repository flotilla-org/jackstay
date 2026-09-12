use std::{
    ffi::CString,
    fmt,
    io::{Read, Write},
    os::unix::net::UnixStream,
};

use serde::Deserialize;

use crate::{
    error::{CaptureTransferError, Result},
    model::PixelFormat,
};

/// An authorized CPU session uses the common arena for every acquisition.
/// Setup is separate so acquired frames can outlive both connection/API owners.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub struct ConnectedSession {
    pub setup: crate::acquisition::socket::CpuSetupClient,
    pub consumer: crate::acquisition::arena::ArenaConsumer,
    pub info: SessionInfo,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl ConnectedSession {
    /// Select and authorize a host session, then negotiate its holding reservation.
    ///
    /// # Safety
    /// The endpoint must be a trusted conforming producer. This process must
    /// remain the sole recipient of the process-bound mappings: do not fork,
    /// forward or replay them. Authorization does not authenticate the producer.
    pub unsafe fn connect(info: SessionInfo, holding: u32) -> Result<Self> {
        use crate::acquisition::socket::CpuSetupClient;
        let mut stream = UnixStream::connect(&info.fd_socket_path).map_err(|error| daemon_error("connect-cpu-session", error))?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .map_err(|error| daemon_error("set-setup-timeout", error))?;
        stream
            .set_write_timeout(Some(std::time::Duration::from_secs(5)))
            .map_err(|error| daemon_error("set-setup-timeout", error))?;
        let request = serde_json::json!({
            "op": "open_cpu_acquisition", "session_id": info.session_id,
            "track_id": info.track_id, "bearer_token": info.bearer_token,
        });
        writeln!(stream, "{request}").map_err(|error| daemon_error("open-cpu-session", error))?;
        let mut reply = Vec::new();
        loop {
            if reply.len() == 16 * 1024 {
                return Err(daemon_error("open-cpu-session", "oversized reply"));
            }
            let mut byte = [0];
            stream
                .read_exact(&mut byte)
                .map_err(|error| daemon_error("open-cpu-session", error))?;
            if byte == *b"\n" {
                break;
            }
            reply.push(byte[0]);
        }
        #[derive(Deserialize)]
        #[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
        enum Reply {
            CpuOpened,
            Rejected { message: String },
        }
        match serde_json::from_slice(&reply).map_err(|error| daemon_error("open-cpu-session", error))? {
            Reply::CpuOpened => {}
            Reply::Rejected { message } => return Err(daemon_error("open-cpu-session", message)),
        }
        // SAFETY: delegated to this method's sole-producer/process contract.
        let mut setup = unsafe { CpuSetupClient::from_stream(stream) };
        let consumer = setup.attach(holding).map_err(|error| daemon_error("admit-cpu-session", error))?;
        Ok(Self { setup, consumer, info })
    }
}

#[derive(Debug, Clone)]
pub struct SyntheticSession {
    pub session_id: String,
    pub source_id: u64,
    pub track_id: u64,
    pub fd_socket_path: String,
}

#[derive(Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub source_id: u64,
    pub track_id: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: PixelFormat,
    pub fd_socket_path: String,
    /// Caller-supplied agent token for protected sessions. `None` keeps the
    /// public setup flow used by synthetic sessions.
    pub bearer_token: Option<String>,
}

impl fmt::Debug for SessionInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionInfo")
            .field("session_id", &self.session_id)
            .field("source_id", &self.source_id)
            .field("track_id", &self.track_id)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride", &self.stride)
            .field("pixel_format", &self.pixel_format)
            .field("fd_socket_path", &self.fd_socket_path)
            .field("bearer_token", &self.bearer_token.as_deref().map(|_| "<redacted>"))
            .finish()
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
        bearer_token: None,
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
        "unknown" => Ok(PixelFormat::Unknown),
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
    use super::{SessionInfo, parse_http_response};
    use crate::model::PixelFormat;
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
    fn session_info_debug_redacts_bearer_token() {
        let info = SessionInfo {
            session_id: "session-1".to_string(),
            source_id: 1,
            track_id: 7,
            width: 2,
            height: 1,
            stride: 8,
            pixel_format: PixelFormat::Bgra8Unorm,
            fd_socket_path: "/tmp/capture-fd.sock".to_string(),
            bearer_token: Some("pta_agent.secret".to_string()),
        };

        let output = format!("{info:?}");
        assert!(output.contains("<redacted>"));
        assert!(!output.contains("pta_agent.secret"));
    }
}
