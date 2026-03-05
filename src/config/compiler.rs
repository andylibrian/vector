//! Configuration compiler: transforms ConfigBuilder → Config.
//!
//! This module implements the validation and compilation pipeline that converts a mutable,
//! unvalidated `ConfigBuilder` into an immutable, validated `Config`.
//!
//! # The Compilation Pipeline
//!
//! ```text
//! ConfigBuilder (mutable, strings for inputs)
//!     ↓
//! 1. Check component names (no dots)
//!     ↓
//! 2. Expand glob patterns in inputs
//!     ↓
//! 3. Check config shape (non-empty, no duplicates)
//!     ↓
//! 4. Check resources (port and other exclusive resource conflicts)
//!     ↓
//! 5. Check outputs (no reserved names)
//!     ↓
//! 6. Check values (numeric ranges, EWMA parameters)
//!     ↓
//! 7. Build graph (resolve string inputs → OutputIds)
//!     ↓
//! 8. Typecheck graph (log/metric/trace compatibility)
//!     ↓
//! 9. Check for cycles
//!     ↓
//! 10. Propagate acknowledgements
//!     ↓
//! Config (immutable, OutputIds for inputs)
//! ```
//!
//! Each step can add errors. We collect errors across stages and return them
//! together when possible, so users can fix multiple issues in one pass.
//!
//! # Input Resolution: String → OutputId
//!
//! The most complex step is converting string inputs to OutputId references.
//!
//! **In ConfigBuilder:**
//! ```yaml
//! sinks:
//!   my_sink:
//!     inputs: ["my_source", "my_transform"]
//! ```
//!
//! These are just strings. We don't know if they're valid.
//!
//! **In Config:**
//! ```rust
//! SinkOuter {
//!     inputs: vec![
//!         OutputId { component: "my_source", port: None },
//!         OutputId { component: "my_transform", port: None },
//!     ]
//! }
//! ```
//!
//! The graph construction (step 7) performs this resolution:
//!
//! 1. Collect all valid outputs (from sources and transforms)
//! 2. Build a map: string → OutputId
//! 3. For each sink/transform input, look up the string in the map
//! 4. If found, replace with OutputId; if not, error
//!
//! This is why graph construction can fail - an input might reference
//! a non-existent component.
//!
//! # Glob Expansion
//!
//! Inputs can use glob patterns:
//!
//! ```yaml
//! sinks:
//!   all_logs:
//!     inputs: ["log_*"]  # Matches log_source1, log_source2, etc.
//! ```
//!
//! The `expand_globs()` function:
//! 1. Collects all valid output IDs as strings
//! 2. For each input pattern, tries to match as a glob
//! 3. If pattern matches, expands to all matching outputs
//! 4. If pattern doesn't match anything, leaves as-is (for better error messages)
//!
//! **Why leave non-matching patterns?**
//!
//! If we remove unmatched patterns, the error would be "no inputs".
//! If we leave them, the error is "input 'nonexistent' doesn't match any components".
//! The second is more helpful - it tells you WHICH input is wrong.
//!
//! # Enrichment Tables: Dual Components
//!
//! Enrichment tables can generate BOTH a source and a sink:
//!
//! ```yaml
//! enrichment_tables:
//!   memory_table:
//!     type: memory
//!     inputs: ["some_source"]  # Needs a sink to write to the table
//!     source_config:           # Creates a source to read from the table
//!       source_key: "memory_table_source"
//! ```
//!
//! The compiler handles this by:
//! 1. Extracting both derived components
//! 2. Adding them to the relevant collections
//! 3. Including them in graph construction
//!
//! This is why we have `sources_and_table_sources` and `all_sinks` variables.
//!
//! # Acknowledgement Propagation
//!
//! End-to-end acknowledgements work by propagating settings from sinks back to sources:
//!
//! ```yaml
//! sinks:
//!   kafka:
//!     type: kafka
//!     acknowledgements:
//!       enabled: true
//! ```
//!
//! The compiler traces the dependency chain:
//! 1. Find all sinks with acknowledgements enabled
//! 2. For each, walk backwards through inputs
//! 3. Mark all source ancestors with `sink_acknowledgements: true`
//!
//! This ensures sources know to wait for confirmation before advancing their read position.
//!
//! # Why This Order?
//!
//! **Name checking before glob expansion:**
//! Glob expansion might create components with dots (e.g., `route.errors`).
//! We want to catch user-defined dotted names BEFORE expansion.
//!
//! **Shape checking before graph construction:**
//! Graph construction is expensive. We want to fail fast if the config is missing sources/sinks.
//!
//! **Graph construction before type checking:**
//! Type checking needs the graph to know what types flow where.
//!
//! **All checks before acknowledgement propagation:**
//! Acknowledgement propagation modifies the config. We want the config fully validated first.

