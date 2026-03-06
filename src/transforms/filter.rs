//! The `filter` transform - the simplest possible transform example.
//!
//! This transform demonstrates the `FunctionTransform` trait with the most minimal implementation.
//! It's the "Hello World" of Vector transforms, perfect for understanding the fundamentals.
//!
//! # What This Transform Does
//!
//! The filter transform evaluates a condition against each event:
//! - **Match**: Event passes through to the output
//! - **No match**: Event is dropped (with telemetry tracking)
//!
//! # Why This Is a FunctionTransform
//!
//! The filter is a perfect fit for `FunctionTransform` because:
//! 1. **Stateless**: Whether event N passes has no effect on event N+1
//! 2. **Synchronous**: No async I/O, no waiting for external resources
//! 3. **Single output**: Events either go to the output or are dropped
//! 4. **Parallelizable**: Processing events in any order is fine
//!
//! # Configuration Example
//!
//! ```yaml
//! transforms:
//!   my_filter:
//!     type: filter
//!     inputs: ["my_source"]
//!     condition:
//!       type: vrl
//!       source: '.level == "error"'
//! ```
//!
//! # Performance Characteristics
//!
//! With `enable_concurrency() = true`, the topology can parallelize this:
//! ```
//!             +-> [filter worker 1] -+
//! [events] ---+-> [filter worker 2] -+--> [filtered events]
//!             +-> [filter worker 3] -+
//! ```
//!
//! Each worker gets its own clone of the `Filter` struct. The condition check
//! is CPU-bound, so parallelization helps with multi-core machines.

use vector_lib::{
    config::clone_input_definitions,
    configurable::configurable_component,
    internal_event::{Count, InternalEventHandle as _, Registered},
};

use crate::{
    conditions::{AnyCondition, Condition},
    config::{
        DataType, GenerateConfig, Input, OutputId, TransformConfig, TransformContext,
        TransformOutput,
    },
    event::Event,
    internal_events::FilterEventsDropped,
    schema,
    transforms::{FunctionTransform, OutputBuffer, Transform},
};

/// Configuration for the `filter` transform.
///
/// This struct is what gets deserialized from the YAML config. It contains
/// only the condition to evaluate - one of the simplest possible configurations.
///
/// The `#[configurable_component]` attribute:
/// - Generates JSON Schema for the component
/// - Auto-generates documentation
/// - Enables the `type: "filter"` deserialization
///
/// # Example Configuration
///
/// ```yaml
/// type: filter
/// condition:
///   type: vrl
///   source: '.level == "error"'
/// ```
///
/// Or using a reference:
///
/// ```yaml
/// type: filter
/// condition: "my_condition_reference"
/// ```
#[configurable_component(transform("filter", "Filter events based on a set of conditions."))]
#[derive(Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct FilterConfig {
    #[configurable(derived)]
    /// The condition that every input event is matched against.
    ///
    /// If an event is matched by the condition, it is forwarded. Otherwise, the event is dropped.
    condition: AnyCondition,
}

/// Boilerplate: Allow creating FilterConfig from an AnyCondition.
///
/// This is useful in tests and when programmatically building configs.
impl From<AnyCondition> for FilterConfig {
    fn from(condition: AnyCondition) -> Self {
        Self { condition }
    }
}

/// Generates a default config for documentation/examples.
///
/// The `GenerateConfig` trait is used by:
/// - The `vector generate` CLI command
/// - Example generation in documentation
/// - Config validation tests
///
/// This returns a simple "keep events where .message == 'value'" example.
impl GenerateConfig for FilterConfig {
    fn generate_config() -> toml::Value {
        toml::from_str(r#"condition = ".message == \"value\"""#).unwrap()
    }
}

/// The `TransformConfig` implementation - the bridge between config and runtime.
///
/// This implementation defines:
/// 1. How to build the runtime `Filter` from this config
/// 2. What event types this transform accepts
/// 3. What event types this transform produces
/// 4. Whether parallelization is allowed
///
/// The `#[typetag::serde(name = "filter")]` attribute connects this implementation
/// to the `type: filter` field in YAML configs.
#[async_trait::async_trait]
#[typetag::serde(name = "filter")]
impl TransformConfig for FilterConfig {
    /// Builds the runtime filter transform.
    ///
    /// This is called during topology building. The method:
    /// 1. Compiles the condition from config into an executable form
    /// 2. Creates the `Filter` struct that implements `FunctionTransform`
    /// 3. Wraps it in `Transform::function()`
    ///
    /// # Why async?
    ///
    /// The `build()` method is async because some transforms need async I/O
    /// during initialization (e.g., fetching remote schemas). The filter doesn't
    /// need this, but the trait requires it for consistency.
    ///
    /// # Context Usage
    ///
    /// The filter uses:
    /// - `enrichment_tables`: For VRL conditions that do lookups
    /// - `metrics_storage`: For VRL program metrics
    ///
    /// It doesn't use:
    /// - `key`: Not needed (no logging with component ID)
    /// - `globals`: Not needed (no timezone or log schema dependency)
    /// - `merged_schema_definition`: Not needed (no schema-aware processing)
    async fn build(&self, context: &TransformContext) -> crate::Result<Transform> {
        // Compile the condition: config -> executable
        let condition = self
            .condition
            .build(&context.enrichment_tables, &context.metrics_storage)?;

        // Create the runtime transform and wrap it
        Ok(Transform::function(Filter::new(condition)))
    }

