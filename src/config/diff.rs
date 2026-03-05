//! Configuration diff computation for hot reload.
//!
//! When Vector's config files change, we need to apply updates without
//! restarting everything. This module computes the minimal set of changes
//! needed to transform one config into another.
//!
//! # The Problem: Zero-Downtime Updates
//!
//! Consider a running Vector with 50 components:
//! - 10 sources reading from various files/streams
//! - 20 transforms processing data
//! - 20 sinks sending to different destinations
//!
//! If you change ONE transform's config, we don't want to:
//! 1. Stop all 50 components
//! 2. Restart all 50 components
//! 3. Lose in-flight events in all buffers
//!
//! Instead, we want:
//! 1. Identify which components changed
//! 2. Stop only the affected components
//! 3. Restart only those components
//! 4. Keep everything else running
//!
//! # ConfigDiff Structure
//!
//! ```ignore
//! pub struct ConfigDiff {
//!     pub sources: Difference,        // Changes to sources
//!     pub transforms: Difference,     // Changes to transforms
//!     pub sinks: Difference,          // Changes to sinks
//!     pub enrichment_tables: Difference, // Changes to enrichment tables
//!     pub components_to_reload: HashSet<ComponentKey>,  // Forced reloads
//! }
//!
//! pub struct Difference {
//!     pub to_remove: HashSet<ComponentKey>,  // Components to shut down
//!     pub to_change: HashSet<ComponentKey>,  // Components to restart
//!     pub to_add: HashSet<ComponentKey>,     // Components to start fresh
//! }
//! ```
//!
//! # How Diff is Computed
//!
//! The algorithm is surprisingly simple:
//!
//! 1. **Serialize both configs to JSON**
//!    ```ignore
//!    let old_value = serde_json::to_value(&old["my_source"])?;
//!    let new_value = serde_json::to_value(&new["my_source"])?;
//!    ```
//!
//! 2. **Compare the serialized values**
//!    ```ignore
//!    if old_value != new_value {
//!        // Component changed!
//!    }
//!    ```
//!
//! **Why JSON, not equality comparison?**
//!
//! Rust's `#[derive(PartialEq)]` would work, but:
//! - Two configs might be semantically identical but structurally different
//!   (e.g., different field order in YAML)
//! - JSON provides canonical representation (sorted keys, consistent formatting)
//! - Handles HashMaps correctly (order shouldn't matter)
//!
//! **Why not compare YAML strings?**
//!
//! YAML has too much flexibility:
//! - `{"a": 1, "b": 2}` vs `{"b": 2, "a": 1}` - different strings, same value
//! - `enabled: true` vs `enabled: True` - different YAML, same meaning
//!
//! JSON normalization ensures we only detect ACTUAL changes.
//!
//! # The Three Categories
//!
//! **to_remove**: Components that existed in old config but not in new
//! ```yaml
//! # Old config
//! sources:
//!   old_source:
//!     type: file
//!     path: /var/log/old.log
//!
//! # New config - "old_source" removed
//! sources:
//!   new_source:
//!     type: file
//!     path: /var/log/new.log
//! ```
//!
//! **to_add**: Components that exist in new config but not in old
//! ```yaml
//! # Old config - no "new_source"
//! # New config - "new_source" added
//! sources:
//!   new_source:
//!     type: file
//!     path: /var/log/new.log
//! ```
//!
//! **to_change**: Components that exist in both but with different configs
//! ```yaml
//! # Old config
//! transforms:
//!   parser:
//!     type: remap
//!     source: '.message = .raw'
//!
//! # New config - "parser" still exists but config changed
//! transforms:
//!   parser:
//!     type: remap
//!     source: '.message = string(.raw)'  # Changed!
//! ```
//!
//! # Cascading Changes
//!
//! When a component changes, downstream components may need to reload:
//!
//! ```text
//! Source A (changed) → Transform B → Sink C
//! ```
//!
//! If Source A's OUTPUT TYPE changed (e.g., from logs to metrics), Transform B
//! might be incompatible. The topology builder handles this by:
//! 1. Checking which changed components have different outputs
//! 2. Propagating the change downstream
//! 3. Adding affected components to `components_to_reload`
//!
//! This is tracked separately from `to_change` because the component's OWN
//! config didn't change, but it needs to restart anyway.
//!
//! # Enrichment Tables: A Special Case
//!
//! Enrichment tables can generate both a source AND a sink component:
//!
//! ```yaml
//! enrichment_tables:
//!   my_memory_table:
//!     type: memory
//!     inputs: ["some_source"]  # Creates a sink to write to the table
//!     source_config:           # Creates a source to read from the table
//!       source_key: "my_memory_table_source"
//! ```
//!
//! The diff logic handles this by:
//! 1. Extracting both derived components (source + sink)
//! 2. Adding both to the diff if the table config changed
//!
//! This is why `from_enrichment_tables()` is a separate method - it has to
//! decompose the enrichment table into its constituent parts.
//!
//! # The `flip()` Method
//!
//! During reload, we sometimes need to "undo" a diff:
//!
//! ```ignore
//! let diff = ConfigDiff::new(&old, &new);
//! // ... apply changes to go from old → new ...
//!
//! let reverse_diff = diff.flip();  // new → old
//! // ... use to undo changes if something fails ...
//! ```
//!
//! This swaps `to_remove` ↔ `to_add`, giving us the reverse transformation.
//!
//! # Hot Reload Flow
//!
//! ```text
//! 1. Config file changes
//!    ↓
//! 2. Watcher detects change (watcher.rs)
//!    ↓
//! 3. New config loaded and validated (loading/mod.rs)
//!    ↓
//! 4. ConfigDiff::new(old, new) computed (this module)
//!    ↓
//! 5. Topology builder applies diff:
//!    a. Stop components in to_remove
//!    b. Stop components in to_change
//!    c. Start new components in to_add
//!    d. Restart components in to_change with new config
//!    ↓
//! 6. Running topology updated, zero downtime
//! ```
//!
//! # IndexMap: Why Order Matters
//!
//! Config uses `IndexMap` (not `HashMap`) for component storage. This is
//! critical for diff stability:
//!
//! ```ignore
//! // HashMap iteration order is undefined
//! // Two identical configs might serialize differently!
//!
//! // IndexMap preserves insertion order
//! // Same config always serializes the same way
//! ```
//!
//! Without stable ordering, we'd get false positives: configs that are
//! semantically identical but serialize differently would appear as "changed".
use std::collections::HashSet;

