//! Sink configuration traits and types.
//!
//! Sinks are the exit points for data from Vector. They:
//! - Receive processed events from transforms (or directly from sources)
//! - Serialize and send data to external systems
//! - Handle acknowledgements for end-to-end delivery guarantees
//!
//! # Buffer Management
//!
//! Sinks have configurable buffers that provide:
//! - Backpressure handling (block or drop when full)
//! - Persistence across restarts (disk buffers)
//! - In-memory buffering for throughput
//!
//! # Health Checks
//!
//! Sinks can have health checks that verify connectivity
//! before Vector declares itself ready.
//!
//! # Implementing a Sink
//!
//! 1. Create a config struct implementing `SinkConfig`
//! 2. Implement `build()` to create the sink runtime
//! 3. Define input type requirements (logs, metrics, or both)
//!
//! # The Buffer System
//!
//! Every sink has an associated buffer, configured via `BufferConfig`:
//!
//! ```yaml
//! sinks:
//!   my_sink:
//!     type: http
//!     inputs: ["my_source"]
//!     buffer:
//!       type: disk
//!       max_size: 104900000  # 100MB
//!       when_full: block
//! ```
//!
//! **Why buffers exist:**
//!
//! Sinks send data to external systems, which have varying throughput:
//! - Local file: Very fast
//! - Cloud service: Network latency, rate limits
//! - Database: Query overhead, connection limits
//!
//! Without buffers:
//! 1. Slow sink would block upstream transforms/sources
//! 2. Backpressure would propagate through the entire pipeline
//! 3. Fast sources would be throttled by slow sinks
//!
//! With buffers:
//! 1. Fast sources write to buffer (fast)
//! 2. Buffer absorbs temporary slowdowns
//! 3. Slow sink drains buffer at its own pace
//! 4. Buffer full? Apply configured strategy (block/drop)
//!
//! **Buffer types:**
//!
//! - `Memory`: Fast, but data lost on restart, limited by RAM
//! - `DiskV2`: Persistent, survives restarts, limited by disk space
//!
//! **Resource conflicts:**
//!
//! Disk buffers claim a unique resource:
//! ```ignore
//! fn resources(&self, id: &ComponentKey) -> Vec<Resource> {
//!     vec![Resource::DiskBuffer(id.to_string())]
//! }
//! ```
//!
//! This prevents two sinks from writing to the same buffer file, which
//! would corrupt data.
//!
//! # Health Checks
//!
//! Sinks can verify connectivity before Vector becomes "ready":
//!
//! ```yaml
//! sinks:
//!   my_sink:
//!     type: http
//!     healthcheck:
//!       enabled: true
//!       uri: "http://api.example.com/health"
//!       timeout: 10s
//! ```
//!
//! When `--require-healthy` is set:
//! 1. Vector builds all components
//! 2. Runs health checks for all sinks with `healthcheck.enabled = true`
//! 3. If ANY health check fails, Vector exits with an error
//! 4. If ALL health checks pass, Vector enters running state
//!
//! This ensures misconfigured sinks are caught before accepting traffic.
//!
//! # Acknowledgements (End-to-End Delivery)
//!
//! Sinks can enable acknowledgements to provide delivery guarantees:
//!
//! ```yaml
//! sinks:
//!   kafka_sink:
//!     type: kafka
//!     acknowledgements:
//!       enabled: true
//! ```
//!
//! **How it works:**
//!
//! 1. Source receives an event (e.g., from a file)
//! 2. Event flows through transforms to the sink
//! 3. Sink sends event to Kafka and waits for confirmation
//! 4. Confirmation propagates back to the source
//! 5. Source marks the file offset as "processed"
//!
//! If Vector crashes mid-pipeline:
//! - Without acks: Events in-flight are lost, source already advanced
//! - With acks: Source hasn't advanced, events are replayed on restart
//!
//! **The propagation chain:**
//!
//! ```ignore
//! // In config/mod.rs
//! fn propagate_acknowledgements(&mut self) {
//!     // Find sinks with acks enabled
//!     for sink in self.sinks.values().filter(|s| s.acknowledgements().enabled()) {
//!         // Trace back to sources
//!         for input in sink.inputs {
//!             self.propagate_acks_rec(input);
//!         }
//!     }
//! }
//! ```
//!
//! This is why sources have a `sink_acknowledgements` field - it's set
//! automatically during validation, not by the user.
//!
//! # Input Type Constraints
//!
//! Sinks declare what event types they accept:
//!
//! ```ignore
//! impl SinkConfig for HttpSinkConfig {
//!     fn input(&self) -> Input {
//!         Input::log()  // Only accepts log events
//!     }
//! }
//!
//! impl SinkConfig for PrometheusSinkConfig {
//!     fn input(&self) -> Input {
//!         Input::metric()  // Only accepts metrics
//!     }
//! }
//!
//! impl SinkConfig for ConsoleSinkConfig {
//!     fn input(&self) -> Input {
//!         Input::any()  // Accepts logs, metrics, or traces
//!     }
//! }
//! ```
//!
//! The graph validator ensures type compatibility during config compilation.
//!
//! # The SinkContext
//!
//! When building a sink, Vector provides runtime dependencies:
//!
//! ```ignore
//! pub struct SinkContext {
//!     pub healthcheck: SinkHealthcheckOptions,  // Health check config
//!     pub globals: GlobalOptions,               // Global config
//!     pub enrichment_tables: TableRegistry,     // Lookup tables
//!     pub proxy: ProxyConfig,                   // HTTP proxy settings
//!     pub app_name: String,                     // "vector" (for User-Agent)
//! }
//! ```
//!
//! This is passed to `SinkConfig::build()`, which returns:
//! ```ignore
//! async fn build(&self, cx: SinkContext) -> Result<(VectorSink, Healthcheck)>;
//! ```
//!
//! - `VectorSink`: The running sink instance
//! - `Healthcheck`: A future that verifies connectivity

