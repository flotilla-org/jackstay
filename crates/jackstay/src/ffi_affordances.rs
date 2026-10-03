//! Typed C affordances backed by the Rust channel implementation.
use std::{ptr, slice, str};

use serde_json::json;

use crate::{affordances::*, ffi::*};
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FtAffString {
    pub data: *const u8,
    pub len: usize,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FtAffOptionalString {
    pub present: u32,
    pub value: FtAffString,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FtAffOptionalNumber {
    pub present: u32,
    pub value: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FtAffSize {
    pub present: u32,
    pub width: f64,
    pub height: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FtAffArtwork {
    pub kind: u32,
    pub value: FtAffString,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FtAffMedia {
    pub status: u32,
    pub position: FtAffOptionalNumber,
    pub rate: f64,
    pub duration: FtAffOptionalNumber,
    pub title: FtAffOptionalString,
    pub artwork: FtAffArtwork,
    pub capabilities: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FtAffNavigation {
    pub url: FtAffOptionalString,
    pub title: FtAffOptionalString,
    pub can_go_back: u32,
    pub can_go_forward: u32,
    pub loading: u32,
    pub capabilities: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FtAffAxis {
    pub scrollable: u32,
    pub content_length: f64,
    pub viewport_length: f64,
    pub position: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FtAffScroll {
    pub x: FtAffAxis,
    pub y: FtAffAxis,
    pub capabilities: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FtAffWindow {
    pub title: FtAffOptionalString,
    pub requested_size: FtAffSize,
    pub ready: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FtAffPresentation {
    pub visible: u32,
    pub preferred_size: FtAffSize,
    pub scale: f64,
    pub focused: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FtAffSnapshot {
    pub domain: u32,
    pub withdrawn: u32,
    pub media: FtAffMedia,
    pub navigation: FtAffNavigation,
    pub cursor: u32,
    pub scroll: FtAffScroll,
    pub window: FtAffWindow,
    pub presentation: FtAffPresentation,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FtAffVerb {
    pub domain: u32,
    pub verb: u32,
    pub number: f64,
    pub url: FtAffString,
    pub axis: u32,
    pub step: u32,
    pub direction: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FtAffEventView {
    pub kind: u32,
    pub snapshot: FtAffSnapshot,
    pub verb: FtAffVerb,
}
pub struct FtAffordancesProducer(pub Producer);
pub struct FtAffordancesHost(pub Host);
pub struct FtAffordancesEvent(Event);
fn domain(n: u32) -> Option<Domain> {
    Some(match n {
        1 => Domain::Media,
        2 => Domain::Navigation,
        3 => Domain::Cursor,
        4 => Domain::Scroll,
        5 => Domain::Window,
        6 => Domain::Presentation,
        _ => return None,
    })
}
fn boolean(v: u32) -> Option<bool> {
    match v {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}
unsafe fn string(v: FtAffString) -> Option<String> {
    if v.len > 131072 || (v.data.is_null() && v.len != 0) {
        return None;
    }
    if v.len == 0 {
        return Some(String::new());
    }
    str::from_utf8(unsafe { slice::from_raw_parts(v.data, v.len) })
        .ok()
        .map(str::to_owned)
}
unsafe fn optional_string(v: FtAffOptionalString) -> Option<Option<String>> {
    Some(if boolean(v.present)? {
        Some(unsafe { string(v.value) }?)
    } else {
        None
    })
}
fn optional_number(v: FtAffOptionalNumber) -> Option<Option<f64>> {
    Some(if boolean(v.present)? {
        if !v.value.is_finite() {
            return None;
        }
        Some(v.value)
    } else {
        None
    })
}
fn size(v: FtAffSize) -> Option<Option<Size>> {
    Some(if boolean(v.present)? {
        Some(Size {
            width: v.width,
            height: v.height,
        })
    } else {
        None
    })
}
const MEDIA: &[&str] = &["play", "pause", "stop", "next", "previous", "seek_absolute", "seek_relative"];
const NAV: &[&str] = &["back", "forward", "reload", "stop", "load"];
const SCROLL: &[&str] = &["scroll_by_step", "set_position"];
fn names(d: Domain) -> &'static [&'static str] {
    match d {
        Domain::Media => MEDIA,
        Domain::Navigation => NAV,
        Domain::Scroll => SCROLL,
        _ => &[],
    }
}
fn caps(bits: u32, names: &[&str]) -> std::collections::BTreeMap<String, bool> {
    names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.to_string(), bits & (1 << i) != 0))
        .collect()
}
fn axis(v: FtAffAxis) -> Option<Axis> {
    Some(Axis {
        scrollable: boolean(v.scrollable)?,
        content_length: v.content_length,
        viewport_length: v.viewport_length,
        position: v.position,
    })
}
unsafe fn snapshot(v: &FtAffSnapshot) -> Option<Snapshot> {
    let d = domain(v.domain)?;
    if boolean(v.withdrawn)? {
        return Some(Snapshot::Withdraw(d));
    }
    Some(match d {
        Domain::Media => {
            let m = v.media;
            Snapshot::Media(Media {
                status: match m.status {
                    0 => "unknown",
                    1 => "playing",
                    2 => "paused",
                    3 => "stopped",
                    4 => "buffering",
                    _ => return None,
                }
                .into(),
                position: optional_number(m.position)?,
                rate: m.rate,
                duration: optional_number(m.duration)?,
                title: unsafe { optional_string(m.title) }?,
                artwork: match m.artwork.kind {
                    0 => None,
                    1 => Some(Artwork::Url {
                        url: unsafe { string(m.artwork.value) }?,
                    }),
                    2 => Some(Artwork::Icon {
                        name: unsafe { string(m.artwork.value) }?,
                    }),
                    _ => return None,
                },
                capabilities: caps(m.capabilities, MEDIA),
            })
        }
        Domain::Navigation => {
            let n = v.navigation;
            Snapshot::Navigation(Navigation {
                url: unsafe { optional_string(n.url) }?,
                title: unsafe { optional_string(n.title) }?,
                can_go_back: boolean(n.can_go_back)?,
                can_go_forward: boolean(n.can_go_forward)?,
                loading: boolean(n.loading)?,
                capabilities: caps(n.capabilities, NAV),
            })
        }
        Domain::Cursor => Snapshot::Cursor(CURSORS.get(v.cursor as usize)?.to_string()),
        Domain::Scroll => Snapshot::Scroll(Scroll {
            x: axis(v.scroll.x)?,
            y: axis(v.scroll.y)?,
            capabilities: caps(v.scroll.capabilities, SCROLL),
        }),
        Domain::Window => Snapshot::Window(Window {
            title: unsafe { optional_string(v.window.title) }?,
            requested_size: size(v.window.requested_size)?,
            ready: boolean(v.window.ready)?,
        }),
        Domain::Presentation => Snapshot::Presentation(Presentation {
            visible: boolean(v.presentation.visible)?,
            preferred_size: size(v.presentation.preferred_size)?,
            scale: v.presentation.scale,
            focused: boolean(v.presentation.focused)?,
        }),
    })
}
unsafe fn verb(v: &FtAffVerb) -> Option<Verb> {
    let d = domain(v.domain)?;
    let name = *names(d).get(v.verb.checked_sub(1)? as usize)?;
    let body = match (d, name) {
        (Domain::Media, "seek_absolute") => json!({"position":v.number}),
        (Domain::Media, "seek_relative") => json!({"offset":v.number}),
        (Domain::Navigation, "load") => json!({"url":unsafe{string(v.url)}?}),
        (Domain::Scroll, n) => {
            let axis = match v.axis {
                0 => "x",
                1 => "y",
                _ => return None,
            };
            if n == "set_position" {
                json!({"axis":axis,"position":v.number})
            } else {
                json!({"axis":axis,"step":match v.step{0=>"small",1=>"large",_=>return None},"direction":match v.direction{0=>"decrement",1=>"increment",_=>return None}})
            }
        }
        _ => json!({}),
    };
    if !v.number.is_finite() && matches!(name, "seek_absolute" | "seek_relative" | "set_position") {
        return None;
    }
    Some(Verb {
        domain: d,
        name: name.into(),
        body,
    })
}
fn status(r: std::io::Result<()>) -> FtStatus {
    match r {
        Ok(()) => FT_STATUS_OK,
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => FT_STATUS_INVALID_ARGUMENT,
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => FT_STATUS_CLOSED,
        Err(_) => FT_STATUS_ERROR,
    }
}
/// # Safety
/// Handles must be live and not concurrently destroyed. Pointer arguments and
/// outputs must be valid and disjoint; output handles start null. Borrowed
/// strings have their stated byte lengths. Event views last until destruction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_affordances_producer_publish(p: *mut FtAffordancesProducer, v: *const FtAffSnapshot) -> FtStatus {
    let (Some(p), Some(v)) = (unsafe { p.as_ref() }, unsafe { v.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Some(v) = (unsafe { snapshot(v) }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    status(p.0.publish(v))
}
/// # Safety
/// Handles must be live and not concurrently destroyed. Pointer arguments and
/// outputs must be valid and disjoint; output handles start null. Borrowed
/// strings have their stated byte lengths. Event views last until destruction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_affordances_host_publish(p: *mut FtAffordancesHost, v: *const FtAffSnapshot) -> FtStatus {
    let (Some(p), Some(v)) = (unsafe { p.as_ref() }, unsafe { v.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    match unsafe { snapshot(v) } {
        Some(Snapshot::Presentation(v)) => status(p.0.publish(v)),
        Some(Snapshot::Withdraw(Domain::Presentation)) => status(p.0.withdraw()),
        _ => FT_STATUS_INVALID_ARGUMENT,
    }
}
/// # Safety
/// Handles must be live and not concurrently destroyed. Pointer arguments and
/// outputs must be valid and disjoint; output handles start null. Borrowed
/// strings have their stated byte lengths. Event views last until destruction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_affordances_host_send(p: *mut FtAffordancesHost, v: *const FtAffVerb) -> FtStatus {
    let (Some(p), Some(v)) = (unsafe { p.as_ref() }, unsafe { v.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Some(v) = (unsafe { verb(v) }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    status(p.0.send(v))
}
macro_rules! owners {
    ($ty:ty,$poll:ident,$close:ident,$destroy:ident) => {
        /// # Safety
        /// Handles must be live and not concurrently destroyed. Pointer arguments and
        /// outputs must be valid and disjoint; output handles start null. Borrowed
        /// strings have their stated byte lengths. Event views last until destruction.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $poll(p: *mut $ty, out: *mut *mut FtAffordancesEvent) -> FtStatus {
            let (Some(p), Some(out)) = (unsafe { p.as_ref() }, unsafe { out.as_mut() }) else {
                return FT_STATUS_INVALID_ARGUMENT;
            };
            if !out.is_null() {
                return FT_STATUS_INVALID_ARGUMENT;
            }
            match p.0.poll() {
                Some(e) => {
                    *out = Box::into_raw(Box::new(FtAffordancesEvent(e)));
                    FT_STATUS_OK
                }
                None => FT_STATUS_EMPTY,
            }
        }
        /// # Safety
        /// Handles must be live and not concurrently destroyed. Pointer arguments and
        /// outputs must be valid and disjoint; output handles start null. Borrowed
        /// strings have their stated byte lengths. Event views last until destruction.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $close(p: *mut $ty) {
            if let Some(p) = unsafe { p.as_ref() } {
                p.0.close()
            }
        }
        /// # Safety
        /// Handles must be live and not concurrently destroyed. Pointer arguments and
        /// outputs must be valid and disjoint; output handles start null. Borrowed
        /// strings have their stated byte lengths. Event views last until destruction.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $destroy(p: *mut *mut $ty) {
            if let Some(p) = unsafe { p.as_mut() } {
                if !p.is_null() {
                    drop(unsafe { Box::from_raw(*p) });
                    *p = ptr::null_mut();
                }
            }
        }
    };
}
owners!(
    FtAffordancesProducer,
    ft_affordances_producer_poll,
    ft_affordances_producer_close,
    ft_affordances_producer_destroy
);
owners!(
    FtAffordancesHost,
    ft_affordances_host_poll,
    ft_affordances_host_close,
    ft_affordances_host_destroy
);
/// # Safety
/// Handles must be live and not concurrently destroyed. Pointer arguments and
/// outputs must be valid and disjoint; output handles start null. Borrowed
/// strings have their stated byte lengths. Event views last until destruction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_affordances_event_destroy(p: *mut *mut FtAffordancesEvent) {
    if let Some(p) = unsafe { p.as_mut() } {
        if !p.is_null() {
            drop(unsafe { Box::from_raw(*p) });
            *p = ptr::null_mut();
        }
    }
}
fn view_string(s: &str) -> FtAffString {
    FtAffString {
        data: s.as_ptr(),
        len: s.len(),
    }
}
fn view_optional_string(s: &Option<String>) -> FtAffOptionalString {
    FtAffOptionalString {
        present: u32::from(s.is_some()),
        value: s.as_deref().map_or(FtAffString { data: ptr::null(), len: 0 }, view_string),
    }
}
fn view_number(n: Option<f64>) -> FtAffOptionalNumber {
    FtAffOptionalNumber {
        present: u32::from(n.is_some()),
        value: n.unwrap_or(0.),
    }
}
fn view_size(s: &Option<Size>) -> FtAffSize {
    FtAffSize {
        present: u32::from(s.is_some()),
        width: s.as_ref().map_or(0., |s| s.width),
        height: s.as_ref().map_or(0., |s| s.height),
    }
}
fn bits(c: &std::collections::BTreeMap<String, bool>, names: &[&str]) -> u32 {
    names
        .iter()
        .enumerate()
        .fold(0, |b, (i, n)| b | if c.get(*n) == Some(&true) { 1 << i } else { 0 })
}
fn view_axis(a: &Axis) -> FtAffAxis {
    FtAffAxis {
        scrollable: u32::from(a.scrollable),
        content_length: a.content_length,
        viewport_length: a.viewport_length,
        position: a.position,
    }
}
/// # Safety
/// Handles must be live and not concurrently destroyed. Pointer arguments and
/// outputs must be valid and disjoint; output handles start null. Borrowed
/// strings have their stated byte lengths. Event views last until destruction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_affordances_event_view(e: *const FtAffordancesEvent, out: *mut FtAffEventView) -> FtStatus {
    let (Some(e), Some(out)) = (unsafe { e.as_ref() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    // All fields are numeric or nullable pointers; all-zero is a valid empty view.
    *out = unsafe { std::mem::zeroed() };
    match &e.0 {
        Event::Closed => out.kind = 3,
        Event::Snapshot(s) => {
            out.kind = 1;
            out.snapshot.domain = s.domain() as u32;
            match s {
                Snapshot::Withdraw(_) => out.snapshot.withdrawn = 1,
                Snapshot::Media(m) => {
                    out.snapshot.media = FtAffMedia {
                        status: match m.status.as_str() {
                            "playing" => 1,
                            "paused" => 2,
                            "stopped" => 3,
                            "buffering" => 4,
                            _ => 0,
                        },
                        position: view_number(m.position),
                        rate: m.rate,
                        duration: view_number(m.duration),
                        title: view_optional_string(&m.title),
                        artwork: match &m.artwork {
                            None => FtAffArtwork {
                                kind: 0,
                                value: FtAffString { data: ptr::null(), len: 0 },
                            },
                            Some(Artwork::Url { url }) => FtAffArtwork {
                                kind: 1,
                                value: view_string(url),
                            },
                            Some(Artwork::Icon { name }) => FtAffArtwork {
                                kind: 2,
                                value: view_string(name),
                            },
                        },
                        capabilities: bits(&m.capabilities, MEDIA),
                    }
                }
                Snapshot::Navigation(n) => {
                    out.snapshot.navigation = FtAffNavigation {
                        url: view_optional_string(&n.url),
                        title: view_optional_string(&n.title),
                        can_go_back: u32::from(n.can_go_back),
                        can_go_forward: u32::from(n.can_go_forward),
                        loading: u32::from(n.loading),
                        capabilities: bits(&n.capabilities, NAV),
                    }
                }
                Snapshot::Cursor(s) => out.snapshot.cursor = CURSORS.iter().position(|c| *c == s).unwrap_or(1) as u32,
                Snapshot::Scroll(s) => {
                    out.snapshot.scroll = FtAffScroll {
                        x: view_axis(&s.x),
                        y: view_axis(&s.y),
                        capabilities: bits(&s.capabilities, SCROLL),
                    }
                }
                Snapshot::Window(w) => {
                    out.snapshot.window = FtAffWindow {
                        title: view_optional_string(&w.title),
                        requested_size: view_size(&w.requested_size),
                        ready: u32::from(w.ready),
                    }
                }
                Snapshot::Presentation(p) => {
                    out.snapshot.presentation = FtAffPresentation {
                        visible: u32::from(p.visible),
                        preferred_size: view_size(&p.preferred_size),
                        scale: p.scale,
                        focused: u32::from(p.focused),
                    }
                }
            }
        }
        Event::Verb(v) => {
            out.kind = 2;
            out.verb.domain = v.domain as u32;
            out.verb.verb = names(v.domain).iter().position(|n| *n == v.name).unwrap_or(0) as u32 + 1;
            out.verb.number = v.body["position"].as_f64().or(v.body["offset"].as_f64()).unwrap_or(0.);
            if let Some(s) = v.body["url"].as_str() {
                out.verb.url = view_string(s)
            }
            out.verb.axis = u32::from(v.body["axis"] == "y");
            out.verb.step = u32::from(v.body["step"] == "large");
            out.verb.direction = u32::from(v.body["direction"] == "increment");
        }
    }
    FT_STATUS_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    // ADR-0005 parity: all v1 snapshot variants and nullable states round-trip
    // through borrowed C views, including Unicode, cursor tags and withdrawal.
    #[test]
    fn every_snapshot_has_a_lossless_typed_c_view() {
        let axis = Axis {
            scrollable: true,
            content_length: 20.,
            viewport_length: 5.,
            position: 4.,
        };
        let mut snapshots = vec![
            Snapshot::Media(Media {
                status: "playing".into(),
                position: Some(5.),
                rate: -2.,
                duration: Some(20.),
                title: Some("🐈".into()),
                artwork: Some(Artwork::Icon { name: "audio".into() }),
                capabilities: caps(127, MEDIA),
            }),
            Snapshot::Navigation(Navigation {
                url: Some("https://example.invalid".into()),
                title: None,
                can_go_back: true,
                can_go_forward: false,
                loading: true,
                capabilities: caps(31, NAV),
            }),
            Snapshot::Scroll(Scroll {
                x: axis.clone(),
                y: axis,
                capabilities: caps(3, SCROLL),
            }),
            Snapshot::Window(Window {
                title: Some("title".into()),
                requested_size: Some(Size { width: 1., height: 2. }),
                ready: true,
            }),
            Snapshot::Presentation(Presentation::default()),
        ];
        for cursor in CURSORS {
            snapshots.push(Snapshot::Cursor((*cursor).into()));
        }
        for d in [
            Domain::Media,
            Domain::Navigation,
            Domain::Cursor,
            Domain::Scroll,
            Domain::Window,
            Domain::Presentation,
        ] {
            snapshots.push(Snapshot::Withdraw(d));
        }
        for s in snapshots {
            let event = FtAffordancesEvent(Event::Snapshot(s.clone()));
            let mut view: FtAffEventView = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { ft_affordances_event_view(&event, &mut view) }, FT_STATUS_OK);
            assert_eq!(unsafe { snapshot(&view.snapshot) }.unwrap(), s);
        }
    }
    // Every domain/verb pair retains its identity and typed body in C; media
    // and navigation stop must stay distinct despite sharing the word "stop".
    #[test]
    fn every_verb_has_a_typed_c_view() {
        for d in [Domain::Media, Domain::Navigation, Domain::Scroll] {
            for (i, name) in names(d).iter().enumerate() {
                let mut raw: FtAffVerb = unsafe { std::mem::zeroed() };
                raw.domain = d as u32;
                raw.verb = i as u32 + 1;
                raw.number = 4.;
                raw.url = view_string("untrusted:file:///tmp/a");
                raw.axis = 1;
                raw.step = 1;
                raw.direction = 1;
                let v = unsafe { verb(&raw) }.unwrap();
                assert_eq!(v.name, *name);
                let event = FtAffordancesEvent(Event::Verb(v.clone()));
                let mut view: FtAffEventView = unsafe { std::mem::zeroed() };
                assert_eq!(unsafe { ft_affordances_event_view(&event, &mut view) }, FT_STATUS_OK);
                assert_eq!(unsafe { verb(&view.verb) }.unwrap(), v);
            }
        }
    }
}
