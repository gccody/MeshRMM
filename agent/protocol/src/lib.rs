//! Shared, platform-neutral protocol for PulseRMM remote-control sessions.

pub mod control;
pub mod input;
pub mod session;
pub mod signaling;
pub mod video;

pub use control::*;
pub use input::*;
pub use session::*;
pub use signaling::*;
pub use video::*;

pub const PROTOCOL_VERSION: u16 = 2;
