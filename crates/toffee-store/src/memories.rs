use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use toffee_core::{EventId, Memory, MemoryId, MemoryKind, Scope};

use crate::{Result, Store, StoreError};

#[derive(Debug, Default, Clone)]
pub struct MemoryListFilter {
    pub scope_any_of: Option<Vec<String>>,
    pub kind: Option<MemoryKind>,
    pub include_deleted: bool,
    pub include_superseded: bool,
    pub limit: Option<usize>,
}

impl Store {
    /// Insert a new memory verbatim. The caller is responsible for setting
    /// `created_at == updated_at` on fresh inserts.
    pub fn insert_memory(&self, memory: &Memory) -> Result<()> {
        let scope_json = serde_json::to_string(&memory.scope)?;
        let source_ids: Vec<String> = memory
            .source_event_ids
            .iter()
            .map(|e| e.0.clone())
            .collect();
        let source_ids_json = serde_json::to_string(&source_ids)?;
        self.with_conn(|conn| {
            conn.execute(
                r#"INSERT INTO memories
                    (id, kind, scope_json, subject, predicate, object, text,
                     confidence, source_event_ids_json,
                     created_at, updated_at, superseded_by, deleted_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, NULL)"#,
                params![
                    memory.id.as_str(),
                    memory.kind.as_str(),
                    scope_json,
                    memory.subject,
                    memory.predicate,
                    memory.object,
                    memory.text,
                    memory.confidence,
                    source_ids_json,
                    memory.created_at.to_rfc3339(),
                    memory.updated_at.to_rfc3339(),
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_memory(&self, id: &MemoryId) -> Result<Option<Memory>> {
        let row: Option<Memory> = self.with_conn(|conn| {
            let row = conn
                .query_row(
                    r#"SELECT id, kind, scope_json, subject, predicate, object, text,
                              confidence, source_event_ids_json,
                              created_at, updated_at, superseded_by, deleted_at
                       FROM memories WHERE id = ?1"#,
                    params![id.as_str()],
                    row_to_memory,
                )
                .optional()?;
            Ok(row)
        })?;
        match row {
            None => Ok(None),
            Some(mut m) => {
                m.entities = self.entity_ids_for_memory(&m.id)?;
                Ok(Some(m))
            }
        }
    }

    pub fn list_memories(&self, filter: &MemoryListFilter) -> Result<Vec<Memory>> {
        let mut sql = String::from(
            r#"SELECT id, kind, scope_json, subject, predicate, object, text,
                      confidence, source_event_ids_json,
                      created_at, updated_at, superseded_by, deleted_at
               FROM memories
               WHERE 1=1 "#,
        );
        if !filter.include_deleted {
            sql.push_str("AND deleted_at IS NULL ");
        }
        if !filter.include_superseded {
            sql.push_str("AND superseded_by IS NULL ");
        }
        if filter.kind.is_some() {
            sql.push_str("AND kind = ?1 ");
        }
        sql.push_str("ORDER BY updated_at DESC, id DESC ");
        if let Some(limit) = filter.limit {
            sql.push_str(&format!("LIMIT {} ", limit));
        }

        let mut filtered: Vec<Memory> = self.with_conn(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows: Vec<Memory> = if let Some(kind) = filter.kind.as_ref() {
                stmt.query_map(params![kind.as_str()], row_to_memory)?
                    .collect::<std::result::Result<Vec<_>, _>>()?
            } else {
                stmt.query_map([], row_to_memory)?
                    .collect::<std::result::Result<Vec<_>, _>>()?
            };
            let filtered = if let Some(scopes) = &filter.scope_any_of {
                rows.into_iter()
                    .filter(|m| {
                        let memory_scopes = m.scope.as_slice();
                        scopes
                            .iter()
                            .any(|s| memory_scopes.iter().any(|ms| ms == s))
                    })
                    .collect()
            } else {
                rows
            };
            Ok(filtered)
        })?;
        for m in &mut filtered {
            m.entities = self.entity_ids_for_memory(&m.id)?;
        }
        Ok(filtered)
    }

    pub fn update_memory_confidence(
        &self,
        id: &MemoryId,
        confidence: f64,
        updated_at: DateTime<Utc>,
    ) -> Result<()> {
        self.with_conn(|conn| {
            let n = conn.execute(
                "UPDATE memories SET confidence = ?1, updated_at = ?2 WHERE id = ?3",
                params![confidence, updated_at.to_rfc3339(), id.as_str()],
            )?;
            if n == 0 {
                return Err(StoreError::NotFound(format!("memory {id}")));
            }
            Ok(())
        })
    }

    pub fn soft_delete_memory(&self, id: &MemoryId, deleted_at: DateTime<Utc>) -> Result<()> {
        self.with_conn(|conn| {
            let n = conn.execute(
                "UPDATE memories SET deleted_at = ?1, updated_at = ?1 WHERE id = ?2",
                params![deleted_at.to_rfc3339(), id.as_str()],
            )?;
            if n == 0 {
                return Err(StoreError::NotFound(format!("memory {id}")));
            }
            Ok(())
        })
    }

    pub fn supersede_memory(
        &self,
        old: &MemoryId,
        new: &MemoryId,
        when: DateTime<Utc>,
    ) -> Result<()> {
        self.with_conn(|conn| {
            let n = conn.execute(
                "UPDATE memories SET superseded_by = ?1, updated_at = ?2 WHERE id = ?3",
                params![new.as_str(), when.to_rfc3339(), old.as_str()],
            )?;
            if n == 0 {
                return Err(StoreError::NotFound(format!("memory {old}")));
            }
            Ok(())
        })
    }

    /// Find active (non-deleted, non-superseded) memories whose
    /// `(subject, predicate)` match exactly. Used by the conflict detector.
    pub fn find_active_by_subject_predicate(
        &self,
        scope_any_of: &[String],
        subject: &str,
        predicate: &str,
    ) -> Result<Vec<Memory>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                r#"SELECT id, kind, scope_json, subject, predicate, object, text,
                          confidence, source_event_ids_json,
                          created_at, updated_at, superseded_by, deleted_at
                   FROM memories
                   WHERE subject = ?1 AND predicate = ?2
                     AND deleted_at IS NULL AND superseded_by IS NULL"#,
            )?;
            let candidates: Vec<Memory> = stmt
                .query_map(params![subject, predicate], row_to_memory)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let matched = candidates
                .into_iter()
                .filter(|m| {
                    let ms = m.scope.as_slice();
                    scope_any_of.iter().any(|s| ms.iter().any(|x| x == s))
                })
                .collect();
            Ok(matched)
        })
    }

