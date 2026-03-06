//! Transform system - processes events as they flow through the topology.
//!
//! Transforms are the middle layer of Vector's data pipeline. They sit between
//! sources (which produce events) and sinks (which consume events).
//!
//! # The Three Transform Types
//!
//! Vector provides three transform traits, each optimized for different use cases.
//! Choosing the right trait is crucial for performance and correctness.
//!
//! ## FunctionTransform - The Simplest Option
//!
//! For stateless, one-event-at-a-time processing:
//!
//! ```ignore
//! impl FunctionTransform for MyFilter {
//!     fn transform(&mut self, output: &mut OutputBuffer, event: Event) {
//!         if self.condition.matches(&event) {
//!             output.push(event);  // Keep it
//!         }
//!         // Dropping the event: don't push anything
//!     }
//! }
//! ```
//!
//! **When to use:**
//! - Stateless processing (no memory between events)
//! - One event in, zero or more events out
//! - CPU-bound work (can benefit from parallelization)
//! - No async I/O needed
//!
//! **Examples:** `filter`, `remap` (simple cases), field manipulation
//!
//! **Performance characteristics:**
//! - Can be parallelized automatically (when `enable_concurrency() = true`)
//! - Inlined into the topology event loop (no separate task overhead)
//! - Zero-cost abstraction for simple cases
//!
//! ## SyncTransform - Multi-Output Routing
//!
//! Like `FunctionTransform`, but with named outputs:
//!
//! ```ignore
//! impl SyncTransform for MyRouter {
//!     fn transform(&mut self, event: Event, output: &mut TransformOutputsBuf) {
//!         if event_matches_route_a(&event) {
//!             output.push(Some("route_a"), event);
//!         } else if event_matches_route_b(&event) {
//!             output.push(Some("route_b"), event);
//!         } else {
//!             output.push(None, event);  // Default output
//!         }
//!     }
//! }
//! ```
//!
//! **When to use:**
//! - Need to send events to different downstream paths
//! - Still stateless and synchronous
//! - No async I/O needed
//!
//! **Examples:** `route`, `swimlanes`, `remap` with `reroute_dropped`
//!
//! **Why not just use FunctionTransform?**
//! `FunctionTransform` only has a single output buffer. Named outputs require
//! separate downstream channels in the topology, which `SyncTransform` provides.
//!
//! ## TaskTransform - Async and Stateful
//!
//! For transforms that need full control over the event stream:
//!
//! ```ignore
//! impl TaskTransform<Event> for MyAggregator {
//!     fn transform(
//!         self: Box<Self>,
//!         input: Pin<Box<dyn Stream<Item = Event> + Send>>,
//!     ) -> Pin<Box<dyn Stream<Item = Event> + Send>> {
//!         // Full control: buffer events, wait for timers, do async I/O
//!         Box::pin(async_stream::stream! {
//!             let mut buffer = Vec::new();
//!             let mut interval = tokio::time::interval(Duration::from_secs(10));
//!             
//!             loop {
//!                 tokio::select! {
//!                     _ = interval.tick() => {
//!                         // Flush aggregated events
//!                         for event in aggregate(&buffer) {
//!                             yield event;
//!                         }
//!                         buffer.clear();
//!                     }
//!                     Some(event) = input.next() => {
//!                         buffer.push(event);
//!                     }
//!                     else => break,
//!                 }
//!             }
//!         })
//!     }
//! }
//! ```
//!
//! **When to use:**
//! - Need to buffer events across time windows
//! - Need async I/O (database lookups, HTTP calls)
//! - Need to maintain state between events
//! - Need backpressure awareness
//!
//! **Examples:** `aggregate`, `reduce`, `dedupe`, `lua` transform
//!
//! **Performance characteristics:**
//! - Runs as a separate Tokio task (adds scheduling overhead)
//! - Cannot be parallelized (single task owns the stream)
//! - Full control over when events are emitted
//!
//! # OutputBuffer vs TransformOutputsBuf
//!
//! These types represent where transformed events go:
//!
//! **OutputBuffer** (for `FunctionTransform`):
//! - Single, unnamed output
//! - Methods: `push(event)`, `drain()`, `len()`
//! - Events go to the default downstream channel
//!
//! **TransformOutputsBuf** (for `SyncTransform`):
//! - Multiple named outputs plus a default
//! - Methods: `push(None, event)` for default, `push(Some("name"), event)` for named
//! - Creates separate channels in the topology
//!
//! # How the Topology Uses These Traits
//!
//! The topology builder wraps each transform type differently:
//!
//! ```ignore
//! // FunctionTransform / SyncTransform: Inlined in the pipeline loop
//! loop {
//!     let events = input_channel.recv().await;
//!     let mut output = buffer.new_buf();
//!     for event in events {
//!         transform.transform(&mut output, event);
//!     }
//!     output_channel.send(output).await;
//! }
//!
//! // TaskTransform: Separate task with stream ownership
//! let output_stream = transform.transform(input_stream);
//! // Spawn as independent task
//! tokio::spawn(async move {
//!     while let Some(event) = output_stream.next().await {
//!         output_channel.send(event).await;
//!     }
//! });
//! ```
//!
//! This is why `FunctionTransform` is more efficient for simple cases - there's no
//! additional task boundary or stream overhead.
//!
//! # Concurrency Model
//!
//! When `enable_concurrency()` returns `true`:
//!
//! ```ignore
//! // Without concurrency:
//! [input] --> [transform] --> [output]
//!
//! // With concurrency (FunctionTransform only):
//!             +-> [transform worker 1] -+
//! [input] ----+-> [transform worker 2] -+--> [output]
//!             +-> [transform worker 3] -+
//! ```
//!
//! Multiple workers pull from the same input queue. This is safe because
//! `FunctionTransform` is stateless - each worker has its own clone of the transform.
//! Events may be processed out of order, but this doesn't matter for stateless transforms.
//!
//! # Choosing the Right Transform Type
//!
//! | Need | FunctionTransform | SyncTransform | TaskTransform |
//! |------|-------------------|---------------|---------------|
//! | Stateless per-event | Yes | Yes | Overkill |
//! | Multiple named outputs | No | Yes | No |
//! | Async I/O | No | No | Yes |
//! | Buffering/windowing | No | No | Yes |
//! | Parallel execution | Yes | Yes | No |
//! | Ordering guarantees | No | No | Depends on implementation |

