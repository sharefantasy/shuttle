//! Durable Goal control primitives for shuttle.
//!
//! The local backend is a standalone, single-host authority backed by an
//! append-only SQLite journal. It deliberately does not claim distributed
//! consensus, remote write authority, or compatibility with an unavailable
//! external backend. Every mutation is revision-CAS guarded and controller
//! writes are additionally protected by a monotonically increasing fencing
//! epoch.

mod artifact;
mod contracts;
mod external;
mod store;

pub use artifact::{ArtifactRef, ArtifactStore};
pub use contracts::*;
pub use external::*;
pub use store::{LocalGoalStore, StorePaths};
