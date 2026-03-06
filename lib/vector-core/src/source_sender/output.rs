//! Output - A single output channel from a source.
//!
//! Each source can have one or more outputs. Each output is represented by
//! an `Output` instance, which wraps a channel sender and handles:
//!
//! - Batching events into chunks
//! - Tracking send timing for lag metrics
//! - Attaching schema definitions to events
//! - Handling send timeouts and errors
//!
//! # The Relationship Between SourceSender and Output
//!
//! ```
//! SourceSender
//!   |- default_output: Output  (unnamed output, used by most sources)
//!   `- named_outputs: HashMap<String, Output>  (for multi-output sources)
//! ```
//!
//! When a source calls `out.send_batch(events)`, it's delegating to the
//! default `Output`. When it calls `out.send_batch_named("traces", events)`,
//! it's using a named `Output`.

use std::{
    fmt,
    num::NonZeroUsize,
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::Utc;
use futures::{Stream, StreamExt as _};
use metrics::Histogram;
use tracing::Span;
use vector_buffers::{
    config::MemoryBufferSize,
    topology::channel::{self, ChannelMetricMetadata, LimitedReceiver, LimitedSender},
};
use vector_common::{
    byte_size_of::ByteSizeOf,
    internal_event::{
        self, ComponentEventsDropped, ComponentEventsTimedOut, Count, CountByteSize, EventsSent,
        InternalEventHandle as _, RegisterInternalEvent as _, Registered, UNINTENTIONAL,
    },
};
use vrl::value::Value;

use super::{CHUNK_SIZE, SendError, SourceSenderItem};
use crate::{
    EstimatedJsonEncodedSizeOf,
    config::{OutputId, log_schema},
    event::{Event, EventArray, EventContainer as _, EventRef, array},
    schema::Definition,
};

/// Metric prefix for source buffer utilization.
const UTILIZATION_METRIC_PREFIX: &str = "source_buffer";

/// Tracks events that haven't been sent yet.
///
/// # Why This Struct Exists
///
/// When a source calls `send_batch()`, it's possible for the future to be
/// cancelled before completion. This can happen in HTTP servers when a client
/// sends a new request on a connection that has a pending request.
///
/// If we don't track unsent events, they would be silently dropped without
/// any telemetry. This struct ensures that dropped events are properly
/// counted and logged.
///
/// # How It Works
///
/// 1. When sending starts, we create `UnsentEventCount::new(count)`
/// 2. As events are successfully sent, we call `decr(count)`
/// 3. If the future is dropped before completion, `Drop::drop` emits
///    a `ComponentEventsDropped` event with the remaining count
///
/// This pattern ensures we never lose track of events, even when futures
/// are cancelled unexpectedly.
pub(super) struct UnsentEventCount {
    count: usize,
    span: Span,
}

impl UnsentEventCount {
    fn new(count: usize) -> Self {
        Self {
            count,
            span: Span::current(),
        }
    }

    /// Decrease the count when events are successfully sent.
    const fn decr(&mut self, count: usize) {
        self.count = self.count.saturating_sub(count);
    }

    /// Discard tracking - used when we're about to emit a different error.
    const fn discard(&mut self) {
        self.count = 0;
    }

    /// Called when send times out - emits the appropriate telemetry.
    fn timed_out(&mut self) {
        ComponentEventsTimedOut {
            reason: "Source send timed out.",
        }
        .register()
        .emit(Count(self.count));
        self.count = 0;
    }
}

impl Drop for UnsentEventCount {
    fn drop(&mut self) {
        // If we still have unsent events, emit a telemetry event
        if self.count > 0 {
            let _enter = self.span.enter();
            internal_event::emit(ComponentEventsDropped::<UNINTENTIONAL> {
                count: self.count,
                reason: "Source send cancelled.",
            });
        }
    }
}

/// A single output channel from a source.
///
/// # Anatomy of an Output
///
/// An `Output` contains:
///
/// - **sender**: The channel to send events through. This is a `LimitedSender`,
///   which means it has backpressure - when the buffer is full, sends block.
///
/// - **lag_time**: Histogram metric tracking how long events wait before being
///   processed. This measures "source lag" - if a source generates events faster
///   than downstream can process, lag increases.
///
/// - **events_sent**: Telemetry handle for counting events that successfully
///   flow through this output.
///
/// - **log_definition**: Schema definition for log events. This is attached
///   to each event as it passes through, enabling schema-aware processing.
///
/// - **id**: The output identifier, used for acknowledgements and routing.
///
/// - **timeout**: Optional timeout for send operations. If the channel is
///   blocked for longer than this, the send fails with a timeout error.
#[derive(Clone)]
pub(super) struct Output {
    sender: LimitedSender<SourceSenderItem>,
    lag_time: Option<Histogram>,
    events_sent: Registered<EventsSent>,
    /// The schema definition that will be attached to Log events sent through here
    log_definition: Option<Arc<Definition>>,
    /// The OutputId related to this source sender. This is set as the `upstream_id` in
    /// `EventMetadata` for all event sent through here.
    id: Arc<OutputId>,
    timeout: Option<Duration>,
}

#[expect(clippy::missing_fields_in_debug)]
impl fmt::Debug for Output {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Output")
            .field("sender", &self.sender)
            .field("output_id", &self.id)
            .field("timeout", &self.timeout)
            // `metrics::Histogram` is missing `impl Debug`
            .finish()
    }
}

