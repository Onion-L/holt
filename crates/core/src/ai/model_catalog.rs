//! Port of `pi-core/ai/src/model-catalog.ts`.
//!
//! TypeScript's `flattenModelCatalog` merges typed model groups into a keyed
//! catalog and carries the grouping purely at the type level. The runtime
//! behavior is the object spread: later groups override earlier ids. Rust
//! callers keep models in vectors and merge with the same last-wins rule via
//! [`flatten_model_catalog`].

use std::collections::BTreeMap;

use crate::ai::types::Model;

/// Port of `flattenModelCatalog`: merges model groups with last-wins id
/// override and returns the models keyed by id.
pub fn flatten_model_catalog(groups: &[Vec<Model>]) -> BTreeMap<String, Model> {
    let mut catalog = BTreeMap::new();
    for group in groups {
        for model in group {
            catalog.insert(model.id.clone(), model.clone());
        }
    }
    catalog
}
