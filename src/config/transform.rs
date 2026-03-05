//! Transform configuration traits and types.
//!
//! Transforms are the processing nodes in Vector's data pipeline. They:
//! - Receive events from sources or other transforms
//! - Modify, filter, route, or enrich events
//! - Emit events to downstream transforms or sinks
//!
//! # Transform vs Source: The Key Difference
//!
//! Unlike sources, transforms have INPUTS. This is reflected in `TransformOuter`:
//!
//! ```ignore
//! pub struct TransformOuter<T> {
//!     pub inputs: Inputs<T>,        // Which components feed into this transform
//!     pub inner: BoxedTransform,    // The actual transform configuration
//! }
//! ```
//!
//! The `inputs` field is why we have two type parameters in the config system:
//! - `TransformOuter<String>` during building (inputs are raw strings from YAML)
//! - `TransformOuter<OutputId>` after compilation (inputs are resolved references)
//!
//! # The TransformConfig Trait
//!
//! Every transform implements `TransformConfig`:
//!
//! ```ignore
//! #[async_trait]
//! #[typetag::serde(tag = "type")]
//! pub trait TransformConfig: DynClone + NamedComponent + Debug + Send + Sync {
//!     async fn build(&self, cx: &TransformContext) -> Result<Transform>;
//!     fn input(&self) -> Input;                                      // What types we accept
//!     fn outputs(&self, cx: &TransformContext, ...) -> Vec<TransformOutput>; // What types we produce
//!     fn validate(&self, merged_definition: &schema::Definition) -> Result<(), Vec<String>>;
//! }
//! ```
//!
//! **Input/Output Type Declaration:**
//!
//! The `input()` and `outputs()` methods declare type constraints:
//!
//! ```ignore
//! impl TransformConfig for RemapConfig {
//!     fn input(&self) -> Input {
//!         Input::log()  // Only accepts log events
//!     }
//!     
//!     fn outputs(&self, ...) -> Vec<TransformOutput> {
//!         vec![TransformOutput::new(DataType::Log, ...)]  // Produces log events
//!     }
//! }
//! ```
//!
//! This is used by `graph.rs` to validate type compatibility:
//! - A `Log → Log` transform can't be fed by a `Metric` source
//! - An `Any → Any` transform can handle any input type
//!
//! # Schema-Aware Transforms
//!
//! Some transforms (like `remap`) need to know the schema of incoming events:
//!
//! ```ignore
//! pub struct TransformContext {
//!     pub merged_schema_definition: schema::Definition,  // Merged schema from all inputs
//!     pub schema_definitions: HashMap<Option<String>, HashMap<OutputId, schema::Definition>>,
//! }
//! ```
//!
//! The `merged_schema_definition` combines schemas from all inputs. This allows:
//! - VRL programs to use autocomplete with known field names
//! - Type coercion to be more lenient (field already known as string? no need to coerce)
//! - Better error messages (expected `string`, got `integer`)
//!
//! # Multiple Outputs (Branching Transforms)
//!
//! Some transforms have multiple outputs (like the `route` transform):
//!
//! ```yaml
//! transforms:
//!   router:
//!     type: route
//!     inputs: ["my_source"]
//!     route:
//!       errors: '.level == "error"'
//!       info: '.level == "info"'
//!
//! sinks:
//!   error_sink:
//!     type: http
//!     inputs: ["router.errors"]  # Note the dotted syntax
//! ```
//!
//! The `outputs()` method returns multiple `TransformOutput` entries, each with a `port`:
//!
//! ```ignore
//! vec![
//!     TransformOutput::new(DataType::Log, ...).with_port("errors"),
//!     TransformOutput::new(DataType::Log, ...).with_port("info"),
//! ]
//! ```
//!
//! The `OutputId` struct captures this:
//! ```ignore
//! pub struct OutputId {
//!     pub component: ComponentKey,  // "router"
//!     pub port: Option<String>,     // Some("errors")
//! }
//! ```
//!
//! # Transform Expansion (Nested Topologies)
//!
//! Some transforms expand into sub-topologies (e.g., `reduce` creates internal transforms).
//! The `nestable()` method controls this:
//!
//! ```ignore
//! fn nestable(&self, parents: &HashSet<&'static str>) -> bool {
//!     // Prevent infinite recursion
//!     !parents.contains("reduce")
//! }
//! ```
//!
//! This prevents infinite expansion chains and detects known incompatibilities.
//!
//! # Concurrency Control
//!
//! Transforms can opt into parallel execution:
//!
//! ```ignore
//! fn enable_concurrency(&self) -> bool {
//!     true  // This transform can be run in parallel
//! }
//! ```
//!
//! When enabled, the topology spawns multiple instances of the transform and
//! fans out events across them. This is useful for CPU-intensive transforms
//! (like `remap` with complex VRL programs) but adds overhead for lightweight
//! transforms (like `filter`).
//!
//! # Hot Reload Considerations
//!
//! During hot reload, transforms can have external files they watch:
//!
//! ```ignore
//! fn files_to_watch(&self) -> Vec<&PathBuf> {
//!     &self.vrl_program_file  // Watch this file for changes
//! }
//! ```
//!
//! When these files change, the transform is rebuilt even if its config
//! appears unchanged. This is tracked by `transform_keys_with_external_files()`.
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use async_trait::async_trait;
use dyn_clone::DynClone;
use serde::Serialize;
use vector_lib::{
    config::{GlobalOptions, Input, LogNamespace, TransformOutput},
    configurable::{
        Configurable, GenerateError, Metadata, NamedComponent,
        attributes::CustomAttribute,
        configurable_component,
        schema::{SchemaGenerator, SchemaObject},
    },
    id::Inputs,
    schema,
    transform::Transform,
};
use vector_vrl_metrics::MetricsStorage;

