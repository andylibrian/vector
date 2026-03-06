//! SourceSender - The primary interface for sources to emit events.
//!
//! This module provides the `SourceSender` type, which is passed to every source
//! as part of `SourceContext`. Sources use it to send events into the pipeline.

#[cfg(any(test, feature = "test"))]
use std::time::Duration;
use std::{collections::HashMap, time::Instant};

use futures::Stream;
#[cfg(any(test, feature = "test"))]
use futures::StreamExt as _;
#[cfg(any(test, feature = "test"))]
use metrics::histogram;
use vector_buffers::EventCount;
#[cfg(any(test, feature = "test"))]
use vector_buffers::topology::channel::LimitedReceiver;
#[cfg(any(test, feature = "test"))]
use vector_common::internal_event::DEFAULT_OUTPUT;
#[cfg(doc)]
use vector_common::internal_event::{ComponentEventsDropped, EventsSent};
use vector_common::{
    byte_size_of::ByteSizeOf,
    finalization::{AddBatchNotifier, BatchNotifier},
    json_size::JsonSize,
};

use super::{Builder, Output, SendError};
#[cfg(any(test, feature = "test"))]
use super::{LAG_TIME_NAME, TEST_BUFFER_SIZE};
use crate::{
    EstimatedJsonEncodedSizeOf,
    event::{Event, EventArray, EventContainer, array::EventArrayIntoIter},
};
#[cfg(any(test, feature = "test"))]
use crate::{
    config::OutputId,
    event::{EventStatus, into_event_stream},
};

/// SourceSenderItem is a thin wrapper around [EventArray] used to track the send duration of a batch.
///
/// # Why This Wrapper Exists
///
/// You might wonder why we don't just send `EventArray` directly through the channel.
/// The answer is **lag time tracking**.
///
/// We want to measure how long it takes for events to flow from the source to the
/// transform/sink. This requires tracking when the batch was created. But `EventArray`
/// doesn't have a timestamp field - it's a pure data container.
///
/// Instead of modifying `EventArray`, we wrap it in `SourceSenderItem`:
/// - `events`: The actual batch of events
/// - `send_reference`: The instant when the batch was created
///
/// The receiver uses `send_reference` to calculate the lag time when the batch
/// is finally processed.
///
/// This is needed because the send duration is calculated as the difference between when the batch
/// is sent from the origin component to when the batch is enqueued on the receiving component's input buffer.
/// For sources in particular, this requires the batch to be enqueued on two channels: the origin component's pump
/// channel and then the receiving component's input buffer.
#[derive(Debug)]
pub struct SourceSenderItem {
    /// The batch of events to send.
    pub events: EventArray,
    /// Reference instant used to calculate send duration.
    pub send_reference: Instant,
}

impl AddBatchNotifier for SourceSenderItem {
    fn add_batch_notifier(&mut self, notifier: BatchNotifier) {
        self.events.add_batch_notifier(notifier);
    }
}

impl ByteSizeOf for SourceSenderItem {
    fn allocated_bytes(&self) -> usize {
        self.events.allocated_bytes()
    }
}

impl EventCount for SourceSenderItem {
    fn event_count(&self) -> usize {
        self.events.event_count()
    }
}

impl EstimatedJsonEncodedSizeOf for SourceSenderItem {
    fn estimated_json_encoded_size_of(&self) -> JsonSize {
        self.events.estimated_json_encoded_size_of()
    }
}

impl EventContainer for SourceSenderItem {
    type IntoIter = EventArrayIntoIter;

    fn len(&self) -> usize {
        self.events.len()
    }

    fn into_events(self) -> Self::IntoIter {
        self.events.into_events()
    }
}

impl From<SourceSenderItem> for EventArray {
    fn from(val: SourceSenderItem) -> Self {
        val.events
    }
}