use indexmap::IndexMap;
use vector_lib::config::OutputId;

use super::{ComponentKey, Config, EnrichmentTableOuter};

#[derive(Debug)]
pub struct ConfigDiff {
    pub sources: Difference,
    pub transforms: Difference,
    pub sinks: Difference,
    /// This difference does not only contain the actual enrichment_tables keys, but also keys that
    /// may be used for their source and sink components (if available).
    pub enrichment_tables: Difference,
    pub components_to_reload: HashSet<ComponentKey>,
}

impl ConfigDiff {
    pub fn initial(initial: &Config) -> Self {
        Self::new(&Config::default(), initial, HashSet::new())
    }

    pub fn new(old: &Config, new: &Config, components_to_reload: HashSet<ComponentKey>) -> Self {
        ConfigDiff {
            sources: Difference::new(&old.sources, &new.sources, &components_to_reload),
            transforms: Difference::new(&old.transforms, &new.transforms, &components_to_reload),
            sinks: Difference::new(&old.sinks, &new.sinks, &components_to_reload),
            enrichment_tables: Difference::from_enrichment_tables(
                &old.enrichment_tables,
                &new.enrichment_tables,
            ),
            components_to_reload,
        }
    }

    /// Swaps removed with added in Differences.
    pub const fn flip(mut self) -> Self {
        self.sources.flip();
        self.transforms.flip();
        self.sinks.flip();
        self.enrichment_tables.flip();
        self
    }

