use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use toffee_core::{Entity, EntityId, EntityType, Memory, MemoryId};

use crate::{Result, Store, StoreError};

#[derive(Debug, Clone, Default)]
pub struct EntityListFilter {
    pub entity_type: Option<EntityType>,
    pub name_prefix: Option<String>,
    pub limit: Option<usize>,
}

impl Store {
    pub fn insert_entity(&self, entity: &Entity) -> Result<()> {
        let aliases_json = serde_json::to_string(&entity.aliases)?;
        self.with_conn(|conn| {
            conn.execute(
                r#"INSERT INTO entities
                    (id, entity_type, name, aliases_json, summary,
                     created_at, updated_at, deleted_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)"#,
                params![
                    entity.id.as_str(),
                    entity.entity_type.as_str(),
                    entity.name,
                    aliases_json,
                    entity.summary,
                    entity.created_at.to_rfc3339(),
                    entity.updated_at.to_rfc3339(),
                ],
            )?;
            Ok(())
        })
    }

    /// Find an entity whose name OR any alias matches `name` (case-insensitive)
    /// and is not deleted.
    pub fn find_entity_by_name(&self, name: &str) -> Result<Option<Entity>> {
        let needle = name.to_lowercase();
        self.with_conn(|conn| {
            // First exact name match.
            let exact = conn
                .query_row(
                    r#"SELECT id, entity_type, name, aliases_json, summary,
                              created_at, updated_at
                       FROM entities
                       WHERE LOWER(name) = ?1 AND deleted_at IS NULL"#,
                    params![needle],
                    row_to_entity,
                )
                .optional()?;
            if exact.is_some() {
                return Ok(exact);
            }
            // Alias scan (JSON arrays are tiny here).
            let mut stmt = conn.prepare(
                r#"SELECT id, entity_type, name, aliases_json, summary,
                          created_at, updated_at
                   FROM entities
                   WHERE aliases_json IS NOT NULL AND deleted_at IS NULL"#,
            )?;
            let candidates: Vec<Entity> = stmt
                .query_map([], row_to_entity)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for e in candidates {
                if e.aliases.iter().any(|a| a.to_lowercase() == needle) {
                    return Ok(Some(e));
                }
            }
            Ok(None)
        })
    }

    pub fn get_entity(&self, id: &EntityId) -> Result<Option<Entity>> {
        self.with_conn(|conn| {
            let row = conn
                .query_row(
                    r#"SELECT id, entity_type, name, aliases_json, summary,
                              created_at, updated_at
                       FROM entities WHERE id = ?1 AND deleted_at IS NULL"#,
                    params![id.as_str()],
                    row_to_entity,
                )
                .optional()?;
            Ok(row)
        })
    }

    pub fn list_entities(&self, filter: &EntityListFilter) -> Result<Vec<Entity>> {
        let mut sql = String::from(
            r#"SELECT id, entity_type, name, aliases_json, summary,
                      created_at, updated_at
               FROM entities
               WHERE deleted_at IS NULL "#,
        );
        if filter.entity_type.is_some() {
            sql.push_str("AND entity_type = ?1 ");
        }
        if filter.name_prefix.is_some() {
            sql.push_str(if filter.entity_type.is_some() {
                "AND name LIKE ?2 "
            } else {
                "AND name LIKE ?1 "
            });
        }
        sql.push_str("ORDER BY name ASC ");
        if let Some(limit) = filter.limit {
            sql.push_str(&format!("LIMIT {} ", limit));
        }

        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows: Vec<Entity> = match (&filter.entity_type, &filter.name_prefix) {
                (Some(t), Some(p)) => stmt
                    .query_map(params![t.as_str(), format!("{p}%")], row_to_entity)?
                    .collect::<std::result::Result<Vec<_>, _>>()?,
                (Some(t), None) => stmt
                    .query_map(params![t.as_str()], row_to_entity)?
                    .collect::<std::result::Result<Vec<_>, _>>()?,
                (None, Some(p)) => stmt
                    .query_map(params![format!("{p}%")], row_to_entity)?
                    .collect::<std::result::Result<Vec<_>, _>>()?,
                (None, None) => stmt
                    .query_map([], row_to_entity)?
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            };
            Ok(rows)
        })
    }

    pub fn entity_count_active(&self) -> Result<i64> {
        self.with_conn(|conn| {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM entities WHERE deleted_at IS NULL",
                [],
                |r| r.get(0),
            )?;
            Ok(n)
        })
    }

    pub fn soft_delete_entity(&self, id: &EntityId, when: DateTime<Utc>) -> Result<()> {
        self.with_conn(|conn| {
            let n = conn.execute(
                "UPDATE entities SET deleted_at = ?1, updated_at = ?1 WHERE id = ?2",
                params![when.to_rfc3339(), id.as_str()],
            )?;
            if n == 0 {
                return Err(StoreError::NotFound(format!("entity {id}")));
            }
            Ok(())
        })
    }

    // --- memory_entities link table ---------------------------------------

    pub fn link_memory_entity(&self, memory: &MemoryId, entity: &EntityId) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                r#"INSERT OR IGNORE INTO memory_entities (memory_id, entity_id)
                   VALUES (?1, ?2)"#,
                params![memory.as_str(), entity.as_str()],
            )?;
            Ok(())
        })
    }

    /// Entity ids linked to a memory. Cheaper than `entities_for_memory` —
    /// avoids hydrating the full Entity rows when callers only need ids
    /// (e.g. for populating `Memory.entities`).
    pub fn entity_ids_for_memory(&self, memory: &MemoryId) -> Result<Vec<String>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT entity_id FROM memory_entities WHERE memory_id = ?1 ORDER BY entity_id ASC",
            )?;
            let rows: Vec<String> = stmt
                .query_map(params![memory.as_str()], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Entities linked to a memory.
    pub fn entities_for_memory(&self, memory: &MemoryId) -> Result<Vec<Entity>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                r#"SELECT e.id, e.entity_type, e.name, e.aliases_json, e.summary,
                          e.created_at, e.updated_at
                   FROM entities e
                   JOIN memory_entities me ON me.entity_id = e.id
                   WHERE me.memory_id = ?1 AND e.deleted_at IS NULL
                   ORDER BY e.name ASC"#,
            )?;
            let rows: Vec<Entity> = stmt
                .query_map(params![memory.as_str()], row_to_entity)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Memories linked to an entity, filtered to scopes the caller cares
    /// about (any-of). Deleted and superseded memories are excluded.
    pub fn memories_for_entity(
        &self,
        entity: &EntityId,
        scope_any_of: Option<&[String]>,
        limit: usize,
    ) -> Result<Vec<Memory>> {
        // Reuse list_memories filter logic for the scope filtering instead of
        // shipping yet another bespoke SQL.
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                r#"SELECT m.id, m.kind, m.scope_json, m.subject, m.predicate, m.object,
                          m.text, m.confidence, m.source_event_ids_json,
                          m.created_at, m.updated_at, m.superseded_by, m.deleted_at
                   FROM memories m
                   JOIN memory_entities me ON me.memory_id = m.id
                   WHERE me.entity_id = ?1
                     AND m.deleted_at IS NULL AND m.superseded_by IS NULL
                   ORDER BY m.updated_at DESC, m.id DESC
                   LIMIT ?2"#,
            )?;
            let rows: Vec<Memory> = stmt
                .query_map(
                    params![entity.as_str(), limit as i64],
                    crate::memories::row_to_memory_public,
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let filtered = if let Some(scopes) = scope_any_of {
                rows.into_iter()
                    .filter(|m| {
                        let ms = m.scope.as_slice();
                        scopes.iter().any(|s| ms.iter().any(|x| x == s))
                    })
                    .collect()
            } else {
                rows
            };
            Ok(filtered)
        })
    }
}

