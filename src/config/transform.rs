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
//! - A `Log -> Log` transform can't be fed by a `Metric` source
//! - An `Any -> Any` transform can handle any input type
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
//!
//! # The Build Lifecycle
//!
//! Understanding when each method is called is crucial for implementers:
//!
//! 1. **Parsing**: Config is deserialized from YAML/JSON into your config struct
//! 2. **Validation**: `validate()` is called with the merged input schema
//! 3. **Output Discovery**: `outputs()` is called to build the topology graph
//! 4. **Building**: `build()` is called to create the runtime transform
//!
//! The `build()` method receives a `TransformContext` containing:
//! - Schema information from upstream components
//! - Enrichment tables for VRL lookups
//! - Global configuration options
//!
//! # Thread Safety Requirements
//!
//! Config structs must be `Send + Sync + Clone` because:
//! - They're shared across async tasks during validation
//! - They're cloned during hot reload to compare old vs new configs
//! - They may be accessed concurrently during topology building
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

/// Context provided to transforms during the build phase.
///
/// This struct bundles together everything a transform might need to construct its
/// runtime representation. It's created by the topology builder and passed to
/// `TransformConfig::build()`.
///
/// # Why This Struct Exists
///
/// Without this struct, `build()` would need a dozen parameters:
/// ```ignore
/// fn build(
///     &self,
///     key: ComponentKey,
///     globals: GlobalOptions,
///     enrichment_tables: TableRegistry,
///     schema_definitions: HashMap<...>,
///     merged_schema: Definition,
///     // ... more params
/// ) -> Result<Transform>;
/// ```
///
/// This bundling makes the API stable (new fields can be added without breaking
/// existing implementations) and makes testing easier (you can create a minimal
/// context with `TransformContext::default()`).
///
/// # Schema Propagation
///
/// The schema fields enable type-aware transforms:
/// - `merged_schema_definition`: Combined schema from all upstream inputs
/// - `schema_definitions`: Per-output schemas (for multi-output upstreams)
///
/// The `remap` transform uses these to:
/// 1. Provide better autocomplete in VRL
/// 2. Skip unnecessary type coercions
/// 3. Generate better error messages
///
/// # Example: Using TransformContext
///
/// ```ignore
/// async fn build(&self, context: &TransformContext) -> Result<Transform> {
///     // Access global timezone setting
///     let timezone = context.globals.timezone();
///     
///     // Pass schema to VRL compiler for type checking
///     let program = compile_vrl(
///         &self.source,
///         context.merged_schema_definition.clone(),
///     )?;
///     
///     // Use enrichment tables for GeoIP lookups in VRL
///     let tables = context.enrichment_tables.clone();
///     
///     Ok(Transform::function(MyTransform { program, timezone, tables }))
/// }
/// ```
pub struct TransformContext {
    /// The unique identifier for this transform instance.
    ///
    /// Used for logging, metrics, and error messages. Optional because some
    /// test contexts don't have a component key.
    ///
    /// This is optional because currently there are a lot of places we use `TransformContext` that
    /// may not have the relevant data available (e.g. tests). In the future it'd be nice to make it
    /// required somehow.
    pub key: Option<ComponentKey>,

    /// Global configuration options shared across all components.
    ///
    /// Contains settings like:
    /// - `timezone`: Default timezone for timestamp parsing
    /// - `log_schema`: Field name mappings (e.g., "message" vs "message")
    /// - `data_dir`: Directory for persistent state
    pub globals: GlobalOptions,

    /// Registry of enrichment tables available for lookups.
    ///
    /// Enrichment tables are external data sources that VRL can query:
    /// - GeoIP tables for IP geolocation
    /// - CSV files for custom lookups
    /// - Database tables for dynamic enrichment
    ///
    /// Transforms that use VRL should pass this to the compiler.
    pub enrichment_tables: vector_lib::enrichment::TableRegistry,

    /// Storage for VRL program metrics.
    ///
    /// Tracks VRL-specific metrics like function call counts and durations.
    /// Shared across all VRL programs in this transform.
    pub metrics_storage: MetricsStorage,

    /// Tracks the schema IDs assigned to schemas exposed by the transform.
    ///
    /// Given a transform can expose multiple [`TransformOutput`] channels, the ID is tied to the identifier of
    /// that `TransformOutput`.
    ///
    /// This maps: output_port -> (upstream_output_id -> schema_definition)
    ///
    /// For single-output transforms, the outer key is `None`. For multi-output
    /// transforms like `route`, each named output has its own entry.
    pub schema_definitions: HashMap<Option<String>, HashMap<OutputId, schema::Definition>>,