    /// Checks whether the given component is present at all.
    pub fn contains(&self, key: &ComponentKey) -> bool {
        self.sources.contains(key)
            || self.transforms.contains(key)
            || self.sinks.contains(key)
            || self.enrichment_tables.contains(key)
    }

    /// Checks whether the given component is changed.
    pub fn is_changed(&self, key: &ComponentKey) -> bool {
        self.sources.is_changed(key)
            || self.transforms.is_changed(key)
            || self.sinks.is_changed(key)
            || self.enrichment_tables.contains(key)
    }

    /// Checks whether the given component is removed.
    pub fn is_removed(&self, key: &ComponentKey) -> bool {
        self.sources.is_removed(key)
            || self.transforms.is_removed(key)
            || self.sinks.is_removed(key)
            || self.enrichment_tables.contains(key)
    }
}

#[derive(Debug)]
pub struct Difference {
    pub to_remove: HashSet<ComponentKey>,
    pub to_change: HashSet<ComponentKey>,
    pub to_add: HashSet<ComponentKey>,
}

impl Difference {
    fn new<C>(
        old: &IndexMap<ComponentKey, C>,
        new: &IndexMap<ComponentKey, C>,
        need_change: &HashSet<ComponentKey>,
    ) -> Self
    where
        C: serde::Serialize + serde::Deserialize<'static>,
    {
        let old_names = old.keys().cloned().collect::<HashSet<_>>();
        let new_names = new.keys().cloned().collect::<HashSet<_>>();

        let to_change = old_names
            .intersection(&new_names)
            .filter(|&n| {
                // This is a hack around the issue of comparing two
                // trait objects. Json is used here over toml since
                // toml does not support serializing `None`
                // to_value is used specifically (instead of string)
                // to avoid problems comparing serialized HashMaps,
                // which can iterate in varied orders.
                let old_value = serde_json::to_value(&old[n]).unwrap();
                let new_value = serde_json::to_value(&new[n]).unwrap();
                old_value != new_value || need_change.contains(n)
            })
            .cloned()
            .collect::<HashSet<_>>();

        let to_remove = &old_names - &new_names;
        let to_add = &new_names - &old_names;

        Self {
            to_remove,
            to_change,
            to_add,
        }
    }

    fn from_enrichment_tables(
        old: &IndexMap<ComponentKey, EnrichmentTableOuter<OutputId>>,
        new: &IndexMap<ComponentKey, EnrichmentTableOuter<OutputId>>,
    ) -> Self {
        let old_table_keys = extract_table_component_keys(old);
        let new_table_keys = extract_table_component_keys(new);

        let to_change = old_table_keys
            .intersection(&new_table_keys)
            .filter(|(table_key, _derived_component_key)| {
                // This is a hack around the issue of comparing two
                // trait objects. Json is used here over toml since
                // toml does not support serializing `None`
                // to_value is used specifically (instead of string)
                // to avoid problems comparing serialized HashMaps,
                // which can iterate in varied orders.
                let old_value = serde_json::to_value(&old[*table_key]).unwrap();
                let new_value = serde_json::to_value(&new[*table_key]).unwrap();
                old_value != new_value
            })
            .cloned()
            .map(|(_table_key, derived_component_key)| derived_component_key)
            .collect::<HashSet<_>>();

        // Extract only the derived component keys for the final difference calculation
        let old_component_keys = old_table_keys
            .into_iter()
            .map(|(_table_key, component_key)| component_key)
            .collect::<HashSet<_>>();
        let new_component_keys = new_table_keys
            .into_iter()
            .map(|(_table_key, component_key)| component_key)
            .collect::<HashSet<_>>();

        let to_remove = &old_component_keys - &new_component_keys;
        let to_add = &new_component_keys - &old_component_keys;

        Self {
            to_remove,
            to_change,
            to_add,
        }
    }

    /// Checks whether or not any components are being changed or added.
    pub fn any_changed_or_added(&self) -> bool {
        !(self.to_change.is_empty() && self.to_add.is_empty())
    }

    /// Checks whether or not any components are being changed or removed.
    pub fn any_changed_or_removed(&self) -> bool {
        !(self.to_change.is_empty() && self.to_remove.is_empty())
    }