use std::{collections::HashMap, pin::Pin, sync::Arc};

use futures::{Stream, StreamExt};

use crate::{
    config::OutputId,
    event::{Event, EventArray, EventContainer, EventMutRef, into_event_stream},
    schema::Definition,
};

mod outputs;
#[cfg(feature = "lua")]
pub mod runtime_transform;

pub use outputs::{OutputBuffer, TransformOutputs, TransformOutputsBuf};

/// The transform enum - each transform is one of these variants.
///
/// This is the runtime representation of a transform, created by `TransformConfig::build()`.
/// The variant determines how the topology executes the transform.
///
/// # Why an Enum?
///
/// Using an enum instead of a trait object for the top-level `Transform` allows:
/// 1. **Pattern matching**: The topology can handle each variant with appropriate optimization
/// 2. **Size efficiency**: No vtable overhead for the common case
/// 3. **Clear semantics**: Each variant has distinct execution semantics
///
/// # Execution Models
///
/// ```ignore
/// // Function/Synchronous: Inlined in the pipeline loop
/// loop {
///     let events = input.recv().await;
///     for event in events {
///         transform.transform(&mut output, event);
///     }
///     output.send().await;
/// }
///
/// // Task: Separate task with stream ownership
/// let output_stream = transform.transform(input_stream);
/// tokio::spawn(async move {
///     while let Some(event) = output_stream.next().await {
///         output.send(event).await;
///     }
/// });
/// ```
///
/// While function transforms can be run out of order, or concurrently, task
/// transforms act as a coordination or barrier point.
pub enum Transform {
    /// Simple stateless transform that processes one event at a time.
    ///
    /// **Execution:** Inlined in the topology event loop. May be parallelized
    /// if `enable_concurrency()` returns true.
    ///
    /// **Use when:** Stateless, synchronous, one-event-at-a-time processing.
    Function(Box<dyn FunctionTransform>),

