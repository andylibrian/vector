//! Runtime for the `blackhole` sink.
//!
//! The sink drops all events, while still updating acknowledgements and metrics.
//! Optional rate limiting and periodic counter logging are supported.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use tokio::{
    select,
    sync::watch,
    time::{interval, sleep_until},
};
use vector_lib::{
    EstimatedJsonEncodedSizeOf,
    internal_event::{
        ByteSize, BytesSent, CountByteSize, EventsSent, InternalEventHandle as _, Output, Protocol,
    },
};

use crate::{
    event::{EventArray, EventContainer, EventStatus, Finalizable},
    sinks::{blackhole::config::BlackholeConfig, util::StreamSink},
};

/// Runtime state for the `blackhole` sink.
pub struct BlackholeSink {
    /// Total number of events received.
    total_events: Arc<AtomicUsize>,

    /// Total estimated bytes received.
    total_raw_bytes: Arc<AtomicUsize>,

    /// Sink configuration.
    config: BlackholeConfig,

    /// Timestamp used for rate limiting.
    last: Option<Instant>,
}

impl BlackholeSink {
    pub fn new(config: BlackholeConfig) -> Self {
        BlackholeSink {
            config,
            total_events: Arc::new(AtomicUsize::new(0)),
            total_raw_bytes: Arc::new(AtomicUsize::new(0)),
            last: None,
        }
    }
}

/// Drops incoming `EventArray` batches while tracking metrics and finalizers.
#[async_trait]
impl StreamSink<EventArray> for BlackholeSink {
    async fn run(mut self: Box<Self>, mut input: BoxStream<'_, EventArray>) -> Result<(), ()> {
        // Reporting runs in a background task and reads these shared counters.
        let total_events = Arc::clone(&self.total_events);
        let total_raw_bytes = Arc::clone(&self.total_raw_bytes);
        let (shutdown, mut tripwire) = watch::channel(());
        let events_sent = register!(EventsSent::from(Output(None)));
        let bytes_sent = register!(BytesSent::from(Protocol("blackhole".into())));

        if self.config.print_interval_secs.as_secs() > 0 {
            let interval_dur = self.config.print_interval_secs;
            tokio::spawn(async move {
                let mut print_interval = interval(interval_dur);
                loop {
                    select! {
                        _ = print_interval.tick() => {
                            info!(
                                events = total_events.load(Ordering::Relaxed),
                                raw_bytes_collected = total_raw_bytes.load(Ordering::Relaxed),
                                internal_log_rate_limit = false,
                                "Collected events."
                            );
                        },
                        _ = tripwire.changed() => break,
                    }
                }

                info!(
                    events = total_events.load(Ordering::Relaxed),
                    raw_bytes_collected = total_raw_bytes.load(Ordering::Relaxed),
                    internal_log_rate_limit = false,
                    "Collected events."
                );
            });
        }

        while let Some(mut events) = input.next().await {
            if let Some(rate) = self.config.rate {
                let factor: f32 = 1.0 / rate as f32;
                let secs: f32 = factor * (events.len() as f32);
                let until = self.last.unwrap_or_else(Instant::now) + Duration::from_secs_f32(secs);
                sleep_until(until.into()).await;
                self.last = Some(until);
            }

            let message_len = events.estimated_json_encoded_size_of();

            _ = self.total_events.fetch_add(events.len(), Ordering::AcqRel);
            _ = self
                .total_raw_bytes
                .fetch_add(message_len.get(), Ordering::AcqRel);

            let finalizers = events.take_finalizers();
            finalizers.update_status(EventStatus::Delivered);

            events_sent.emit(CountByteSize(events.len(), message_len));
            bytes_sent.emit(ByteSize(message_len.get()));
        }

        _ = shutdown.send(());

        Ok(())
    }
}
