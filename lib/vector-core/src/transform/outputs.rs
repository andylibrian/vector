//! Output buffering for transforms.
//!
//! When a transform processes events, it needs somewhere to put the results. The types in this
//! module provide that buffering, with different capabilities for different use cases.
//!
//! # The Buffer Types
//!
//! **OutputBuffer**: Single output buffer used by `FunctionTransform`
//! - Simple push/drain interface
//! - Automatically coalesces events of the same type into arrays
//! - Used when there's only one downstream path
//!
//! **TransformOutputsBuf**: Multi-output buffer used by `SyncTransform`
//! - Contains one `OutputBuffer` for each named output
//! - Plus a primary buffer for the default output
//! - Used when events can be routed to different downstream paths
//!
//! **TransformOutputs**: The sending side that consumes `TransformOutputsBuf`
//! - Owns the fanout channels for each output
//! - Sends buffered events downstream
//! - Handles telemetry (events sent counts)
//!
//! # Why Automatic Coalescing?
//!
//! Events come in different types: logs, metrics, traces. For efficiency, Vector processes
//! them in type-homogeneous batches (`EventArray`). When a transform pushes events one at a time,
//! `OutputBuffer` automatically groups them:
//!
//! ```ignore
//! // Push events one at a time:
//! output.push(log_event_1);
//! output.push(log_event_2);
//! output.push(metric_event_1);
//!
//! // Internal structure becomes:
//! [LogArray([log_event_1, log_event_2]), MetricArray([metric_event_1])]
//! ```
//!
//! This avoids the overhead of creating a new `EventArray` for each event while still
//! maintaining the type separation that downstream components expect.
//!
//! # Memory Management
//!
//! Buffers are reused across batches to reduce allocations:
//! ```ignore
//! loop {
//!     let mut buf = outputs.new_buf_with_capacity(1024);
//!     for event in events {
//!         transform.transform(&mut buf, event);
//!     }
//!     outputs.send(buf).await;
//!     // buf is dropped here, memory returned to pool
//! }
//! ```

use std::{collections::HashMap, error, sync::Arc, time::Instant};

use vector_common::{
    EventDataEq,
    byte_size_of::ByteSizeOf,
    internal_event::{
        self, CountByteSize, DEFAULT_OUTPUT, EventsSent, InternalEventHandle as _, Registered,
        register,
    },
    json_size::JsonSize,
};

use crate::{
    config,
    config::{ComponentKey, OutputId},
    event::{EstimatedJsonEncodedSizeOf, Event, EventArray, EventContainer, EventMutRef, EventRef},
    fanout::{self, Fanout},
    schema,
};

/// Internal representation of a single transform output.
///
/// Each output has:
/// - A `Fanout` to send events to downstream subscribers
/// - Telemetry tracking for events sent
/// - Schema definitions for runtime type checking
/// - The output's ID for lineage tracking
///
/// This struct is not public - users interact with buffers, not with outputs directly.
struct TransformOutput {
    /// The fanout channel that distributes events to all downstream subscribers.
    ///
    /// When you configure multiple sinks to read from the same transform output,
    /// they all subscribe to this fanout. Each event is cloned and sent to each subscriber.
    fanout: Fanout,

    /// Telemetry handle for tracking events sent through this output.
    ///
    /// Emits `EventsSent` internal events for observability into transform throughput.
    events_sent: Registered<EventsSent>,

    /// Schema definitions indexed by upstream output ID.
    ///
    /// When multiple upstream components feed into this transform, each may have
    /// a different schema. This map allows looking up the correct schema based
    /// on which upstream component an event came from.
    log_schema_definitions: HashMap<OutputId, Arc<schema::Definition>>,

    /// The ID of this output, used for event lineage tracking.
    ///
    /// Events passing through have their `upstream_id` set to this value,
    /// allowing downstream components to know their origin.
    output_id: Arc<OutputId>,
}