use std::{cell::RefCell, path::PathBuf, time::Duration};

use async_trait::async_trait;
use dyn_clone::DynClone;
use serde::Serialize;
use serde_with::serde_as;
use vector_lib::{
    buffers::{BufferConfig, BufferType},
    config::{AcknowledgementsConfig, GlobalOptions, Input},
    configurable::{
        Configurable, GenerateError, Metadata, NamedComponent,
        attributes::CustomAttribute,
        configurable_component,
        schema::{SchemaGenerator, SchemaObject},
    },
    id::Inputs,
    sink::VectorSink,
};
use vector_vrl_metrics::MetricsStorage;

use super::{ComponentKey, ProxyConfig, Resource, dot_graph::GraphConfig, schema};
use crate::{
    extra_context::ExtraContext,
    sinks::{Healthcheck, util::UriSerde},
};

/// Type-erased sink configuration.
///
/// This allows storing any sink config in a unified collection.
pub type BoxedSink = Box<dyn SinkConfig>;

impl Configurable for BoxedSink {
    fn referenceable_name() -> Option<&'static str> {
        Some("vector::sinks::Sinks")
    }

    fn metadata() -> Metadata {
        let mut metadata = Metadata::default();
        metadata.set_description("Configurable sinks in Vector.");
        metadata.add_custom_attribute(CustomAttribute::kv("docs::enum_tagging", "internal"));
        metadata.add_custom_attribute(CustomAttribute::kv("docs::enum_tag_field", "type"));
        metadata
    }

    fn generate_schema(
        generator: &RefCell<SchemaGenerator>,
    ) -> Result<SchemaObject, GenerateError> {
        vector_lib::configurable::component::SinkDescription::generate_schemas(generator)
    }
}

impl<T: SinkConfig + 'static> From<T> for BoxedSink {
    fn from(value: T) -> Self {
        Box::new(value)
    }
}

