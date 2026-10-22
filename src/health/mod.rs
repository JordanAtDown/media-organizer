//! Supervision: shared health state, an HTTP server, and a probe client.

pub mod state;

pub use state::{evaluate, HealthState, Status};