/// Manages the outputs of a transform and sends buffered events downstream.
///
/// This is the "sending side" that owns the fanout channels. It works in tandem
/// with `TransformOutputsBuf` (the "buffering side"):
///
/// ```ignore
/// // During transform setup:
/// let (outputs, control_channels) = TransformOutputs::new(specs, component_key);
///
/// // During event processing:
/// let mut buffer = outputs.new_buf_with_capacity(1024);
/// for event in events {
///     transform.transform(&mut buffer, event);  // Fills buffer
/// }
/// outputs.send(buffer).await;  // Sends to fanouts
/// ```
///
/// # Multi-Output Support
///
/// For transforms with multiple outputs (like `route`):
/// - `primary_output`: The default/unnamed output
/// - `named_outputs`: Named outputs like "errors", "info"
///
/// Each output has its own fanout channel and telemetry.
pub struct TransformOutputs {
    /// The specification for each output (used to create buffers).
    outputs_spec: Vec<config::TransformOutput>,

    /// The default output (port = None).
    ///
    /// Most transforms have only this output. Events pushed with `output.push(None, event)`
    /// go here.
    primary_output: Option<TransformOutput>,

    /// Named outputs indexed by their port name.
    ///
    /// Events pushed with `output.push(Some("errors"), event)` go to the "errors" entry.
    named_outputs: HashMap<String, TransformOutput>,
}

impl TransformOutputs {
    /// Creates a new `TransformOutputs` from the output specifications.
    ///
    /// This is called during topology building. It:
    /// 1. Creates a fanout channel for each output
    /// 2. Registers telemetry for events sent
    /// 3. Returns control channels for downstream subscription
    ///
    /// The control channels are used by the topology to connect downstream components:
    /// ```ignore
    /// let (outputs, controls) = TransformOutputs::new(specs, &component_key);
    ///
    /// // Later, when connecting to a sink:
    /// let control_channel = controls.get(&Some("errors"));
    /// sink.subscribe(control_channel);
    /// ```
    pub fn new(
        outputs_in: Vec<config::TransformOutput>,
        component_key: &ComponentKey,
    ) -> (Self, HashMap<Option<String>, fanout::ControlChannel>) {
        let outputs_spec = outputs_in.clone();
        let mut primary_output = None;
        let mut named_outputs = HashMap::new();
        let mut controls = HashMap::new();

        for output in outputs_in {
            // Each output gets its own fanout channel
            let (fanout, control) = Fanout::new();

            // Arc-wrap the schema definitions for efficient cloning
            let log_schema_definitions = output
                .log_schema_definitions
                .into_iter()
                .map(|(id, definition)| (id, Arc::new(definition)))
                .collect();

            match output.port {
                None => {
                    // Default output
                    primary_output = Some(TransformOutput {
                        fanout,
                        events_sent: register(EventsSent::from(internal_event::Output(Some(
                            DEFAULT_OUTPUT.into(),
                        )))),
                        log_schema_definitions,
                        output_id: Arc::new(OutputId {
                            component: component_key.clone(),
                            port: None,
                        }),
                    });
                    controls.insert(None, control);
                }
                Some(name) => {
                    // Named output (e.g., "errors" from route transform)
                    named_outputs.insert(
                        name.clone(),
                        TransformOutput {
                            fanout,
                            events_sent: register(EventsSent::from(internal_event::Output(Some(
                                name.clone().into(),
                            )))),
                            log_schema_definitions,
                            output_id: Arc::new(OutputId {
                                component: component_key.clone(),
                                port: Some(name.clone()),
                            }),
                        },
                    );
                    controls.insert(Some(name.clone()), control);
                }
            }
        }

        let me = Self {
            outputs_spec,
            primary_output,
            named_outputs,
        };

        (me, controls)
    }