/// Fully resolved sink component.
///
/// Wraps the sink configuration with its inputs, buffer settings,
/// health check options, and proxy configuration.
#[configurable_component]
#[configurable(metadata(docs::component_base_type = "sink"))]
#[derive(Clone, Debug)]
pub struct SinkOuter<T>
where
    T: Configurable + Serialize + 'static,
{
    /// Graph configuration for visualization.
    #[configurable(derived)]
    #[serde(default, skip_serializing_if = "vector_lib::serde::is_default")]
    pub graph: GraphConfig,

    /// Transforms/sources that feed into this sink.
    #[configurable(derived)]
    pub inputs: Inputs<T>,

    /// The full URI to make HTTP healthcheck requests to.
    ///
    /// This must be a valid URI, which requires at least the scheme and host. All other
    /// components -- port, path, etc -- are allowed as well.
    #[configurable(deprecated, metadata(docs::hidden), validation(format = "uri"))]
    pub healthcheck_uri: Option<UriSerde>,

    /// Health check configuration.
    #[configurable(derived, metadata(docs::advanced))]
    #[serde(default, deserialize_with = "crate::serde::bool_or_struct")]
    pub healthcheck: SinkHealthcheckOptions,

    /// Buffer configuration for this sink.
    #[configurable(derived)]
    #[serde(default, skip_serializing_if = "vector_lib::serde::is_default")]
    pub buffer: BufferConfig,

    /// Proxy configuration for HTTP-based sinks.
    #[configurable(derived)]
    #[serde(default, skip_serializing_if = "vector_lib::serde::is_default")]
    pub proxy: ProxyConfig,

    /// The actual sink configuration.
    #[serde(flatten)]
    #[configurable(metadata(docs::hidden))]
    pub inner: BoxedSink,
}

impl<T> SinkOuter<T>
where
    T: Configurable + Serialize,
{
    pub fn new<I, IS>(inputs: I, inner: IS) -> SinkOuter<T>
    where
        I: IntoIterator<Item = T>,
        IS: Into<BoxedSink>,
    {
        SinkOuter {
            inputs: Inputs::from_iter(inputs),
            buffer: Default::default(),
            healthcheck: SinkHealthcheckOptions::default(),
            healthcheck_uri: None,
            inner: inner.into(),
            proxy: Default::default(),
            graph: Default::default(),
        }
    }

    pub fn resources(&self, id: &ComponentKey) -> Vec<Resource> {
        let mut resources = self.inner.resources();
        for stage in self.buffer.stages() {
            match stage {
                BufferType::Memory { .. } => {}
                BufferType::DiskV2 { .. } => resources.push(Resource::DiskBuffer(id.to_string())),
            }
        }
        resources
    }

    pub fn healthcheck(&self) -> SinkHealthcheckOptions {
        if self.healthcheck_uri.is_some() && self.healthcheck.uri.is_some() {
            warn!(
                "Both `healthcheck.uri` and `healthcheck_uri` options are specified. Using value of `healthcheck.uri`."
            )
        } else if self.healthcheck_uri.is_some() {
            warn!(
                "The `healthcheck_uri` option has been deprecated, use `healthcheck.uri` instead."
            )
        }
        SinkHealthcheckOptions {
            uri: self
                .healthcheck
                .uri
                .clone()
                .or_else(|| self.healthcheck_uri.clone()),
            ..self.healthcheck.clone()
        }
    }

    pub const fn proxy(&self) -> &ProxyConfig {
        &self.proxy
    }

    pub(super) fn map_inputs<U>(self, f: impl Fn(&T) -> U) -> SinkOuter<U>
    where
        U: Configurable + Serialize,
    {
        let inputs = self.inputs.iter().map(f).collect::<Vec<_>>();
        self.with_inputs(inputs)
    }

    pub(crate) fn with_inputs<I, U>(self, inputs: I) -> SinkOuter<U>
    where
        I: IntoIterator<Item = U>,
        U: Configurable + Serialize,
    {
        SinkOuter {
            inputs: Inputs::from_iter(inputs),
            inner: self.inner,
            buffer: self.buffer,
            healthcheck: self.healthcheck,
            healthcheck_uri: self.healthcheck_uri,
            proxy: self.proxy,
            graph: self.graph,
        }
    }
}

/// Healthcheck configuration.
#[serde_as]
#[configurable_component]
#[derive(Clone, Debug)]
#[serde(default)]
pub struct SinkHealthcheckOptions {
    /// Whether or not to check the health of the sink when Vector starts up.
    pub enabled: bool,

