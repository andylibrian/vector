//! Event system - the core data model for Vector.
//!
//! # The Unified Event Model: Why It Exists
//!
//! Observability pipelines face a fundamental challenge: data comes from many sources
//! in many formats (JSON, syslog, Prometheus metrics, OpenTelemetry traces, etc.).
//! Without a unified internal representation, every transform and sink would need to
//! understand every input format, creating an N×M complexity explosion.
//!
//! Vector solves this by converting all incoming data into one of three `Event` variants
//! at the source boundary. After this conversion, every downstream component works with
//! the same types. This architectural decision confines format complexity to the edges
//! (sources and sinks), while the core pipeline operates on a universal representation.
//!
//! # The Three Event Types
//!
//! Events are the fundamental unit of data flowing through Vector. This module
//! defines three event types:
//!
//! - **Log**: Structured log data (key-value pairs) - the most flexible and common type
//! - **Metric**: Time-series metrics (counters, gauges, histograms, etc.) - typed numerical data
//! - **Trace**: Distributed tracing spans - follows OpenTelemetry conventions
//!
//! # Event Flow Through the Pipeline
//!
//! ```text
//! External Data → Source → [Event] → Transform → [Event] → Sink → External Destination
//!                            ↑                          ↑
//!                    Format conversion           Pure Event processing
//!                    happens here                happens here
//! ```
//!
//! 1. **Sources** produce events from external systems (perform format conversion)
//! 2. **Transforms** process/modify events (work with Event types, not formats)
//! 3. **Sinks** serialize and send events to destinations (convert back to target format)
//!
//! # Key Architectural Concepts
//!
//! ## Event Arrays (Batching for Throughput)
//!
//! Events are typically processed in batches (`EventArray`) for efficiency.
//! This reduces per-event overhead and enables better throughput. The `EventContainer`
//! trait provides a unified interface for working with both single events and arrays.
//!
//! ## Copy-on-Write (Efficient Fanout)
//!
//! When a source's output fans out to multiple sinks, every sink needs a copy of the event.
//! Naively cloning events with large nested data is expensive. Vector uses `Arc`-based
//! copy-on-write semantics: reads share memory, writes create private copies only when needed.
//!
//! ## Finalization (End-to-End Acknowledgements)
//!
//! Events can have finalizers attached - callbacks that are invoked when the event is
//! either successfully delivered or dropped. This enables end-to-end acknowledgements,
//! crucial for reliable data delivery. Sources attach finalizers, sinks update their status.
//!
//! ## Metadata (The Invisible Sidecar)
//!
//! Each event carries metadata that is not part of user-visible data:
//! - **Source component ID** - for lineage tracking
//! - **Timestamps** - for latency tracking
//! - **Schema definitions** - for structure tracking
//! - **Secrets** - sensitive values that should never be logged
//! - **Finalizers** - delivery tracking callbacks
//!
//! This separation is critical: transforms can freely modify event data without accidentally
//! exposing secrets or breaking delivery tracking.
//!
//! # Why Not a Fixed Struct for Logs?
//!
//! Log events could be modeled as a fixed struct with fields like `message`, `timestamp`, `host`.
//! But log formats vary enormously - a Kubernetes pod log has different fields from an Apache
//! access log, which differs from an application JSON log. A fixed struct would require either:
//! - A rigid schema (losing data that doesn't fit)
//! - A catch-all `extras` map (making the struct pointless)
//!
//! Instead, `LogEvent` stores data as a flexible `Value::Object` (a `BTreeMap<KeyString, Value>`),
//! allowing any source to insert any fields, and any transform to access them by path.
//!
//! # Thread Safety and Performance
//!
//! - Events use `Arc` for cheap cloning and efficient memory sharing
//! - Size calculations are cached to avoid repeated expensive computations
//! - The enum is deliberately kept small to minimize branching overhead

use std::{convert::TryInto, fmt::Debug, sync::Arc};