    pub fn memory_count_active(&self) -> Result<i64> {
        self.with_conn(|conn| {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM memories WHERE deleted_at IS NULL AND superseded_by IS NULL",
                [],
                |r| r.get(0),
            )?;
            Ok(n)
        })
    }
}

/// Re-exported for sibling store modules (`entities`) that need to hydrate
/// memories from a JOIN query.
pub(crate) fn row_to_memory_public(row: &rusqlite::Row<'_>) -> rusqlite::Result<Memory> {
    row_to_memory(row)
}

fn row_to_memory(row: &rusqlite::Row<'_>) -> rusqlite::Result<Memory> {
    let id: String = row.get(0)?;
    let kind_str: String = row.get(1)?;
    let scope_json: String = row.get(2)?;
    let subject: Option<String> = row.get(3)?;
    let predicate: Option<String> = row.get(4)?;
    let object: Option<String> = row.get(5)?;
    let text: String = row.get(6)?;
    let confidence: f64 = row.get(7)?;
    let source_event_ids_json: String = row.get(8)?;
    let created_at_str: String = row.get(9)?;
    let updated_at_str: String = row.get(10)?;
    let superseded_by: Option<String> = row.get(11)?;
    let _deleted_at: Option<String> = row.get(12)?;

    let kind = MemoryKind::parse(&kind_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            Box::<dyn std::error::Error + Send + Sync>::from(format!("unknown kind: {kind_str}")),
        )
    })?;
    let scope: Scope = serde_json::from_str(&scope_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let source_event_ids_raw: Vec<String> =
        serde_json::from_str(&source_event_ids_json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(e))
        })?;
    let source_event_ids: Vec<EventId> = source_event_ids_raw.into_iter().map(EventId).collect();
    let created_at = parse_ts(&created_at_str, 9)?;
    let updated_at = parse_ts(&updated_at_str, 10)?;

    Ok(Memory {
        id: MemoryId(id),
        kind,
        scope,
        text,
        subject,
        predicate,
        object,
        entities: vec![],
        confidence,
        source_event_ids,
        created_at,
        updated_at,
        superseded_by: superseded_by.map(MemoryId),
    })
}

