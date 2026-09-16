//! Public C ownership layer over the shared input implementation.
use std::{ffi::c_char, ptr, slice, str, time::Duration};

use crate::{
    ffi::*,
    input::{
        transport::{Client, ConnectError, Server},
        *,
    },
};
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FtInputGeometry {
    pub revision: u64,
    pub width: f64,
    pub height: f64,
}
impl From<Geometry> for FtInputGeometry {
    fn from(g: Geometry) -> Self {
        Self {
            revision: g.revision,
            width: g.width,
            height: g.height,
        }
    }
}
impl From<FtInputGeometry> for Geometry {
    fn from(g: FtInputGeometry) -> Self {
        Self {
            revision: g.revision,
            width: g.width,
            height: g.height,
        }
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FtInputConfig {
    pub modes: u32,
    pub capabilities: u32,
    pub max_events: u32,
    pub max_bytes: u32,
    pub max_text_bytes: u32,
    pub idle_timeout_ms: u32,
    pub independent_contributions: u32,
    pub interaction_cancel: u32,
    pub geometry: FtInputGeometry,
}
impl From<Config> for FtInputConfig {
    fn from(c: Config) -> Self {
        Self {
            modes: c.modes,
            capabilities: c.capabilities,
            max_events: c.max_events as u32,
            max_bytes: c.max_bytes as u32,
            max_text_bytes: c.max_text_bytes as u32,
            idle_timeout_ms: c.idle_timeout.as_millis() as u32,
            independent_contributions: u32::from(c.independent_contributions),
            interaction_cancel: u32::from(c.interaction_cancel),
            geometry: c.geometry.into(),
        }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FtInputEvent {
    pub kind: u32,
    pub action: u32,
    pub key_kind: u32,
    pub modifiers: u32,
    pub press: u64,
    pub geometry_revision: u64,
    pub button: u32,
    pub scroll_unit: u32,
    pub x: f64,
    pub y: f64,
    pub pointer_x: f64,
    pub pointer_y: f64,
    pub key: [c_char; 64],
    pub text: *const u8,
    pub text_len: usize,
}
impl Default for FtInputEvent {
    fn default() -> Self {
        Self {
            kind: 0,
            action: 0,
            key_kind: 0,
            modifiers: 0,
            press: 0,
            geometry_revision: 0,
            button: 0,
            scroll_unit: 0,
            x: 0.,
            y: 0.,
            pointer_x: 0.,
            pointer_y: 0.,
            key: [0; 64],
            text: ptr::null(),
            text_len: 0,
        }
    }
}
#[repr(C)]
#[derive(Default)]
pub struct FtInputOperation {
    pub controller: u64,
    pub epoch: u64,
    pub sequence: u64,
    pub scope: u32,
    pub reason: u32,
    pub mode: u32,
    pub reserved: u32,
    pub event: FtInputEvent,
}
#[repr(C)]
#[derive(Default)]
pub struct FtInputStatus {
    pub kind: u32,
    pub result: i32,
    pub sequence: u64,
    pub epoch: u64,
    pub reason: u32,
    pub clean: u32,
    pub geometry: FtInputGeometry,
}
pub struct FtInputTarget(pub(crate) Target);
pub struct FtInputServer {
    pub(crate) _server: Server,
}
pub struct FtInputClient(pub(crate) Client);
pub struct FtInputWork {
    target: Target,
    work: Work,
}
pub(crate) fn status(e: Error) -> FtStatus {
    match e {
        Error::Invalid => FT_STATUS_INVALID_ARGUMENT,
        Error::Unsupported => FT_STATUS_UNSUPPORTED,
        Error::Busy => FT_STATUS_DRAINING,
        Error::Closed => FT_STATUS_CLOSED,
        Error::Stale => FT_STATUS_STALE,
        Error::Overflow => FT_STATUS_CAPACITY,
        Error::CleanupFailed => FT_STATUS_RECOVERY_REQUIRED,
    }
}
fn result(r: Result<(), Error>) -> FtStatus {
    r.map_or_else(status, |_| FT_STATUS_OK)
}
pub(crate) fn mode(v: u32) -> Option<Mode> {
    match v {
        1 => Some(Mode::Physical),
        2 => Some(Mode::SourceText),
        4 => Some(Mode::Cooperative),
        _ => None,
    }
}
fn reason(r: Reason) -> u32 {
    match r {
        Reason::Focus => 1,
        Reason::Geometry => 2,
        Reason::Disconnect => 3,
        Reason::Expired => 4,
        Reason::Overflow => 5,
        Reason::Execution => 6,
    }
}
fn outcome(v: u32) -> Option<Outcome> {
    match v {
        0 => Some(Outcome::Executed),
        1 => Some(Outcome::Rejected),
        2 => Some(Outcome::Unsupported),
        3 => Some(Outcome::Partial),
        4 => Some(Outcome::Uncertain),
        _ => None,
    }
}
fn outcome_code(o: Outcome) -> i32 {
    match o {
        Outcome::Executed => 0,
        Outcome::Rejected => 1,
        Outcome::Unsupported => 2,
        Outcome::Partial => 3,
        Outcome::Uncertain => 4,
    }
}
fn action(v: u32) -> Result<Action, Error> {
    match v {
        1 => Ok(Action::Down),
        2 => Ok(Action::Up),
        3 => Ok(Action::Repeat),
        _ => Err(Error::Invalid),
    }
}
fn action_code(a: Action) -> u32 {
    match a {
        Action::Down => 1,
        Action::Up => 2,
        Action::Repeat => 3,
    }
}
unsafe fn event(e: &FtInputEvent) -> Result<Event, Error> {
    let p = Position {
        revision: e.geometry_revision,
        x: e.x,
        y: e.y,
    };
    Ok(match e.kind {
        1 => {
            let n = e.key.iter().position(|&b| b == 0).ok_or(Error::Invalid)?;
            let bytes: Vec<u8> = e.key[..n].iter().map(|&b| b as u8).collect();
            let name = str::from_utf8(&bytes).map_err(|_| Error::Invalid)?.to_owned();
            let key = match e.key_kind {
                1 => Key::Physical(name),
                2 => Key::Logical(name),
                _ => return Err(Error::Invalid),
            };
            Event::Key {
                press: e.press,
                action: action(e.action)?,
                key,
                modifiers: e.modifiers,
            }
        }
        2 => {
            if e.text.is_null() || e.text_len == 0 || e.text_len > 16384 {
                return Err(Error::Invalid);
            }
            // SAFETY: C caller supplies a readable text_len-byte buffer.
            let text = str::from_utf8(unsafe { slice::from_raw_parts(e.text, e.text_len) })
                .map_err(|_| Error::Invalid)?
                .to_owned();
            Event::Text(text)
        }
        3 => Event::Motion(p),
        4 => Event::Button {
            button: e.button,
            action: action(e.action)?,
            position: p,
        },
        5 => Event::Scroll {
            x: e.x,
            y: e.y,
            unit: match e.scroll_unit {
                1 => ScrollUnit::Pixel,
                2 => ScrollUnit::Line,
                3 => ScrollUnit::Page,
                _ => return Err(Error::Invalid),
            },
            position: Position {
                revision: e.geometry_revision,
                x: e.pointer_x,
                y: e.pointer_y,
            },
        },
        _ => return Err(Error::Invalid),
    })
}
fn describe(e: &Event) -> FtInputEvent {
    let mut out = FtInputEvent::default();
    let position = match e {
        Event::Key {
            press,
            action,
            key,
            modifiers,
        } => {
            out.kind = 1;
            out.press = *press;
            out.action = action_code(*action);
            out.modifiers = *modifiers;
            let name = match key {
                Key::Physical(n) => {
                    out.key_kind = 1;
                    n
                }
                Key::Logical(n) => {
                    out.key_kind = 2;
                    n
                }
            };
            for (dst, src) in out.key.iter_mut().zip(name.bytes()) {
                *dst = src as c_char;
            }
            None
        }
        Event::Text(t) => {
            out.kind = 2;
            out.text = t.as_ptr();
            out.text_len = t.len();
            None
        }
        Event::Motion(p) => {
            out.kind = 3;
            Some(p)
        }
        Event::Button { button, action, position } => {
            out.kind = 4;
            out.button = *button;
            out.action = action_code(*action);
            Some(position)
        }
        Event::Scroll { x, y, unit, position } => {
            out.kind = 5;
            out.x = *x;
            out.y = *y;
            out.scroll_unit = match unit {
                ScrollUnit::Pixel => 1,
                ScrollUnit::Line => 2,
                ScrollUnit::Page => 3,
            };
            out.geometry_revision = position.revision;
            out.pointer_x = position.x;
            out.pointer_y = position.y;
            None
        }
    };
    if let Some(p) = position {
        out.geometry_revision = p.revision;
        out.x = p.x;
        out.y = p.y;
    }
    out
}
/// # Safety
/// `out` is null or writable, correctly aligned storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_input_config_default(out: *mut FtInputConfig) {
    if let Some(out) = unsafe { out.as_mut() } {
        *out = Config::default().into();
    }
}
/// # Safety
/// Config is readable; out is writable and null. Arguments are disjoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_input_target_create(config: *const FtInputConfig, out: *mut *mut FtInputTarget) -> FtStatus {
    let (Some(c), Some(out)) = (unsafe { config.as_ref() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() || c.independent_contributions > 1 || c.interaction_cancel > 1 {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let c = Config {
        modes: c.modes,
        capabilities: c.capabilities,
        max_events: c.max_events as usize,
        max_bytes: c.max_bytes as usize,
        max_text_bytes: c.max_text_bytes as usize,
        idle_timeout: Duration::from_millis(c.idle_timeout_ms.into()),
        geometry: c.geometry.into(),
        independent_contributions: c.independent_contributions != 0,
        interaction_cancel: c.interaction_cancel != 0,
    };
    match Target::new(c) {
        Ok(t) => {
            *out = Box::into_raw(Box::new(FtInputTarget(t)));
            FT_STATUS_OK
        }
        Err(e) => status(e),
    }
}
/// # Safety
/// Target is live; fd owns a connected Unix stream. Out is writable and null.
/// After basic checks, fd is consumed on every outcome. Arguments are disjoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_input_target_serve(target: *mut FtInputTarget, fd: *mut i32, out: *mut *mut FtInputServer) -> FtStatus {
    let (Some(t), Some(fd), Some(out)) = (unsafe { target.as_ref() }, unsafe { fd.as_mut() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if *fd < 0 || !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let Ok(stream) = (unsafe { crate::ffi_acquisition::setup_server::take_stream(fd) }) else {
        return FT_STATUS_ERROR;
    };
    match Server::start(t.0.clone(), stream) {
        Ok(s) => {
            *out = Box::into_raw(Box::new(FtInputServer { _server: s }));
            FT_STATUS_OK
        }
        Err(_) => FT_STATUS_ERROR,
    }
}
/// # Safety
/// Target is live, out writable and null. Host continues pumping during cleanup.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_input_target_next(target: *mut FtInputTarget, out: *mut *mut FtInputWork) -> FtStatus {
    let (Some(t), Some(out)) = (unsafe { target.as_ref() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    match t.0.next() {
        Some(work) => {
            *out = Box::into_raw(Box::new(FtInputWork { target: t.0.clone(), work }));
            FT_STATUS_OK
        }
        None => FT_STATUS_EMPTY,
    }
}
/// # Safety
/// Work is live; out writable and disjoint. Returned text borrows work.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_input_work_describe(work: *const FtInputWork, out: *mut FtInputOperation) -> FtStatus {
    let (Some(w), Some(out)) = (unsafe { work.as_ref() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    *out = FtInputOperation {
        mode: w.work.mode.bit(),
        controller: w.work.controller,
        epoch: w.work.epoch,
        sequence: w.work.sequence,
        ..Default::default()
    };
    match &w.work.operation {
        Operation::Event(e) => out.event = describe(e),
        Operation::Cleanup { scope, reason: r } => {
            out.event.kind = 6;
            out.scope = match scope {
                Scope::All => 1,
                Scope::Pointer => 2,
            };
            out.reason = reason(*r);
        }
    }
    FT_STATUS_OK
}
/// # Safety
/// Work points to a live, exclusively owned handle. Valid outcome consumes it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_input_work_complete(work: *mut *mut FtInputWork, value: u32) -> FtStatus {
    let (Some(slot), Some(outcome)) = (unsafe { work.as_mut() }, outcome(value)) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if slot.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: caller transfers this unique allocation; null the owning slot.
    let w = unsafe { Box::from_raw(std::mem::replace(slot, ptr::null_mut())) };
    result(w.target.complete(w.work.id, outcome))
}
/// # Safety
/// Target is live and geometry readable; destruction cannot race this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_input_target_geometry(target: *mut FtInputTarget, g: *const FtInputGeometry) -> FtStatus {
    let (Some(t), Some(g)) = (unsafe { target.as_ref() }, unsafe { g.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    result(t.0.set_geometry((*g).into()))
}
/// # Safety
/// Target is live; host has independently resolved failed executor cleanup.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_input_target_resolve(target: *mut FtInputTarget) -> FtStatus {
    let Some(t) = (unsafe { target.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    result(t.0.resolve_failed_cleanup())
}
/// # Safety
/// Target slot is writable and owns its handle; no concurrent calls on destruction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_input_target_destroy(target: *mut *mut FtInputTarget) -> FtStatus {
    let Some(slot) = (unsafe { target.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Some(t) = (unsafe { slot.as_ref() }) else { return FT_STATUS_OK };
    if t.0.failed() {
        return FT_STATUS_RECOVERY_REQUIRED;
    }
    if !t.0.idle() {
        return FT_STATUS_DRAINING;
    }
    // SAFETY: idle target, sole C owner; workers retain independent Rust references.
    drop(unsafe { Box::from_raw(std::mem::replace(slot, ptr::null_mut())) });
    FT_STATUS_OK
}
/// # Safety
/// Slot is writable and exclusively owns a server handle, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_input_server_destroy(server: *mut *mut FtInputServer) {
    if let Some(slot) = unsafe { server.as_mut() } {
        if !slot.is_null() {
            drop(unsafe { Box::from_raw(std::mem::replace(slot, ptr::null_mut())) });
        }
    }
}
/// # Safety
/// Fd exclusively owns a connected Unix stream; out writable and null. Consumes
/// fd after basic checks on every outcome. Blocks for bounded admission.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_input_client_connect(fd: *mut i32, value: u32, out: *mut *mut FtInputClient) -> FtStatus {
    let (Some(fd), Some(mode), Some(out)) = (unsafe { fd.as_mut() }, mode(value), unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if *fd < 0 || !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let Ok(stream) = (unsafe { crate::ffi_acquisition::setup_server::take_stream(fd) }) else {
        return FT_STATUS_ERROR;
    };
    match Client::connect(stream, mode) {
        Ok(c) => {
            *out = Box::into_raw(Box::new(FtInputClient(c)));
            FT_STATUS_OK
        }
        Err(ConnectError::Admission(e)) => status(e),
        Err(_) => FT_STATUS_ERROR,
    }
}
/// # Safety
/// Client is live; all output pointers are writable and disjoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_input_client_describe(
    client: *const FtInputClient,
    out: *mut FtInputConfig,
    controller: *mut u64,
    epoch: *mut u64,
) -> FtStatus {
    let (Some(c), Some(out), Some(id), Some(epoch)) = (
        unsafe { client.as_ref() },
        unsafe { out.as_mut() },
        unsafe { controller.as_mut() },
        unsafe { epoch.as_mut() },
    ) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let w = c.0.welcome();
    *out = w.config.into();
    *id = w.controller;
    *epoch = w.epoch;
    FT_STATUS_OK
}
/// # Safety
/// Client/event are live; text buffer readable for text_len; sequence writable and
/// disjoint. Event and text are copied before returning.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_input_client_send(client: *mut FtInputClient, e: *const FtInputEvent, sequence: *mut u64) -> FtStatus {
    let (Some(c), Some(e), Some(sequence)) = (unsafe { client.as_ref() }, unsafe { e.as_ref() }, unsafe { sequence.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let e = match unsafe { event(e) } {
        Ok(e) => e,
        Err(e) => return status(e),
    };
    match c.0.send(e) {
        Ok(s) => {
            *sequence = s;
            FT_STATUS_OK
        }
        Err(e) => status(e),
    }
}
/// # Safety
/// Client is live; out writable and disjoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_input_client_poll(client: *mut FtInputClient, out: *mut FtInputStatus) -> FtStatus {
    let (Some(c), Some(out)) = (unsafe { client.as_ref() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Some(s) = c.0.poll() else { return FT_STATUS_EMPTY };
    *out = FtInputStatus::default();
    match s {
        Status::Completed { sequence, outcome } => {
            out.kind = 1;
            out.sequence = sequence;
            out.result = outcome_code(outcome);
        }
        Status::Rejected { sequence, error } => {
            out.kind = 2;
            out.sequence = sequence;
            out.result = status(error);
        }
        Status::Reset { epoch, geometry } => {
            out.kind = 3;
            out.epoch = epoch;
            out.geometry = geometry.into();
        }
        Status::Closed { reason: r, clean } => {
            out.kind = 4;
            out.reason = reason(r);
            out.clean = u32::from(clean);
        }
    }
    FT_STATUS_OK
}
/// # Safety
/// Client is live; destruction cannot race this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_input_client_reset(client: *mut FtInputClient) -> FtStatus {
    let Some(c) = (unsafe { client.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    result(c.0.reset())
}
/// # Safety
/// Client is null or live; destruction cannot race this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_input_client_close(client: *mut FtInputClient) {
    if let Some(c) = unsafe { client.as_ref() } {
        c.0.close();
    }
}
/// # Safety
/// Slot is writable and exclusively owns its client handle, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_input_client_destroy(client: *mut *mut FtInputClient) {
    if let Some(slot) = unsafe { client.as_mut() } {
        if !slot.is_null() {
            drop(unsafe { Box::from_raw(std::mem::replace(slot, ptr::null_mut())) });
        }
    }
}

/// # Safety
/// Server is live; destruction cannot race this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_input_server_poll(server: *const FtInputServer) -> FtStatus {
    let Some(s) = (unsafe { server.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if s._server.finished() { FT_STATUS_OK } else { FT_STATUS_EMPTY }
}
