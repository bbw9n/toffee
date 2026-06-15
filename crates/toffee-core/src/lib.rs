//! Pure types and pure functions for toffee.
//!
//! No I/O, no async. Everything in this crate is `Send + Sync` and trivial to
//! construct in tests.

pub mod event;
pub mod paths;
pub mod scope;

pub use event::{Actor, Event, EventId, EventInput};
pub use scope::Scope;