fn parse_ts(s: &str, col: usize) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, Box::new(e))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use toffee_core::{MemoryId, MemoryKind, Scope};

    fn sample(
        kind: MemoryKind,
        subject: Option<&str>,
        predicate: Option<&str>,
        object: Option<&str>,
    ) -> Memory {
        let now = Utc::now();
        Memory {
            id: MemoryId::generate(),
            kind,
            scope: Scope::new(["project:test"]),
            text: "the parser uses pest".into(),
            subject: subject.map(String::from),
            predicate: predicate.map(String::from),
            object: object.map(String::from),
            entities: vec![],
            confidence: 0.9,
            source_event_ids: vec![],
            created_at: now,
            updated_at: now,
            superseded_by: None,
        }
    }

    #[test]
    fn insert_and_get() {
        let store = Store::open_in_memory().unwrap();
        let mem = sample(
            MemoryKind::Claim,
            Some("parser"),
            Some("uses"),
            Some("pest"),
        );
        store.insert_memory(&mem).unwrap();
        let back = store.get_memory(&mem.id).unwrap().unwrap();
        assert_eq!(back.kind, MemoryKind::Claim);
        assert_eq!(back.confidence, 0.9);
    }

    #[test]
    fn list_filters_by_kind_and_scope() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_memory(&sample(MemoryKind::Claim, Some("a"), Some("b"), Some("c")))
            .unwrap();
        store
            .insert_memory(&sample(
                MemoryKind::Decision,
                Some("a"),
                Some("b"),
                Some("c"),
            ))
            .unwrap();
        let claims = store
            .list_memories(&MemoryListFilter {
                kind: Some(MemoryKind::Claim),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(claims.len(), 1);
        let in_scope = store
            .list_memories(&MemoryListFilter {
                scope_any_of: Some(vec!["project:test".into()]),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(in_scope.len(), 2);
        let other = store
            .list_memories(&MemoryListFilter {
                scope_any_of: Some(vec!["project:other".into()]),
                ..Default::default()
            })
            .unwrap();
        assert!(other.is_empty());
    }

    #[test]
    fn confidence_update_and_soft_delete() {
        let store = Store::open_in_memory().unwrap();
        let mem = sample(MemoryKind::Claim, Some("a"), Some("b"), Some("c"));
        let id = mem.id.clone();
        store.insert_memory(&mem).unwrap();
        store
            .update_memory_confidence(&id, 0.42, Utc::now())
            .unwrap();
        let back = store.get_memory(&id).unwrap().unwrap();
        assert!((back.confidence - 0.42).abs() < 1e-9);

        assert_eq!(store.memory_count_active().unwrap(), 1);
        store.soft_delete_memory(&id, Utc::now()).unwrap();
        assert_eq!(store.memory_count_active().unwrap(), 0);
        // Default list excludes deleted.
        let listed = store.list_memories(&MemoryListFilter::default()).unwrap();
        assert!(listed.is_empty());
    }

    #[test]
    fn find_by_subject_predicate_respects_scope() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_memory(&sample(
                MemoryKind::Claim,
                Some("parser"),
                Some("uses"),
                Some("pest"),
            ))
            .unwrap();
        let hits = store
            .find_active_by_subject_predicate(&["project:test".to_string()], "parser", "uses")
            .unwrap();
        assert_eq!(hits.len(), 1);
        let miss = store
            .find_active_by_subject_predicate(&["project:other".to_string()], "parser", "uses")
            .unwrap();
        assert!(miss.is_empty());
    }
}