    /// Declares what event types this transform accepts.
    ///
    /// `Input::all()` means: "I can handle logs, metrics, and traces."
    ///
    /// The filter doesn't care about event type - it just evaluates the condition.
    /// A VRL condition like `.level == "error"` works on logs (has .level field)
    /// but also won't crash on metrics (condition just returns false).
    fn input(&self) -> Input {
        Input::all()
    }

    /// Declares what this transform outputs.
    ///
    /// The filter has a single output that:
    /// - Passes through whatever event types came in
    /// - Maintains the same schema as the input
    ///
    /// # Schema Propagation
    ///
    /// `clone_input_definitions(input_definitions)` copies the input schemas
    /// to the output. This is correct because:
    /// - We don't modify events (filtering doesn't change schema)
    /// - We pass events through unchanged
    ///
    /// If we were modifying events, we'd need to compute a new schema.
    fn outputs(
        &self,
        _: &TransformContext,
        input_definitions: &[(OutputId, schema::Definition)],
    ) -> Vec<TransformOutput> {
        vec![TransformOutput::new(
            DataType::all_bits(),                       // Output any event type
            clone_input_definitions(input_definitions), // Same schema as input
        )]
    }

    /// Enables parallel execution for better throughput.
    ///
    /// Returns `true` because:
    /// - The filter is stateless (no memory between events)
    /// - Event order doesn't matter (filtering is independent)
    /// - The condition check is CPU-bound
    ///
    /// With concurrency enabled, the topology can spawn multiple filter workers:
    /// ```
    /// [input] --> [worker 1] -+
    ///          --> [worker 2] -+--> [output]
    ///          --> [worker 3] -+
    /// ```
    ///
    /// This is safe because each worker gets its own clone of `Filter`,
    /// and the condition check doesn't depend on previous events.
    fn enable_concurrency(&self) -> bool {
        true
    }
}

/// The runtime filter transform.
///
/// This struct implements `FunctionTransform` and does the actual work of filtering.
/// It's created from `FilterConfig::build()` and lives for the lifetime of the topology.
///
/// # Fields
///
/// - `condition`: The compiled condition to evaluate (from `AnyCondition`)
/// - `events_dropped`: Telemetry handle for tracking dropped events
///
/// # Why Clone?
///
/// When `enable_concurrency()` is true, the topology clones this struct for each
/// parallel worker. The condition must be cloneable (it's Arc-wrapped internally),
/// and the telemetry handle is separate for each clone.
#[derive(Clone)]
pub struct Filter {
    /// The compiled condition that determines if an event passes.
    ///
    /// `Condition` is the runtime form of `AnyCondition`. It has a `check()` method
    /// that evaluates the condition against an event.
    condition: Condition,