use indexmap::{IndexMap, IndexSet};
use vector_lib::id::Inputs;

use super::{
    Config, OutputId, builder::ConfigBuilder, graph::Graph, transform::get_transform_output_ids,
    validation,
};

pub fn compile(mut builder: ConfigBuilder) -> Result<(Config, Vec<String>), Vec<String>> {
    let mut errors = Vec::new();

    // component names should not have dots in the configuration file
    // but components can expand (like route) to have components with a dot
    // so this check should be done before expanding components
    if let Err(name_errors) = validation::check_names(
        builder
            .transforms
            .keys()
            .chain(builder.sources.keys())
            .chain(builder.sinks.keys()),
    ) {
        errors.extend(name_errors);
    }

    expand_globs(&mut builder);

    if let Err(type_errors) = validation::check_shape(&builder) {
        errors.extend(type_errors);
    }

    if let Err(type_errors) = validation::check_resources(&builder) {
        errors.extend(type_errors);
    }

    if let Err(output_errors) = validation::check_outputs(&builder) {
        errors.extend(output_errors);
    }

    if let Err(alpha_errors) = validation::check_values(&builder) {
        errors.extend(alpha_errors);
    }

    let ConfigBuilder {
        global,
        #[cfg(feature = "api")]
        api,
        schema,
        healthchecks,
        enrichment_tables,
        sources,
        sinks,
        transforms,
        tests,
        provider: _,
        secret,
        graceful_shutdown_duration,
        allow_empty: _,
    } = builder;
    let all_sinks = sinks
        .clone()
        .into_iter()
        .chain(
            enrichment_tables
                .iter()
                .filter_map(|(key, table)| table.as_sink(key)),
        )
        .collect::<IndexMap<_, _>>();
    let sources_and_table_sources = sources
        .clone()
        .into_iter()
        .chain(
            enrichment_tables
                .iter()
                .filter_map(|(key, table)| table.as_source(key)),
        )
        .collect::<IndexMap<_, _>>();

    let graph = match Graph::new(
        &sources_and_table_sources,
        &transforms,
        &all_sinks,
        schema,
        global.wildcard_matching.unwrap_or_default(),
    ) {
        Ok(graph) => graph,
        Err(graph_errors) => {
            errors.extend(graph_errors);
            return Err(errors);
        }
    };

    if let Err(type_errors) = graph.typecheck() {
        errors.extend(type_errors);
    }

    if let Err(e) = graph.check_for_cycles() {
        errors.push(e);
    }

    // Inputs are resolved from string into OutputIds as part of graph construction, so update them
    // here before adding to the final config (the types require this).
    let sinks = sinks
        .into_iter()
        .map(|(key, sink)| {
            let inputs = graph.inputs_for(&key);
            (key, sink.with_inputs(inputs))
        })
        .collect();
    let transforms = transforms
        .into_iter()
        .map(|(key, transform)| {
            let inputs = graph.inputs_for(&key);
            (key, transform.with_inputs(inputs))
        })
        .collect();
    let enrichment_tables = enrichment_tables
        .into_iter()
        .map(|(key, table)| {
            let inputs = graph.inputs_for(&key);
            (key, table.with_inputs(inputs))
        })
        .collect();
    let tests = tests
        .into_iter()
        .map(|test| test.resolve_outputs(&graph))
        .collect::<Result<Vec<_>, Vec<_>>>()?;

    if errors.is_empty() {
        let mut config = Config {
            global,
            #[cfg(feature = "api")]
            api,
            schema,
            healthchecks,
            enrichment_tables,
            sources,
            sinks,
            transforms,
            tests,
            secret,
            graceful_shutdown_duration,
        };

        config.propagate_acknowledgements()?;

        let warnings = validation::warnings(&config);

        Ok((config, warnings))
    } else {
        Err(errors)
    }
}

