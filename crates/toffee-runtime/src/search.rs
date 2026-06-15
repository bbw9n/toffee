//! Placeholder for future read-path orchestration.
//!
//! Today, [`crate::Runtime::search_memory`] does over-fetch + post-filter
//! inline. Phase 4 (`read_context`) will compose multiple index queries
//! (vector + entity + recent-event) here.
