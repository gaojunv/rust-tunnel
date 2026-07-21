pub mod client_registry;
pub mod tunnel_stream;

// Temporary: re-export everything from crate::server::control so external code
// like `crate::server::control::ServerState` remains usable until Task 3.4 fully
// migrates control.rs into control_plane/.
pub use crate::server::control::*;