/// Expand globs in input lists
pub(crate) fn expand_globs(config: &mut ConfigBuilder) {
    let candidates = config
        .sources
        .iter()
        .flat_map(|(key, s)| {
            s.inner
                .outputs(config.schema.log_namespace())
                .into_iter()
                .map(|output| OutputId {
                    component: key.clone(),
                    port: output.port,
                })
        })
        .chain(config.transforms.iter().flat_map(|(key, t)| {
            get_transform_output_ids(t.inner.as_ref(), key.clone(), config.schema.log_namespace())
        }))
        .map(|output_id| output_id.to_string())
        .collect::<IndexSet<String>>();

    for (id, transform) in config.transforms.iter_mut() {
        expand_globs_inner(&mut transform.inputs, &id.to_string(), &candidates);
    }

    for (id, sink) in config.sinks.iter_mut() {
        expand_globs_inner(&mut sink.inputs, &id.to_string(), &candidates);
    }
}

enum InputMatcher {
    Pattern(glob::Pattern),
    String(String),
}

impl InputMatcher {
    fn matches(&self, candidate: &str) -> bool {
        use InputMatcher::*;

        match self {
            Pattern(pattern) => pattern.matches(candidate),
            String(s) => s == candidate,
        }
    }
}

fn expand_globs_inner(inputs: &mut Inputs<String>, id: &str, candidates: &IndexSet<String>) {
    let raw_inputs = std::mem::take(inputs);
    for raw_input in raw_inputs {
        let matcher = glob::Pattern::new(&raw_input)
            .map(InputMatcher::Pattern)
            .unwrap_or_else(|error| {
                warn!(message = "Invalid glob pattern for input.", component_id = %id, %error);
                InputMatcher::String(raw_input.to_string())
            });
        let mut matched = false;
        for input in candidates {
            if matcher.matches(input) && input != id {
                matched = true;
                inputs.extend(Some(input.to_string()))
            }
        }
        // If it didn't work as a glob pattern, leave it in the inputs as-is. This lets us give
        // more accurate error messages about nonexistent inputs.
        if !matched {
            inputs.extend(Some(raw_input))
        }
    }
}

#[cfg(test)]
mod test {
    use vector_lib::config::ComponentKey;

    use super::*;
    use crate::test_util::mock::{basic_sink, basic_source, basic_transform};

    #[test]
    fn glob_expansion() {
        let mut builder = ConfigBuilder::default();
        builder.add_source("foo1", basic_source().1);
        builder.add_source("foo2", basic_source().1);
        builder.add_source("bar", basic_source().1);
        builder.add_transform("foos", &["foo*"], basic_transform("", 1.0));
        builder.add_sink("baz", &["foos*", "b*"], basic_sink(1).1);
        builder.add_sink("quix", &["*oo*"], basic_sink(1).1);
        builder.add_sink("quux", &["*"], basic_sink(1).1);

        let config = builder.build().expect("build should succeed");

        assert_eq!(
            config
                .transforms
                .get(&ComponentKey::from("foos"))
                .map(|item| without_ports(item.inputs.clone()))
                .unwrap(),
            vec![ComponentKey::from("foo1"), ComponentKey::from("foo2")]
        );
        assert_eq!(
            config
                .sinks
                .get(&ComponentKey::from("baz"))
                .map(|item| without_ports(item.inputs.clone()))
                .unwrap(),
            vec![ComponentKey::from("foos"), ComponentKey::from("bar")]
        );
        assert_eq!(
            config
                .sinks
                .get(&ComponentKey::from("quux"))
                .map(|item| without_ports(item.inputs.clone()))
                .unwrap(),
            vec![
                ComponentKey::from("foo1"),
                ComponentKey::from("foo2"),
                ComponentKey::from("bar"),
                ComponentKey::from("foos")
            ]
        );
        assert_eq!(
            config
                .sinks
                .get(&ComponentKey::from("quix"))
                .map(|item| without_ports(item.inputs.clone()))
                .unwrap(),
            vec![
                ComponentKey::from("foo1"),
                ComponentKey::from("foo2"),
                ComponentKey::from("foos")
            ]
        );
    }

    fn without_ports(outputs: Inputs<OutputId>) -> Vec<ComponentKey> {
        outputs
            .into_iter()
            .map(|output| {
                assert!(output.port.is_none());
                output.component
            })
            .collect()
    }
}
