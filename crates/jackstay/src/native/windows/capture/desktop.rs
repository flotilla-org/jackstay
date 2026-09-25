//! Whether the capturing session's desktop can currently be seen. A locked
//! workstation or a disconnected RDP session pauses capture (#23): neither is
//! a failure, and capture resumes when the desktop returns.

use std::fmt;

use ::windows::{
    Win32::System::RemoteDesktop::{
        WTS_CONNECTSTATE_CLASS, WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK, WTSActive, WTSConnected, WTSDisconnected, WTSFreeMemory,
        WTSINFOEXW, WTSQuerySessionInformationW, WTSSessionInfoEx,
    },
    core::PWSTR,
};

/// Why the desktop is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopUnavailable {
    /// The session is locked (the secure desktop is showing).
    Locked,
    /// The session has no connected display: an RDP client disconnected, or
    /// another session took the console.
    Disconnected,
}

impl fmt::Display for DesktopUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Locked => "session locked",
            Self::Disconnected => "session disconnected",
        })
    }
}

/// A source of desktop availability. [`SessionDesktop`] asks the Remote
/// Desktop Services session API; tests inject their own.
pub trait DesktopMonitor: Send + Sync + fmt::Debug {
    /// `None` while the desktop is available.
    fn unavailable(&self) -> Option<DesktopUnavailable>;
}

/// The current process's session, from `WTSQuerySessionInformation`
/// (`WTSSessionInfoEx`): its connect state and lock flag. A failed query
/// counts as available: an unknown state must not stop capture.
#[derive(Debug, Default, Clone, Copy)]
pub struct SessionDesktop;

/// The session's raw connect state and lock flag, for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionState {
    pub session_id: u32,
    /// `WTS_CONNECTSTATE_CLASS` (0 active, 1 connected, 4 disconnected, ...).
    pub connect_state: i32,
    /// `WTS_SESSIONSTATE_LOCK` (0), `_UNLOCK` (1) or `_UNKNOWN` (-1).
    pub lock_flag: i32,
}

impl SessionDesktop {
    pub fn query() -> ::windows::core::Result<SessionState> {
        let mut buffer = PWSTR::null();
        let mut bytes = 0;
        // SAFETY: the server handle None is the local server; the out
        // pointers are live locals. The buffer is freed below.
        unsafe { WTSQuerySessionInformationW(None, WTS_CURRENT_SESSION, WTSSessionInfoEx, &mut buffer, &mut bytes) }?;
        let state = (|| {
            if (bytes as usize) < std::mem::size_of::<WTSINFOEXW>() || buffer.is_null() {
                return Err(::windows::core::Error::from_hresult(::windows::Win32::Foundation::E_UNEXPECTED));
            }
            // SAFETY: WTSSessionInfoEx returns a WTSINFOEXW of at least the
            // reported size; level 1 is the only defined level.
            let info = unsafe { &*buffer.0.cast::<WTSINFOEXW>() };
            if info.Level != 1 {
                return Err(::windows::core::Error::from_hresult(::windows::Win32::Foundation::E_UNEXPECTED));
            }
            // SAFETY: level 1 selects this union member.
            let level = unsafe { info.Data.WTSInfoExLevel1 };
            Ok(SessionState {
                session_id: level.SessionId,
                connect_state: level.SessionState.0,
                lock_flag: level.SessionFlags,
            })
        })();
        // SAFETY: allocated by WTSQuerySessionInformationW above.
        unsafe { WTSFreeMemory(buffer.0.cast()) };
        state
    }
}

impl SessionState {
    #[must_use]
    pub fn unavailable(&self) -> Option<DesktopUnavailable> {
        let connected = [WTSActive, WTSConnected].contains(&WTS_CONNECTSTATE_CLASS(self.connect_state));
        if !connected || WTS_CONNECTSTATE_CLASS(self.connect_state) == WTSDisconnected {
            return Some(DesktopUnavailable::Disconnected);
        }
        (self.lock_flag == WTS_SESSIONSTATE_LOCK as i32).then_some(DesktopUnavailable::Locked)
    }
}

impl DesktopMonitor for SessionDesktop {
    fn unavailable(&self) -> Option<DesktopUnavailable> {
        Self::query().ok().and_then(|state| state.unavailable())
    }
}
