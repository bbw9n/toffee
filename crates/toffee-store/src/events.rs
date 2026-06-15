use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use toffee_core::{Actor, Event, EventId, EventInput, Scope};

use crate::{Result, Store, StoreError};

/// A stored event row, mirroring [`Event`] but with already-serialized scope
/// for downstream consumers that prefer to skip a deserialization step.
#[derive(Debug, Clone)]
pub struct EventRecord {
    pub event: Event,
}

impl Store {
    /// Append a new event. Assigns the id and `created_at` timestamp.
    pub fn append_event(&self, input: EventInput) -> Result<Event> {
        let id = EventId::generate();
        let created_at = Utc::now();
        let event = Event::from_input(input, id, created_at);
        self.insert_event(&event)?;
        Ok(event)
    }

    /// Insert an already-constructed event verbatim. Used by tests and the
    /// future worker replay path.
    pub fn insert_event(&self, event: &Event) -> Result<()> {
        let scope_json = serde_json::to_string(&event.scope)?;
        let payload_json = serde_json::to_string(&event.payload)?;
        let created_at = event.created_at.to_rfc3339();
        self.with_conn(|conn| {
            conn.execute(
                r#"INSERT INTO events
                    (id, scope_json, session_id, run_id, actor, event_type, payload_json, created_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#,
                params![
                    event.id.as_str(),
                    scope_json,
                    event.session_id,
                    event.run_id,
                    event.actor.as_str(),
                    event.event_type,
                    payload_json,
                    created_at,
                ],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    pub fn get_event(&self, id: &EventId) -> Result<Option<Event>> {
        self.with_conn(|conn| {
            let row = conn
                .query_row(
                    r#"SELECT id, scope_json, session_id, run_id, actor, event_type, payload_json, created_at
                       FROM events WHERE id = ?1"#,
                    params![id.as_str()],
                    row_to_event,
                )
                .optional()?;
            Ok(row)
        })
    }

    pub fn event_count(&self) -> Result<i64> {
        self.with_conn(|conn| {
            let n: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?;
            Ok(n)
        })
    }

    /// List events in insertion order, newest last. Used by the CLI and the
    /// future worker for replay; this is not a public RPC method in Phase 0.
    pub fn list_events(&self, limit: usize) -> Result<Vec<Event>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                r#"SELECT id, scope_json, session_id, run_id, actor, event_type, payload_json, created_at
                   FROM events ORDER BY created_at ASC, id ASC LIMIT ?1"#,
            )?;
            let rows = stmt
                .query_map(params![limit as i64], row_to_event)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }
}

/// Re-exported for sibling store modules (`worker_state`) that need to
/// hydrate events from arbitrary queries.
pub(crate) fn row_to_event_public(row: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    row_to_event(row)
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    let id: String = row.get(0)?;
    let scope_json: String = row.get(1)?;
    let session_id: Option<String> = row.get(2)?;
    let run_id: Option<String> = row.get(3)?;
    let actor_str: String = row.get(4)?;
    let event_type: String = row.get(5)?;
    let payload_json: String = row.get(6)?;
    let created_at_str: String = row.get(7)?;

    let scope: Scope = serde_json::from_str(&scope_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let payload: serde_json::Value = serde_json::from_str(&payload_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let actor: Actor = parse_actor(&actor_str);
    let created_at: DateTime<Utc> = DateTime::parse_from_rfc3339(&created_at_str)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&Utc);

    Ok(Event {
        id: EventId(id),
        scope,
        actor,
        event_type,
        payload,
        session_id,
        run_id,
        created_at,
    })
}

fn parse_actor(s: &str) -> Actor {
    match s {
        "user" => Actor::User,
        "agent" => Actor::Agent,
        "tool" => Actor::Tool,
        "system" => Actor::System,
        other => Actor::Other(other.to_string()),
    }
}

// Squelch unused-warning for StoreError import in some builds.
#[allow(dead_code)]
fn _ensure_err_in_scope(e: StoreError) -> StoreError {
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input() -> EventInput {
        EventInput {
            scope: Scope::new(["project:test"]),
            actor: Actor::User,
            event_type: "user_message".to_string(),
            payload: json!({"text": "hi"}),
            session_id: Some("s1".into()),
            run_id: Some("r1".into()),
        }
    }

    #[test]
    fn append_then_get_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let ev = store.append_event(input()).unwrap();
        let fetched = store.get_event(&ev.id).unwrap().unwrap();
        assert_eq!(fetched.id, ev.id);
        assert_eq!(fetched.event_type, "user_message");
        assert_eq!(fetched.actor, Actor::User);
        assert_eq!(fetched.scope, Scope::new(["project:test"]));
        assert_eq!(fetched.payload["text"], "hi");
    }

    #[test]
    fn count_grows_with_appends() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.event_count().unwrap(), 0);
        store.append_event(input()).unwrap();
        store.append_event(input()).unwrap();
        assert_eq!(store.event_count().unwrap(), 2);
    }

    #[test]
    fn restart_survives_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("toffee.db");
        let id;
        {
            let store = Store::open(&path).unwrap();
            id = store.append_event(input()).unwrap().id;
        }
        let store = Store::open(&path).unwrap();
        let fetched = store.get_event(&id).unwrap();
        assert!(fetched.is_some());
    }
}
