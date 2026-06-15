use rusqlite::{params, Connection};

use crate::Result;

const MIGRATIONS: &[(i64, &str)] = &[
    (
        1,
        r#"
        CREATE TABLE events (
          id TEXT PRIMARY KEY,
          scope_json TEXT NOT NULL,
          session_id TEXT,
          run_id TEXT,
          actor TEXT NOT NULL,
          event_type TEXT NOT NULL,
          payload_json TEXT NOT NULL,
          created_at TEXT NOT NULL
        );
        CREATE INDEX idx_events_run ON events(run_id);
        CREATE INDEX idx_events_created ON events(created_at);
        "#,
    ),
    (
        2,
        r#"
        CREATE TABLE memories (
          id TEXT PRIMARY KEY,
          kind TEXT NOT NULL,
          scope_json TEXT NOT NULL,
          subject TEXT,
          predicate TEXT,
          object TEXT,
          text TEXT NOT NULL,
          confidence REAL NOT NULL,
          source_event_ids_json TEXT NOT NULL,
          created_at TEXT NOT NULL,
          updated_at TEXT NOT NULL,
          superseded_by TEXT,
          deleted_at TEXT,
          CHECK (
            kind NOT IN ('claim','decision','preference')
            OR (subject IS NOT NULL AND predicate IS NOT NULL AND object IS NOT NULL)
          )
        );
        CREATE INDEX idx_memories_kind ON memories(kind);
        CREATE INDEX idx_memories_subject ON memories(subject, predicate);
        CREATE INDEX idx_memories_active ON memories(deleted_at, superseded_by);

        CREATE TABLE conflicts (
          id TEXT PRIMARY KEY,
          scope_json TEXT NOT NULL,
          subject TEXT,
          predicate TEXT,
          competing_memory_ids_json TEXT NOT NULL,
          resolution TEXT NOT NULL DEFAULT 'unresolved',
          created_at TEXT NOT NULL,
          resolved_at TEXT
        );
        CREATE INDEX idx_conflicts_unresolved ON conflicts(resolution)
          WHERE resolution = 'unresolved';

        CREATE TABLE worker_state (
          worker_id TEXT PRIMARY KEY,
          last_processed_event_id TEXT,
          last_processed_at TEXT
        );
        "#,
    ),
    (
        3,
        r#"
        CREATE TABLE entities (
          id TEXT PRIMARY KEY,
          entity_type TEXT NOT NULL,
          name TEXT NOT NULL,
          aliases_json TEXT,
          summary TEXT,
          created_at TEXT NOT NULL,
          updated_at TEXT NOT NULL,
          deleted_at TEXT
        );
        CREATE INDEX idx_entities_type ON entities(entity_type);
        CREATE INDEX idx_entities_name ON entities(name);
        CREATE UNIQUE INDEX idx_entities_name_unique_active
          ON entities(LOWER(name))
          WHERE deleted_at IS NULL;

        CREATE TABLE memory_entities (
          memory_id TEXT NOT NULL,
          entity_id TEXT NOT NULL,
          PRIMARY KEY (memory_id, entity_id)
        );
        CREATE INDEX idx_memory_entities_entity ON memory_entities(entity_id);
        "#,
    ),
    (
        4,
        r#"
        -- One row per (memory, model) pair. SQLite is the source of truth;
        -- the in-memory HNSW index is rebuilt from this table on startup.
        CREATE TABLE embeddings (
          seq_id INTEGER PRIMARY KEY AUTOINCREMENT,
          memory_id TEXT NOT NULL,
          scope_json TEXT NOT NULL,
          kind TEXT NOT NULL,
          model TEXT NOT NULL,
          vector BLOB NOT NULL,
          created_at TEXT NOT NULL
        );
        CREATE INDEX idx_embeddings_memory ON embeddings(memory_id);
        CREATE INDEX idx_embeddings_model ON embeddings(model);
        "#,
    ),
    (
        5,
        r#"
        -- One row per event the worker could not process. Recorded for
        -- operator visibility; no automatic retry today.
        CREATE TABLE worker_failures (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          event_id TEXT NOT NULL,
          worker_id TEXT NOT NULL,
          error TEXT NOT NULL,
          occurred_at TEXT NOT NULL,
          retried_at TEXT
        );
        CREATE INDEX idx_worker_failures_event ON worker_failures(event_id);
        CREATE INDEX idx_worker_failures_occurred ON worker_failures(occurred_at);
        "#,
    ),
];

pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
           version INTEGER PRIMARY KEY,
           applied_at TEXT NOT NULL
         );",
    )?;
    let current: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    for (version, sql) in MIGRATIONS {
        if *version > current {
            conn.execute_batch(sql)?;
            conn.execute(
                "INSERT INTO schema_version (version, applied_at) VALUES (?1, ?2)",
                params![version, chrono::Utc::now().to_rfc3339()],
            )?;
            tracing::info!(version, "applied migration");
        }
    }
    Ok(())
}
