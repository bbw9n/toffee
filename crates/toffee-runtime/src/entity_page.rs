//! Assemble an `EntityPage` for a given entity. Honors scope inheritance:
//! when the caller asks for `project:foo`, the page also includes memories
//! in `user:me` and `global` scope (per RFC §11 / open question #1).

use std::collections::HashMap;

use toffee_core::{Entity, EntityId, EntityPage};
use toffee_store::Store;

use crate::{Result, RuntimeError};

const PAGE_MEMORY_LIMIT: usize = 200;

pub fn build(
    store: &Store,
    identifier: &str,
    requested_scopes: Option<Vec<String>>,
) -> Result<EntityPage> {
    let entity = lookup(store, identifier)?
        .ok_or_else(|| RuntimeError::NotFound(format!("entity {identifier}")))?;

    let expanded_scopes = requested_scopes
        .as_ref()
        .map(|s| toffee_core::expand_inherited(s));
    let memories = store.memories_for_entity(
        &entity.id,
        expanded_scopes.as_deref(),
        PAGE_MEMORY_LIMIT,
    )?;

    let co_occurring = compute_co_occurrence(store, &entity.id, &memories)?;

    Ok(EntityPage {
        entity,
        memories,
        co_occurring,
    })
}

fn lookup(store: &Store, identifier: &str) -> Result<Option<Entity>> {
    if identifier.starts_with("ent_") {
        Ok(store.get_entity(&EntityId(identifier.to_string()))?)
    } else {
        Ok(store.find_entity_by_name(identifier)?)
    }
}

fn compute_co_occurrence(
    store: &Store,
    primary: &EntityId,
    memories: &[toffee_core::Memory],
) -> Result<Vec<(Entity, usize)>> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for m in memories {
        let linked = store.entities_for_memory(&m.id)?;
        for e in linked {
            if e.id == *primary {
                continue;
            }
            *counts.entry(e.id.0.clone()).or_insert(0) += 1;
        }
    }
    let mut out: Vec<(Entity, usize)> = Vec::with_capacity(counts.len());
    for (id, n) in counts {
        if let Some(e) = store.get_entity(&EntityId(id))? {
            out.push((e, n));
        }
    }
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.name.cmp(&b.0.name)));
    Ok(out)
}