pub use array::{EventArray, EventContainer, LogArray, MetricArray, TraceArray, into_event_stream};
pub use estimated_json_encoded_size_of::EstimatedJsonEncodedSizeOf;
pub use finalization::{
    BatchNotifier, BatchStatus, BatchStatusReceiver, EventFinalizer, EventFinalizers, EventStatus,
    Finalizable,
};
pub use log_event::LogEvent;
pub use metadata::{DatadogMetricOriginMetadata, EventMetadata, WithMetadata};
pub use metric::{Metric, MetricKind, MetricTags, MetricValue, StatisticKind};
pub use r#ref::{EventMutRef, EventRef};
use serde::{Deserialize, Serialize};
pub use trace::TraceEvent;
use vector_buffers::EventCount;
use vector_common::{
    EventDataEq, byte_size_of::ByteSizeOf, config::ComponentKey, finalization,
    internal_event::TaggedEventsSent, json_size::JsonSize, request_metadata::GetEventCountTags,
};
pub use vrl::value::{KeyString, ObjectMap, Value};
#[cfg(feature = "vrl")]
pub use vrl_target::{TargetEvents, VrlTarget};

use crate::config::{LogNamespace, OutputId};

pub mod array;
pub mod discriminant;
mod estimated_json_encoded_size_of;
mod log_event;
#[cfg(feature = "lua")]
pub mod lua;
pub mod merge_state;
mod metadata;
pub mod metric;
pub mod proto;
mod r#ref;
mod ser;
#[cfg(test)]
mod test;
mod trace;
pub mod util;
#[cfg(feature = "vrl")]
mod vrl_target;

pub const PARTIAL: &str = "_partial";

/// The primary event enum - each event flowing through Vector is one of these variants.
///
/// This enum is the universal data type that flows through Vector's pipeline. Every component
/// (source, transform, sink) works with this type, ensuring a consistent interface throughout
/// the system.
///
/// # Design Philosophy
///
/// The enum is deliberately kept small with only three variants. The complexity is pushed into
/// the variants themselves:
/// - `LogEvent` is a flexible key-value store that can represent any structured log data
/// - `Metric` is a structured metric with name, tags, and typed value (counter, gauge, etc.)
/// - `TraceEvent` represents a span in distributed tracing, following OpenTelemetry conventions
///
/// # Why an Enum Instead of a Trait Object?
///
/// Using an enum instead of `Box<dyn EventTrait>` provides:
/// - **Better performance**: No vtable indirection, better cache locality
/// - **Exhaustive matching**: The compiler ensures all variants are handled
/// - **Memory efficiency**: Events are stack-allocated (mostly), with heap data in Arcs
/// - **Pattern matching**: Ergonomic destructuring and access patterns
///
/// # The `large_enum_variant` Clippy Warning
///
/// We explicitly allow this warning because the size difference between variants is intentional:
/// - `LogEvent` is larger due to its flexible field storage
/// - `Metric` and `TraceEvent` are smaller
/// The alternative (boxing LogEvent) would add indirection overhead for the common case.
#[derive(PartialEq, Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum Event {
    /// A structured log event with flexible key-value fields.
    ///
    /// This is the most common event type, used for logs, application events,
    /// and any semi-structured data. LogEvent supports arbitrary field names,
    /// nested structures, and various value types.
    Log(LogEvent),

    /// A metric event with typed numerical data.
    ///
    /// Metrics represent time-series data and have specific semantics based on
    /// their value type (Counter, Gauge, Histogram, etc.). The Metric struct
    /// includes series identification (name, tags) and timestamp information.
    Metric(Metric),

    /// A distributed tracing span.
    ///
    /// Trace events follow OpenTelemetry conventions and carry span information
    /// including trace ID, span ID, parent span ID, and timing information.
    /// Internally, TraceEvent wraps a LogEvent for storage flexibility.
    Trace(TraceEvent),
}

