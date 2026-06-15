use chrono::{DateTime, Utc};
use rusqlite::params;
use toffee_core::{EventId, WorkerFailure};

use crate::{Result, Store};

impl Store {
    pub fn record_worker_failure(
        &self,
        worker_id: &str,
        event_id: &EventId,
        error: &str,
        occurred_at: DateTime<Utc>,
    ) -> Result<i64> {
        self.with_conn(|conn| {
            conn.execute(
                r#"INSERT INTO worker_failures
                    (event_id, worker_id, error, occurred_at, retried_at)
                   VALUES (?1, ?2, ?3, ?4, NULL)"#,
                params![
                    event_id.as_str(),
                    worker_id,
                    error,
                    occurred_at.to_rfc3339(),
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn list_worker_failures(&self, limit: usize) -> Result<Vec<WorkerFailure>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                r#"SELECT id, event_id, worker_id, error, occurred_at, retried_at
                   FROM worker_failures
                   ORDER BY occurred_at DESC, id DESC
                   LIMIT ?1"#,
            )?;
            let rows: Vec<WorkerFailure> = stmt
                .query_map(params![limit as i64], row_to_failure)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn worker_failure_count(&self) -> Result<i64> {
        self.with_conn(|conn| {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM worker_failures", [], |r| r.get(0))?;
            Ok(n)
        })
    }

    /// Count of events strictly after the given checkpoint.
    pub fn count_events_after(
        &self,
        after: Option<&(DateTime<Utc>, EventId)>,
    ) -> Result<i64> {
        self.with_conn(|conn| {
            let n: i64 = match after {
                Some((ts, id)) => conn.query_row(
                    r#"SELECT COUNT(*) FROM events
                       WHERE (created_at > ?1)
                          OR (created_at = ?1 AND id > ?2)"#,
                    params![ts.to_rfc3339(), id.as_str()],
                    |r| r.get(0),
                )?,
                None => conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?,
            };
            Ok(n)
        })
    }

    /// Timestamp of the most recently inserted event, or `None` if there
    /// are no events yet.
    pub fn latest_event_at(&self) -> Result<Option<DateTime<Utc>>> {
        self.with_conn(|conn| {
            let row: Option<String> = conn
                .query_row("SELECT MAX(created_at) FROM events", [], |r| {
                    r.get::<_, Option<String>>(0)
                })?;
            match row {
                None => Ok(None),
                Some(s) => Ok(Some(
                    DateTime::parse_from_rfc3339(&s)
                        .map_err(|e| crate::StoreError::Parse(format!("latest_event_at: {e}")))?
                        .with_timezone(&Utc),
                )),
            }
        })
    }
}

fn row_to_failure(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkerFailure> {
    let id: i64 = row.get(0)?;
    let event_id: String = row.get(1)?;
    let worker_id: String = row.get(2)?;
    let error: String = row.get(3)?;
    let occurred_at_str: String = row.get(4)?;
    let retried_at_str: Option<String> = row.get(5)?;

    let occurred_at = DateTime::parse_from_rfc3339(&occurred_at_str)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
        })?;
    let retried_at = match retried_at_str {
        None => None,
        Some(s) => Some(
            DateTime::parse_from_rfc3339(&s)
                .map(|d| d.with_timezone(&Utc))
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        5,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?,
        ),
    };

    Ok(WorkerFailure {
        id,
        event_id: EventId(event_id),
        worker_id,
        error,
        occurred_at,
        retried_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use toffee_core::EventId;

    #[test]
    fn record_and_list_failures() {
        let store = Store::open_in_memory().unwrap();
        let id = EventId::generate();
        store
            .record_worker_failure("default", &id, "boom", Utc::now())
            .unwrap();
        let listed = store.list_worker_failures(10).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].event_id, id);
        assert_eq!(listed[0].error, "boom");
    }

    #[test]
    fn count_events_after_returns_unprocessed_only() {
        let store = Store::open_in_memory().unwrap();
        let e1 = store
            .append_event(toffee_core::EventInput {
                scope: toffee_core::Scope::new(["g"]),
                actor: toffee_core::Actor::User,
                event_type: "user_message".into(),
                payload: serde_json::json!({"text": "a"}),
                session_id: None,
                run_id: None,
            })
            .unwrap();
        let _e2 = store
            .append_event(toffee_core::EventInput {
                scope: toffee_core::Scope::new(["g"]),
                actor: toffee_core::Actor::User,
                event_type: "user_message".into(),
                payload: serde_json::json!({"text": "b"}),
                session_id: None,
                run_id: None,
            })
            .unwrap();
        assert_eq!(store.count_events_after(None).unwrap(), 2);
        let after = (e1.created_at, e1.id);
        assert_eq!(store.count_events_after(Some(&after)).unwrap(), 1);
    }

    #[test]
    fn latest_event_at_returns_none_on_empty() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.latest_event_at().unwrap().is_none());
    }
}