    /// Synchronous transform that can output to multiple destinations.
    ///
    /// **Execution:** Inlined in the topology event loop. May be parallelized
    /// if `enable_concurrency()` returns true.
    ///
    /// **Use when:** Need to route events to different downstream paths based
    /// on conditions.
    Synchronous(Box<dyn SyncTransform>),

    /// Async transform that coordinates with other transforms.
    ///
    /// **Execution:** Runs as a separate Tokio task with full stream ownership.
    /// Cannot be parallelized.
    ///
    /// **Use when:** Need async I/O, buffering, windowing, or state management.
    Task(Box<dyn TaskTransform<EventArray>>),
}

impl Transform {
    /// Create a new function transform.
    ///
    /// These functions are "stateless" and can be run in parallel, without
    /// regard for coordination.
    ///
    /// **Note:** You should prefer to implement this over [`TaskTransform`]
    /// where possible.
    pub fn function(v: impl FunctionTransform + 'static) -> Self {
        Transform::Function(Box::new(v))
    }

    /// Create a new synchronous transform.
    ///
    /// This is a broader trait than the simple [`FunctionTransform`] in that it allows transforms
    /// to write to multiple outputs. Those outputs must be known in advance and returned via
    /// `TransformConfig::outputs`. Attempting to send to any output not registered in advance is
    /// considered a bug and will cause a panic.
    pub fn synchronous(v: impl SyncTransform + 'static) -> Self {
        Transform::Synchronous(Box::new(v))
    }

    /// Create a new task transform.
    ///
    /// These tasks are coordinated, and map a stream of some `U` to some other
    /// `T`.
    ///
    /// **Note:** You should prefer to implement [`FunctionTransform`] over this
    /// where possible.
    pub fn task(v: impl TaskTransform<EventArray> + 'static) -> Self {
        Transform::Task(Box::new(v))
    }

    /// Create a new task transform over individual `Event`s.
    ///
    /// This is a convenience wrapper that handles the Event -> EventArray conversion
    /// automatically. Use this when your transform naturally works with individual events
    /// rather than batches.
    ///
    /// These tasks are coordinated, and map a stream of some `U` to some other
    /// `T`.
    ///
    /// **Note:** You should prefer to implement [`FunctionTransform`] over this
    /// where possible.
    ///
    /// # Panics
    ///
    /// TODO
    pub fn event_task(v: impl TaskTransform<Event> + 'static) -> Self {
        Transform::Task(Box::new(WrapEventTask(v)))
    }

    /// Transmute the inner transform into a task transform.
    ///
    /// # Panics
    ///
    /// If the transform is a [`FunctionTransform`] this will panic.
    pub fn into_task(self) -> Box<dyn TaskTransform<EventArray>> {
        match self {
            Transform::Task(t) => t,
            _ => {
                panic!("Called `Transform::into_task` on something that was not a task variant.")
            }
        }
    }
}