use super::{ComponentKey, OutputId, dot_graph::GraphConfig, schema::Options as SchemaOptions};
use crate::extra_context::ExtraContext;

/// Type-erased transform configuration.
pub type BoxedTransform = Box<dyn TransformConfig>;

impl Configurable for BoxedTransform {
    fn referenceable_name() -> Option<&'static str> {
        Some("vector::transforms::Transforms")
    }

    fn metadata() -> Metadata {
        let mut metadata = Metadata::default();
        metadata.set_description("Configurable transforms in Vector.");
        metadata.add_custom_attribute(CustomAttribute::kv("docs::enum_tagging", "internal"));
        metadata.add_custom_attribute(CustomAttribute::kv("docs::enum_tag_field", "type"));
        metadata
    }

    fn generate_schema(
        generator: &RefCell<SchemaGenerator>,
    ) -> Result<SchemaObject, GenerateError> {
        vector_lib::configurable::component::TransformDescription::generate_schemas(generator)
    }
}

impl<T: TransformConfig + 'static> From<T> for BoxedTransform {
    fn from(that: T) -> Self {
        Box::new(that)
    }
}

/// Fully resolved transform component.
///
/// Wraps the transform configuration with its input connections.
#[configurable_component]
#[configurable(metadata(docs::component_base_type = "transform"))]
#[derive(Clone, Debug)]
pub struct TransformOuter<T>
where
    T: Configurable + Serialize + 'static,
{
    /// Sources/transforms that feed into this transform.
    #[configurable(derived)]
    pub inputs: Inputs<T>,

    /// Graph configuration for visualization.
    #[configurable(derived)]
    #[serde(default, skip_serializing_if = "vector_lib::serde::is_default")]
    pub graph: GraphConfig,

    /// The actual transform configuration.
    #[serde(flatten)]
    #[configurable(metadata(docs::hidden))]
    pub inner: BoxedTransform,
}

impl<T> TransformOuter<T>
where
    T: Configurable + Serialize,
{
    pub(crate) fn new<I, IT>(inputs: I, inner: IT) -> Self
    where
        I: IntoIterator<Item = T>,
        IT: Into<BoxedTransform>,
    {
        let inputs = Inputs::from_iter(inputs);
        let inner = inner.into();
        TransformOuter {
            inputs,
            inner,
            graph: Default::default(),
        }
    }

    pub(super) fn map_inputs<U>(self, f: impl Fn(&T) -> U) -> TransformOuter<U>
    where
        U: Configurable + Serialize,
    {
        let inputs = self.inputs.iter().map(f).collect::<Vec<_>>();
        self.with_inputs(inputs)
    }

    pub(crate) fn with_inputs<I, U>(self, inputs: I) -> TransformOuter<U>
    where
        I: IntoIterator<Item = U>,
        U: Configurable + Serialize,
    {
        TransformOuter {
            inputs: Inputs::from_iter(inputs),
            inner: self.inner,
            graph: self.graph,
        }
    }
}

pub struct TransformContext {
    // This is optional because currently there are a lot of places we use `TransformContext` that
    // may not have the relevant data available (e.g. tests). In the future it'd be nice to make it
    // required somehow.
    pub key: Option<ComponentKey>,

    pub globals: GlobalOptions,

    pub enrichment_tables: vector_lib::enrichment::TableRegistry,

    pub metrics_storage: MetricsStorage,

    /// Tracks the schema IDs assigned to schemas exposed by the transform.
    ///
    /// Given a transform can expose multiple [`TransformOutput`] channels, the ID is tied to the identifier of
    /// that `TransformOutput`.
    pub schema_definitions: HashMap<Option<String>, HashMap<OutputId, schema::Definition>>,

    /// The schema definition created by merging all inputs of the transform.
    ///
    /// This information can be used by transforms that behave differently based on schema
    /// information, such as the `remap` transform, which passes this information along to the VRL
    /// compiler such that type coercion becomes less of a need for operators writing VRL programs.
    pub merged_schema_definition: schema::Definition,

    pub schema: SchemaOptions,