    /// Telemetry handle for tracking dropped events.
    ///
    /// This emits internal events that show up in metrics:
    /// - `filter_events_dropped_total`: Count of dropped events
    ///
    /// The `Registered<T>` type is a handle to pre-registered telemetry.
    /// Using `register!` at construction time avoids runtime registration overhead.
    events_dropped: Registered<FilterEventsDropped>,
}

impl Filter {
    /// Creates a new filter with the given condition.
    ///
    /// The `register!` macro pre-registers the telemetry event for efficiency.
    pub fn new(condition: Condition) -> Self {
        Self {
            condition,
            events_dropped: register!(FilterEventsDropped),
        }
    }
}

/// The `FunctionTransform` implementation - where the actual filtering happens.
///
/// This is the hot path that gets called for every event. The implementation:
/// 1. Evaluates the condition against the event
/// 2. If match: pushes to output
/// 3. If no match: emits telemetry and drops (by not pushing)
///
/// # The OutputBuffer Pattern
///
/// ```ignore
/// fn transform(&mut self, output: &mut OutputBuffer, event: Event) {
///     // Option 1: Keep the event
///     output.push(event);
///
///     // Option 2: Drop the event
///     // (do nothing - don't push)
///
///     // Option 3: Transform and keep
///     event.as_mut_log().insert("filtered", true);
///     output.push(event);
///
///     // Option 4: Split into multiple
///     output.push(event.clone());
///     output.push(event);
/// }
/// ```
///
/// The filter uses option 1 (keep) or option 2 (drop).
impl FunctionTransform for Filter {
    fn transform(&mut self, output: &mut OutputBuffer, event: Event) {
        // Evaluate the condition
        // `check()` returns (result, event) because some conditions need to
        // inspect the event without cloning (takes ownership, returns it)
        let (result, event) = self.condition.check(event);

        if result {
            // Condition matched: pass through to output
            output.push(event);
        } else {
            // Condition didn't match: drop the event
            // We emit telemetry so users can monitor how many events are being filtered out
            self.events_dropped.emit(Count(1));
        }
        // If we don't push to output, the event is dropped
    }
}

/// Tests for the filter transform.
///
/// These tests demonstrate:
/// 1. Basic filtering (pass vs drop)
/// 2. Multiple event types (logs, metrics)
/// 3. Topology integration (end-to-end)
///
/// # Testing Patterns
///
/// **Unit Testing with OutputBuffer:**
/// ```ignore
/// let mut filter = Filter::new(condition);
/// let mut output = OutputBuffer::default();
/// filter.transform(&mut output, event);
/// assert_eq!(output.len(), 1);  // Event passed through
/// ```
///
/// **Integration Testing with Topology:**
/// ```ignore
/// let (topology, rx) = create_topology(input_stream, config).await;
/// tx.send(event).await.unwrap();
/// assert_eq!(rx.recv().await.unwrap(), expected_event);
/// ```
#[cfg(test)]
mod test {
    use std::sync::Arc;

    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;
    use vector_lib::{
        config::ComponentKey,
        event::{Metric, MetricKind, MetricValue},
    };

    use super::*;
    use crate::{
        conditions::ConditionConfig,
        config::schema::Definition,
        event::{Event, LogEvent},
        test_util::components::assert_transform_compliance,
        transforms::test::create_topology,
    };

    const TEST_SOURCE_COMPONENT_ID: &str = "in";
    const TEST_UPSTREAM_COMPONENT_ID: &str = "transform";
    const TEST_SOURCE_TYPE: &str = "unit_test_stream";

    /// Sets up the metadata that real events would have.
    ///
    /// This ensures tests match production behavior where events carry:
    /// - `source_id`: Which source produced them
    /// - `upstream_id`: Which transform they passed through last
    /// - `source_type`: The type of source (for metrics)
    /// - `schema_definition`: Type information for VRL
    fn set_expected_metadata(event: &mut Event) {
        event.set_source_id(Arc::new(ComponentKey::from(TEST_SOURCE_COMPONENT_ID)));
        event.set_upstream_id(Arc::new(OutputId::from(TEST_UPSTREAM_COMPONENT_ID)));
        event.set_source_type(TEST_SOURCE_TYPE);
        event
            .metadata_mut()
            .set_schema_definition(&Arc::new(Definition::default_legacy_namespace()));
    }

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<super::FilterConfig>();
    }

    /// Tests basic filtering: logs pass, metrics get dropped.
    ///
    /// This test uses a condition that only matches log events.
    /// - Log event: passes through
    /// - Metric event: dropped (no output)
    #[tokio::test]
    async fn filter_basic() {
        assert_transform_compliance(async {
            // Create a filter that only keeps log events
            let transform_config = FilterConfig::from(AnyCondition::from(ConditionConfig::IsLog));

            // Create a test topology
            let (tx, rx) = mpsc::channel(1);
            let (topology, mut out) =
                create_topology(ReceiverStream::new(rx), transform_config).await;

            // Test 1: Log event should pass through
            let mut log = Event::from(LogEvent::from("message"));
            tx.send(log.clone()).await.unwrap();

            // Set the metadata that would normally be set by the topology
            set_expected_metadata(&mut log);

            assert_eq!(out.recv().await.unwrap(), log);

            // Test 2: Metric event should be filtered out
            let metric = Event::from(Metric::new(
                "test metric",
                MetricKind::Incremental,
                MetricValue::Counter { value: 1.0 },
            ));
            tx.send(metric).await.unwrap();

            // Clean up and verify no more events
            drop(tx);
            topology.stop().await;
            assert_eq!(out.recv().await, None); // No metric came through
        })
        .await;
    }
}