    /// Creates a new buffer for collecting transformed events.
    ///
    /// Call this at the start of processing a batch of events. The capacity
    /// is a hint to pre-allocate the right amount of memory.
    ///
    /// ```ignore
    /// let mut buffer = outputs.new_buf_with_capacity(1024);
    /// for event in input_events {
    ///     transform.transform(&mut buffer, event);
    /// }
    /// outputs.send(buffer).await;
    /// ```
    pub fn new_buf_with_capacity(&self, capacity: usize) -> TransformOutputsBuf {
        TransformOutputsBuf::new_with_capacity(self.outputs_spec.clone(), capacity)
    }

    /// Sends the events in the buffer to their respective outputs.
    ///
    /// This is where events actually leave the transform and go downstream:
    /// 1. Schema definitions are attached to events (for runtime type checking)
    /// 2. Events are sent through the fanout to subscribers
    /// 3. Telemetry is emitted (events sent count and byte size)
    ///
    /// # Errors
    ///
    /// If an error occurs while sending events to their respective output, an error variant will be
    /// returned detailing the cause. This typically happens when:
    /// - A downstream channel is closed
    /// - Backpressure causes a timeout
    pub async fn send(
        &mut self,
        buf: &mut TransformOutputsBuf,
    ) -> Result<(), Box<dyn error::Error + Send + Sync>> {
        // Send primary/default output
        if let Some(primary) = self.primary_output.as_mut() {
            let Some(buf) = buf.primary_buffer.as_mut() else {
                unreachable!("mismatched outputs");
            };
            Self::send_single_buffer(buf, primary).await?;
        }

        // Send each named output
        for (key, buf) in &mut buf.named_buffers {
            let Some(output) = self.named_outputs.get_mut(key) else {
                unreachable!("unknown output");
            };
            Self::send_single_buffer(buf, output).await?;
        }
        Ok(())
    }

    /// Sends events from a single buffer to its output.
    ///
    /// This is the core sending logic shared by all outputs:
    /// 1. Attach schema definitions to events (enables runtime type checking)
    /// 2. Send through fanout
    /// 3. Emit telemetry
    async fn send_single_buffer(
        buf: &mut OutputBuffer,
        output: &mut TransformOutput,
    ) -> Result<(), Box<dyn error::Error + Send + Sync>> {
        // Attach schema definitions to each event
        for event in buf.events_mut() {
            super::update_runtime_schema_definition(
                event,
                &output.output_id,
                &output.log_schema_definitions,
            );
        }

        // Track metrics
        let count = buf.len();
        let byte_size = buf.estimated_json_encoded_size_of();

        // Send to all subscribers through the fanout
        buf.send(&mut output.fanout).await?;

        // Emit telemetry
        output.events_sent.emit(CountByteSize(count, byte_size));
        Ok(())
    }
}

/// Buffer for collecting events across multiple transform outputs.
///
/// This is what `SyncTransform::transform()` receives. It holds the events
/// until the transform finishes processing a batch, then they're sent.
///
/// # Structure
///
/// ```ignore
/// TransformOutputsBuf {
///     primary_buffer: Some(OutputBuffer),     // For output.push(None, event)
///     named_buffers: {
///         "errors": OutputBuffer,              // For output.push(Some("errors"), event)
///         "info": OutputBuffer,                // For output.push(Some("info"), event)
///     }
/// }
/// ```
///
/// # Usage Pattern
///
/// ```ignore
/// // 1. Create buffer (usually done by the topology)
/// let mut output = outputs.new_buf_with_capacity(1024);
///
/// // 2. Transform pushes events to appropriate outputs
/// for event in events {
///     if is_error(&event) {
///         output.push(Some("errors"), event);
///     } else {
///         output.push(None, event);  // Default output
///     }
/// }
///
/// // 3. Topology sends the buffer
/// outputs.send(output).await;
/// ```
///
/// # Design Note: Why Two Buffer Types?
///
/// `FunctionTransform` uses `OutputBuffer` (single output) while `SyncTransform`
/// uses `TransformOutputsBuf` (multiple outputs). This separation:
/// 1. Avoids allocation overhead for single-output transforms (the common case)
/// 2. Makes the API explicit about what a transform can do
/// 3. Enables compile-time verification of output routing
#[derive(Debug, Clone)]
pub struct TransformOutputsBuf {
    /// Buffer for the default/primary output.
    ///
    /// `push(None, event)` writes here. `None` means "no named output, use default".
    /// Single-output transforms have only this buffer.
    pub(super) primary_buffer: Option<OutputBuffer>,