    /// Checks whether the given component is present at all.
    pub fn contains(&self, id: &ComponentKey) -> bool {
        self.to_add.contains(id) || self.to_change.contains(id) || self.to_remove.contains(id)
    }

    /// Checks whether the given component is present as a change or addition.
    pub fn contains_new(&self, id: &ComponentKey) -> bool {
        self.to_add.contains(id) || self.to_change.contains(id)
    }

    /// Checks whether or not the given component is changed.
    pub fn is_changed(&self, key: &ComponentKey) -> bool {
        self.to_change.contains(key)
    }

    /// Checks whether the given component is present as an addition.
    pub fn is_added(&self, id: &ComponentKey) -> bool {
        self.to_add.contains(id)
    }

    /// Checks whether or not the given component is removed.
    pub fn is_removed(&self, key: &ComponentKey) -> bool {
        self.to_remove.contains(key)
    }

    const fn flip(&mut self) {
        std::mem::swap(&mut self.to_remove, &mut self.to_add);
    }

    pub fn changed_and_added(&self) -> impl Iterator<Item = &ComponentKey> {
        self.to_change.iter().chain(self.to_add.iter())
    }

    pub fn removed_and_changed(&self) -> impl Iterator<Item = &ComponentKey> {
        self.to_change.iter().chain(self.to_remove.iter())
    }
}

/// Helper function to extract component keys from enrichment tables.
fn extract_table_component_keys(
    tables: &IndexMap<ComponentKey, EnrichmentTableOuter<OutputId>>,
) -> HashSet<(&ComponentKey, ComponentKey)> {
    tables
        .iter()
        .flat_map(|(table_key, table)| {
            vec![
                table
                    .as_source(table_key)
                    .map(|(component_key, _)| (table_key, component_key)),
                table
                    .as_sink(table_key)
                    .map(|(component_key, _)| (table_key, component_key)),
            ]
        })
        .flatten()
        .collect()
}

#[cfg(all(test, feature = "enrichment-tables-memory"))]
mod tests {
    use crate::config::ConfigBuilder;
    use indoc::indoc;

    use super::*;

    #[test]
    fn diff_enrichment_tables_uses_correct_keys() {
        let old_config: Config = serde_yaml::from_str::<ConfigBuilder>(indoc! {r#"
            enrichment_tables:
              memory_table:
                type: "memory"
                ttl: 10
                inputs: []
                source_config:
                  source_key: "memory_table_source"
                  export_expired_items: true
                  export_interval: 50

              memory_table_unchanged:
                type: "memory"
                ttl: 10
                inputs: []

              memory_table_old:
                type: "memory"
                ttl: 10
                inputs: []

            sources:
              test:
                type: "test_basic"

            sinks:
              test_sink:
                type: "test_basic"
                inputs: ["test"]
        "#})
        .unwrap()
        .build()
        .unwrap();

        let new_config: Config = serde_yaml::from_str::<ConfigBuilder>(indoc! {r#"
            enrichment_tables:
              memory_table:
                type: "memory"
                ttl: 20
                inputs: []
                source_config:
                  source_key: "memory_table_source"
                  export_expired_items: true
                  export_interval: 50

              memory_table_unchanged:
                type: "memory"
                ttl: 10
                inputs: []

              memory_table_new:
                type: "memory"
                ttl: 1000
                inputs: []

            sources:
              test:
                type: "test_basic"

            sinks:
              test_sink:
                type: "test_basic"
                inputs: ["test"]
        "#})
        .unwrap()
        .build()
        .unwrap();

        let diff = Difference::from_enrichment_tables(
            &old_config.enrichment_tables,
            &new_config.enrichment_tables,
        );

        assert_eq!(diff.to_add, HashSet::from_iter(["memory_table_new".into()]));
        assert_eq!(
            diff.to_remove,
            HashSet::from_iter(["memory_table_old".into()])
        );
        assert_eq!(
            diff.to_change,
            HashSet::from_iter(["memory_table".into(), "memory_table_source".into()])
        );
    }
}
