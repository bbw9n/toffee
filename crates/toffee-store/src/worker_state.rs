use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use toffee_core::{Event, EventId};

use crate::{Result, Store};

pub const DEFAULT_WORKER_ID: &str = "default";

#[derive(Debug, Clone)]
pub struct WorkerCheckpoint {
    pub worker_id: String,
    pub last_processed_event_id: Option<EventId>,
    pub last_processed_at: Option<DateTime<Utc>>,
}

impl Store {
    pub fn worker_checkpoint(&self, worker_id: &str) -> Result<Option<WorkerCheckpoint>> {
        self.with_conn(|conn| {
            let row = conn
                .query_row(
                    "SELECT worker_id, last_processed_event_id, last_processed_at
                     FROM worker_state WHERE worker_id = ?1",
                    params![worker_id],
                    |r| {
                        let wid: String = r.get(0)?;
                        let id: Option<String> = r.get(1)?;
                        let ts: Option<String> = r.get(2)?;
                        Ok((wid, id, ts))
                    },
                )
                .optional()?;
            let Some((worker_id, id, ts)) = row else {
                return Ok(None);
            };
            let last_processed_at = match ts {
                None => None,
                Some(s) => Some(
                    DateTime::parse_from_rfc3339(&s)
                        .map_err(|e| crate::StoreError::Parse(format!("worker_state.last_processed_at: {e}")))?
                        .with_timezone(&Utc),
                ),
            };
            Ok(Some(WorkerCheckpoint {
                worker_id,
                last_processed_event_id: id.map(EventId),
                last_processed_at,
            }))
        })
    }

    pub fn set_worker_checkpoint(
        &self,
        worker_id: &str,
        last_event: &EventId,
        when: DateTime<Utc>,
    ) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                r#"INSERT INTO worker_state (worker_id, last_processed_event_id, last_processed_at)
                   VALUES (?1, ?2, ?3)
                   ON CONFLICT(worker_id) DO UPDATE SET
                     last_processed_event_id = excluded.last_processed_event_id,
                     last_processed_at = excluded.last_processed_at"#,
                params![worker_id, last_event.as_str(), when.to_rfc3339()],
            )?;
            Ok(())
        })
    }

    /// Return up to `limit` events whose `(created_at, id)` ordering places
    /// them strictly after the given checkpoint. If `after` is `None`, returns
    /// the earliest events.
    pub fn events_after(
        &self,
        after: Option<&(DateTime<Utc>, EventId)>,
        limit: usize,
    ) -> Result<Vec<Event>> {
        self.with_conn(|conn| {
            let sql = match after {
                Some(_) => {
                    r#"SELECT id, scope_json, session_id, run_id, actor, event_type, payload_json, created_at
                       FROM events
                       WHERE (created_at > ?1)
                          OR (created_at = ?1 AND id > ?2)
                       ORDER BY created_at ASC, id ASC LIMIT ?3"#
                }
                None => {
                    r#"SELECT id, scope_json, session_id, run_id, actor, event_type, payload_json, created_at
                       FROM events
                       ORDER BY created_at ASC, id ASC LIMIT ?1"#
                }
            };
            let mut stmt = conn.prepare(sql)?;
            let rows: Vec<Event> = match after {
                Some((ts, id)) => stmt
                    .query_map(params![ts.to_rfc3339(), id.as_str(), limit as i64], crate::events::row_to_event_public)?
                    .collect::<std::result::Result<Vec<_>, _>>()?,
                None => stmt
                    .query_map(params![limit as i64], crate::events::row_to_event_public)?
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            };
            Ok(rows)
        })
    }
}
