# Vector Sinks — Developer Onboarding Guide

This document explains how sinks work in Vector: how they receive events from the pipeline and deliver them to external destinations.

It answers:

- What traits a sink must implement and how `build()` works
- The two sink execution models (Tower-based Sink vs StreamSink)
- How the encoding pipeline works (Transformer → Encoder → Framer)
- How healthchecks validate configuration at startup
- How the acknowledgement flow reports delivery status
- A walkthrough of the `console` sink as a concrete example

For unfamiliar terms, see the [Glossary](./glossary.md).

## What Problem Do Sinks Solve? (Conceptual Background)

### The reliable delivery challenge

Getting data into a pipeline is the easy part. Getting it *out* reliably is where the hard problems live. Each destination has different requirements:

- **Elasticsearch** wants bulk HTTP requests with specific JSON formatting, and rejects malformed documents.
- **Kafka** wants records sent to specific partitions, with acknowledgement from brokers.
- **S3** wants objects uploaded in batches, with multipart uploads for large files.
- **Console (stdout)** just wants bytes written to a file descriptor.

Without a sink abstraction, the pipeline core would need to know the delivery protocol, retry semantics, batching rules, and error handling for every destination. Sinks encapsulate all of this behind a uniform interface: **receive events, serialize them, deliver them, and report whether delivery succeeded.**

### StreamSink vs Tower Sink: two execution models

Sinks fall into two categories based on how they interact with destinations:

**StreamSink** — The sink owns a simple processing loop. It reads events from a stream, serializes them, writes them to the destination, and reports delivery status. This works well for simple, single-connection destinations:

```
Console sink (StreamSink):
  while let Some(events) = input.next().await {
      for event in events {
          serialize(event) ──▸ write to stdout
      }
      finalizers.update_status(Delivered)     ← report success
  }
```

**Tower Sink** — The sink uses Tower's `Service` trait for request/response-oriented destinations. Tower middleware handles batching, retries, concurrency, and rate limiting automatically:

```
Elasticsearch sink (Tower):
  Events arrive ──▸ [Batch layer: accumulate 1000 events]
                         │
                    ──▸ [Retry layer: retry on 429/5xx with exponential backoff]
                         │
                    ──▸ [Concurrency layer: up to 10 in-flight requests]
                         │
                    ──▸ [Rate limit layer: max 100 requests/sec]
                         │
                    ──▸ HTTP POST to /_bulk endpoint
```

Tower sinks are more complex to implement but get retry logic, concurrency control, and rate limiting for free. If your destination is an HTTP API that accepts batch requests, Tower is almost always the right choice.

### The encoding pipeline: events to bytes

A sink must convert in-memory `Event` values to bytes in the format the destination expects. This happens in three stages:

```
Event: { "message": "disk full", "host": "web-1", "password": "secret123" }
  │
  ▼
Transformer: apply encoding.except_fields = ["password"]
  → { "message": "disk full", "host": "web-1" }
  │
  ▼
Serializer (JSON): convert to JSON bytes
  → b'{"message":"disk full","host":"web-1"}'
  │
  ▼
Framer (newline-delimited): append \n
  → b'{"message":"disk full","host":"web-1"}\n'
```

The three stages are independent and composable. You can combine any transformer options, any serializer (JSON, Protobuf, CSV, ...), and any framer (newline, length-prefixed, none). This means a single `console` sink can output JSON, plain text, or logfmt — just by changing the `encoding.codec` config field.

### Acknowledgements: closing the delivery loop

In a best-effort pipeline, a source reads data, sends it downstream, and moves on. If the sink fails to deliver, the data is lost. For critical data, this is unacceptable.

End-to-end acknowledgements solve this by tracking every event from source to sink:

```
1. Kafka source reads message offset 42
   │
   └──▸ creates EventFinalizer, attaches to event
        (finalizer holds a reference back to the Kafka consumer)

2. Event flows through transforms
   │
   └──▸ finalizers travel with the event (preserved through Arc sharing)

3. Elasticsearch sink receives event, sends bulk request
   │
   ├── HTTP 200 OK
   │   └──▸ finalizers.update_status(Delivered)
   │        └──▸ Kafka source: commit offset 42 ✓
   │
   └── HTTP 503 Service Unavailable
       └──▸ finalizers.update_status(Errored)
            └──▸ Kafka source: do NOT commit offset 42
                 (message will be redelivered on restart)
```