    /// Timeout duration for healthcheck in seconds.
    #[serde_as(as = "serde_with::DurationSecondsWithFrac<f64>")]
    #[serde(
        default = "default_healthcheck_timeout",
        skip_serializing_if = "is_default_healthcheck_timeout"
    )]
    pub timeout: Duration,

    /// The full URI to make HTTP healthcheck requests to.
    ///
    /// This must be a valid URI, which requires at least the scheme and host. All other
    /// components -- port, path, etc -- are allowed as well.
    #[configurable(validation(format = "uri"))]
    pub uri: Option<UriSerde>,
}

const fn default_healthcheck_timeout() -> Duration {
    Duration::from_secs(10)
}

fn is_default_healthcheck_timeout(timeout: &Duration) -> bool {
    timeout == &default_healthcheck_timeout()
}

impl Default for SinkHealthcheckOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            uri: None,
            timeout: default_healthcheck_timeout(),
        }
    }
}

impl From<bool> for SinkHealthcheckOptions {
    fn from(enabled: bool) -> Self {
        Self {
            enabled,
            ..Default::default()
        }
    }
}

impl From<UriSerde> for SinkHealthcheckOptions {
    fn from(uri: UriSerde) -> Self {
        Self {
            uri: Some(uri),
            ..Default::default()
        }
    }
}

/// Generalized interface for describing and building sink components.
#[async_trait]
#[typetag::serde(tag = "type")]
pub trait SinkConfig: DynClone + NamedComponent + core::fmt::Debug + Send + Sync {
    /// Builds the sink with the given context.
    ///
    /// If the sink is built successfully, `Ok(...)` is returned containing the sink and the sink's
    /// healthcheck.
    ///
    /// # Errors
    ///
    /// If an error occurs while building the sink, an error variant explaining the issue is
    /// returned.
    async fn build(&self, cx: SinkContext) -> crate::Result<(VectorSink, Healthcheck)>;

    /// Gets the input configuration for this sink.
    fn input(&self) -> Input;

    /// Gets the files to watch to trigger reload
    fn files_to_watch(&self) -> Vec<&PathBuf> {
        Vec::new()
    }

    /// Gets the list of resources, if any, used by this sink.
    ///
    /// Resources represent dependencies -- network ports, file descriptors, and so on -- that
    /// cannot be shared between components at runtime. This ensures that components can not be
    /// configured in a way that would deadlock the spawning of a topology, and as well, allows
    /// Vector to determine the correct order for rebuilding a topology during configuration reload
    /// when resources must first be reclaimed before being reassigned, and so on.
    fn resources(&self) -> Vec<Resource> {
        Vec::new()
    }

    /// Gets the acknowledgements configuration for this sink.
    fn acknowledgements(&self) -> &AcknowledgementsConfig;
}

dyn_clone::clone_trait_object!(SinkConfig);

#[derive(Clone, Debug)]
pub struct SinkContext {
    pub healthcheck: SinkHealthcheckOptions,
    pub globals: GlobalOptions,
    pub enrichment_tables: vector_lib::enrichment::TableRegistry,
    pub metrics_storage: MetricsStorage,
    pub proxy: ProxyConfig,
    pub schema: schema::Options,
    pub app_name: String,
    pub app_name_slug: String,

    /// Extra context data provided by the running app and shared across all components. This can be
    /// used to pass shared settings or other data from outside the components.
    pub extra_context: ExtraContext,
}

impl Default for SinkContext {
    fn default() -> Self {
        Self {
            healthcheck: Default::default(),
            globals: Default::default(),
            enrichment_tables: Default::default(),
            metrics_storage: Default::default(),
            proxy: Default::default(),
            schema: Default::default(),
            app_name: crate::get_app_name().to_string(),
            app_name_slug: crate::get_slugified_app_name(),
            extra_context: Default::default(),
        }
    }
}

impl SinkContext {
    pub const fn globals(&self) -> &GlobalOptions {
        &self.globals
    }

    pub const fn proxy(&self) -> &ProxyConfig {
        &self.proxy
    }
}
