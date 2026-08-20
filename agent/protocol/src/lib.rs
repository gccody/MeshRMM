//! Shared, platform-neutral protocol for PulseRMM view-only remote sessions.

pub mod control;
pub mod session;
pub mod signaling;
pub mod video;

pub use control::*;
pub use session::*;
pub use signaling::*;
pub use video::*;

pub const PROTOCOL_VERSION: u16 = 1;
