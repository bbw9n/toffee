use rusqlite::{params, Connection};

use crate::Result;

const MIGRATIONS: &[(i64, &str)] = &[(
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
)];

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
