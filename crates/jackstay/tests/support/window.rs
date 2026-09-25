//! A small top-level window this test process owns, painted one solid colour,
//! so Windows.Graphics.Capture can be exercised without Porthole or any other
//! application's windows. It is shown without activation (it never takes the
//! keyboard focus) near the top-left of the desktop, and closed on drop.
#![allow(dead_code, reason = "each test crate uses a different subset")]

use std::{
    sync::{
        atomic::{AtomicU32, Ordering},
        mpsc,
    },
    thread::JoinHandle,
};

use windows::{
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::{
            Dwm::{DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DwmSetWindowAttribute},
            Gdi::{BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect, PAINTSTRUCT, UpdateWindow},
        },
        UI::{
            HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext},
            WindowsAndMessaging::{
                AdjustWindowRectEx, CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
                GWLP_USERDATA, GetClientRect, GetMessageW, GetWindowLongPtrW, MSG, PostMessageW, PostQuitMessage, RegisterClassW,
                SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos,
                ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WM_CLOSE, WM_DESTROY, WM_PAINT, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
            },
        },
    },
    core::w,
};

/// A colour as the window paints it: `0x00BBGGRR`.
fn colorref(rgb: [u8; 3]) -> COLORREF {
    COLORREF(u32::from(rgb[0]) | (u32::from(rgb[1]) << 8) | (u32::from(rgb[2]) << 16))
}

unsafe extern "system" fn procedure(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: standard window procedure calls on this thread's own window.
    unsafe {
        match message {
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let dc = BeginPaint(window, &mut paint);
                let mut client = RECT::default();
                let _ = GetClientRect(window, &mut client);
                let brush = CreateSolidBrush(COLORREF(GetWindowLongPtrW(window, GWLP_USERDATA) as u32));
                FillRect(dc, &client, brush);
                let _ = DeleteObject(brush.into());
                let _ = EndPaint(window, &paint);
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(window);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }
}

pub struct TestWindow {
    window: isize,
    thread: Option<JoinHandle<()>>,
}

static CLASS: AtomicU32 = AtomicU32::new(0);

impl TestWindow {
    /// A window with a `width` x `height` client area (physical pixels),
    /// painted `rgb`, with its own message loop thread.
    pub fn open(width: u32, height: u32, rgb: [u8; 3]) -> Self {
        let (sender, receiver) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            // SAFETY: plain Win32 window creation and a message loop, all on
            // this thread.
            unsafe {
                SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                if CLASS.fetch_add(1, Ordering::SeqCst) == 0 {
                    let class = WNDCLASSW {
                        style: CS_HREDRAW | CS_VREDRAW,
                        lpfnWndProc: Some(procedure),
                        lpszClassName: w!("JackstayTestCaptureWindow"),
                        ..Default::default()
                    };
                    assert_ne!(RegisterClassW(&class), 0);
                }
                let mut frame = RECT {
                    left: 0,
                    top: 0,
                    right: width as i32,
                    bottom: height as i32,
                };
                AdjustWindowRectEx(&mut frame, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE(0)).unwrap();
                let window = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("JackstayTestCaptureWindow"),
                    w!("Jackstay capture test"),
                    WS_OVERLAPPEDWINDOW & !WS_VISIBLE,
                    40,
                    40,
                    frame.right - frame.left,
                    frame.bottom - frame.top,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap();
                SetWindowLongPtrW(window, GWLP_USERDATA, colorref(rgb).0 as isize);
                // Windows 11 rounds top-level corners, which would make the
                // client area non-uniform at its bottom corners.
                let corners = DWMWCP_DONOTROUND;
                let _ = DwmSetWindowAttribute(
                    window,
                    DWMWA_WINDOW_CORNER_PREFERENCE,
                    (&raw const corners).cast(),
                    std::mem::size_of_val(&corners) as u32,
                );
                let _ = ShowWindow(window, SW_SHOWNOACTIVATE);
                let _ = UpdateWindow(window);
                sender.send(window.0 as isize).unwrap();
                let mut message = MSG::default();
                while GetMessageW(&mut message, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        });
        Self {
            window: receiver.recv().unwrap(),
            thread: Some(thread),
        }
    }

    pub fn hwnd(&self) -> HWND {
        HWND(self.window as *mut _)
    }

    pub fn set_color(&self, rgb: [u8; 3]) {
        // SAFETY: the window is alive until close; SetWindowLongPtr and
        // InvalidateRect are safe across threads.
        unsafe {
            SetWindowLongPtrW(self.hwnd(), GWLP_USERDATA, colorref(rgb).0 as isize);
            let _ = InvalidateRect(Some(self.hwnd()), None, true);
        }
    }

    /// Resize the client area to `width` x `height` physical pixels.
    pub fn resize(&self, width: u32, height: u32) {
        let mut frame = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        // SAFETY: plain Win32 calls on a live window.
        unsafe {
            let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            AdjustWindowRectEx(&mut frame, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE(0)).unwrap();
            SetWindowPos(
                self.hwnd(),
                None,
                0,
                0,
                frame.right - frame.left,
                frame.bottom - frame.top,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
            .unwrap();
            SetThreadDpiAwarenessContext(previous);
            let _ = InvalidateRect(Some(self.hwnd()), None, true);
        }
    }

    /// Minimize without activating anything.
    pub fn minimize(&self) {
        // SAFETY: ShowWindow on a live window, from any thread.
        let _ = unsafe { ShowWindow(self.hwnd(), SW_SHOWMINNOACTIVE) };
    }

    /// Restore from minimized, without activation.
    pub fn restore(&self) {
        // SAFETY: as minimize.
        let _ = unsafe { ShowWindow(self.hwnd(), SW_SHOWNOACTIVATE) };
        self.set_color(self.color());
    }

    fn color(&self) -> [u8; 3] {
        // SAFETY: reading this window's own user data.
        let value = unsafe { GetWindowLongPtrW(self.hwnd(), GWLP_USERDATA) } as u32;
        [value as u8, (value >> 8) as u8, (value >> 16) as u8]
    }

    pub fn close(&mut self) {
        if let Some(thread) = self.thread.take() {
            // SAFETY: posting to a window owned by the loop thread.
            unsafe {
                let _ = PostMessageW(Some(self.hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            thread.join().unwrap();
        }
    }
}

impl Drop for TestWindow {
    fn drop(&mut self) {
        self.close();
    }
}