impl Output {
    /// Creates a new output with its associated buffer channel.
    ///
    /// # Parameters
    ///
    /// - `n`: Maximum number of events the buffer can hold
    /// - `output`: Name of this output (for metrics)
    /// - `lag_time`: Optional histogram for tracking event lag
    /// - `log_definition`: Schema definition for events
    /// - `output_id`: Identifier for this output
    /// - `timeout`: Optional send timeout
    ///
    /// # Returns
    ///
    /// Returns a tuple of:
    /// 1. The `Output` instance (used by the source to send events)
    /// 2. A `LimitedReceiver` (used by the topology to receive events)
    ///
    /// The sender and receiver are connected by a bounded channel with
    /// backpressure. When the buffer is full, sends will block (or timeout).
    pub(super) fn new_with_buffer(
        n: usize,
        output: String,
        lag_time: Option<Histogram>,
        log_definition: Option<Arc<Definition>>,
        output_id: OutputId,
        timeout: Option<Duration>,
        ewma_half_life_seconds: Option<f64>,
    ) -> (Self, LimitedReceiver<SourceSenderItem>) {
        let limit = MemoryBufferSize::MaxEvents(NonZeroUsize::new(n).unwrap());
        let metrics = ChannelMetricMetadata::new(UTILIZATION_METRIC_PREFIX, Some(output.clone()));
        let (tx, rx) = channel::limited(limit, Some(metrics), ewma_half_life_seconds);
        (
            Self {
                sender: tx,
                lag_time,
                events_sent: internal_event::register(EventsSent::from(internal_event::Output(
                    Some(output.into()),
                ))),
                log_definition,
                id: Arc::new(output_id),
                timeout,
            },
            rx,
        )
    }

    /// Core send implementation that handles batching and metadata.
    ///
    /// # What This Does
    ///
    /// 1. Records the send start time for lag tracking
    /// 2. Attaches lag time metrics to each event
    /// 3. Attaches schema definitions to events
    /// 4. Attaches the output ID to events (for acknowledgements)
    /// 5. Sends through the channel (with optional timeout)
    /// 6. Emits telemetry for successful sends
    ///
    /// # Error Handling
    ///
    /// - If the channel is closed, returns `SendError::Closed`
    /// - If timeout is configured and send takes too long, returns `SendError::Timeout`
    pub(super) async fn send(
        &mut self,
        mut events: EventArray,
        unsent_event_count: &mut UnsentEventCount,
    ) -> Result<(), SendError> {
        let send_reference = Instant::now();
        let reference = Utc::now().timestamp_millis();
        events
            .iter_events()
            .for_each(|event| self.emit_lag_time(event, reference));

        events.iter_events_mut().for_each(|mut event| {
            // Attach runtime schema definitions from the source
            // This enables schema-aware processing downstream
            if let Some(log_definition) = &self.log_definition {
                event.metadata_mut().set_schema_definition(log_definition);
            }
            // Set the upstream ID for acknowledgement tracking
            event.metadata_mut().set_upstream_id(Arc::clone(&self.id));
        });

        let byte_size = events.estimated_json_encoded_size_of();
        let count = events.len();
        self.send_with_timeout(events, send_reference).await?;
        self.events_sent.emit(CountByteSize(count, byte_size));
        unsent_event_count.decr(count);
        Ok(())
    }

