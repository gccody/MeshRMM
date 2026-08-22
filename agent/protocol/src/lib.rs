//! Shared, platform-neutral protocol for MeshRMM remote-control sessions.

pub mod control;
pub mod input;
pub mod session;
pub mod video;

pub use control::*;
pub use input::*;
pub use meshrmm_protocol_types::*;
pub use session::*;
pub use video::*;

pub const PROTOCOL_VERSION: u16 = 2;
