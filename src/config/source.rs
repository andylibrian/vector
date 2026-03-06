//! Source configuration traits and types.
//!
//! Sources are the entry points for data into Vector. They:
//! - Read data from external systems (files, network, APIs)
//! - Convert raw data into Vector events (logs, metrics, traces)
//! - Push events into the topology for processing
//!
//! # The SourceConfig Trait
//!
//! Every source implements `SourceConfig`, which defines:
//! - How to parse its configuration from YAML/JSON
//! - How to build a running source instance
//! - What outputs it produces (for type checking)
//! - What system resources it needs (for conflict detection)
//!
//! # Component Registration
//!
//! Sources register themselves using two attributes:
//!
//! ```ignore
//! #[derive(Debug, Clone)]
//! #[serde(deny_unknown_fields)]
//! struct FileConfig {
//!     include: Vec<PathBuf>,
//! }
//!
//! #[async_trait]
//! #[typetag::serde(name = "file")]  // Registers "file" → FileConfig
//! impl SourceConfig for FileConfig {
//!     async fn build(&self, cx: SourceContext) -> Result<Source> { ... }
//!     fn outputs(&self, global_log_namespace: LogNamespace) -> Vec<SourceOutput> { ... }
//! }
//! ```
//!
//! The `#[typetag::serde]` attribute:
//! - Creates a global registry entry: `"file"` → deserializer for `FileConfig`
//! - Enables polymorphic deserialization: `{ "type": "file", ... }` → `FileConfig`
//! - Eliminates the need for a giant match statement in the config parser
//!
//! # Dynamic Dispatch via BoxedSource
//!
//! Notice that `SourceOuter` stores `BoxedSource` (a type alias for `Box<dyn SourceConfig>`):
//!
//! ```ignore
//! pub struct SourceOuter {
//!     pub inner: BoxedSource,  // Box<dyn SourceConfig>
//! }
//! ```
//!
//! **Why Box<dyn Trait>?**
//!
//! Different sources have different config types:
//! - `FileConfig` for file sources
//! - `HttpServerConfig` for HTTP sources
//! - `KafkaConfig` for Kafka sources
//!
//! We need to store ALL of these in the same `IndexMap`. Rust's type system
//! requires collections to be homogeneous (same type), so we use trait objects:
//! - `dyn SourceConfig` - any type implementing the trait
//! - `Box<dyn SourceConfig>` - heap-allocated, dynamically dispatched
//!
//! This is a common pattern in plugin architectures where the set of types
//! isn't known at compile time.
//!
//! # Source Outputs and Type Checking
//!
//! The `outputs()` method declares what data types a source produces:
//!
//! ```ignore
//! impl SourceConfig for FileConfig {
//!     fn outputs(&self, _: LogNamespace) -> Vec<SourceOutput> {
//!         vec![SourceOutput::new_logs(DataType::Log, schema)]
//!     }
//! }
//! ```
//!
//! This is used during validation to ensure:
//! - A log-only source doesn't feed into a metrics-only sink
//! - Type conversions are possible where needed
//!
//! Multiple outputs are possible (e.g., a source that produces both logs and metrics).
//!
//! # Resource Declaration
//!
//! Sources declare exclusive system resources they need:
//!
//! ```ignore
//! impl SourceConfig for HttpServerConfig {
//!     fn resources(&self) -> Vec<Resource> {
//!         vec![Resource::tcp(self.address)]
//!     }
//! }
//! ```
//!
//! The config validator checks for conflicts BEFORE starting components:
//! - Two HTTP servers can't both bind to port 8080
//! - Two components can't claim the same exclusive resource
//!
//! This prevents runtime errors and provides clear error messages.
//!
//! # The SourceContext
//!
//! When building a source, Vector provides a context with runtime dependencies:
//!
//! ```ignore
//! pub struct SourceContext {
//!     pub key: ComponentKey,           // Source name for logging/metrics
//!     pub globals: GlobalOptions,      // Global config (data_dir, etc.)
//!     pub out: SourceSender,           // Channel to send events to topology
//!     pub shutdown: ShutdownSignal,    // Signal to stop gracefully
//!     pub acknowledgements: bool,      // Whether end-to-end acks are enabled
//! }
//! ```
//!
//! The `out: SourceSender` is the source's output channel. Sources push events
//! into this channel, and the topology routes them to connected transforms/sinks.
use std::{cell::RefCell, collections::HashMap, time::Duration};

use async_trait::async_trait;
use dyn_clone::DynClone;
use vector_config::{Configurable, GenerateError, Metadata, NamedComponent};
use vector_config_common::{
    attributes::CustomAttribute,
    schema::{SchemaGenerator, SchemaObject},
};
use vector_config_macros::configurable_component;
use vector_lib::{
    config::{
        AcknowledgementsConfig, GlobalOptions, LogNamespace, SourceAcknowledgementsConfig,
        SourceOutput,
    },
    source::Source,
};
use vector_vrl_metrics::MetricsStorage;

