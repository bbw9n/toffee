//! Detect whether a candidate contradicts an existing active memory in the
//! same scope.
//!
//! Phase 1 rule: same `subject` + `predicate` in any shared scope, different
//! `object` → conflict. Episodes are excluded (they have no SPO).

use toffee_core::{Memory, MemoryCandidate};
use toffee_store::Store;

use crate::Result;

#[derive(Debug, Clone)]
pub struct ConflictFinding {
    /// The currently-active memories with the same SPO subject + predicate.
    pub competing: Vec<Memory>,
}

pub fn find(candidate: &MemoryCandidate, store: &Store) -> Result<Option<ConflictFinding>> {
    let (Some(subject), Some(predicate), Some(object)) = (
        candidate.subject.as_deref(),
        candidate.predicate.as_deref(),
        candidate.object.as_deref(),
    ) else {
        return Ok(None);
    };
    let scope_strings = candidate.scope.as_slice().to_vec();
    let matches = store.find_active_by_subject_predicate(&scope_strings, subject, predicate)?;
    let competing: Vec<Memory> = matches
        .into_iter()
        .filter(|m| m.object.as_deref() != Some(object))
        .collect();
    if competing.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ConflictFinding { competing }))
    }
}