/// A simple stateless transform.
///
/// This is the most common and efficient transform type. It processes one event at a time,
/// synchronously, with no access to async operations. The transform can:
/// - Pass the event through unchanged
/// - Drop the event (don't push to output)
/// - Transform the event and push it
/// - Push multiple derived events (fan-out)
///
/// # Why This Trait?
///
/// The simplicity of this trait enables powerful optimizations:
/// 1. **Parallelization**: Multiple clones can run in parallel (when `enable_concurrency() = true`)
/// 2. **Inlining**: The topology can inline this into its event loop (no task boundary)
/// 3. **Batching**: The topology handles batching efficiently
///
/// # Implementation Example
///
/// ```ignore
/// struct Filter {
///     condition: Condition,
/// }
///
/// impl FunctionTransform for Filter {
///     fn transform(&mut self, output: &mut OutputBuffer, event: Event) {
///         if self.condition.matches(&event) {
///             output.push(event);
///         }
///         // Dropping: just don't push to output
///     }
/// ```
///
/// # Thread Safety
///
/// - `Send + Sync`: Required for parallel execution
/// - `DynClone`: Each parallel worker gets its own clone
/// - No mutable state between calls (except for internal metrics/caches)
///
/// **Note:** You should prefer to implement this over [`TaskTransform`]
/// where possible, as it's more efficient.
pub trait FunctionTransform: Send + dyn_clone::DynClone + Sync {
    /// Transform a single event.
    ///
    /// The implementation receives one event and can push zero or more events to the output.
    /// The output buffer handles batching automatically - just call `push()` for each
    /// event you want to emit.
    ///
    /// # Event Ownership
    ///
    /// You take ownership of the event. You can:
    /// - Push it as-is: `output.push(event)`
    /// - Modify it: `event.as_mut_log().insert("field", value); output.push(event)`
    /// - Drop it: just return without pushing
    /// - Split it into multiple: create new events and push each one
    ///
    /// # Performance Tips
    ///
    /// - Avoid allocations in the hot path
    /// - Use `Event::as_mut_log()` instead of cloning
    /// - Batch your output by pushing multiple times (the buffer handles this)
    fn transform(&mut self, output: &mut OutputBuffer, event: Event);
}

dyn_clone::clone_trait_object!(FunctionTransform);

/// Transforms that need full control over the event stream.
///
/// This is the most powerful transform type, giving you complete ownership of a stream
/// of events. Use this when you need:
/// - **Async operations**: Database lookups, HTTP calls, file I/O
/// - **Buffering**: Collecting events over time windows
/// - **State management**: Deduplication, aggregation, session tracking
/// - **Backpressure handling**: Controlling the flow based on downstream state
///
/// # Why This Trait?
///
/// Some transforms can't work event-by-event. Consider aggregation:
/// - Need to collect events for 10 seconds
/// - Can't emit anything until the window closes
/// - Must buffer and combine multiple events
///
/// This trait gives you the stream, and you decide when to emit.
///
/// # Implementation Example
///
/// ```ignore
/// struct WindowedAggregator {
///     window_secs: u64,
/// }
///
/// impl TaskTransform<Event> for WindowedAggregator {
///     fn transform(
///         self: Box<Self>,
///         input: Pin<Box<dyn Stream<Item = Event> + Send>>,
///     ) -> Pin<Box<dyn Stream<Item = Event> + Send>> {
///         Box::pin(async_stream::stream! {
///             let mut buffer = Vec::new();
///             let mut interval = tokio::time::interval(Duration::from_secs(self.window_secs));
///             
///             loop {
///                 tokio::select! {
///                     _ = interval.tick() => {
///                         // Window closed, emit aggregated result
///                         if !buffer.is_empty() {
///                             yield aggregate(&buffer);
///                             buffer.clear();
///                         }
///                     }
///                     Some(event) = input.next() => {
///                         buffer.push(event);
///                     }
///                     else => {
///                         // Input closed, flush remaining
///                         if !buffer.is_empty() {
///                             yield aggregate(&buffer);
///                         }
///                         break;
///                     }
///                 }
///             }
///         })
///     }
/// }
/// ```
///
/// # Invariants
///
/// * It is an illegal invariant to implement `FunctionTransform` for a
///   `TaskTransform` or vice versa.
pub trait TaskTransform<T: EventContainer + 'static>: Send + 'static {
    /// Transform the input stream into an output stream.
    ///
    /// You receive full ownership of the input stream and must return an output stream.
    /// The returned stream will be consumed by the topology.
    ///
    /// # The `self: Box<Self>` Pattern
    ///
    /// Note that `self` is consumed. This is intentional:
    /// - TaskTransforms can't be cloned (they may have state)
    /// - The transform runs once, for the lifetime of the topology
    /// - Taking ownership prevents accidental sharing
    ///
    /// # Stream Composition
    ///
    /// Use the `async_stream` crate or `futures::stream` combinators to build
    /// your output stream. Common patterns:
    ///
    /// ```ignore
    /// // Filter: pass through some events
    /// input.filter(|event| async { should_keep(&event) }).boxed()
    ///
    /// // Map: transform each event
    /// input.map(|event| async move { transform(event) }).boxed()
    ///
    /// // Buffer and batch
    /// input.chunks_timeout(100, Duration::from_secs(1)).boxed()
    /// ```
    fn transform(
        self: Box<Self>,
        task: Pin<Box<dyn Stream<Item = T> + Send>>,
    ) -> Pin<Box<dyn Stream<Item = T> + Send>>;