    /// Buffers for named outputs.
    ///
    /// `push(Some("name"), event)` writes to `named_buffers["name"]`.
    /// Only multi-output transforms like `route` have entries here.
    pub(super) named_buffers: HashMap<String, OutputBuffer>,
}

impl TransformOutputsBuf {
    /// Creates a new buffer with pre-allocated capacity.
    ///
    /// The `outputs_in` spec determines which buffers to create:
    /// - If there's an output with `port = None`, create `primary_buffer`
    /// - For each output with `port = Some(name)`, create an entry in `named_buffers`
    ///
    /// The `capacity` is a hint for pre-allocation to reduce reallocations.
    pub fn new_with_capacity(outputs_in: Vec<config::TransformOutput>, capacity: usize) -> Self {
        let mut primary_buffer = None;
        let mut named_buffers = HashMap::new();

        for output in outputs_in {
            match output.port {
                None => {
                    primary_buffer = Some(OutputBuffer::with_capacity(capacity));
                }
                Some(name) => {
                    // Named outputs don't pre-allocate since they're typically smaller
                    named_buffers.insert(name.clone(), OutputBuffer::default());
                }
            }
        }

        Self {
            primary_buffer,
            named_buffers,
        }
    }

    /// Adds a new event to the appropriate output buffer.
    ///
    /// - `name = None`: Push to the primary/default output
    /// - `name = Some("port")`: Push to the named output
    ///
    /// # Panics
    ///
    /// Panics if there is no output with the given name. This indicates a bug:
    /// the transform is trying to use an output it didn't declare in `outputs()`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Route errors to the "errors" output
    /// if event.as_log().get("level") == Some(&Value::from("error")) {
    ///     output.push(Some("errors"), event);
    /// } else {
    ///     // Everything else to default
    ///     output.push(None, event);
    /// }
    /// ```
    pub fn push(&mut self, name: Option<&str>, event: Event) {
        match name {
            Some(name) => self.named_buffers.get_mut(name),
            None => self.primary_buffer.as_mut(),
        }
        .expect("unknown output")
        .push(event);
    }

    /// Drains all events from the default output buffer.
    ///
    /// This is used in tests and when converting back to individual events.
    ///
    /// # Panics
    ///
    /// Panics if there is no default output (i.e., the transform only has named outputs).
    pub fn drain(&mut self) -> impl Iterator<Item = Event> + '_ {
        self.primary_buffer
            .as_mut()
            .expect("no default output")
            .drain()
    }

    /// Drains all events from a named output buffer.
    ///
    /// # Panics
    ///
    /// Panics if there is no output with the given name.
    pub fn drain_named(&mut self, name: &str) -> impl Iterator<Item = Event> + '_ {
        self.named_buffers
            .get_mut(name)
            .expect("unknown output")
            .drain()
    }

    /// Takes ownership of the primary buffer, replacing it with an empty one.
    ///
    /// This is more efficient than `drain()` when you need an owned `OutputBuffer`.
    ///
    /// # Panics
    ///
    /// Panics if there is no default output.
    pub fn take_primary(&mut self) -> OutputBuffer {
        std::mem::take(self.primary_buffer.as_mut().expect("no default output"))
    }

    /// Takes ownership of all named buffers, replacing them with empty ones.
    pub fn take_all_named(&mut self) -> HashMap<String, OutputBuffer> {
        std::mem::take(&mut self.named_buffers)
    }

    /// Applies `f` to each [`EventArray`] currently buffered in this outputs buffer.
    ///
    /// This is useful for cross-cutting instrumentation (e.g. latency timestamp propagation)
    /// that needs mutable access to the buffered arrays before they are sent.
    pub fn for_each_array_mut(&mut self, mut f: impl FnMut(&mut EventArray)) {
        if let Some(primary) = self.primary_buffer.as_mut() {
            primary.for_each_array_mut(&mut f);
        }
        for buf in self.named_buffers.values_mut() {
            buf.for_each_array_mut(&mut f);
        }
    }
}

