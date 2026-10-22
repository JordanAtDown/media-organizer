//! Supervision: shared health state, an HTTP server, and a probe client.

pub mod client;
pub mod server;
pub mod state;

pub use server::HealthServer;
pub use state::{evaluate, HealthState, Status};