/// The primary interface for sources to send events into the pipeline.
///
/// # How Sources Use SourceSender
///
/// Sources receive a `SourceSender` in their `SourceContext`. They use it to
/// send events via one of these methods:
///
/// ```ignore
/// // Send a single event
/// out.send_event(event).await?;
///
/// // Send a batch of events (more efficient for bulk operations)
/// out.send_batch(vec![event1, event2, event3]).await?;
///
/// // Send to a named output (for multi-output sources)
/// out.send_batch_named("errors", error_events).await?;
/// ```
///
/// # Internal Structure
///
/// SourceSender has two types of outputs:
///
/// - **default_output**: The primary output that most sources use. This is
///   the unnamed output that connects to downstream transforms/sinks.
///
/// - **named_outputs**: Additional outputs for sources that produce multiple
///   types of data (e.g., OpenTelemetry produces logs, metrics, and traces).
///
/// # Why Clone?
///
/// SourceSender is `Clone` because some sources spawn multiple concurrent tasks
/// that all need to send events. For example, an HTTP server source spawns a
/// task per connection, and each task needs its own `SourceSender` handle.
///
/// Cloning is cheap - it only clones the channel sender reference, not the
/// underlying buffer.
#[derive(Debug, Clone)]
pub struct SourceSender {
    // The default output is optional because some sources, e.g. `datadog_agent`
    // and `opentelemetry`, can be configured to only output to named outputs.
    pub(super) default_output: Option<Output>,
    pub(super) named_outputs: HashMap<String, Output>,
}

impl SourceSender {
    /// Creates a new builder for constructing a SourceSender.
    ///
    /// The builder pattern is used because SourceSender needs to be configured
    /// with its outputs before being used. The topology system uses this to:
    /// 1. Create a builder
    /// 2. Add outputs (channels) for each declared output
    /// 3. Build the final SourceSender
    pub fn builder() -> Builder {
        Builder::default()
    }

    #[cfg(any(test, feature = "test"))]
    pub fn new_test_sender_with_options(
        n: usize,
        timeout: Option<Duration>,
    ) -> (Self, LimitedReceiver<SourceSenderItem>) {
        let lag_time = Some(histogram!(LAG_TIME_NAME));
        let output_id = OutputId {
            component: "test".to_string().into(),
            port: None,
        };
        let (default_output, rx) = Output::new_with_buffer(
            n,
            DEFAULT_OUTPUT.to_owned(),
            lag_time,
            None,
            output_id,
            timeout,
            None,
        );
        (
            Self {
                default_output: Some(default_output),
                named_outputs: Default::default(),
            },
            rx,
        )
    }

    /// Creates a test sender and receiver pair.
    ///
    /// This is used extensively in source unit tests to verify events
    /// are produced correctly. The returned stream yields individual
    /// events, making it easy to assert on event content.
    #[cfg(any(test, feature = "test"))]
    pub fn new_test() -> (Self, impl Stream<Item = Event> + Unpin) {
        let (pipe, recv) = Self::new_test_sender_with_options(TEST_BUFFER_SIZE, None);
        let recv = recv.into_stream().flat_map(into_event_stream);
        (pipe, recv)
    }

    /// Creates a test sender that automatically finalizes events with the given status.
    ///
    /// This is useful for testing acknowledgement behavior. When testing sources
    /// that support acknowledgements, you need to simulate the sink confirming
    /// delivery. This method creates a receiver that automatically marks events
    /// as delivered (or errored) when they're received.
    #[cfg(any(test, feature = "test"))]
    pub fn new_test_finalize(status: EventStatus) -> (Self, impl Stream<Item = Event> + Unpin) {
        let (pipe, recv) = Self::new_test_sender_with_options(TEST_BUFFER_SIZE, None);
        // In a source test pipeline, there is no sink to acknowledge
        // events, so we have to add a map to the receiver to handle the
        // finalization.
        let recv = recv.into_stream().flat_map(move |mut item| {
            item.events.iter_events_mut().for_each(|mut event| {
                let metadata = event.metadata_mut();
                metadata.update_status(status);
                metadata.update_sources();
            });
            into_event_stream(item)
        });
        (pipe, recv)
    }

    #[cfg(any(test, feature = "test"))]
    pub fn new_test_errors(
        error_at: impl Fn(usize) -> bool,
    ) -> (Self, impl Stream<Item = Event> + Unpin) {
        let (pipe, recv) = Self::new_test_sender_with_options(TEST_BUFFER_SIZE, None);
        // In a source test pipeline, there is no sink to acknowledge
        // events, so we have to add a map to the receiver to handle the
        // finalization.
        let mut count: usize = 0;
        let recv = recv.into_stream().flat_map(move |mut item| {
            let status = if error_at(count) {
                EventStatus::Errored
            } else {
                EventStatus::Delivered
            };
            count += 1;
            item.events.iter_events_mut().for_each(|mut event| {
                let metadata = event.metadata_mut();
                metadata.update_status(status);
                metadata.update_sources();
            });
            into_event_stream(item)
        });
        (pipe, recv)
    }