/// Calculate the in-memory byte size of this event for buffer accounting.
///
/// This is used by Vector's buffering system to track memory usage and enforce
/// size limits. Each variant delegates to its inner type's implementation.
///
/// **Why this matters**: Vector buffers events in memory (and optionally on disk).
/// Accurate size tracking prevents OOM conditions and enables fair resource allocation.
impl ByteSizeOf for Event {
    fn allocated_bytes(&self) -> usize {
        match self {
            Event::Log(log_event) => log_event.allocated_bytes(),
            Event::Metric(metric_event) => metric_event.allocated_bytes(),
            Event::Trace(trace_event) => trace_event.allocated_bytes(),
        }
    }
}

/// Estimate the JSON-encoded size for throughput metrics.
///
/// This provides a fast approximation of how large the event will be when serialized
/// to JSON, without actually performing the serialization. Used for:
/// - Pre-allocating buffers
/// - Throughput metrics (bytes/second)
/// - Batch size decisions
impl EstimatedJsonEncodedSizeOf for Event {
    fn estimated_json_encoded_size_of(&self) -> JsonSize {
        match self {
            Event::Log(log_event) => log_event.estimated_json_encoded_size_of(),
            Event::Metric(metric_event) => metric_event.estimated_json_encoded_size_of(),
            Event::Trace(trace_event) => trace_event.estimated_json_encoded_size_of(),
        }
    }
}

/// Count events - always 1 for a single Event.
///
/// This trait is shared with EventArray, which returns its length.
/// Having a unified trait allows generic code to work with both single events
/// and batches without branching.
impl EventCount for Event {
    fn event_count(&self) -> usize {
        1
    }
}

/// Extract finalizers for delivery acknowledgement.
///
/// This is called by sinks before sending events. The finalizers track whether
/// the event was successfully delivered, encountered an error, or was dropped.
/// This enables end-to-end acknowledgement from sources.
///
/// **How it works**:
/// 1. Source creates an EventFinalizer and attaches it to event metadata
/// 2. Event flows through transforms (finalizers are preserved via Arc)
/// 3. Sink calls `take_finalizers()` before sending
/// 4. After write result, sink updates finalizer status
/// 5. Source receives callback and commits (e.g., Kafka offset commit)
impl Finalizable for Event {
    fn take_finalizers(&mut self) -> EventFinalizers {
        match self {
            Event::Log(log_event) => log_event.take_finalizers(),
            Event::Metric(metric) => metric.take_finalizers(),
            Event::Trace(trace_event) => trace_event.take_finalizers(),
        }
    }
}

impl GetEventCountTags for Event {
    fn get_tags(&self) -> TaggedEventsSent {
        match self {
            Event::Log(log) => log.get_tags(),
            Event::Metric(metric) => metric.get_tags(),
            Event::Trace(trace) => trace.get_tags(),
        }
    }
}

impl Event {
    /// Return self as a `LogEvent` reference.
    ///
    /// # When to Use
    ///
    /// Use this when you know the event is a log and need read-only access.
    /// This is common in transforms that only process log events.
    ///
    /// # Panics
    ///
    /// This function panics if self is anything other than an `Event::Log`.
    /// The panic message includes the actual event type for debugging.
    ///
    /// # Example
    ///
    /// ```ignore
    /// fn process_log(event: &Event) {
    ///     let log = event.as_log();
    ///     if let Some(msg) = log.get("message") {
    ///         println!("Message: {:?}", msg);
    ///     }
    /// }
    /// ```
    pub fn as_log(&self) -> &LogEvent {
        match self {
            Event::Log(log) => log,
            _ => panic!("Failed type coercion, {self:?} is not a log event"),
        }
    }

    /// Return self as a mutable `LogEvent` reference.
    ///
    /// # When to Use
    ///
    /// Use this when you need to modify log event fields. This will trigger
    /// copy-on-write semantics if the event is shared via Arc.
    ///
    /// # Panics
    ///
    /// This function panics if self is anything other than an `Event::Log`.
    pub fn as_mut_log(&mut self) -> &mut LogEvent {
        match self {
            Event::Log(log) => log,
            _ => panic!("Failed type coercion, {self:?} is not a log event"),
        }
    }