impl ByteSizeOf for TransformOutputsBuf {
    fn allocated_bytes(&self) -> usize {
        self.primary_buffer.size_of()
            + self
                .named_buffers
                .values()
                .map(ByteSizeOf::size_of)
                .sum::<usize>()
    }
}

/// Buffer for collecting events of a single output.
///
/// This is the fundamental building block used by both `FunctionTransform` and `SyncTransform`.
/// It automatically coalesces events by type for efficient batch processing.
///
/// # Automatic Type Coalescing
///
/// Events are stored in `EventArray`s, which are type-homogeneous (all logs, all metrics, or all traces).
/// When you push events one at a time, this buffer automatically groups them:
///
/// ```ignore
/// let mut buf = OutputBuffer::default();
///
/// // Push in any order
/// buf.push(log_event_1);
/// buf.push(log_event_2);
/// buf.push(metric_event);
/// buf.push(log_event_3);
///
/// // Internal structure:
/// [
///   EventArray::Logs([log_event_1, log_event_2]),  // First two logs coalesced
///   EventArray::Metrics([metric_event]),            // Type changed, new array
///   EventArray::Logs([log_event_3]),                // Type changed again
/// ]
/// ```
///
/// This optimization:
/// - Avoids wrapping each event in its own array
/// - Maintains type separation for downstream efficiency
/// - Reduces allocations for common cases (same-type batches)
///
/// # When Coalescing Doesn't Happen
///
/// If the last array in the buffer is a different type, a new array is created.
/// This can happen with interleaved event types. The buffer doesn't scan backwards
/// to find a matching type - it only checks the most recent array.
#[derive(Debug, Default, Clone)]
pub struct OutputBuffer(pub(super) Vec<EventArray>);

impl OutputBuffer {
    /// Creates a new buffer with pre-allocated capacity.
    ///
    /// Use this when you know approximately how many events you'll process.
    /// The capacity is for `EventArray`s, not individual events.
    pub fn with_capacity(capacity: usize) -> Self {
        Self(Vec::with_capacity(capacity))
    }

    /// Adds an event to the buffer, coalescing by type when possible.
    ///
    /// This is the hot path for event processing. The implementation:
    /// 1. Checks if the last array matches the event type
    /// 2. If yes, appends to that array (no allocation)
    /// 3. If no, creates a new single-element array
    ///
    /// The match is done with pattern matching for efficiency:
    /// ```ignore
    /// match (event, last_array) {
    ///     (Event::Log(log), Some(EventArray::Logs(logs))) => logs.push(log),
    ///     (Event::Metric(m), Some(EventArray::Metrics(metrics))) => metrics.push(m),
    ///     (event, _) => self.0.push(event.into()),
    /// }
    /// ```
    pub fn push(&mut self, event: Event) {
        // Coalesce multiple pushes of the same type into one array.
        match (event, self.0.last_mut()) {
            (Event::Log(log), Some(EventArray::Logs(logs))) => {
                logs.push(log);
            }
            (Event::Metric(metric), Some(EventArray::Metrics(metrics))) => {
                metrics.push(metric);
            }
            (Event::Trace(trace), Some(EventArray::Traces(traces))) => {
                traces.push(trace);
            }
            (event, _) => {
                self.0.push(event.into());
            }
        }
    }

