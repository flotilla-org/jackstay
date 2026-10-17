//! C handshake v3: offers, selections and admissions as typed structs over
//! Local Endpoint connections, and notices and late selections while
//! attached. The CBOR messages never cross the ABI.
use std::{
    ffi::{CStr, CString, c_char},
    ptr, slice,
    time::Duration,
};

use crate::{
    audio::Descriptor,
    ffi::*,
    ffi_affordances::{FtAffordancesHost, FtAffordancesProducer},
    ffi_audio::{FtAudioDescriptor, FtPlayback, FtPlaybackDoorbell, FtPlaybackRing, destroy, emit},
    ffi_input::{self, FtInputClient, FtInputServer, FtInputTarget},
    ffi_local::{FtLocalConnection, restore_connection, take_connection},
    handshake::{self, Admission, Bytes, Direction, Kind, Notice, Notices, Offer, Offered, Refusal, Selected, Track, Tracks},
    input::Mode,
    local::PeerIdentity,
    model::ClockDomain,
};

pub const FT_TRACK_FROM_SOURCE: u32 = 0;
pub const FT_TRACK_TO_SOURCE: u32 = 1;
pub const FT_TRACK_BOTH: u32 = 2;

pub const FT_TRACK_ADMITTED: u32 = 0;
pub const FT_TRACK_REFUSED_UNKNOWN: u32 = 1;
pub const FT_TRACK_REFUSED_UNAVAILABLE: u32 = 2;
pub const FT_TRACK_REFUSED_NOT_WRITABLE: u32 = 3;

pub const FT_TRACK_NOTICE_ADDED: u32 = 1;
pub const FT_TRACK_NOTICE_REMOVED: u32 = 2;
pub const FT_TRACK_NOTICE_UPDATED: u32 = 3;

/// One offered track. Strings are NUL-terminated UTF-8.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtTrackOffer {
    pub id: u64,
    /// A core kind or a reverse-domain app kind.
    pub kind: *const c_char,
    pub label: *const c_char,
    /// App kinds only.
    pub descriptor: *const u8,
    pub descriptor_len: usize,
    pub direction: u32,
    pub reserved: u32,
    /// Playback only; zero otherwise.
    pub audio: FtAudioDescriptor,
}