    /// Consume self and return the inner `LogEvent`.
    ///
    /// # When to Use
    ///
    /// Use this when you need ownership of the LogEvent and don't need the
    /// Event wrapper anymore. This is more efficient than cloning.
    ///
    /// # Panics
    ///
    /// This function panics if self is anything other than an `Event::Log`.
    pub fn into_log(self) -> LogEvent {
        match self {
            Event::Log(log) => log,
            _ => panic!("Failed type coercion, {self:?} is not a log event"),
        }
    }

    /// Fallibly convert self into a `LogEvent`.
    ///
    /// # When to Use
    ///
    /// Use this when you're not sure if the event is a log. Returns `None`
    /// for non-log events without panicking.
    ///
    /// # Example
    ///
    /// ```ignore
    /// if let Some(log) = event.try_into_log() {
    ///     process_log(log);
    /// } else {
    ///     warn!("Expected log event, got {:?}", event);
    /// }
    /// ```
    pub fn try_into_log(self) -> Option<LogEvent> {
        match self {
            Event::Log(log) => Some(log),
            _ => None,
        }
    }

    /// Return self as a `LogEvent` reference if it's a log.
    ///
    /// This is the non-panicking version of `as_log()`. Returns `None` for
    /// non-log events.
    pub fn maybe_as_log(&self) -> Option<&LogEvent> {
        match self {
            Event::Log(log) => Some(log),
            _ => None,
        }
    }

    /// Return self as a `Metric` reference.
    ///
    /// # When to Use
    ///
    /// Use this when you know the event is a metric and need read-only access.
    /// Common in metric-specific transforms and aggregators.
    ///
    /// # Panics
    ///
    /// This function panics if self is anything other than an `Event::Metric`.
    pub fn as_metric(&self) -> &Metric {
        match self {
            Event::Metric(metric) => metric,
            _ => panic!("Failed type coercion, {self:?} is not a metric"),
        }
    }

    /// Return self as a mutable `Metric` reference.
    ///
    /// # When to Use
    ///
    /// Use this when you need to modify metric fields (e.g., updating tags,
    /// modifying values). This will trigger copy-on-write if the metric is shared.
    ///
    /// # Panics
    ///
    /// This function panics if self is anything other than an `Event::Metric`.
    pub fn as_mut_metric(&mut self) -> &mut Metric {
        match self {
            Event::Metric(metric) => metric,
            _ => panic!("Failed type coercion, {self:?} is not a metric"),
        }
    }

    /// Consume self and return the inner `Metric`.
    ///
    /// # When to Use
    ///
    /// Use this when you need ownership of the Metric and don't need the
    /// Event wrapper anymore.
    ///
    /// # Panics
    ///
    /// This function panics if self is anything other than an `Event::Metric`.
    pub fn into_metric(self) -> Metric {
        match self {
            Event::Metric(metric) => metric,
            _ => panic!("Failed type coercion, {self:?} is not a metric"),
        }
    }

    /// Fallibly convert self into a `Metric`.
    ///
    /// Returns `None` for non-metric events without panicking.
    pub fn try_into_metric(self) -> Option<Metric> {
        match self {
            Event::Metric(metric) => Some(metric),
            _ => None,
        }
    }

    /// Return self as a `TraceEvent` reference.
    ///
    /// # When to Use
    ///
    /// Use this when working with distributed tracing spans. Trace events
    /// follow OpenTelemetry conventions.
    ///
    /// # Panics
    ///
    /// This function panics if self is anything other than an `Event::Trace`.
    pub fn as_trace(&self) -> &TraceEvent {
        match self {
            Event::Trace(trace) => trace,
            _ => panic!("Failed type coercion, {self:?} is not a trace event"),
        }
    }

    /// Return self as a mutable `TraceEvent` reference.
    ///
    /// # Panics
    ///
    /// This function panics if self is anything other than an `Event::Trace`.
    pub fn as_mut_trace(&mut self) -> &mut TraceEvent {
        match self {
            Event::Trace(trace) => trace,
            _ => panic!("Failed type coercion, {self:?} is not a trace event"),
        }
    }