fn row_to_entity(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entity> {
    let id: String = row.get(0)?;
    let entity_type_str: String = row.get(1)?;
    let name: String = row.get(2)?;
    let aliases_json: Option<String> = row.get(3)?;
    let summary: Option<String> = row.get(4)?;
    let created_at_str: String = row.get(5)?;
    let updated_at_str: String = row.get(6)?;

    let entity_type = EntityType::parse(&entity_type_str).unwrap_or(EntityType::Other);
    let aliases: Vec<String> = match aliases_json {
        None => vec![],
        Some(s) => serde_json::from_str(&s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
        })?,
    };
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
        })?;
    let updated_at = DateTime::parse_from_rfc3339(&updated_at_str)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
        })?;

    Ok(Entity {
        id: EntityId(id),
        entity_type,
        name,
        aliases,
        summary,
        created_at,
        updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use toffee_core::{Memory, MemoryKind, Scope};

    fn sample_entity(name: &str, t: EntityType) -> Entity {
        let now = Utc::now();
        Entity {
            id: EntityId::generate(),
            entity_type: t,
            name: name.to_string(),
            aliases: vec![],
            summary: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn insert_and_find_by_name_case_insensitive() {
        let store = Store::open_in_memory().unwrap();
        let ent = sample_entity("Pest", EntityType::Library);
        store.insert_entity(&ent).unwrap();
        let found = store.find_entity_by_name("pest").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, ent.id);
    }

    #[test]
    fn list_filters_by_type() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_entity(&sample_entity("Pest", EntityType::Library))
            .unwrap();
        store
            .insert_entity(&sample_entity("alice", EntityType::Person))
            .unwrap();
        let libs = store
            .list_entities(&EntityListFilter {
                entity_type: Some(EntityType::Library),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(libs.len(), 1);
        assert_eq!(libs[0].name, "Pest");
    }

    #[test]
    fn link_memory_to_entity_and_query_back() {
        let store = Store::open_in_memory().unwrap();
        let ent = sample_entity("Pest", EntityType::Library);
        store.insert_entity(&ent).unwrap();
        let now = Utc::now();
        let mem = Memory {
            id: MemoryId::generate(),
            kind: MemoryKind::Claim,
            scope: Scope::new(["project:test"]),
            text: "uses Pest".into(),
            subject: Some("parser".into()),
            predicate: Some("uses".into()),
            object: Some("Pest".into()),
            entities: vec![],
            confidence: 0.9,
            source_event_ids: vec![],
            created_at: now,
            updated_at: now,
            superseded_by: None,
        };
        store.insert_memory(&mem).unwrap();
        store.link_memory_entity(&mem.id, &ent.id).unwrap();

        let entities = store.entities_for_memory(&mem.id).unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].name, "Pest");

        let memories = store.memories_for_entity(&ent.id, None, 10).unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].id, mem.id);
    }

    #[test]
    fn duplicate_link_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        let ent = sample_entity("X", EntityType::Concept);
        store.insert_entity(&ent).unwrap();
        let now = Utc::now();
        let mem = Memory {
            id: MemoryId::generate(),
            kind: MemoryKind::Episode,
            scope: Scope::new(["g"]),
            text: "x".into(),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
            confidence: 0.5,
            source_event_ids: vec![],
            created_at: now,
            updated_at: now,
            superseded_by: None,
        };
        store.insert_memory(&mem).unwrap();
        store.link_memory_entity(&mem.id, &ent.id).unwrap();
        store.link_memory_entity(&mem.id, &ent.id).unwrap();
        let entities = store.entities_for_memory(&mem.id).unwrap();
        assert_eq!(entities.len(), 1);
    }
}
