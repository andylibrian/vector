//! Vector source components - the entry points for data into Vector.
//!
//! # What Are Sources?
//!
//! Sources ingest data from external systems and convert it into Events that
//! flow through the Vector pipeline. Every Vector topology has at least one
//! source.
//!
//! # Source Types
//!
//! Sources can be categorized by how they receive data:
//!
//! - **Polling sources**: Periodically fetch data (e.g., `host_metrics`, `aws_s3`)
//! - **Listening sources**: Accept incoming connections (e.g., `http_server`, `syslog`)
//! - **Tailing sources**: Watch files for new data (e.g., `file`, `kubernetes_logs`)
//!
//! # Feature Flags
//!
//! Each source is behind a feature flag (e.g., `sources-file`, `sources-http_server`).
//! This allows building minimal Vector binaries with only needed sources.
//!
//! The pattern `#[cfg(feature = "sources-foo")]` means:
//! - The module is only compiled if the feature is enabled
//! - The source won't appear in the binary if disabled
//! - Documentation reflects which sources are available
//!
//! # Adding a New Source
//!
//! 1. Create `src/sources/my_source.rs` (or `src/sources/my_source/` for complex sources)
//! 2. Define a config struct with `#[configurable_component(source("my_source", "..."))]`
//! 3. Implement `SourceConfig` with `#[typetag::serde(name = "my_source")]`
//! 4. Add feature flag to `Cargo.toml`: `sources-my_source = []`
//! 5. Register module here: `#[cfg(feature = "sources-my_source")] pub mod my_source;`
//! 6. Add internal events in `src/internal_events/`
//! 7. Run `make check-component-docs` to verify documentation

#![allow(missing_docs)]
use snafu::Snafu;

// Feature-gated source module registrations
// Each source is compiled only if its feature flag is enabled
// This keeps binary size small and compile times fast

#[cfg(feature = "sources-amqp")]
pub mod amqp;
#[cfg(feature = "sources-apache_metrics")]
pub mod apache_metrics;
#[cfg(feature = "sources-aws_ecs_metrics")]
pub mod aws_ecs_metrics;
#[cfg(feature = "sources-aws_kinesis_firehose")]
pub mod aws_kinesis_firehose;
#[cfg(feature = "sources-aws_s3")]
pub mod aws_s3;
#[cfg(feature = "sources-aws_sqs")]
pub mod aws_sqs;
#[cfg(feature = "sources-datadog_agent")]
pub mod datadog_agent;
#[cfg(feature = "sources-demo_logs")]
pub mod demo_logs; // Good example source for learning
#[cfg(feature = "sources-dnstap")]
pub mod dnstap;
#[cfg(feature = "sources-docker_logs")]
pub mod docker_logs;
#[cfg(feature = "sources-eventstoredb_metrics")]
pub mod eventstoredb_metrics;
#[cfg(feature = "sources-exec")]
pub mod exec;
#[cfg(feature = "sources-file")]
pub mod file; // Example of a tailing source
#[cfg(any(
    feature = "sources-stdin",
    all(unix, feature = "sources-file_descriptor")
))]
pub mod file_descriptors;
#[cfg(feature = "sources-fluent")]
pub mod fluent;
#[cfg(feature = "sources-gcp_pubsub")]
pub mod gcp_pubsub;
#[cfg(feature = "sources-heroku_logs")]
pub mod heroku_logs;
#[cfg(feature = "sources-host_metrics")]
pub mod host_metrics;
#[cfg(feature = "sources-http_client")]
pub mod http_client;
#[cfg(feature = "sources-http_server")]
pub mod http_server; // Example of a listening source
#[cfg(feature = "sources-internal_logs")]
pub mod internal_logs;
#[cfg(feature = "sources-internal_metrics")]
pub mod internal_metrics;
#[cfg(all(unix, feature = "sources-journald"))]
pub mod journald;
#[cfg(feature = "sources-kafka")]
pub mod kafka;
#[cfg(feature = "sources-kubernetes_logs")]
pub mod kubernetes_logs;
#[cfg(feature = "sources-logstash")]
pub mod logstash;
#[cfg(feature = "sources-mongodb_metrics")]
pub mod mongodb_metrics;
#[cfg(feature = "sources-mqtt")]
pub mod mqtt;
#[cfg(feature = "sources-nats")]
pub mod nats;
#[cfg(feature = "sources-nginx_metrics")]
pub mod nginx_metrics;
#[cfg(feature = "sources-okta")]
pub mod okta;
#[cfg(feature = "sources-opentelemetry")]
pub mod opentelemetry;
#[cfg(feature = "sources-postgresql_metrics")]
pub mod postgresql_metrics;
#[cfg(any(
    feature = "sources-prometheus-scrape",
    feature = "sources-prometheus-remote-write",
    feature = "sources-prometheus-pushgateway"
))]
pub mod prometheus;
#[cfg(feature = "sources-pulsar")]
pub mod pulsar;
#[cfg(feature = "sources-redis")]
pub mod redis;
#[cfg(feature = "sources-socket")]
pub mod socket;
#[cfg(feature = "sources-splunk_hec")]
pub mod splunk_hec;
#[cfg(feature = "sources-static_metrics")]
pub mod static_metrics;
#[cfg(feature = "sources-statsd")]
pub mod statsd;
#[cfg(feature = "sources-syslog")]
pub mod syslog;
#[cfg(feature = "sources-vector")]
pub mod vector;
#[cfg(feature = "sources-websocket")]
pub mod websocket;

// Shared utilities for source implementations
pub mod util;

// Re-export the Source type alias
pub use vector_lib::source::Source;

/// Common build errors for sources.
///
/// # Why This Enum Exists
///
/// Sources can fail to build for various reasons. Rather than using string
/// errors, we define common error types for better error handling and
/// more helpful error messages.
#[allow(dead_code)] // Easier than listing out all the features that use this
#[derive(Debug, Snafu)]
pub enum BuildError {
    /// Failed to parse a URI in the source configuration.
    #[snafu(display("URI parse error: {}", source))]
    UriParseError { source: ::http::uri::InvalidUri },

    /// Failed to compile VRL program in the source configuration.
    #[snafu(display("VRL compilation error: {}", message))]
    VrlCompilationError { message: String },
}