impl Default for FtTrackOffer {
    fn default() -> Self {
        Self {
            id: 0,
            kind: ptr::null(),
            label: ptr::null(),
            descriptor: ptr::null(),
            descriptor_len: 0,
            direction: FT_TRACK_FROM_SOURCE,
            reserved: 0,
            audio: FtAudioDescriptor::default(),
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FtTrackNotice {
    /// One FT_TRACK_NOTICE_* value.
    pub r#type: u32,
    pub reserved: u32,
    pub track: FtTrackOffer,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FtTrackSelection {
    pub id: u64,
    pub required: u32,
    pub write: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FtTrackAdmission {
    pub id: u64,
    /// FT_TRACK_ADMITTED or one FT_TRACK_REFUSED_* value.
    pub result: u32,
    pub reserved: u32,
}

impl From<&FtTrackSelection> for Selected {
    fn from(s: &FtTrackSelection) -> Self {
        Self {
            id: s.id,
            required: s.required != 0,
            write: s.write != 0,
        }
    }
}

impl From<Admission> for FtTrackAdmission {
    fn from(admission: Admission) -> Self {
        Self {
            id: admission.id,
            result: match admission.refusal {
                None => FT_TRACK_ADMITTED,
                Some(Refusal::Unknown) => FT_TRACK_REFUSED_UNKNOWN,
                Some(Refusal::Unavailable) => FT_TRACK_REFUSED_UNAVAILABLE,
                Some(Refusal::NotWritable) => FT_TRACK_REFUSED_NOT_WRITABLE,
            },
            reserved: 0,
        }
    }
}

/// A received offer awaiting selection, with C views of its tracks.
pub struct FtHandshakeOffer {
    offered: Offered,
    peer: PeerIdentity,
    source: CString,
    _strings: Vec<CString>,
    tracks: Vec<FtTrackOffer>,
}

fn status(error: handshake::Error) -> FtStatus {
    match error {
        handshake::Error::Input(error) => ffi_input::status(error),
        handshake::Error::Refused { .. } => FT_STATUS_UNSUPPORTED,
        handshake::Error::Io(error) if error.kind() == std::io::ErrorKind::TimedOut => FT_STATUS_TIMEOUT,
        _ => FT_STATUS_ERROR,
    }
}

/// # Safety
/// `items` holds `count` live entries when `count` is non-zero.
unsafe fn items<'a, T>(items: *const T, count: usize) -> &'a [T] {
    // SAFETY: caller guarantees the entries; checked non-null.
    if count == 0 || items.is_null() {
        &[]
    } else {
        unsafe { slice::from_raw_parts(items, count) }
    }
}

/// # Safety
/// `text` is null or a live NUL-terminated string.
unsafe fn text(text: *const c_char) -> Option<String> {
    // SAFETY: caller guarantees a live NUL-terminated string when non-null.
    (!text.is_null()).then(|| unsafe { CStr::from_ptr(text) }.to_str().ok().map(str::to_owned))?
}

/// A playback track's descriptor comes from `audio`, encoded here.
///
/// # Safety
/// `track` fields reference live strings and `descriptor_len` readable bytes.
unsafe fn track(track: &FtTrackOffer) -> Option<Track> {
    // SAFETY: caller guarantees live strings.
    let kind = Kind::from(unsafe { text(track.kind) }?);
    let descriptor = match track.descriptor_len {
        0 if kind == Kind::Playback => flotilla_cbor::to_vec(&Descriptor::try_from(track.audio).ok()?, &handshake::LIMITS).ok()?,
        0 => Vec::new(),
        len if track.descriptor.is_null() || len > handshake::LIMITS.max_byte_string || kind == Kind::Playback => return None,
        // SAFETY: caller guarantees the readable byte range.
        len => unsafe { slice::from_raw_parts(track.descriptor, len) }.to_vec(),
    };
    Some(Track {
        id: track.id,
        kind,
        direction: match track.direction {
            FT_TRACK_FROM_SOURCE => Direction::FromSource,
            FT_TRACK_TO_SOURCE => Direction::ToSource,
            FT_TRACK_BOTH => Direction::Both,
            _ => return None,
        },
        // SAFETY: as above.
        label: unsafe { text(track.label) }?,
        descriptor: Bytes(descriptor),
    })
}

/// A C view of `track` whose strings `strings` keeps alive. A playback
/// descriptor is decoded into `audio`; its bytes stay hidden.
fn c_track(track: &Track, strings: &mut Vec<CString>) -> Option<FtTrackOffer> {
    let (kind, label) = (CString::new(track.kind.as_str()).ok()?, CString::new(track.label.as_str()).ok()?);
    let playback = track.kind == Kind::Playback;
    let view = FtTrackOffer {
        id: track.id,
        kind: kind.as_ptr(),
        label: label.as_ptr(),
        descriptor: if playback { ptr::null() } else { track.descriptor.0.as_ptr() },
        descriptor_len: if playback { 0 } else { track.descriptor.0.len() },
        direction: match track.direction {
            Direction::FromSource => FT_TRACK_FROM_SOURCE,
            Direction::ToSource => FT_TRACK_TO_SOURCE,
            Direction::Both => FT_TRACK_BOTH,
        },
        reserved: 0,
        audio: if playback {
            Descriptor::decode(&track.descriptor).ok()?.into()
        } else {
            Default::default()
        },
    };
    strings.extend([kind, label]);
    Some(view)
}

/// Offer the host-selected source's tracks and admit the peer's selection.
/// A non-null target authorizes input; NULL refuses any input track.
/// `out_admissions` holds `track_count` entries; `*out_admission_count` gets
/// the number written, in selection order. On success the same connection is
/// returned in `*connection`: serve CPU setup on it when a video track was
/// admitted. After basic checks the connection is consumed on failure,
/// including a required refusal (FT_STATUS_UNSUPPORTED).
///
/// # Safety
/// Pointers are valid and disjoint; `tracks` holds `track_count` live entries
/// and `playback` `playback_count` live handles; `*connection` is live and
/// exclusively owned; outputs start null and `out_notices` may be null; the
/// target is null or live until return and its executor keeps pumping.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_handshake_accept_local(
    connection: *mut *mut FtLocalConnection,
    source_label: *const c_char,
    clock_domain: u32,
    tracks: *const FtTrackOffer,
    track_count: usize,
    playback: *const *const FtPlayback,
    playback_count: usize,
    authorized_input: *mut FtInputTarget,
    out_admissions: *mut FtTrackAdmission,
    out_admission_count: *mut usize,
    out_input: *mut *mut FtInputServer,
    out_affordances: *mut *mut FtAffordancesProducer,
    out_notices: *mut *mut FtTrackNotices,
) -> FtStatus {
    // SAFETY: caller supplies valid, disjoint outputs.
    let (Some(count), Some(input), Some(affordances)) = (unsafe { out_admission_count.as_mut() }, unsafe { out_input.as_mut() }, unsafe {
        out_affordances.as_mut()
    }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let clock = match clock_domain {
        FT_CLOCK_DOMAIN_UNKNOWN => ClockDomain::Unknown,
        FT_CLOCK_DOMAIN_HOST_TIME => ClockDomain::HostTime,
        _ => return FT_STATUS_INVALID_ARGUMENT,
    };
    // SAFETY: null or writable.
    let mut notices = unsafe { out_notices.as_mut() };
    if !input.is_null()
        || !affordances.is_null()
        || notices.as_ref().is_some_and(|n| !n.is_null())
        || (track_count > 0 && (tracks.is_null() || out_admissions.is_null()))
    {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: caller guarantees `track_count` live entries.
    let entries = unsafe { items(tracks, track_count) };
    // SAFETY: caller guarantees `playback_count` live handles.
    let Some(rings) = unsafe { items(playback, playback_count) }
        .iter()
        .map(|p| unsafe { p.as_ref() }.map(|p| p.0.clone()))
        .collect::<Option<Vec<_>>>()
    else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    // SAFETY: entries reference live strings and descriptors.
    let (Some(source), Some(tracks)) = (unsafe { text(source_label) }, entries.iter().map(|t| unsafe { track(t) }).collect()) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let target = unsafe { authorized_input.as_ref() }.map(|target| target.0.clone());
    // SAFETY: the slot is valid; ownership moves exactly once.
    let Some(taken) = (unsafe { take_connection(connection) }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let FtLocalConnection { stream, peer, .. } = *taken;
    match handshake::accept(stream, &Offer { source, clock, tracks }, target, &rings) {
        Ok(accepted) => {
            for (index, admission) in accepted.admissions.iter().enumerate() {
                // SAFETY: the host refuses selections longer than its offer.
                unsafe { *out_admissions.add(index) = (*admission).into() };
            }
            *count = accepted.admissions.len();
            *input = accepted
                .input
                .map_or(ptr::null_mut(), |server| Box::into_raw(Box::new(FtInputServer { _server: server })));
            *affordances = accepted
                .affordances
                .map_or(ptr::null_mut(), |p| Box::into_raw(Box::new(FtAffordancesProducer(p))));
            if let Some(out) = notices.as_mut() {
                emit(out, FtTrackNotices(accepted.notices));
            }
            // SAFETY: take_connection nulled this valid slot above.
            unsafe {
                restore_connection(
                    connection,
                    FtLocalConnection {
                        stream: accepted.media,
                        peer,
                        video_bell: accepted.video_bell,
                    },
                )
            };
            FT_STATUS_OK
        }
        Err(error) => status(error),
    }
}

/// Receive the source's offer on a connection from `ft_local_connect`. After
/// basic checks the connection moves into `*out_offer` (nulled on success and
/// consumed on failure). The five-second bound covers the selection too.
///
/// # Safety
/// `*connection` is live and exclusively owned; `out_offer` is writable,
/// disjoint and starts null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_handshake_connect_local(
    connection: *mut *mut FtLocalConnection,
    out_offer: *mut *mut FtHandshakeOffer,
) -> FtStatus {
    // SAFETY: caller supplies valid output storage.
    let Some(out) = (unsafe { out_offer.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: the slot is valid; ownership moves exactly once.
    let Some(taken) = (unsafe { take_connection(connection) }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let FtLocalConnection { stream, peer, .. } = *taken;
    let offered = match handshake::connect(stream) {
        Ok(offered) => offered,
        Err(error) => return status(error),
    };
    let Ok(source) = CString::new(offered.offer().source.as_str()) else {
        return FT_STATUS_ERROR;
    };
    let mut strings = Vec::new();
    let Some(tracks) = offered.offer().tracks.iter().map(|t| c_track(t, &mut strings)).collect() else {
        return FT_STATUS_ERROR;
    };
    *out = Box::into_raw(Box::new(FtHandshakeOffer {
        offered,
        peer,
        source,
        _strings: strings,
        tracks,
    }));
    FT_STATUS_OK
}

/// Borrow the offer's source label and tracks, valid until the offer is
/// selected or destroyed, and copy its FT_CLOCK_DOMAIN_* value.
///
/// # Safety
/// `offer` is live; outputs are writable and disjoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_handshake_offer_describe(
    offer: *const FtHandshakeOffer,
    out_source_label: *mut *const c_char,
    out_clock_domain: *mut u32,
    out_tracks: *mut *const FtTrackOffer,
    out_track_count: *mut usize,
) -> FtStatus {
    // SAFETY: caller supplies live, disjoint pointers.
    let (Some(offer), Some(source), Some(clock), Some(tracks), Some(count)) = (
        unsafe { offer.as_ref() },
        unsafe { out_source_label.as_mut() },
        unsafe { out_clock_domain.as_mut() },
        unsafe { out_tracks.as_mut() },
        unsafe { out_track_count.as_mut() },
    ) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    *source = offer.source.as_ptr();
    *clock = offer.offered.offer().clock as u32;
    *tracks = offer.tracks.as_ptr();
    *count = offer.tracks.len();
    FT_STATUS_OK
}

/// Select tracks; `input_mode` is one FT_INPUT_MODE_* value, or zero when no
/// input track is selected. `out_admissions` receives `count` entries in
/// selection order. On success the media connection is returned in
/// `*out_connection` for CPU setup, and channels for admitted input and
/// affordances tracks in their outputs. `*out_input_status` is EMPTY without
/// input, OK with an admitted client, or an optional input's clean admission
/// rejection. After basic checks the offer is consumed and nulled on every
/// outcome; a required refusal fails with that status or FT_STATUS_UNSUPPORTED.
/// `out_playback` (`count` entries), `out_doorbell` and `out_tracks` may be
/// null to decline them.
///
/// # Safety
/// `*offer` is live and exclusively owned; `selections` holds `count` entries;
/// outputs are writable, disjoint and handle outputs start null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_handshake_select(
    offer: *mut *mut FtHandshakeOffer,
    selections: *const FtTrackSelection,
    count: usize,
    input_mode: u32,
    out_admissions: *mut FtTrackAdmission,
    out_connection: *mut *mut FtLocalConnection,
    out_input: *mut *mut FtInputClient,
    out_input_status: *mut FtStatus,
    out_affordances: *mut *mut FtAffordancesHost,
    out_playback: *mut *mut FtPlaybackRing,
    out_doorbell: *mut *mut FtPlaybackDoorbell,
    out_tracks: *mut *mut FtTracks,
) -> FtStatus {
    // SAFETY: null or writable, disjoint outputs.
    let (mut doorbell_out, mut tracks_out) = (unsafe { out_doorbell.as_mut() }, unsafe { out_tracks.as_mut() });
    // SAFETY: caller supplies valid, disjoint outputs.
    let (Some(slot), Some(connection), Some(input), Some(input_status), Some(affordances)) = (
        unsafe { offer.as_mut() },
        unsafe { out_connection.as_mut() },
        unsafe { out_input.as_mut() },
        unsafe { out_input_status.as_mut() },
        unsafe { out_affordances.as_mut() },
    ) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if slot.is_null()
        || !connection.is_null()
        || !input.is_null()
        || !affordances.is_null()
        || doorbell_out.as_ref().is_some_and(|d| !d.is_null())
        || tracks_out.as_ref().is_some_and(|t| !t.is_null())
        || (count > 0 && (selections.is_null() || out_admissions.is_null()))
    {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: checked non-null; the caller owns this live offer exclusively.
    let tracks = &unsafe { &**slot }.offered.offer().tracks;
    // SAFETY: caller guarantees `count` entries.
    let selection: Vec<Selected> = unsafe { items(selections, count) }.iter().map(Selected::from).collect();
    let input_selected = selection
        .iter()
        .any(|s| tracks.iter().any(|t| t.id == s.id && t.kind == Kind::Input));
    // Without input selected the mode is unused.
    let Some(mode) = ffi_input::mode(input_mode).or((!input_selected && input_mode == 0).then_some(Mode::Cooperative)) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    // SAFETY: this API allocated the box; ownership moves exactly once.
    let FtHandshakeOffer { offered, peer, .. } = *unsafe { Box::from_raw(std::mem::replace(slot, ptr::null_mut())) };
    *input_status = FT_STATUS_EMPTY;
    match offered.select(&selection, mode) {
        Ok(connected) => {
            for (index, admission) in connected.admissions.iter().enumerate() {
                // SAFETY: the reply answers exactly `count` selections.
                unsafe { *out_admissions.add(index) = (*admission).into() };
            }
            *input_status = connected.input_error.map_or(
                if connected.input.is_some() { FT_STATUS_OK } else { FT_STATUS_EMPTY },
                ffi_input::status,
            );
            *input = connected
                .input
                .map_or(ptr::null_mut(), |c| Box::into_raw(Box::new(FtInputClient(c))));
            *affordances = connected
                .affordances
                .map_or(ptr::null_mut(), |h| Box::into_raw(Box::new(FtAffordancesHost(h))));
            if !out_playback.is_null() {
                let mut rings = connected.playback.into_iter().peekable();
                for (index, selected) in selection.iter().enumerate() {
                    let ring = rings.next_if(|r| r.id() == selected.id);
                    let ring = ring.map_or(ptr::null_mut(), |r| Box::into_raw(Box::new(FtPlaybackRing(r))));
                    // SAFETY: the caller supplies `count` writable entries.
                    unsafe { *out_playback.add(index) = ring };
                }
            }
            if let (Some(out), Some(doorbell)) = (doorbell_out.as_mut(), connected.doorbell) {
                emit(out, FtPlaybackDoorbell(doorbell));
            }
            if let Some(out) = tracks_out.as_mut() {
                emit(out, FtTracks(connected.tracks, None, Vec::new()));
            }
            *connection = Box::into_raw(Box::new(FtLocalConnection {
                stream: connected.media,
                peer,
                video_bell: None,
            }));
            FT_STATUS_OK
        }
        Err(error) => status(error),
    }
}

/// Abandon an offer without selecting, closing its connection.
///
/// # Safety
/// `offer` is null or points to null or a live, exclusively owned offer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_handshake_offer_destroy(offer: *mut *mut FtHandshakeOffer) {
    // SAFETY: caller guarantees a valid slot; ownership moves exactly once.
    if let Some(slot) = unsafe { offer.as_mut() }
        && !slot.is_null()
    {
        drop(unsafe { Box::from_raw(std::mem::replace(slot, ptr::null_mut())) });
    }
}

/// A host's notices and late selections for one incarnation.
pub struct FtTrackNotices(Notices);

/// A consumer's tracks channel, and its last notice with that view's strings.
pub struct FtTracks(Tracks, Option<Notice>, Vec<CString>);

destroy!(ft_track_notices_destroy: FtTrackNotices, ft_tracks_destroy: FtTracks);

/// Validation failures of the host's own edits are argument errors.
fn edit(result: Result<(), handshake::Error>) -> FtStatus {
    match result {
        Ok(()) => FT_STATUS_OK,
        Err(handshake::Error::Protocol(_)) => FT_STATUS_INVALID_ARGUMENT,
        Err(error) => status(error),
    }
}

/// # Safety
/// `notices` is live with serialized calls; `track` references live strings
/// and bytes; `playback` is null or live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_track_notices_add(
    notices: *mut FtTrackNotices,
    track: *const FtTrackOffer,
    playback: *const FtPlayback,
) -> FtStatus {
    // SAFETY: caller supplies valid pointers and live track fields.
    let (Some(notices), Some(Some(track))) = (
        unsafe { notices.as_mut() },
        unsafe { track.as_ref() }.map(|t| unsafe { self::track(t) }),
    ) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    // SAFETY: null or live.
    edit(notices.0.add(track, unsafe { playback.as_ref() }.map(|p| p.0.clone())))
}

/// # Safety
/// As [`ft_track_notices_add`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_track_notices_update(notices: *mut FtTrackNotices, track: *const FtTrackOffer) -> FtStatus {
    // SAFETY: as above.
    let (Some(notices), Some(Some(track))) = (
        unsafe { notices.as_mut() },
        unsafe { track.as_ref() }.map(|t| unsafe { self::track(t) }),
    ) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    edit(notices.0.update(track))
}

/// # Safety
/// `notices` is live with serialized calls.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_track_notices_remove(notices: *mut FtTrackNotices, id: u64) -> FtStatus {
    // SAFETY: as above.
    let Some(notices) = (unsafe { notices.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    edit(notices.0.remove(id))
}

/// OK when a late selection was answered, EMPTY if none started in time.
///
/// # Safety
/// `notices` is live with serialized calls.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_track_notices_serve(notices: *mut FtTrackNotices, timeout_ms: u32) -> FtStatus {
    // SAFETY: as above.
    let Some(notices) = (unsafe { notices.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    match notices.0.serve(Duration::from_millis(timeout_ms.into())) {
        Ok(true) => FT_STATUS_OK,
        Ok(false) => FT_STATUS_EMPTY,
        Err(error) => status(error),
    }
}

/// The next notice, borrowed until the next call on `tracks`; EMPTY if none
/// arrives within `timeout_ms`.
///
/// # Safety
/// `tracks` is live with serialized calls; `out` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_tracks_next(tracks: *mut FtTracks, timeout_ms: u32, out: *mut FtTrackNotice) -> FtStatus {
    // SAFETY: caller supplies valid pointers.
    let (Some(FtTracks(tracks, last, strings)), Some(out)) = (unsafe { tracks.as_mut() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let notice = match tracks.next(Duration::from_millis(timeout_ms.into())) {
        Ok(Some(notice)) => last.insert(notice),
        Ok(None) => return FT_STATUS_EMPTY,
        Err(error) => return status(error),
    };
    strings.clear();
    let (r#type, track) = match notice {
        Notice::Added { track } => (FT_TRACK_NOTICE_ADDED, c_track(track, strings)),
        Notice::Updated { track } => (FT_TRACK_NOTICE_UPDATED, c_track(track, strings)),
        Notice::Removed { id } => (
            FT_TRACK_NOTICE_REMOVED,
            Some(FtTrackOffer {
                id: *id,
                ..Default::default()
            }),
        ),
    };
    let Some(track) = track else {
        return FT_STATUS_ERROR;
    };
    *out = FtTrackNotice {
        r#type,
        reserved: 0,
        track,
    };
    FT_STATUS_OK
}

/// # Safety
/// `tracks` is live with serialized calls; pointers are valid and disjoint;
/// handle outputs start null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_tracks_select(
    tracks: *mut FtTracks,
    selection: *const FtTrackSelection,
    out_admission: *mut FtTrackAdmission,
    out_position: *mut u32,
    out_ring: *mut *mut FtPlaybackRing,
    out_doorbell: *mut *mut FtPlaybackDoorbell,
) -> FtStatus {
    // SAFETY: caller supplies valid, disjoint pointers.
    let (Some(tracks), Some(selection), Some(admission), Some(position), Some(ring), Some(doorbell)) = (
        unsafe { tracks.as_mut() },
        unsafe { selection.as_ref() },
        unsafe { out_admission.as_mut() },
        unsafe { out_position.as_mut() },
        unsafe { out_ring.as_mut() },
        unsafe { out_doorbell.as_mut() },
    ) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !ring.is_null() || !doorbell.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    match tracks.0.select(selection.into()) {
        Ok(late) => {
            *admission = late.admission.into();
            *position = late.position as u32;
            if let Some(r) = late.playback {
                emit(ring, FtPlaybackRing(r));
            }
            if let Some(d) = late.doorbell {
                emit(doorbell, FtPlaybackDoorbell(d));
            }
            FT_STATUS_OK
        }
        Err(error) => status(error),
    }
}
