use chrono::{DateTime, Utc};
use rusqlite::params;
use toffee_core::{MemoryId, MemoryKind, Scope};

use crate::{Result, Store};

/// One row from the `embeddings` table.
#[derive(Debug, Clone)]
pub struct EmbeddingRow {
    pub seq_id: i64,
    pub memory_id: MemoryId,
    pub scope: Scope,
    pub kind: MemoryKind,
    pub model: String,
    pub vector: Vec<f32>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewEmbedding {
    pub memory_id: MemoryId,
    pub scope: Scope,
    pub kind: MemoryKind,
    pub model: String,
    pub vector: Vec<f32>,
}

impl Store {
    /// Insert one embedding row. Returns the assigned `seq_id`.
    pub fn insert_embedding(&self, e: &NewEmbedding) -> Result<i64> {
        let scope_json = serde_json::to_string(&e.scope)?;
        let bytes = f32_vec_to_bytes(&e.vector);
        let created = Utc::now().to_rfc3339();
        self.with_conn(|conn| {
            conn.execute(
                r#"INSERT INTO embeddings
                    (memory_id, scope_json, kind, model, vector, created_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6)"#,
                params![
                    e.memory_id.as_str(),
                    scope_json,
                    e.kind.as_str(),
                    e.model,
                    bytes,
                    created,
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn embedding_count(&self) -> Result<i64> {
        self.with_conn(|conn| {
            let n: i64 = conn.query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))?;
            Ok(n)
        })
    }

    /// Iterate all embeddings under a given model, in ascending `seq_id`
    /// order. Used by the daemon to rebuild the in-memory HNSW index.
    pub fn list_embeddings_for_model(&self, model: &str) -> Result<Vec<EmbeddingRow>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                r#"SELECT seq_id, memory_id, scope_json, kind, model, vector, created_at
                   FROM embeddings WHERE model = ?1 ORDER BY seq_id ASC"#,
            )?;
            let rows: Vec<EmbeddingRow> = stmt
                .query_map(params![model], row_to_embedding)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Whether any embedding exists for `memory_id` under `model`.
    pub fn embedding_exists(&self, memory_id: &MemoryId, model: &str) -> Result<bool> {
        self.with_conn(|conn| {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM embeddings WHERE memory_id = ?1 AND model = ?2",
                params![memory_id.as_str(), model],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        })
    }

    /// Delete all embeddings for a memory. Called when a memory is forgotten.
    pub fn delete_embeddings_for_memory(&self, memory_id: &MemoryId) -> Result<usize> {
        self.with_conn(|conn| {
            let n = conn.execute(
                "DELETE FROM embeddings WHERE memory_id = ?1",
                params![memory_id.as_str()],
            )?;
            Ok(n)
        })
    }

    /// Wipe the embeddings table. Used by `daemon.rebuild_indexes` before
    /// re-embedding every active memory from scratch.
    pub fn clear_embeddings(&self) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM embeddings", [])?;
            Ok(())
        })
    }
}

fn row_to_embedding(row: &rusqlite::Row<'_>) -> rusqlite::Result<EmbeddingRow> {
    let seq_id: i64 = row.get(0)?;
    let memory_id: String = row.get(1)?;
    let scope_json: String = row.get(2)?;
    let kind_str: String = row.get(3)?;
    let model: String = row.get(4)?;
    let bytes: Vec<u8> = row.get(5)?;
    let created_at_str: String = row.get(6)?;

    let scope: Scope = serde_json::from_str(&scope_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let kind = MemoryKind::parse(&kind_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            Box::<dyn std::error::Error + Send + Sync>::from(format!("unknown kind: {kind_str}")),
        )
    })?;
    let vector = bytes_to_f32_vec(&bytes);
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
        })?;
    Ok(EmbeddingRow {
        seq_id,
        memory_id: MemoryId(memory_id),
        scope,
        kind,
        model,
        vector,
        created_at,
    })
}

fn f32_vec_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn bytes_to_f32_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use toffee_core::{MemoryId, MemoryKind, Scope};

    fn new(model: &str, mem: &str, vec: Vec<f32>) -> NewEmbedding {
        NewEmbedding {
            memory_id: MemoryId(mem.into()),
            scope: Scope::new(["project:test"]),
            kind: MemoryKind::Claim,
            model: model.into(),
            vector: vec,
        }
    }

    #[test]
    fn insert_then_list_round_trips() {
        let store = Store::open_in_memory().unwrap();
        let seq = store
            .insert_embedding(&new("m1", "mem_a", vec![0.1, 0.2, 0.3]))
            .unwrap();
        assert!(seq > 0);
        let all = store.list_embeddings_for_model("m1").unwrap();
        assert_eq!(all.len(), 1);
        let row = &all[0];
        assert_eq!(row.memory_id.0, "mem_a");
        assert_eq!(row.kind, MemoryKind::Claim);
        assert_eq!(row.vector.len(), 3);
        assert!((row.vector[0] - 0.1).abs() < 1e-6);
    }

    #[test]
    fn model_filter_excludes_other_models() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_embedding(&new("m1", "mem_a", vec![1.0]))
            .unwrap();
        store
            .insert_embedding(&new("m2", "mem_b", vec![1.0]))
            .unwrap();
        assert_eq!(store.list_embeddings_for_model("m1").unwrap().len(), 1);
        assert_eq!(store.list_embeddings_for_model("m2").unwrap().len(), 1);
        assert_eq!(store.list_embeddings_for_model("none").unwrap().len(), 0);
    }

    #[test]
    fn delete_for_memory_removes_all_models() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_embedding(&new("m1", "mem_a", vec![1.0]))
            .unwrap();
        store
            .insert_embedding(&new("m2", "mem_a", vec![2.0]))
            .unwrap();
        let n = store
            .delete_embeddings_for_memory(&MemoryId("mem_a".into()))
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(store.embedding_count().unwrap(), 0);
    }

    #[test]
    fn embedding_exists_check() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_embedding(&new("m1", "mem_a", vec![1.0]))
            .unwrap();
        assert!(store
            .embedding_exists(&MemoryId("mem_a".into()), "m1")
            .unwrap());
        assert!(!store
            .embedding_exists(&MemoryId("mem_a".into()), "m2")
            .unwrap());
    }
}