    /// Consume self and return the inner `TraceEvent`.
    ///
    /// # Panics
    ///
    /// This function panics if self is anything other than an `Event::Trace`.
    pub fn into_trace(self) -> TraceEvent {
        match self {
            Event::Trace(trace) => trace,
            _ => panic!("Failed type coercion, {self:?} is not a trace event"),
        }
    }

    /// Fallibly convert self into a `TraceEvent`.
    ///
    /// Returns `None` for non-trace events without panicking.
    pub fn try_into_trace(self) -> Option<TraceEvent> {
        match self {
            Event::Trace(trace) => Some(trace),
            _ => None,
        }
    }

    /// Get a reference to the event's metadata.
    ///
    /// Every event carries metadata that is not part of the user-visible data.
    /// This includes source tracking, finalizers, schema information, and secrets.
    pub fn metadata(&self) -> &EventMetadata {
        match self {
            Self::Log(log) => log.metadata(),
            Self::Metric(metric) => metric.metadata(),
            Self::Trace(trace) => trace.metadata(),
        }
    }

    /// Get a mutable reference to the event's metadata.
    ///
    /// Use this to modify metadata fields like source_id, finalizers, or schema.
    pub fn metadata_mut(&mut self) -> &mut EventMetadata {
        match self {
            Self::Log(log) => log.metadata_mut(),
            Self::Metric(metric) => metric.metadata_mut(),
            Self::Trace(trace) => trace.metadata_mut(),
        }
    }

    /// Destroy the event and extract just the metadata.
    ///
    /// This is useful when you need to transfer metadata to a new event
    /// or inspect metadata after processing is complete.
    pub fn into_metadata(self) -> EventMetadata {
        match self {
            Self::Log(log) => log.into_parts().1,
            Self::Metric(metric) => metric.into_parts().2,
            Self::Trace(trace) => trace.into_parts().1,
        }
    }

    /// Attach a batch notifier to this event.
    ///
    /// Batch notifiers enable acknowledgement of when a group of events (a batch) has been
    /// delivered or failed. This is crucial for:
    /// - Kafka sources: commit offsets only after successful delivery
    /// - File sources: update file position markers only after write
    /// - HTTP sources: acknowledge successful requests
    ///
    /// When an event is created with a batch notifier, it shares that notifier with all
    /// other events in the same batch. The notifier tracks the aggregate status
    /// (all events must succeed for the batch to be marked as delivered).
    #[must_use]
    pub fn with_batch_notifier(self, batch: &BatchNotifier) -> Self {
        match self {
            Self::Log(log) => log.with_batch_notifier(batch).into(),
            Self::Metric(metric) => metric.with_batch_notifier(batch).into(),
            Self::Trace(trace) => trace.with_batch_notifier(batch).into(),
        }
    }

    /// Optionally attach a batch notifier to this event.
    #[must_use]
    pub fn with_batch_notifier_option(self, batch: &Option<BatchNotifier>) -> Self {
        match self {
            Self::Log(log) => log.with_batch_notifier_option(batch).into(),
            Self::Metric(metric) => metric.with_batch_notifier_option(batch).into(),
            Self::Trace(trace) => trace.with_batch_notifier_option(batch).into(),
        }
    }

    /// Returns a reference to the source component ID.
    ///
    /// The source ID identifies which source component created this event.
    /// This is used for:
    /// - Routing decisions in transforms
    /// - Lineage tracking
    /// - Metrics attribution (which source produced this event)
    #[must_use]
    pub fn source_id(&self) -> Option<&Arc<ComponentKey>> {
        self.metadata().source_id()
    }

    /// Sets the source component ID in the event metadata.
    ///
    /// This is typically called by sources when creating events.
    pub fn set_source_id(&mut self, source_id: Arc<ComponentKey>) {
        self.metadata_mut().set_source_id(source_id);
    }

    /// Sets the upstream component ID in the event metadata.
    ///
    /// The upstream ID identifies the previous component in the pipeline graph.
    /// This is used for schema resolution - events carry the schema from their upstream component.
    pub fn set_upstream_id(&mut self, upstream_id: Arc<OutputId>) {
        self.metadata_mut().set_upstream_id(upstream_id);
    }