    /// Sends events through the channel with optional timeout.
    ///
    /// # Timeout Behavior
    ///
    /// If `timeout` is set:
    /// - Wraps the send in `tokio::time::timeout`
    /// - Returns `SendError::Timeout` if the timeout elapses
    /// - This allows sources to fail fast when downstream is too slow
    ///
    /// If `timeout` is not set:
    /// - Send will block indefinitely until buffer has space
    /// - This is the default behavior (backpressure propagates to source)
    async fn send_with_timeout(
        &mut self,
        events: EventArray,
        send_reference: Instant,
    ) -> Result<(), SendError> {
        let item = SourceSenderItem {
            events,
            send_reference,
        };
        if let Some(timeout) = self.timeout {
            match tokio::time::timeout(timeout, self.sender.send(item)).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(error.into()),
                Err(_elapsed) => Err(SendError::Timeout),
            }
        } else {
            self.sender.send(item).await.map_err(Into::into)
        }
    }

    /// Send a single event (or small batch).
    ///
    /// This is the primary method used by sources. The event is wrapped
    /// in an `EventArray` and sent through the channel.
    pub(super) async fn send_event(
        &mut self,
        event: impl Into<EventArray>,
    ) -> Result<(), SendError> {
        let event: EventArray = event.into();
        // Track unsent events in case the future is cancelled
        let mut unsent_event_count = UnsentEventCount::new(event.len());
        self.send(event, &mut unsent_event_count)
            .await
            .inspect_err(|error| {
                if let SendError::Timeout = error {
                    unsent_event_count.timed_out();
                }
            })
    }

    /// Send a stream of events, automatically chunking into batches.
    ///
    /// This is useful when you have an async stream of events (e.g., from
    /// a codec decoder). The method uses `ready_chunks()` to group events
    /// into batches of `CHUNK_SIZE` for efficiency.
    pub(super) async fn send_event_stream<S, E>(&mut self, events: S) -> Result<(), SendError>
    where
        S: Stream<Item = E> + Unpin,
        E: Into<Event> + ByteSizeOf,
    {
        let mut stream = events.ready_chunks(CHUNK_SIZE);
        while let Some(events) = stream.next().await {
            self.send_batch(events.into_iter()).await?;
        }
        Ok(())
    }

    /// Send a batch of events.
    ///
    /// # Why This Is Efficient
    ///
    /// When you have multiple events ready, sending them as a batch is more
    /// efficient than individual sends:
    /// - Fewer channel operations
    /// - Better cache locality
    /// - Events stay together through the pipeline
    ///
    /// # Chunking
    ///
    /// If the batch is larger than `CHUNK_SIZE`, it's automatically split
    /// into multiple chunks. This prevents any single send from blocking
    /// for too long.
    pub(super) async fn send_batch<I, E>(&mut self, events: I) -> Result<(), SendError>
    where
        E: Into<Event> + ByteSizeOf,
        I: IntoIterator<Item = E>,
        <I as IntoIterator>::IntoIter: ExactSizeIterator,
    {
        let events = events.into_iter().map(Into::into);
        let mut unsent_event_count = UnsentEventCount::new(events.len());
        for events in array::events_into_arrays(events, Some(CHUNK_SIZE)) {
            self.send(events, &mut unsent_event_count)
                .await
                .inspect_err(|error| match error {
                    SendError::Timeout => {
                        unsent_event_count.timed_out();
                    }
                    SendError::Closed => {
                        // The unsent event count is discarded here because the callee emits the
                        // `StreamClosedError`.
                        unsent_event_count.discard();
                    }
                })?;
        }
        Ok(())
    }

    /// Calculate and emit the lag time for an event.
    ///
    /// # What Lag Time Measures
    ///
    /// Lag time is the difference between:
    /// 1. When the event was created (timestamp in the event)
    /// 2. When the event was sent through the output
    ///
    /// This metric helps identify:
    /// - Sources generating events faster than downstream can process
    /// - Backpressure building up in the pipeline
    /// - Processing bottlenecks
    ///
    /// The value is emitted as a histogram metric, which can be used
    /// to see the distribution of lag times (p50, p95, p99).
    pub(super) fn emit_lag_time(&self, event: EventRef<'_>, reference: i64) {
        if let Some(lag_time_metric) = &self.lag_time {
            let timestamp = match event {
                EventRef::Log(log) => {
                    log_schema()
                        .timestamp_key_target_path()
                        .and_then(|timestamp_key| {
                            log.get(timestamp_key).and_then(get_timestamp_millis)
                        })
                }
                EventRef::Metric(metric) => metric
                    .timestamp()
                    .map(|timestamp| timestamp.timestamp_millis()),
                EventRef::Trace(trace) => {
                    log_schema()
                        .timestamp_key_target_path()
                        .and_then(|timestamp_key| {
                            trace.get(timestamp_key).and_then(get_timestamp_millis)
                        })
                }
            };
            if let Some(timestamp) = timestamp {
                // This will truncate precision for values larger than 2**52, but at that point the user
                // probably has much larger problems than precision.
                #[expect(clippy::cast_precision_loss)]
                let lag_time = (reference - timestamp) as f64 / 1000.0;
                lag_time_metric.record(lag_time);
            }
        }
    }
}

const fn get_timestamp_millis(value: &Value) -> Option<i64> {
    match value {
        Value::Timestamp(timestamp) => Some(timestamp.timestamp_millis()),
        _ => None,
    }
}
