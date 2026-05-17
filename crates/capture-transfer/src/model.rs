#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(u64);

impl SourceId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TrackId(u64);

impl TrackId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(u64);

impl SessionId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FrameSequence(u64);

impl FrameSequence {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDesc {
    pub kind: SourceKind,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Window,
    Display,
    Surface,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackDesc {
    Video(VideoTrackDesc),
}

impl TrackDesc {
    #[must_use]
    pub const fn track_type(&self) -> TrackType {
        match self {
            Self::Video(_) => TrackType::Video,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoTrackDesc {
    pub width: u32,
    pub height: u32,
    pub pixel_format: PixelFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackType {
    Video,
    Audio,
    AccessibilityEvents,
    Metadata,
    InputCorrelation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadKind {
    CpuShm,
    IoSurface,
    DmaBuf,
    D3dSharedResource,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Unknown = 0,
    Bgra8Unorm = 1,
    Rgba8Unorm = 2,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSpace {
    Unknown = 0,
    Srgb = 1,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockDomain {
    Unknown = 0,
    UnixTime = 1,
    MediaTime = 2,
    HostTime = 3,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSyncKind {
    Unknown = 0,
    CpuCopyComplete = 1,
    SckSampleReady = 2,
    NativeTimeline = 3,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DamageKind {
    Unknown = 0,
    FullFrame = 1,
    None = 2,
    InlineRects = 3,
    SidecarRects = 4,
}

#[cfg(test)]
mod tests {
    use super::{ClockDomain, ColorSpace, DamageKind, FrameSyncKind, PixelFormat};

    #[test]
    fn frame_metadata_defaults_are_explicit() {
        assert_eq!(PixelFormat::Unknown as u32, 0);
        assert_eq!(PixelFormat::Bgra8Unorm as u32, 1);
        assert_eq!(PixelFormat::Rgba8Unorm as u32, 2);
        assert_eq!(ClockDomain::Unknown as u32, 0);
        assert_eq!(ClockDomain::UnixTime as u32, 1);
        assert_eq!(ClockDomain::MediaTime as u32, 2);
        assert_eq!(FrameSyncKind::CpuCopyComplete as u32, 1);
        assert_eq!(DamageKind::FullFrame as u32, 1);
        assert_eq!(ColorSpace::Unknown as u32, 0);
    }
}