    /// Wrap the transform task to process and emit individual
    /// events. This is used to simplify testing task transforms.
    fn transform_events(
        self: Box<Self>,
        task: Pin<Box<dyn Stream<Item = Event> + Send>>,
    ) -> Pin<Box<dyn Stream<Item = Event> + Send>>
    where
        T: From<Event>,
        T::IntoIter: Send,
    {
        self.transform(task.map(Into::into).boxed())
            .flat_map(into_event_stream)
            .boxed()
    }
}

/// Multi-output synchronous transform.
///
/// Broader than the simple [`FunctionTransform`], this trait allows transforms to write to
/// multiple named outputs. This is essential for routing transforms that send different
/// events to different downstream paths.
///
/// # Why This Trait?
///
/// Some transforms need to split the event stream:
/// - `route`: Send errors to one sink, info logs to another
/// - `remap` with `reroute_dropped`: Send successful events to main output, failed to "dropped"
/// - `swimlanes`: Route events based on conditions to named lanes
///
/// # Named Outputs
///
/// Each output has a name (or `None` for the default). The names must be declared
/// in `TransformConfig::outputs()`:
///
/// ```ignore
/// fn outputs(&self, ...) -> Vec<TransformOutput> {
///     vec![
///         TransformOutput::new(DataType::Log, ...).with_port("errors"),
///         TransformOutput::new(DataType::Log, ...).with_port("info"),
///         TransformOutput::new(DataType::Log, ...),  // Default (no port)
///     ]
/// }
/// ```
///
/// Then in `transform()`:
/// ```ignore
/// output.push(Some("errors"), error_event);
/// output.push(Some("info"), info_event);
/// output.push(None, default_event);
/// ```
///
/// # Implementation Example
///
/// ```ignore
/// struct Router {
///     routes: Vec<(String, Condition)>,
/// }
///
/// impl SyncTransform for Router {
///     fn transform(&mut self, event: Event, output: &mut TransformOutputsBuf) {
///         for (name, condition) in &self.routes {
///             if condition.matches(&event) {
///                 output.push(Some(name), event.clone());
///                 return;
///             }
///         }
///         // No route matched, send to default
///         output.push(None, event);
///     }
/// }
/// ```
///
/// Those outputs must be known in advance and returned via
/// `TransformConfig::outputs`. Attempting to send to any output not registered in advance is
/// considered a bug and will cause a panic.
pub trait SyncTransform: Send + dyn_clone::DynClone + Sync {
    /// Transform a single event, potentially to multiple outputs.
    ///
    /// Like `FunctionTransform::transform`, but with named output support.
    /// Push to `None` for the default output, or `Some("name")` for named outputs.
    fn transform(&mut self, event: Event, output: &mut TransformOutputsBuf);

    /// Transform multiple events in a batch.
    ///
    /// The default implementation calls `transform()` for each event.
    /// Override this if you can optimize batch processing (e.g., bulk lookups).
    fn transform_all(&mut self, events: EventArray, output: &mut TransformOutputsBuf) {
        for event in events.into_events() {
            self.transform(event, output);
        }
    }
}

dyn_clone::clone_trait_object!(SyncTransform);

// Blanket implementation: Any FunctionTransform can be used as a SyncTransform
// with a single default output.
impl<T> SyncTransform for T
where
    T: FunctionTransform,
{
    fn transform(&mut self, event: Event, output: &mut TransformOutputsBuf) {
        FunctionTransform::transform(
            self,
            output.primary_buffer.as_mut().expect("no default output"),
            event,
        );
    }
}