    /// Extra context data provided by the running app and shared across all components. This can be
    /// used to pass shared settings or other data from outside the components.
    pub extra_context: ExtraContext,
}

impl Default for TransformContext {
    fn default() -> Self {
        Self {
            key: Default::default(),
            globals: Default::default(),
            enrichment_tables: Default::default(),
            metrics_storage: Default::default(),
            schema_definitions: HashMap::from([(None, HashMap::new())]),
            merged_schema_definition: schema::Definition::any(),
            schema: SchemaOptions::default(),
            extra_context: Default::default(),
        }
    }
}

impl TransformContext {
    // clippy allow avoids an issue where vrl is flagged off and `globals` is
    // the sole field in the struct
    #[allow(clippy::needless_update)]
    pub fn new_with_globals(globals: GlobalOptions) -> Self {
        Self {
            globals,
            ..Default::default()
        }
    }

    #[cfg(test)]
    pub fn new_test(
        schema_definitions: HashMap<Option<String>, HashMap<OutputId, schema::Definition>>,
    ) -> Self {
        Self {
            schema_definitions,
            ..Default::default()
        }
    }

    /// Gets the log namespacing to use. The passed in value is from the transform itself
    /// and will override any global default if it's set.
    ///
    /// This should only be used for transforms that don't originate from a log (eg: `metric_to_log`)
    /// Most transforms will keep the log_namespace value that already exists on the event.
    pub fn log_namespace(&self, namespace: Option<bool>) -> LogNamespace {
        namespace
            .or(self.schema.log_namespace)
            .unwrap_or(false)
            .into()
    }
}

/// Generalized interface for describing and building transform components.
#[async_trait]
#[typetag::serde(tag = "type")]
pub trait TransformConfig: DynClone + NamedComponent + core::fmt::Debug + Send + Sync {
    /// Builds the transform with the given context.
    ///
    /// If the transform is built successfully, `Ok(...)` is returned containing the transform.
    ///
    /// # Errors
    ///
    /// If an error occurs while building the transform, an error variant explaining the issue is
    /// returned.
    async fn build(&self, globals: &TransformContext) -> crate::Result<Transform>;

    /// Gets the input configuration for this transform.
    fn input(&self) -> Input;

    /// Gets the list of outputs exposed by this transform.
    ///
    /// The provided `merged_definition` can be used by transforms to understand the expected shape
    /// of events flowing through the transform.
    fn outputs(
        &self,
        globals: &TransformContext,
        input_definitions: &[(OutputId, schema::Definition)],
    ) -> Vec<TransformOutput>;

    /// Validates that the configuration of the transform is valid.
    ///
    /// This would generally be where logical conditions were checked, such as ensuring a transform
    /// isn't using a named output that matches a reserved output name, and so on.
    ///
    /// # Errors
    ///
    /// If validation does not succeed, an error variant containing a list of all validation errors
    /// is returned.
    fn validate(&self, _merged_definition: &schema::Definition) -> Result<(), Vec<String>> {
        Ok(())
    }

    /// Whether or not concurrency should be enabled for this transform.
    ///
    /// When enabled, this transform may be run in parallel in order to attempt to maximize
    /// throughput for this node in the topology. Transforms should generally not run concurrently
    /// unless they are compute-heavy, as there is a cost/overhead associated with fanning out
    /// events to the parallel transform tasks.
    fn enable_concurrency(&self) -> bool {
        false
    }

    /// Whether or not this transform can be nested, given the types of transforms it would be
    /// nested within.
    ///
    /// For some transforms, they can expand themselves into a subtopology of nested transforms.
    /// However, in order to prevent an infinite recursion of nested transforms, we may want to only
    /// allow one layer of "expansion". Additionally, there may be known issues with a transform
    /// that is nested under another specific transform interacting poorly, or incorrectly.
    ///
    /// This method allows a transform to report if it can or cannot function correctly if it is
    /// nested under transforms of a specific type, or if such nesting is fundamentally disallowed.
    fn nestable(&self, _parents: &HashSet<&'static str>) -> bool {
        true
    }

    /// Gets the files to watch to trigger reload
    fn files_to_watch(&self) -> Vec<&PathBuf> {
        Vec::new()
    }
}

dyn_clone::clone_trait_object!(TransformConfig);

/// Often we want to call outputs just to retrieve the OutputId's without needing
/// the schema definitions.
pub fn get_transform_output_ids<T: TransformConfig + ?Sized>(
    transform: &T,
    key: ComponentKey,
    global_log_namespace: LogNamespace,
) -> impl Iterator<Item = OutputId> + '_ {
    transform
        .outputs(
            &TransformContext {
                schema: SchemaOptions {
                    log_namespace: Some(global_log_namespace.into()),
                    ..Default::default()
                },
                ..Default::default()
            },
            &[(key.clone().into(), schema::Definition::any())],
        )
        .into_iter()
        .map(move |output| OutputId {
            component: key.clone(),
            port: output.port,
        })
}
