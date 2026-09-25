//! A window's client area inside its captured frame. WGC captures a window's
//! DWM extended frame bounds; the client area is offset within them by the
//! title bar and borders. Coordinates are physical pixels: the calculation runs
//! per-monitor DPI aware whatever the host process's awareness.

use ::windows::Win32::{
    Foundation::{HWND, POINT, RECT},
    Graphics::{
        Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
        Gdi::ClientToScreen,
    },
    UI::{
        HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext},
        WindowsAndMessaging::GetClientRect,
    },
};

struct DpiScope(::windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT);

impl DpiScope {
    fn per_monitor() -> Self {
        // SAFETY: changes only this thread's awareness; restored on drop.
        Self(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
    }
}

impl Drop for DpiScope {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: restores the context returned above.
            unsafe { SetThreadDpiAwarenessContext(self.0) };
        }
    }
}

fn client_offset(window: HWND) -> Option<(i32, i32, u32, u32)> {
    let _dpi = DpiScope::per_monitor();
    let mut bounds = RECT::default();
    let mut client = RECT::default();
    let mut origin = POINT::default();
    // SAFETY: out pointers are live locals of the stated sizes; a destroyed
    // window fails these calls.
    unsafe {
        DwmGetWindowAttribute(
            window,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&raw mut bounds).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
        .ok()?;
        GetClientRect(window, &mut client).ok()?;
        ClientToScreen(window, &mut origin).ok().ok()?;
    }
    Some((
        origin.x - bounds.left,
        origin.y - bounds.top,
        u32::try_from(client.right - client.left).ok()?,
        u32::try_from(client.bottom - client.top).ok()?,
    ))
}

/// `(left, top, width, height)` to publish from a `width` x `height` frame:
/// the window's client area clamped to the frame, or the whole frame.
pub(super) fn client_region(window: Option<HWND>, width: u32, height: u32) -> (u32, u32, u32, u32) {
    let Some((left, top, client_width, client_height)) = window.and_then(client_offset) else {
        return (0, 0, width, height);
    };
    let left = u32::try_from(left.max(0)).unwrap_or(0).min(width);
    let top = u32::try_from(top.max(0)).unwrap_or(0).min(height);
    (left, top, client_width.min(width - left), client_height.min(height - top))
}