    /// The schema definition created by merging all inputs of the transform.
    ///
    /// This information can be used by transforms that behave differently based on schema
    /// information, such as the `remap` transform, which passes this information along to the VRL
    /// compiler such that type coercion becomes less of a need for operators writing VRL programs.
    ///
    /// When a transform has multiple inputs, their schemas are merged. If input A
    /// has `{"message": string}` and input B has `{"message": string, "host": string}`,
    /// the merged schema is `{"message": string}` (intersection).
    pub merged_schema_definition: schema::Definition,

    /// Schema-related options specific to this transform.
    ///
    /// Contains settings like:
    /// - `log_namespace`: Whether to use new Vector namespace
    pub schema: SchemaOptions,

    /// Extra context data provided by the running app and shared across all components. This can be
    /// used to pass shared settings or other data from outside the components.
    ///
    /// This is an escape hatch for custom builds that need to pass arbitrary
    /// data to components. Most transforms won't need this.
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
///
/// This is the core trait that every transform must implement. It defines the contract
/// between configuration and runtime. The trait uses `#[typetag::serde]` to enable
/// deserialization from configuration files where the `type` field determines
/// which concrete type to instantiate.
///
/// # The Type Tag System
///
/// The `#[typetag::serde(tag = "type")]` attribute enables polymorphic deserialization:
///
/// ```yaml
/// transforms:
///   my_filter:
///     type: filter        # <- This selects FilterConfig
///     condition: "..."
///   
///   my_remap:
///     type: remap         # <- This selects RemapConfig
///     source: "..."
/// ```
///
/// Each implementation must also have `#[typetag::serde(name = "filter")]`
/// to specify its type name.
///
/// # Required Trait Bounds
///
/// - `DynClone`: Allows cloning trait objects (needed for config diffing during reload)
/// - `NamedComponent`: Provides the component type name (e.g., "filter", "remap")
/// - `Debug`: For logging and error messages
/// - `Send + Sync`: Required for async/parallel processing
///
/// # Implementation Example
///
/// ```ignore
/// #[async_trait]
/// #[typetag::serde(name = "filter")]
/// impl TransformConfig for FilterConfig {
///     async fn build(&self, context: &TransformContext) -> Result<Transform> {
///         // Build the runtime transform
///         let condition = self.condition.build(&context.enrichment_tables)?;
///         Ok(Transform::function(Filter::new(condition)))
///     }
///
///     fn input(&self) -> Input {
///         Input::all()  // Accepts logs, metrics, and traces
///     }
///
///     fn outputs(&self, _: &TransformContext, inputs: &[(OutputId, Definition)])
///         -> Vec<TransformOutput>
///     {
///         // Pass through the same types we receive
///         vec![TransformOutput::new(DataType::all_bits(), clone_input_definitions(inputs))]
///     }
///
///     fn enable_concurrency(&self) -> bool {
///         true  // Stateless, safe to parallelize
///     }
/// }
/// ```
#[async_trait]
#[typetag::serde(tag = "type")]
pub trait TransformConfig: DynClone + NamedComponent + core::fmt::Debug + Send + Sync {
    /// Builds the transform with the given context.
    ///
    /// This is where configuration becomes runtime. The transform should:
    /// 1. Validate any remaining configuration (beyond what `validate()` checked)
    /// 2. Compile any DSLs (e.g., VRL programs)
    /// 3. Create the appropriate `Transform` variant
    ///
    /// The returned `Transform` variant determines the execution model:
    /// - `Transform::function()` for `FunctionTransform` (simplest, fastest)
    /// - `Transform::synchronous()` for `SyncTransform` (multiple outputs)
    /// - `Transform::task()` for `TaskTransform` (async, stateful)
    ///
    /// # Why async?
    ///
    /// Some transforms need to perform async I/O during build:
    /// - Fetching remote schema information
    /// - Connecting to external services
    /// - Loading large enrichment tables
    ///
    /// # Errors
    ///
    /// If an error occurs while building the transform, an error variant explaining the issue is
    /// returned. The error will be displayed to the user during config validation.
    async fn build(&self, globals: &TransformContext) -> crate::Result<Transform>;

    /// Gets the input configuration for this transform.
    ///
    /// This declares what event types the transform accepts. The topology builder
    /// uses this to validate that upstream components produce compatible types.
    ///
    /// Common patterns:
    /// - `Input::all()` - Accepts logs, metrics, and traces (e.g., `filter`)
    /// - `Input::log()` - Only accepts log events (e.g., `remap` on logs)
    /// - `Input::metric()` - Only accepts metric events (e.g., `aggregate`)
    fn input(&self) -> Input;