use super::{ComponentKey, ProxyConfig, Resource, dot_graph::GraphConfig, schema};
use crate::{SourceSender, extra_context::ExtraContext, shutdown::ShutdownSignal};

pub type BoxedSource = Box<dyn SourceConfig>;

impl Configurable for BoxedSource {
    fn referenceable_name() -> Option<&'static str> {
        Some("vector::sources::Sources")
    }

    fn metadata() -> Metadata {
        let mut metadata = Metadata::default();
        metadata.set_description("Configurable sources in Vector.");
        metadata.add_custom_attribute(CustomAttribute::kv("docs::enum_tagging", "internal"));
        metadata.add_custom_attribute(CustomAttribute::kv("docs::enum_tag_field", "type"));
        metadata
    }

    fn generate_schema(
        generator: &RefCell<SchemaGenerator>,
    ) -> Result<SchemaObject, GenerateError> {
        vector_lib::configurable::component::SourceDescription::generate_schemas(generator)
    }
}

impl<T: SourceConfig + 'static> From<T> for BoxedSource {
    fn from(value: T) -> Self {
        Box::new(value)
    }
}

/// Fully resolved source component.
#[configurable_component]
#[configurable(metadata(docs::component_base_type = "source"))]
#[derive(Clone, Debug)]
pub struct SourceOuter {
    #[configurable(derived)]
    #[serde(default, skip_serializing_if = "vector_lib::serde::is_default")]
    pub proxy: ProxyConfig,

    #[configurable(derived)]
    #[serde(default, skip_serializing_if = "vector_lib::serde::is_default")]
    pub graph: GraphConfig,

    #[serde(default, skip)]
    pub sink_acknowledgements: bool,

    #[configurable(metadata(docs::hidden))]
    #[serde(flatten)]
    pub(crate) inner: BoxedSource,
}

impl SourceOuter {
    pub(crate) fn new<I: Into<BoxedSource>>(inner: I) -> Self {
        Self {
            proxy: Default::default(),
            graph: Default::default(),
            sink_acknowledgements: false,
            inner: inner.into(),
        }
    }
}

/// Generalized interface for describing and building source components.
///
/// # Dynamic Dispatch with typetag
///
/// The `#[typetag::serde(tag = "type")]` attribute enables dynamic deserialization.
/// When Vector parses a config like:
///
/// ```yaml
/// sources:
///   my_logs:
///     type: file
///     include: ["/var/log/*.log"]
/// ```
///
/// The `type: file` field tells serde to look up the registered type "file"
/// and deserialize the remaining fields into `FileConfig`.
///
/// # Why typetag Instead of a Match Statement?
///
/// A naive approach would be:
/// ```rust,ignore
/// match source_type {
///     "file" => FileConfig::deserialize(...),
///     "http" => HttpConfig::deserialize(...),
///     // ... 47 more cases
/// }
/// ```
///
/// This has several problems:
/// 1. **Coupling**: Every new source requires modifying this central match statement
/// 2. **Feature flags**: Conditional compilation becomes messy with many branches
/// 3. **Testability**: Can't easily mock/stub components in tests
///
/// typetag solves this by maintaining a global registry. Each source registers
/// itself when its module is compiled:
///
/// ```rust,ignore
/// #[typetag::serde(name = "file")]
/// impl SourceConfig for FileConfig { ... }
/// ```
///
/// When serde sees `type: "file"`, it looks up "file" in the registry and calls
/// the registered deserializer. Adding a new source requires zero changes to
/// the config loading code.
#[async_trait]
#[typetag::serde(tag = "type")]
pub trait SourceConfig: DynClone + NamedComponent + core::fmt::Debug + Send + Sync {
    /// Builds the source with the given context.
    ///
    /// If the source is built successfully, `Ok(...)` is returned containing the source.
    ///
    /// # Errors
    ///
    /// If an error occurs while building the source, an error variant explaining the issue is
    /// returned.
    async fn build(&self, cx: SourceContext) -> crate::Result<Source>;

    /// Gets the list of outputs exposed by this source.
    fn outputs(&self, global_log_namespace: LogNamespace) -> Vec<SourceOutput>;

    /// Gets the list of resources, if any, used by this source.
    ///
    /// Resources represent dependencies -- network ports, file descriptors, and so on -- that
    /// cannot be shared between components at runtime. This ensures that components can not be
    /// configured in a way that would deadlock the spawning of a topology, and as well, allows
    /// Vector to determine the correct order for rebuilding a topology during configuration reload
    /// when resources must first be reclaimed before being reassigned, and so on.
    ///
    /// # Example: TCP Port
    ///
    /// A `http_server` source might return:
    /// ```rust,ignore
    /// vec![Resource::tcp(self.address)]
    /// ```
    ///
    /// The config validator checks that no two components claim the same port.
    /// This prevents runtime errors like "address already in use" and enables
    /// Vector to provide clear error messages at startup.
    ///
    /// # Example: File Descriptor
    ///
    /// The `stdin` source returns `vec![Resource::Fd(0)]` because only one
    /// component can read from stdin. If you configure two stdin sources,
    /// the validator catches this at config time, not runtime.
    fn resources(&self) -> Vec<Resource> {
        Vec::new()
    }

