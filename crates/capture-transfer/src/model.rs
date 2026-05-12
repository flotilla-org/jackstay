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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra8Unorm,
    Rgba8Unorm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSpace {
    Unknown,
    Srgb,
}