    /// Gets the list of outputs exposed by this transform.
    ///
    /// Most transforms have a single default output, but some (like `route`)
    /// have multiple named outputs. Each output declares:
    /// - The data type it produces (logs, metrics, traces)
    /// - The schema definition for events on this output
    /// - An optional port name for multi-output transforms
    ///
    /// The provided `merged_definition` can be used by transforms to understand the expected shape
    /// of events flowing through the transform. Schema-aware transforms (like `remap`) use this
    /// for VRL compilation.
    ///
    /// # Multi-Output Example
    ///
    /// ```ignore
    /// fn outputs(&self, ...) -> Vec<TransformOutput> {
    ///     vec![
    ///         TransformOutput::new(DataType::Log, schemas).with_port("errors"),
    ///         TransformOutput::new(DataType::Log, schemas).with_port("info"),
    ///         TransformOutput::new(DataType::Log, schemas).with_port("_unmatched"),
    ///     ]
    /// }
    /// ```
    fn outputs(
        &self,
        globals: &TransformContext,
        input_definitions: &[(OutputId, schema::Definition)],
    ) -> Vec<TransformOutput>;

    /// Validates that the configuration of the transform is valid.
    ///
    /// This is called after config parsing but before building. Use this for
    /// logical validation that can't be expressed in the type system:
    /// - Checking for conflicting options
    /// - Validating references to other components
    /// - Ensuring reserved names aren't used
    ///
    /// This is separate from `build()` because:
    /// 1. It's called earlier in the pipeline (before building the full topology)
    /// 2. It can return multiple errors at once (better UX)
    /// 3. It doesn't need async (simpler error handling)
    ///
    /// # Errors
    ///
    /// If validation does not succeed, an error variant containing a list of all validation errors
    /// is returned. All errors are collected before returning to give users
    /// a complete picture of what needs to be fixed.
    fn validate(&self, _merged_definition: &schema::Definition) -> Result<(), Vec<String>> {
        Ok(())
    }

    /// Whether or not concurrency should be enabled for this transform.
    ///
    /// When enabled, this transform may be run in parallel in order to attempt to maximize
    /// throughput for this node in the topology. The topology spawns multiple worker tasks
    /// that pull from a shared input queue and process events concurrently.
    ///
    /// # When to Enable
    ///
    /// Enable concurrency when:
    /// - The transform is CPU-bound (e.g., complex VRL programs)
    /// - Event ordering doesn't matter
    /// - The transform has no shared mutable state
    ///
    /// # When NOT to Enable
    ///
    /// Don't enable concurrency when:
    /// - The transform maintains ordering guarantees (e.g., dedupe)
    /// - The transform has shared mutable state (most don't)
    /// - The transform is I/O-bound (use TaskTransform instead)
    ///
    /// # Performance Considerations
    ///
    /// There is a cost/overhead associated with fanning out events to parallel tasks.
    /// For very lightweight transforms (like simple filters), the overhead may exceed
    /// the benefit. Benchmark before enabling.
    fn enable_concurrency(&self) -> bool {
        false
    }

    /// Whether or not this transform can be nested, given the types of transforms it would be
    /// nested within.
    ///
    /// Some transforms expand themselves into sub-topologies (e.g., `reduce` creates internal
    /// transforms for aggregation). This method controls whether such expansion is allowed
    /// given the current nesting context.
    ///
    /// # Why This Matters
    ///
    /// 1. **Prevent infinite recursion**: A transform that expands to itself would loop forever
    /// 2. **Detect incompatibilities**: Some transforms don't work well when nested
    /// 3. **Resource limits**: Deep nesting can consume too much memory
    ///
    /// # Example: Preventing Recursion
    ///
    /// ```ignore
    /// fn nestable(&self, parents: &HashSet<&'static str>) -> bool {
    ///     // Don't allow nesting reduce inside reduce
    ///     !parents.contains("reduce")
    /// }
    /// ```
    ///
    /// The `parents` set contains all transform types in the current expansion chain.
    fn nestable(&self, _parents: &HashSet<&'static str>) -> bool {
        true
    }

    /// Gets the files to watch to trigger reload.
    ///
    /// Some transforms reference external files (e.g., VRL programs from files).
    /// When these files change, the transform should be rebuilt even if its
    /// config hasn't changed.
    ///
    /// This is used by the hot reload system to detect when a rebuild is needed.
    /// The paths are watched for modification timestamps, not content changes.
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
