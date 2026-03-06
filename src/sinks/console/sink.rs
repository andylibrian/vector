//! Runtime implementation for the `console` sink.
//!
//! Events are transformed, encoded, and written to either `stdout` or `stderr`.

use async_trait::async_trait;
use bytes::BytesMut;
use futures::{StreamExt, stream::BoxStream};
use tokio::{io, io::AsyncWriteExt};
use tokio_util::codec::Encoder as _;
use vector_lib::{
    EstimatedJsonEncodedSizeOf,
    codecs::encoding::Framer,
    internal_event::{
        ByteSize, BytesSent, CountByteSize, EventsSent, InternalEventHandle as _, Output, Protocol,
    },
};

use crate::{
    codecs::{Encoder, Transformer},
    event::{Event, EventStatus, Finalizable},
    sinks::util::StreamSink,
};

/// Console sink runtime.
pub struct WriterSink<T> {
    /// Output writer (`stdout` or `stderr`).
    pub output: T,

    /// Event transformer configured by sink encoding settings.
    pub transformer: Transformer,

    /// Event encoder (serializer + framer).
    pub encoder: Encoder<Framer>,
}

/// Writes each event to the configured output stream.
#[async_trait]
impl<T> StreamSink<Event> for WriterSink<T>
where
    T: io::AsyncWrite + Send + Sync + Unpin,
{
    async fn run(mut self: Box<Self>, mut input: BoxStream<'_, Event>) -> Result<(), ()> {
        let bytes_sent = register!(BytesSent::from(Protocol("console".into(),)));
        let events_sent = register!(EventsSent::from(Output(None)));

        while let Some(mut event) = input.next().await {
            let event_byte_size = event.estimated_json_encoded_size_of();
            self.transformer.transform(&mut event);

            let finalizers = event.take_finalizers();
            let mut bytes = BytesMut::new();
            self.encoder.encode(event, &mut bytes).map_err(|_| {
                // `Encoder` already emits the underlying internal event.
                finalizers.update_status(EventStatus::Errored);
            })?;

            match self.output.write_all(&bytes).await {
                Err(error) => {
                    error!(message = "Error writing to output. Stopping sink.", %error, internal_log_rate_limit = false);
                    finalizers.update_status(EventStatus::Errored);
                    return Err(());
                }
                Ok(()) => {
                    finalizers.update_status(EventStatus::Delivered);
                    events_sent.emit(CountByteSize(1, event_byte_size));
                    bytes_sent.emit(ByteSize(bytes.len()));
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use futures::future::ready;
    use futures_util::stream;
    use vector_lib::{
        codecs::{JsonSerializerConfig, NewlineDelimitedEncoder},
        sink::VectorSink,
    };

    use super::*;
    use crate::{
        event::{Event, LogEvent},
        test_util::components::{SINK_TAGS, run_and_assert_sink_compliance},
    };

    #[tokio::test]
    async fn component_spec_compliance() {
        let event = Event::Log(LogEvent::from("foo"));

        let encoder = Encoder::<Framer>::new(
            NewlineDelimitedEncoder::default().into(),
            JsonSerializerConfig::default().build().into(),
        );

        let sink = WriterSink {
            output: Vec::new(),
            transformer: Default::default(),
            encoder,
        };

        run_and_assert_sink_compliance(
            VectorSink::from_event_streamsink(sink),
            stream::once(ready(event)),
            &SINK_TAGS,
        )
        .await;
    }
}
