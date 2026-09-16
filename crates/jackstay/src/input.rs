//! Ordered, host-authorized input with explicit executor completion.
//!
//! Targets and controllers are thread-safe. Executors serialize `next`/`complete`:
//! work remains in flight until completed, including after controller disconnect.
mod model;
mod session;
pub use model::*;
pub use session::{Controller, Target};
#[cfg(unix)]
pub mod transport;