    #[cfg(any(test, feature = "test"))]
    pub fn add_outputs(
        &mut self,
        status: EventStatus,
        name: String,
    ) -> impl Stream<Item = SourceSenderItem> + Unpin + use<> {
        // The lag_time parameter here will need to be filled in if this function is ever used for
        // non-test situations.
        let output_id = OutputId {
            component: "test".to_string().into(),
            port: Some(name.clone()),
        };
        let (output, recv) =
            Output::new_with_buffer(100, name.clone(), None, None, output_id, None, None);
        let recv = recv.into_stream().map(move |mut item| {
            item.events.iter_events_mut().for_each(|mut event| {
                let metadata = event.metadata_mut();
                metadata.update_status(status);
                metadata.update_sources();
            });
            item
        });
        self.named_outputs.insert(name, output);
        recv
    }

    /// Get a mutable reference to the default output, panicking if none exists.
    const fn default_output_mut(&mut self) -> &mut Output {
        self.default_output.as_mut().expect("no default output")
    }

    /// Send a single event to the default output.
    ///
    /// # When To Use This vs send_batch()
    ///
    /// Use `send_event()` when:
    /// - Events arrive one at a time (e.g., reading from a socket)
    /// - The source doesn't naturally batch data
    ///
    /// Use `send_batch()` when:
    /// - You have multiple events ready to send
    /// - You're processing data in chunks (e.g., reading from a file)
    ///
    /// `send_event()` forwards exactly what you provide (as an `EventArray`)
    /// to the default output.
    ///
    /// This internally handles emitting [EventsSent] and [ComponentEventsDropped] events.
    pub async fn send_event(&mut self, event: impl Into<EventArray>) -> Result<(), SendError> {
        self.default_output_mut().send_event(event).await
    }

    /// Send a stream of events to the default output.
    ///
    /// This is useful when you have an async stream producing events (e.g.,
    /// from a codec decoder). The method automatically chunks the stream
    /// into batches of `CHUNK_SIZE` for efficiency.
    ///
    /// This internally handles emitting [EventsSent] and [ComponentEventsDropped] events.
    pub async fn send_event_stream<S, E>(&mut self, events: S) -> Result<(), SendError>
    where
        S: Stream<Item = E> + Unpin,
        E: Into<Event> + ByteSizeOf,
    {
        self.default_output_mut().send_event_stream(events).await
    }

    /// Send a batch of events to the default output.
    ///
    /// # Why Batching Matters
    ///
    /// `send_batch()` chunks large iterators into `CHUNK_SIZE` event arrays
    /// before sending to the channel. This is typically more efficient than
    /// many single-event sends because:
    /// - You know the exact size upfront, avoiding reallocations
    /// - Events are kept together in the same chunk
    /// - Fewer method calls
    ///
    /// This internally handles emitting [EventsSent] and [ComponentEventsDropped] events.
    pub async fn send_batch<I, E>(&mut self, events: I) -> Result<(), SendError>
    where
        E: Into<Event> + ByteSizeOf,
        I: IntoIterator<Item = E>,
        <I as IntoIterator>::IntoIter: ExactSizeIterator,
    {
        self.default_output_mut().send_batch(events).await
    }

    /// Send a batch of events to a named output.
    ///
    /// # Named Outputs
    ///
    /// Some sources produce multiple types of data. For example, the OpenTelemetry
    /// source can output logs, metrics, AND traces. Each type goes to a separate
    /// named output, which can be routed to different transforms/sinks.
    ///
    /// Named outputs are declared in `SourceConfig::outputs()`:
    /// ```ignore
    /// vec![
    ///     SourceOutput::new_maybe_logs(DataType::Log, schema),
    ///     SourceOutput::new_maybe_metrics(DataType::Metric, schema),
    /// ]
    /// ```
    ///
    /// This internally handles emitting [EventsSent] and [ComponentEventsDropped] events.
    pub async fn send_batch_named<I, E>(&mut self, name: &str, events: I) -> Result<(), SendError>
    where
        E: Into<Event> + ByteSizeOf,
        I: IntoIterator<Item = E>,
        <I as IntoIterator>::IntoIter: ExactSizeIterator,
    {
        self.named_outputs
            .get_mut(name)
            .expect("unknown output")
            .send_batch(events)
            .await
    }
}