    /// Sets the source type in the event metadata.
    ///
    /// Examples: "kafka", "http", "file", "syslog"
    /// This is useful for conditional logic in transforms and sinks.
    pub fn set_source_type(&mut self, source_type: &'static str) {
        self.metadata_mut().set_source_type(source_type);
    }

    /// Builder methods for fluent event construction.
    ///
    /// These methods allow chaining configuration calls in a fluent style:
    /// ```ignore
    /// let event = Event::Log(log)
    ///     .with_source_id(source_id)
    ///     .with_source_type("kafka");
    /// ```
    #[must_use]
    pub fn with_source_id(mut self, source_id: Arc<ComponentKey>) -> Self {
        self.metadata_mut().set_source_id(source_id);
        self
    }

    /// Sets the source type and returns self for chaining.
    #[must_use]
    pub fn with_source_type(mut self, source_type: &'static str) -> Self {
        self.metadata_mut().set_source_type(source_type);
        self
    }

    /// Sets the upstream ID and returns self for chaining.
    #[must_use]
    pub fn with_upstream_id(mut self, upstream_id: Arc<OutputId>) -> Self {
        self.metadata_mut().set_upstream_id(upstream_id);
        self
    }

    /// Creates an Event from a JSON value.
    ///
    /// # Namespace Handling
    ///
    /// The `log_namespace` parameter controls how the JSON is interpreted:
    ///
    /// - **Vector namespace**: The JSON value is converted directly to a VRL Value.
    ///   This allows non-object JSON (arrays, primitives) as valid inputs.
    ///
    /// - **Legacy namespace**: Only JSON objects are allowed. This maintains
    ///   backward compatibility with Vector v1 behavior where logs were always objects.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Legacy namespace is used AND the JSON value is not an object
    ///
    /// # Example
    ///
    /// ```ignore
    /// let json = json!({"message": "hello", "level": "info"});
    /// let event = Event::from_json_value(json, LogNamespace::Vector)?;
    /// ```
    pub fn from_json_value(
        value: serde_json::Value,
        log_namespace: LogNamespace,
    ) -> crate::Result<Self> {
        match log_namespace {
            LogNamespace::Vector => Ok(LogEvent::from(Value::from(value)).into()),
            LogNamespace::Legacy => match value {
                serde_json::Value::Object(fields) => Ok(LogEvent::from(
                    fields
                        .into_iter()
                        .map(|(k, v)| (k.into(), v.into()))
                        .collect::<ObjectMap>(),
                )
                .into()),
                _ => Err(crate::Error::from(
                    "Attempted to convert non-Object JSON into an Event.",
                )),
            },
        }
    }
}

impl EventDataEq for Event {
    fn event_data_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Log(a), Self::Log(b)) => a.event_data_eq(b),
            (Self::Metric(a), Self::Metric(b)) => a.event_data_eq(b),
            (Self::Trace(a), Self::Trace(b)) => a.event_data_eq(b),
            _ => false,
        }
    }
}

impl finalization::AddBatchNotifier for Event {
    fn add_batch_notifier(&mut self, batch: BatchNotifier) {
        let finalizer = EventFinalizer::new(batch);
        match self {
            Self::Log(log) => log.add_finalizer(finalizer),
            Self::Metric(metric) => metric.add_finalizer(finalizer),
            Self::Trace(trace) => trace.add_finalizer(finalizer),
        }
    }
}

impl TryInto<serde_json::Value> for Event {
    type Error = serde_json::Error;

    fn try_into(self) -> Result<serde_json::Value, Self::Error> {
        match self {
            Event::Log(fields) => serde_json::to_value(fields),
            Event::Metric(metric) => serde_json::to_value(metric),
            Event::Trace(fields) => serde_json::to_value(fields),
        }
    }
}