This gives at-least-once delivery: a Kafka offset is only committed after the downstream sink confirms successful delivery. If Vector crashes between reading and delivering, the uncommitted messages are redelivered on restart.

When events fan out to multiple sinks, all copies must be delivered before the finalizer reports success. This prevents partial delivery — you don't commit a Kafka offset if the event was delivered to Elasticsearch but failed for S3.

### Healthchecks: fail fast on misconfiguration

A common failure mode is deploying a config with a wrong endpoint URL, expired credentials, or a non-existent Kafka topic. Without healthchecks, Vector would start successfully, accept data, and then silently fail to deliver — potentially buffering or dropping events for hours before someone notices.

Each sink provides a healthcheck that runs at startup:

```
Elasticsearch healthcheck: HTTP GET /_cluster/health → 200 OK? ✓ healthy
Kafka healthcheck:         connect to broker, verify topic exists → ✓ healthy
S3 healthcheck:            HeadBucket on the configured bucket → ✓ healthy
Console healthcheck:       always passes (stdout always works) → ✓ healthy
```

With `--require-healthy`, Vector refuses to start if any healthcheck fails, surfacing misconfiguration immediately rather than losing data at runtime.

## Table of Contents

- [SinkConfig Trait](#sinkconfig-trait)
- [SinkContext](#sinkcontext)
- [VectorSink](#vectorsink)
- [StreamSink](#streamsink)
- [Tower-Based Sink](#tower-based-sink)
- [Encoding Pipeline](#encoding-pipeline)
- [Healthchecks](#healthchecks)
- [Acknowledgement Flow](#acknowledgement-flow)
- [Walkthrough: console Sink](#walkthrough-console-sink)
- [Adding a New Sink](#adding-a-new-sink)
- [Key Files](#key-files)

---

## SinkConfig Trait

Every sink must implement `SinkConfig`, defined at [`src/config/sink.rs:238`](../src/config/sink.rs#L238):

```rust
#[async_trait]
#[typetag::serde(tag = "type")]
pub trait SinkConfig: DynClone + NamedComponent + Debug + Send + Sync {
    /// Build the sink runtime and a healthcheck from this configuration.
    async fn build(&self, cx: SinkContext) -> crate::Result<(VectorSink, Healthcheck)>;

    /// Declare what event types this sink accepts.
    fn input(&self) -> Input;

    /// Declare system resources this sink needs (e.g., network port).
    fn resources(&self) -> Vec<Resource> { Vec::new() }

    /// Acknowledgement configuration for this sink.
    fn acknowledgements(&self) -> &AcknowledgementsConfig;
}
```

Key points:

- **`build()`** returns a tuple of `(VectorSink, Healthcheck)`. The sink processes events; the healthcheck validates connectivity.
- **`input()`** declares accepted event types — used for topology validation.
- **`acknowledgements()`** returns the acknowledgement configuration. When enabled, the sink must report delivery status via finalizers.
- **`#[typetag::serde(name = "my_sink")]`** registers the sink for dynamic deserialization.

---

## SinkContext

Provided to `build()`, defined at [`src/config/sink.rs:276`](../src/config/sink.rs#L276):

```rust
pub struct SinkContext {
    pub healthcheck: SinkHealthcheckOptions,
    pub globals: GlobalOptions,
    pub proxy: ProxyConfig,
    pub schema: schema::Options,
    pub app_name: String,
    pub app_name_slug: String,
    // ...
}
```

| Field | Purpose |
|-------|---------|
| `healthcheck` | Options for the health check (enabled, URI override) |
| `globals` | Global config options |
| `proxy` | HTTP proxy configuration |
| `schema` | Schema options for event handling |

---

## VectorSink

The runtime sink wrapper, defined at [`lib/vector-core/src/sink.rs:10`](../lib/vector-core/src/sink.rs#L10):

```rust
pub enum VectorSink {
    Sink(Box<dyn Sink<EventArray, Error = ()> + Send + Unpin>),
    Stream(Box<dyn StreamSink<EventArray> + Send>),
}
```

Two execution models:

- **`Sink`** — Tower service-based. Implements the `futures::Sink` trait. Used for request/response-oriented sinks where batching, retries, and concurrency are managed by Tower middleware.
- **`Stream`** — Stream-based. Implements `StreamSink`. The sink receives a stream of events and runs a custom processing loop. This is the simpler and more common pattern.

---

## StreamSink

The stream-based execution model, defined at [`lib/vector-core/src/sink.rs:92`](../lib/vector-core/src/sink.rs#L92):

```rust
#[async_trait]
pub trait StreamSink<T> {
    async fn run(self: Box<Self>, input: BoxStream<'_, T>) -> Result<(), ()>;
}
```

The sink receives the full event stream and processes it in a loop:

```rust
#[async_trait]
impl StreamSink<EventArray> for MySink {
    async fn run(mut self: Box<Self>, mut input: BoxStream<'_, EventArray>) -> Result<(), ()> {
        while let Some(events) = input.next().await {
            // Process events...
            let finalizers = events.take_finalizers();

            match self.send(events).await {
                Ok(()) => finalizers.update_status(EventStatus::Delivered),
                Err(e) => finalizers.update_status(EventStatus::Errored),
            }
        }
        Ok(())
    }
}
```

The `run()` method returns when the input stream ends (during shutdown).

---

## Tower-Based Sink

For sinks that benefit from batching, retries, and concurrency control, Vector provides Tower service-based infrastructure:

- **Batching** — Events are accumulated into batches by count or size before sending.
- **Retries** — Failed requests are retried with configurable backoff.
- **Concurrency** — Multiple requests can be in-flight simultaneously.
- **Rate limiting** — Requests per second can be capped.

This is used by HTTP-based sinks (Elasticsearch, Datadog, Loki, etc.) where each batch becomes an HTTP request.

The Tower sink wraps a `Service` that processes batches:

```rust
pub trait Service<Request> {
    type Response;
    type Error;
    type Future: Future<Output = Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>>;
    fn call(&mut self, req: Request) -> Self::Future;
}
```

---

## Encoding Pipeline

Sinks serialize events to bytes before sending. The encoding pipeline has three stages:

```
Event → Transformer → Serializer → Framer → Bytes
```

1. **Transformer** — Pre-encoding mutations (field filtering, timestamp formatting). Configured via `encoding.only_fields`, `encoding.except_fields`, `encoding.timestamp_format`.

2. **Serializer** — Converts an event to bytes in a specific format (JSON, Protobuf, CSV, Text, etc.). See [Codecs](./onboarding-codecs.md).

3. **Framer** — Wraps serialized bytes with framing (newline delimiter, length prefix, etc.).

Sinks configure this via `EncodingConfigWithFraming`:

```rust
#[configurable_component(sink("my_sink", "..."))]
pub struct MySinkConfig {
    #[serde(flatten)]
    pub encoding: EncodingConfigWithFraming,
    // ...
}
```

At build time:

```rust
let transformer = self.encoding.transformer();
let (framer, serializer) = self.encoding.build(SinkType::StreamBased)?;
let encoder = Encoder::<Framer>::new(framer, serializer);
```

During event processing:

```rust
let mut bytes = BytesMut::new();
transformer.transform(&mut event);
encoder.encode(event, &mut bytes)?;
// Send bytes to destination
```

---

## Healthchecks

Every sink returns a healthcheck from `build()`:

```rust
type Healthcheck = BoxFuture<'static, crate::Result<()>>;
```

The healthcheck is an async function that validates the sink can connect to its destination. Examples:

- HTTP sinks: send a HEAD request to the endpoint.
- Kafka sinks: verify broker connectivity.
- File sinks: verify the output path is writable.
- Console sink: always succeeds (returns `future::ok(())`).

Healthchecks run at startup. If `--require-healthy` is set, Vector fails to start if any healthcheck fails.

---

## Acknowledgement Flow

End-to-end delivery tracking through event finalizers:

1. **Source** creates `EventFinalizer` and attaches to event metadata.
2. **Transforms** preserve finalizers (they travel with the event through `Arc` sharing).
3. **Sink** extracts finalizers before sending: `let finalizers = events.take_finalizers();`
4. **Sink** reports outcome:
   - `finalizers.update_status(EventStatus::Delivered)` — success.
   - `finalizers.update_status(EventStatus::Errored)` — transient failure.
   - `finalizers.update_status(EventStatus::Rejected)` — permanent failure.
5. **Source** receives the status and acts accordingly (e.g., Kafka commits offset).

When events fan out to multiple sinks, all copies must be delivered before the source receives confirmation.

Acknowledgements are enabled per-sink:

```yaml
sinks:
  my_sink:
    type: console
    acknowledgements:
      enabled: true
```

---

## Walkthrough: console Sink

The `console` sink at [`src/sinks/console/config.rs:44`](../src/sinks/console/config.rs#L44) is the simplest sink — it writes events to stdout or stderr.

**Configuration:**

```rust
#[configurable_component(sink("console", "Display events in the console."))]
pub struct ConsoleSinkConfig {
    #[serde(default = "default_target")]
    pub target: Target,                         // Stdout or Stderr

    #[serde(flatten)]
    pub encoding: EncodingConfigWithFraming,    // How to serialize events

    #[configurable(derived)]
    pub acknowledgements: AcknowledgementsConfig,
}
```

**Build:**

```rust
impl SinkConfig for ConsoleSinkConfig {
    async fn build(&self, _cx: SinkContext) -> crate::Result<(VectorSink, Healthcheck)> {
        let transformer = self.encoding.transformer();
        let (framer, serializer) = self.encoding.build(SinkType::StreamBased)?;
        let encoder = Encoder::<Framer>::new(framer, serializer);

        let sink = match self.target {
            Target::Stdout => VectorSink::from_event_streamsink(
                WriterSink::new(io::stdout(), transformer, encoder)
            ),
            Target::Stderr => VectorSink::from_event_streamsink(
                WriterSink::new(io::stderr(), transformer, encoder)
            ),
        };

        Ok((sink, future::ok(()).boxed()))  // Healthcheck always passes
    }
}
```

**WriterSink runtime** at [`src/sinks/console/sink.rs:20`](../src/sinks/console/sink.rs#L20):

```rust
pub struct WriterSink<T> {
    pub output: T,
    pub transformer: Transformer,
    pub encoder: Encoder<Framer>,
}
```

For each event:
1. Estimate the event byte size (for telemetry).
2. Apply the transformer (field filtering, etc.).
3. Extract finalizers from the event.
4. Encode the event to bytes.
5. Write bytes to the output (stdout/stderr).
6. Mark finalizers as `Delivered`.
7. Emit `EventsSent` and `BytesSent` telemetry.

---

## Adding a New Sink

Checklist for adding a new sink:

1. **Create the module** under `src/sinks/my_sink/` (typically `config.rs`, `sink.rs`, `mod.rs`).
2. **Define the config struct** with `#[configurable_component(sink("my_sink", "Description."))]`.
3. **Implement `SinkConfig`** with `#[typetag::serde(name = "my_sink")]`.
4. **Implement `StreamSink`** (for simple sinks) or use Tower service infrastructure (for HTTP/batch sinks).
5. **Add a feature flag** in `Cargo.toml` (e.g., `sinks-my_sink = []`).
6. **Register the module** in `src/sinks/mod.rs` behind the feature flag.
7. **Implement a healthcheck** that validates connectivity.
8. **Handle acknowledgements** — call `finalizers.update_status()` on every event.
9. **Add internal events** in `src/internal_events/` for telemetry.
10. **Write tests** — unit tests and integration tests.
11. **Run `make check-component-docs`** to verify documentation generation.

---

## Key Files

| File | Content |
|------|---------|
| [`src/config/sink.rs`](../src/config/sink.rs#L238) | `SinkConfig` trait, `SinkContext` |
| [`lib/vector-core/src/sink.rs`](../lib/vector-core/src/sink.rs#L10) | `VectorSink` enum, `StreamSink` trait |
| [`src/sinks/mod.rs`](../src/sinks/mod.rs) | Feature-gated sink module registration |
| [`src/sinks/console/`](../src/sinks/console/) | Example: simple StreamSink |
| [`src/sinks/blackhole/`](../src/sinks/blackhole/) | Example: minimal sink (discards events) |
| [`src/sinks/http/`](../src/sinks/http/) | Example: HTTP-based Tower sink |
| [`src/sinks/elasticsearch/`](../src/sinks/elasticsearch/) | Example: complex Tower sink with batching |
| [`docs/specs/component.md`](../docs/specs/component.md) | Component specification |

---

## Related Docs

- System overview: [`onboarding-system-overview.md`](./onboarding-system-overview.md)
- Event model (what sinks consume): [`onboarding-event-model.md`](./onboarding-event-model.md)
- Transforms (upstream of sinks): [`onboarding-transforms.md`](./onboarding-transforms.md)
- Codecs (encoding in sinks): [`onboarding-codecs.md`](./onboarding-codecs.md)
- Buffers (preceding sinks): [`onboarding-buffers-backpressure.md`](./onboarding-buffers-backpressure.md)