// TODO: this is a bit ugly when we already have the above impl
impl SyncTransform for Box<dyn FunctionTransform> {
    fn transform(&mut self, event: Event, output: &mut TransformOutputsBuf) {
        FunctionTransform::transform(
            self.as_mut(),
            output.primary_buffer.as_mut().expect("no default output"),
            event,
        );
    }
}

#[allow(clippy::implicit_hasher)]
/// Update the runtime schema definition on an event.
///
/// When events pass through transforms, the schema definition needs to be updated to reflect
/// the transform's output schema. This function:
///
/// 1. Looks up the parent component's schema definition
/// 2. Applies that to the event's metadata
/// 3. Sets the new parent ID for downstream tracking
///
/// # Why This Matters
///
/// Schema definitions are used for:
/// - VRL autocomplete and type checking
/// - Schema validation at sinks
/// - Documentation generation
///
/// The `parent_id` (stored as `upstream_id`) tracks the lineage of the event through
/// the topology. This is used to look up the correct schema definition.
///
/// # The Special Case of No Parent
///
/// Some transforms (like `reduce` or `lua`) don't set a parent ID on their output
/// events because they're creating new events from multiple inputs. In these cases:
/// - All schema definitions must be the same (validated at config time)
/// - We just pick the first one arbitrarily
pub fn update_runtime_schema_definition(
    mut event: EventMutRef,
    output_id: &Arc<OutputId>,
    log_schema_definitions: &HashMap<OutputId, Arc<Definition>>,
) {
    if let EventMutRef::Log(log) = &mut event {
        if let Some(parent_component_id) = log.metadata().upstream_id() {
            if let Some(definition) = log_schema_definitions.get(parent_component_id) {
                log.metadata_mut().set_schema_definition(definition);
            }
        } else {
            // there is no parent defined. That means this event originated from a component that
            // isn't able to track the source, such as `reduce` or `lua`. In these cases, all of the
            // schema definitions _must_ be the same, so the first one is picked
            if let Some(definition) = log_schema_definitions.values().next() {
                log.metadata_mut().set_schema_definition(definition);
            }
        }
    }
    event.metadata_mut().set_upstream_id(Arc::clone(output_id));
}

/// Internal wrapper to adapt `TaskTransform<Event>` to `TaskTransform<EventArray>`.
///
/// This allows transforms that naturally work with individual events to be used
/// in the topology, which expects `EventArray` streams for efficiency.
struct WrapEventTask<T>(T);

impl<T: TaskTransform<Event> + Send + 'static> TaskTransform<EventArray> for WrapEventTask<T> {
    fn transform(
        self: Box<Self>,
        stream: Pin<Box<dyn Stream<Item = EventArray> + Send>>,
    ) -> Pin<Box<dyn Stream<Item = EventArray> + Send>> {
        // Flatten the EventArray stream into individual Events
        let stream = stream.flat_map(into_event_stream).boxed();
        // Transform the individual events
        // Re-package the output Events back into EventArrays
        Box::new(self.0).transform(stream).map(Into::into).boxed()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::event::{LogEvent, Metric, MetricKind, MetricValue};

    #[test]
    fn buffers_output() {
        let mut buf = OutputBuffer::default();
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.0.len(), 0);

        // Push adds a new element
        buf.push(LogEvent::default().into());
        assert_eq!(buf.len(), 1);
        assert_eq!(buf.0.len(), 1);

        // Push of the same type adds to the existing element
        buf.push(LogEvent::default().into());
        assert_eq!(buf.len(), 2);
        assert_eq!(buf.0.len(), 1);

        // Push of a different type adds a new element
        buf.push(
            Metric::new(
                "name",
                MetricKind::Absolute,
                MetricValue::Counter { value: 1.0 },
            )
            .into(),
        );
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.0.len(), 2);

        // And pushing again adds a new element
        buf.push(LogEvent::default().into());
        assert_eq!(buf.len(), 4);
        assert_eq!(buf.0.len(), 3);
    }
}