    /// Appends multiple events from a vector.
    ///
    /// More efficient than calling `push()` in a loop because it avoids
    /// repeatedly checking the last array type.
    pub fn append(&mut self, events: &mut Vec<Event>) {
        for event in events.drain(..) {
            self.push(event);
        }
    }

    /// Extends the buffer with events from an iterator.
    pub fn extend(&mut self, events: impl Iterator<Item = Event>) {
        for event in events {
            self.push(event);
        }
    }

    /// Returns true if the buffer contains no events.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the total number of events across all arrays.
    pub fn len(&self) -> usize {
        self.0.iter().map(EventArray::len).sum()
    }

    /// Returns the number of EventArray slots allocated.
    pub fn capacity(&self) -> usize {
        self.0.capacity()
    }

    /// Returns a reference to the first event, if any.
    pub fn first(&self) -> Option<EventRef<'_>> {
        self.0.first().and_then(|first| match first {
            EventArray::Logs(l) => l.first().map(Into::into),
            EventArray::Metrics(m) => m.first().map(Into::into),
            EventArray::Traces(t) => t.first().map(Into::into),
        })
    }

    /// Drains all events from the buffer.
    ///
    /// After calling this, the buffer is empty but retains its allocated capacity.
    pub fn drain(&mut self) -> impl Iterator<Item = Event> + '_ {
        self.0.drain(..).flat_map(EventArray::into_events)
    }

    /// Applies a function to each `EventArray` in the buffer.
    ///
    /// Used for cross-cutting concerns like latency tracking.
    pub fn for_each_array_mut(&mut self, mut f: impl FnMut(&mut EventArray)) {
        for array in &mut self.0 {
            f(array);
        }
    }

    /// Sends all events through a fanout channel.
    ///
    /// This is called by `TransformOutputs::send()`. The buffer is cleared
    /// as events are sent (via `std::mem::take`).
    ///
    /// The `send_start` timestamp is used for latency tracking in downstream components.
    async fn send(
        &mut self,
        output: &mut Fanout,
    ) -> Result<(), Box<dyn error::Error + Send + Sync>> {
        let send_start = Some(Instant::now());
        for array in std::mem::take(&mut self.0) {
            output.send(array, send_start).await?;
        }

        Ok(())
    }

    /// Iterates over events in the buffer (immutable).
    fn iter_events(&self) -> impl Iterator<Item = EventRef<'_>> {
        self.0.iter().flat_map(EventArray::iter_events)
    }

    /// Iterates over events in the buffer (mutable).
    ///
    /// Used to attach schema definitions before sending.
    fn events_mut(&mut self) -> impl Iterator<Item = EventMutRef<'_>> {
        self.0.iter_mut().flat_map(EventArray::iter_events_mut)
    }

    /// Consumes the buffer and returns all events as an iterator.
    pub fn into_events(self) -> impl Iterator<Item = Event> {
        self.0.into_iter().flat_map(EventArray::into_events)
    }
}

impl ByteSizeOf for OutputBuffer {
    fn allocated_bytes(&self) -> usize {
        self.0.iter().map(ByteSizeOf::size_of).sum()
    }
}

impl EventDataEq<Vec<Event>> for OutputBuffer {
    fn event_data_eq(&self, other: &Vec<Event>) -> bool {
        struct Comparator<'a>(EventRef<'a>);

        impl PartialEq<&Event> for Comparator<'_> {
            fn eq(&self, that: &&Event) -> bool {
                self.0.event_data_eq(that)
            }
        }

        self.iter_events().map(Comparator).eq(other.iter())
    }
}

impl EstimatedJsonEncodedSizeOf for OutputBuffer {
    fn estimated_json_encoded_size_of(&self) -> JsonSize {
        self.0
            .iter()
            .map(EstimatedJsonEncodedSizeOf::estimated_json_encoded_size_of)
            .sum()
    }
}

impl From<Vec<Event>> for OutputBuffer {
    fn from(events: Vec<Event>) -> Self {
        let mut result = Self::default();
        result.extend(events.into_iter());
        result
    }
}
