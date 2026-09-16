use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    Physical,
    SourceText,
    Cooperative,
}
impl Mode {
    pub fn bit(self) -> u32 {
        match self {
            Self::Physical => 1,
            Self::SourceText => 2,
            Self::Cooperative => 4,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Down,
    Up,
    Repeat,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Key {
    Physical(String),
    Logical(String),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollUnit {
    Pixel,
    Line,
    Page,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub revision: u64,
    pub x: f64,
    pub y: f64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    Key {
        press: u64,
        action: Action,
        key: Key,
        modifiers: u32,
    },
    Text(String),
    Motion(Position),
    Button {
        button: u32,
        action: Action,
        position: Position,
    },
    Scroll {
        x: f64,
        y: f64,
        unit: ScrollUnit,
        position: Position,
    },
}
impl Event {
    pub fn bytes(&self) -> usize {
        96 + match self {
            Self::Text(s) => s.len(),
            Self::Key {
                key: Key::Physical(s) | Key::Logical(s),
                ..
            } => s.len(),
            _ => 0,
        }
    }
    pub fn pointer(&self) -> bool {
        matches!(self, Self::Motion(_) | Self::Button { .. } | Self::Scroll { .. })
    }
}
pub const CAP_PHYSICAL: u32 = 1;
pub const CAP_LOGICAL: u32 = 2;
pub const CAP_TEXT: u32 = 4;
pub const CAP_POINTER: u32 = 8;
pub const CAP_SCROLL: u32 = 16;
pub const CAP_ALL: u32 = 31;
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Geometry {
    pub revision: u64,
    pub width: f64,
    pub height: f64,
}
impl Geometry {
    pub fn valid(self) -> bool {
        self.revision != 0 && self.width.is_finite() && self.height.is_finite() && self.width > 0.0 && self.height > 0.0
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub modes: u32,
    pub capabilities: u32,
    pub max_events: usize,
    pub max_bytes: usize,
    pub max_text_bytes: usize,
    pub idle_timeout: Duration,
    pub geometry: Geometry,
    pub independent_contributions: bool,
    pub interaction_cancel: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            modes: 4,
            capabilities: CAP_ALL,
            max_events: 256,
            max_bytes: 256 * 1024,
            max_text_bytes: 16 * 1024,
            idle_timeout: Duration::from_secs(5),
            geometry: Geometry {
                revision: 1,
                width: 320.0,
                height: 180.0,
            },
            independent_contributions: false,
            interaction_cancel: false,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Error {
    Invalid,
    Unsupported,
    Busy,
    Closed,
    Stale,
    Overflow,
    CleanupFailed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Executed,
    Rejected,
    Unsupported,
    Partial,
    Uncertain,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    All,
    Pointer,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reason {
    Focus,
    Geometry,
    Disconnect,
    Expired,
    Overflow,
    Execution,
}
#[derive(Debug, Clone, PartialEq)]
pub enum Operation {
    Event(Event),
    Cleanup { scope: Scope, reason: Reason },
}
#[derive(Debug, Clone, PartialEq)]
pub struct Work {
    pub mode: Mode,
    pub id: u64,
    pub controller: u64,
    pub epoch: u64,
    pub sequence: u64,
    pub operation: Operation,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Status {
    Completed { sequence: u64, outcome: Outcome },
    Rejected { sequence: u64, error: Error },
    Reset { epoch: u64, geometry: Geometry },
    Closed { reason: Reason, clean: bool },
}
