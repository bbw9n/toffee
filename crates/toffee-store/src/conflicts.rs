use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use toffee_core::{ConflictId, ConflictResolution, MemoryConflict, MemoryId, Scope};

use crate::{Result, Store, StoreError};

impl Store {
    pub fn insert_conflict(&self, conflict: &MemoryConflict) -> Result<()> {
        let scope_json = serde_json::to_string(&conflict.scope)?;
        let ids: Vec<String> = conflict
            .competing_memory_ids
            .iter()
            .map(|m| m.0.clone())
            .collect();
        let ids_json = serde_json::to_string(&ids)?;
        self.with_conn(|conn| {
            conn.execute(
                r#"INSERT INTO conflicts
                    (id, scope_json, subject, predicate,
                     competing_memory_ids_json, resolution, created_at, resolved_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#,
                params![
                    conflict.id.as_str(),
                    scope_json,
                    conflict.subject,
                    conflict.predicate,
                    ids_json,
                    conflict.resolution.as_str(),
                    conflict.created_at.to_rfc3339(),
                    conflict.resolved_at.map(|t| t.to_rfc3339()),
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_conflict(&self, id: &ConflictId) -> Result<Option<MemoryConflict>> {
        self.with_conn(|conn| {
            let row = conn
                .query_row(
                    r#"SELECT id, scope_json, subject, predicate,
                              competing_memory_ids_json, resolution,
                              created_at, resolved_at
                       FROM conflicts WHERE id = ?1"#,
                    params![id.as_str()],
                    row_to_conflict,
                )
                .optional()?;
            Ok(row)
        })
    }

    pub fn list_unresolved_conflicts(&self) -> Result<Vec<MemoryConflict>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                r#"SELECT id, scope_json, subject, predicate,
                          competing_memory_ids_json, resolution,
                          created_at, resolved_at
                   FROM conflicts WHERE resolution = 'unresolved'
                   ORDER BY created_at ASC"#,
            )?;
            let rows = stmt
                .query_map([], row_to_conflict)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn list_conflicts(&self, include_resolved: bool) -> Result<Vec<MemoryConflict>> {
        self.with_conn(|conn| {
            let sql = if include_resolved {
                r#"SELECT id, scope_json, subject, predicate,
                          competing_memory_ids_json, resolution,
                          created_at, resolved_at
                   FROM conflicts ORDER BY created_at ASC"#
            } else {
                r#"SELECT id, scope_json, subject, predicate,
                          competing_memory_ids_json, resolution,
                          created_at, resolved_at
                   FROM conflicts WHERE resolution = 'unresolved'
                   ORDER BY created_at ASC"#
            };
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt
                .query_map([], row_to_conflict)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Find an existing unresolved conflict whose `(subject, predicate)`
    /// matches and whose scope intersects the candidate's scope. Used by
    /// the worker to dedup repeated contradictions into a single row.
    pub fn find_unresolved_conflict_for_spo(
        &self,
        scope_any_of: &[String],
        subject: &str,
        predicate: &str,
    ) -> Result<Option<MemoryConflict>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                r#"SELECT id, scope_json, subject, predicate,
                          competing_memory_ids_json, resolution,
                          created_at, resolved_at
                   FROM conflicts
                   WHERE resolution = 'unresolved'
                     AND subject = ?1 AND predicate = ?2
                   ORDER BY created_at ASC"#,
            )?;
            let rows: Vec<MemoryConflict> = stmt
                .query_map(params![subject, predicate], row_to_conflict)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for c in rows {
                let cs = c.scope.as_slice();
                if scope_any_of.iter().any(|s| cs.iter().any(|x| x == s)) {
                    return Ok(Some(c));
                }
            }
            Ok(None)
        })
    }

    /// Append `new_id` to the conflict's competing list. Idempotent — if the
    /// id is already present, this is a no-op.
    pub fn extend_conflict(
        &self,
        conflict_id: &ConflictId,
        new_id: &toffee_core::MemoryId,
    ) -> Result<()> {
        self.with_conn(|conn| {
            let existing: Option<String> = conn
                .query_row(
                    "SELECT competing_memory_ids_json FROM conflicts WHERE id = ?1",
                    params![conflict_id.as_str()],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(json) = existing else {
                return Err(StoreError::NotFound(format!("conflict {conflict_id}")));
            };
            let mut ids: Vec<String> = serde_json::from_str(&json)?;
            if !ids.iter().any(|s| s == new_id.as_str()) {
                ids.push(new_id.0.clone());
            }
            let serialized = serde_json::to_string(&ids)?;
            conn.execute(
                "UPDATE conflicts SET competing_memory_ids_json = ?1 WHERE id = ?2",
                params![serialized, conflict_id.as_str()],
            )?;
            Ok(())
        })
    }

    pub fn resolve_conflict(
        &self,
        id: &ConflictId,
        resolution: ConflictResolution,
        resolved_at: DateTime<Utc>,
    ) -> Result<()> {
        self.with_conn(|conn| {
            let n = conn.execute(
                "UPDATE conflicts SET resolution = ?1, resolved_at = ?2 WHERE id = ?3",
                params![resolution.as_str(), resolved_at.to_rfc3339(), id.as_str()],
            )?;
            if n == 0 {
                return Err(StoreError::NotFound(format!("conflict {id}")));
            }
            Ok(())
        })
    }
}

fn row_to_conflict(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryConflict> {
    let id: String = row.get(0)?;
    let scope_json: String = row.get(1)?;
    let subject: Option<String> = row.get(2)?;
    let predicate: Option<String> = row.get(3)?;
    let competing_ids_json: String = row.get(4)?;
    let resolution_str: String = row.get(5)?;
    let created_at_str: String = row.get(6)?;
    let resolved_at_str: Option<String> = row.get(7)?;

    let scope: Scope = serde_json::from_str(&scope_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let competing_raw: Vec<String> = serde_json::from_str(&competing_ids_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let competing_memory_ids: Vec<MemoryId> = competing_raw.into_iter().map(MemoryId).collect();
    let resolution = ConflictResolution::parse(&resolution_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Text,
            Box::<dyn std::error::Error + Send + Sync>::from(format!(
                "unknown resolution: {resolution_str}"
            )),
        )
    })?;
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
        })?;
    let resolved_at = match resolved_at_str {
        None => None,
        Some(s) => Some(
            DateTime::parse_from_rfc3339(&s)
                .map(|d| d.with_timezone(&Utc))
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        7,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?,
        ),
    };

    Ok(MemoryConflict {
        id: ConflictId(id),
        scope,
        subject,
        predicate,
        competing_memory_ids,
        resolution,
        created_at,
        resolved_at,
    })
}