    /// Whether or not this source can acknowledge the events it emits.
    ///
    /// # End-to-End Acknowledgements
    ///
    /// Generally, Vector uses acknowledgements to track when an event has finally been processed,
    /// either successfully or unsuccessfully. This enables "end-to-end" acknowledgements:
    ///
    /// ```text
    /// Kafka Source → Transform → HTTP Sink
    ///     ↓                              ↓
    ///  "Don't commit       "Data received!"
    ///   offset yet"             ↓
    ///                      "Commit offset"
    /// ```
    ///
    /// The Kafka source holds onto message offsets until the HTTP sink confirms delivery.
    /// This guarantees "at-least-once" delivery semantics.
    ///
    /// # Why Not All Sources Support This?
    ///
    /// Some sources can't acknowledge:
    /// - **File source**: Can't pause log rotation. Once a line is read, it's read.
    /// - **Generator source**: No external system to coordinate with.
    ///
    /// For these sources, enabling acknowledgements would add overhead (tracking
    /// events in memory) without any benefit. By exposing this capability, we can:
    /// 1. Skip acknowledgement overhead for sources that don't need it
    /// 2. Warn users if they enable acks in a topology that can't support them
    ///
    /// By exposing whether or not a source supports acknowledgements, we can avoid situations where
    /// using acknowledgements would only add processing overhead for no benefit to the source, as
    /// well as emit contextual warnings when end-to-end acknowledgements are enabled, but the
    /// topology as configured does not actually support the use of end-to-end acknowledgements.
    fn can_acknowledge(&self) -> bool;

    /// If this source supports timeout returns from the `SourceSender` and the configuration
    /// provides a timeout value, return it here and the `out` channel will be configured with it.
    fn send_timeout(&self) -> Option<Duration> {
        None
    }
}

dyn_clone::clone_trait_object!(SourceConfig);

pub struct SourceContext {
    pub key: ComponentKey,
    pub globals: GlobalOptions,
    pub enrichment_tables: vector_lib::enrichment::TableRegistry,
    pub metrics_storage: MetricsStorage,
    pub shutdown: ShutdownSignal,
    pub out: SourceSender,
    pub proxy: ProxyConfig,
    pub acknowledgements: bool,
    pub schema: schema::Options,

    /// Tracks the schema IDs assigned to schemas exposed by the source.
    ///
    /// Given a source can expose multiple [`SourceOutput`] channels, the ID is tied to the identifier of
    /// that `SourceOutput`.
    pub schema_definitions: HashMap<Option<String>, schema::Definition>,

    /// Extra context data provided by the running app and shared across all components. This can be
    /// used to pass shared settings or other data from outside the components.
    pub extra_context: ExtraContext,
}

impl SourceContext {
    #[cfg(any(test, feature = "test-utils"))]
    pub fn new_shutdown(
        key: &ComponentKey,
        out: SourceSender,
    ) -> (Self, crate::shutdown::SourceShutdownCoordinator) {
        let mut shutdown = crate::shutdown::SourceShutdownCoordinator::default();
        let (shutdown_signal, _) = shutdown.register_source(key, false);
        (
            Self {
                key: key.clone(),
                globals: GlobalOptions::default(),
                enrichment_tables: Default::default(),
                metrics_storage: Default::default(),
                shutdown: shutdown_signal,
                out,
                proxy: Default::default(),
                acknowledgements: false,
                schema_definitions: HashMap::default(),
                schema: Default::default(),
                extra_context: Default::default(),
            },
            shutdown,
        )
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn new_test(
        out: SourceSender,
        schema_definitions: Option<HashMap<Option<String>, schema::Definition>>,
    ) -> Self {
        Self {
            key: ComponentKey::from("default"),
            globals: GlobalOptions::default(),
            enrichment_tables: Default::default(),
            metrics_storage: Default::default(),
            shutdown: ShutdownSignal::noop(),
            out,
            proxy: Default::default(),
            acknowledgements: false,
            schema_definitions: schema_definitions.unwrap_or_default(),
            schema: Default::default(),
            extra_context: Default::default(),
        }
    }

    pub fn do_acknowledgements(&self, config: SourceAcknowledgementsConfig) -> bool {
        let config = AcknowledgementsConfig::from(config);
        if config.enabled() {
            warn!(
                message = "Enabling `acknowledgements` on sources themselves is deprecated in favor of enabling them in the sink configuration, and will be removed in a future version.",
                component_id = self.key.id(),
            );
        }

        config
            .merge_default(&self.globals.acknowledgements)
            .merge_default(&self.acknowledgements.into())
            .enabled()
    }

    /// Gets the log namespacing to use. The passed in value is from the source itself
    /// and will override any global default if it's set.
    pub fn log_namespace(&self, namespace: Option<bool>) -> LogNamespace {
        namespace
            .or(self.schema.log_namespace)
            .unwrap_or(false)
            .into()
    }
}
