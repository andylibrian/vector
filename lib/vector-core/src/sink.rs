//! Core sink runtime abstractions shared by Vector sinks.
//!
//! Vector supports two sink execution models:
//! - `VectorSink::Sink`: a `futures::Sink<EventArray>` pipeline.
//! - `VectorSink::Stream`: a custom `StreamSink<EventArray>` loop.
//!
//! The adapters in this module bridge per-event sinks (`Event`) to the
//! topology-facing batch interface (`EventArray`).

use std::{fmt, iter::IntoIterator, pin::Pin};

use futures::{
    Sink, SinkExt, Stream, StreamExt, stream,
    task::{Context, Poll},
};

use crate::event::{Event, EventArray, EventContainer, into_event_stream};

/// Unified sink wrapper used by the topology.
pub enum VectorSink {
    /// `futures::Sink`-based sink that consumes `EventArray` batches.
    Sink(Box<dyn Sink<EventArray, Error = ()> + Send + Unpin>),

    /// Stream-based sink with a custom `run` loop.
    Stream(Box<dyn StreamSink<EventArray> + Send>),
}

impl VectorSink {
    /// Run the sink against an input stream of event batches.
    ///
    /// # Errors
    ///
    /// Returns `Err(())` when the sink reports an unrecoverable runtime failure.
    pub async fn run(self, input: impl Stream<Item = EventArray> + Send) -> Result<(), ()> {
        match self {
            Self::Sink(sink) => input.map(Ok).forward(sink).await,
            Self::Stream(s) => s.run(Box::pin(input)).await,
        }
    }

    /// Run the `VectorSink` with a one-time `Vec` of `Event`s, for use in tests
    ///
    /// # Errors
    ///
    /// See `VectorSink::run` for errors.
    pub async fn run_events<I>(self, input: I) -> Result<(), ()>
    where
        I: IntoIterator<Item = Event> + Send,
        I::IntoIter: Send,
    {
        self.run(stream::iter(input).map(Into::into)).await
    }

    /// Converts `VectorSink` into a `futures::Sink`
    ///
    /// # Panics
    ///
    /// This function will panic if the self instance is not `VectorSink::Sink`.
    pub fn into_sink(self) -> Box<dyn Sink<EventArray, Error = ()> + Send + Unpin> {
        match self {
            Self::Sink(sink) => sink,
            _ => panic!("Failed type coercion, {self:?} is not a Sink"),
        }
    }

    /// Converts `VectorSink` into a `StreamSink`
    ///
    /// # Panics
    ///
    /// This function will panic if the self instance is not `VectorSink::Stream`.
    pub fn into_stream(self) -> Box<dyn StreamSink<EventArray> + Send> {
        match self {
            Self::Stream(stream) => stream,
            _ => panic!("Failed type coercion, {self:?} is not a Stream"),
        }
    }

    /// Converts an event sink into a `VectorSink`
    ///
    /// Deprecated in favor of `VectorSink::from_event_streamsink`. See [vector/9261]
    /// for more info.
    ///
    /// [vector/9261]: https://github.com/vectordotdev/vector/issues/9261
    #[deprecated]
    pub fn from_event_sink(sink: impl Sink<Event, Error = ()> + Send + Unpin + 'static) -> Self {
        VectorSink::Sink(Box::new(EventSink::new(sink)))
    }

    /// Converts an event stream into a `VectorSink`
    pub fn from_event_streamsink(sink: impl StreamSink<Event> + Send + 'static) -> Self {
        let sink = Box::new(sink);
        VectorSink::Stream(Box::new(EventStream { sink }))
    }
}

impl fmt::Debug for VectorSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VectorSink").finish()
    }
}

// === StreamSink ===

/// Trait for stream-based sinks.
#[async_trait::async_trait]
pub trait StreamSink<T> {
    /// Process the stream until completion or failure.
    async fn run(self: Box<Self>, input: stream::BoxStream<'_, T>) -> Result<(), ()>;
}

/// Adapter from `Sink<Event>` to `Sink<EventArray>`.
struct EventSink<S> {
    sink: S,

    /// Iterator of events from the in-flight batch.
    queue: Option<<EventArray as EventContainer>::IntoIter>,
}

macro_rules! poll_ready_ok {
    ( $e:expr ) => {
        match $e {
            r @ (Poll::Pending | Poll::Ready(Err(_))) => return r,
            Poll::Ready(Ok(ok)) => ok,
        }
    };
}

impl<S: Sink<Event> + Send + Unpin> EventSink<S> {
    fn new(sink: S) -> Self {
        Self { sink, queue: None }
    }

    fn next_event(&mut self) -> Option<Event> {
        match &mut self.queue {
            #[allow(clippy::single_match_else)] // No, clippy, this isn't a single pattern
            Some(queue) => match queue.next() {
                Some(event) => Some(event),
                None => {
                    // Reset the queue to empty after the last event
                    self.queue = None;
                    None
                }
            },
            None => None,
        }
    }

    fn flush_queue(self: &mut Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        while self.queue.is_some() {
            poll_ready_ok!(self.sink.poll_ready_unpin(cx));
            let Some(event) = self.next_event() else {
                break;
            };
            if let Err(err) = self.sink.start_send_unpin(event) {
                return Poll::Ready(Err(err));
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl<S: Sink<Event> + Send + Unpin> Sink<EventArray> for EventSink<S> {
    type Error = S::Error;
    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        poll_ready_ok!(self.flush_queue(cx));
        self.sink.poll_ready_unpin(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, events: EventArray) -> Result<(), Self::Error> {
        assert!(self.queue.is_none()); // Should be guaranteed by `poll_ready`
        self.queue = Some(events.into_events());
        self.next_event()
            .map_or(Ok(()), |event| self.sink.start_send_unpin(event))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        poll_ready_ok!(self.flush_queue(cx));
        self.sink.poll_flush_unpin(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        poll_ready_ok!(self.flush_queue(cx));
        self.sink.poll_close_unpin(cx)
    }
}

/// Adapter from `StreamSink<Event>` to `StreamSink<EventArray>`.
struct EventStream<T> {
    sink: Box<T>,
}

#[async_trait::async_trait]
impl<T: StreamSink<Event> + Send> StreamSink<EventArray> for EventStream<T> {
    async fn run(self: Box<Self>, input: stream::BoxStream<'_, EventArray>) -> Result<(), ()> {
        let input = Box::pin(input.flat_map(into_event_stream));
        self.sink.run(input).await
    }
}