/// Convert protobuf StatisticKind to domain StatisticKind.
///
/// Used when encoding/decoding metrics in protobuf-based transports (for example, gRPC).
impl From<proto::StatisticKind> for StatisticKind {
    fn from(kind: proto::StatisticKind) -> Self {
        match kind {
            proto::StatisticKind::Histogram => StatisticKind::Histogram,
            proto::StatisticKind::Summary => StatisticKind::Summary,
        }
    }
}

/// Convert metric Sample to protobuf DistributionSample
///
/// Distribution samples are used to calculate percentiles and contain:
/// - `value`: The actual measurement value
/// - `rate`: How many times this value occurred (for weighted distributions)
impl From<metric::Sample> for proto::DistributionSample {
    fn from(sample: metric::Sample) -> Self {
        Self {
            value: sample.value,
            rate: sample.rate,
        }
    }
}

/// Convert protobuf DistributionSample back to metric Sample
impl From<proto::DistributionSample> for metric::Sample {
    fn from(sample: proto::DistributionSample) -> Self {
        Self {
            value: sample.value,
            rate: sample.rate,
        }
    }
}

/// Convert protobuf HistogramBucket into metric Bucket
///
/// Histogram buckets define ranges of values. Each bucket has:
/// - `upper_limit`: The upper bound of this bucket (inclusive)
/// - `count`: How many samples fell into this bucket
impl From<proto::HistogramBucket> for metric::Bucket {
    fn from(bucket: proto::HistogramBucket) -> Self {
        Self {
            upper_limit: bucket.upper_limit,
            count: u64::from(bucket.count),
        }
    }
}

/// Convert metric Bucket to protobuf HistogramBucket3 (version 3)
impl From<metric::Bucket> for proto::HistogramBucket3 {
    fn from(bucket: metric::Bucket) -> Self {
        Self {
            upper_limit: bucket.upper_limit,
            count: bucket.count,
        }
    }
}

/// Convert protobuf HistogramBucket3 back to metric Bucket
impl From<proto::HistogramBucket3> for metric::Bucket {
    fn from(bucket: proto::HistogramBucket3) -> Self {
        Self {
            upper_limit: bucket.upper_limit,
            count: bucket.count,
        }
    }
}

/// Convert metric Quantile to protobuf SummaryQuantile
///
/// Quantiles represent specific percentiles in a distribution:
/// - `quantile`: The percentile (0.0 to 1.0)
/// - `value`: The value at this percentile
impl From<metric::Quantile> for proto::SummaryQuantile {
    fn from(quantile: metric::Quantile) -> Self {
        Self {
            quantile: quantile.quantile,
            value: quantile.value,
        }
    }
}

/// Convert protobuf SummaryQuantile back to metric Quantile
impl From<proto::SummaryQuantile> for metric::Quantile {
    fn from(quantile: proto::SummaryQuantile) -> Self {
        Self {
            quantile: quantile.quantile,
            value: quantile.value,
        }
    }
}

/// Convert LogEvent into Event enum
///
/// This is a zero-cost conversion - LogEvent is already an Event variant
impl From<LogEvent> for Event {
    fn from(log: LogEvent) -> Self {
        Event::Log(log)
    }
}

/// Convert Metric into Event enum
impl From<Metric> for Event {
    fn from(metric: Metric) -> Self {
        Event::Metric(metric)
    }
}

/// Convert TraceEvent into Event enum
impl From<TraceEvent> for Event {
    fn from(trace: TraceEvent) -> Self {
        Event::Trace(trace)
    }
}

/// Trait for optionally getting a mutable LogEvent reference
///
/// This is used in generic code that might receive an Event or a mutable LogEvent.
/// It's similar to `maybe_as_log()` but provides mutable access.
pub trait MaybeAsLogMut {
    /// Returns a mutable reference to the inner LogEvent if this is a Log variant.
    ///
    /// Returns `None` for Metric or Trace variants.
    fn maybe_as_log_mut(&mut self) -> Option<&mut LogEvent>;
}

impl MaybeAsLogMut for Event {
    fn maybe_as_log_mut(&mut self) -> Option<&mut LogEvent> {
        match self {
            Event::Log(log) => Some(log),
            _ => None,
        }
    }
}
