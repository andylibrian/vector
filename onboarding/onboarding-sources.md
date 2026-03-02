# Vector Sources — Developer Onboarding Guide

This document explains how sources work in Vector: how they ingest data from external systems, convert it to events, and push it into the pipeline.

It answers:

- What traits a source must implement and how `build()` works
- What `SourceContext` provides (shutdown signals, output channels, enrichment tables)
- How `SourceSender` batches and delivers events
- Common source patterns (polling, listening, tailing)
- How decoders integrate with sources
- How internal events provide telemetry
- A walkthrough of the `demo_logs` source as a concrete example

For unfamiliar terms, see the [Glossary](./glossary.md).

## Table of Contents

- [SourceConfig Trait](#sourceconfig-trait)
- [SourceContext](#sourcecontext)
- [SourceSender](#sourcesender)
- [Common Source Patterns](#common-source-patterns)
- [Decoder Integration](#decoder-integration)
- [Internal Events and Telemetry](#internal-events-and-telemetry)
- [Walkthrough: demo_logs Source](#walkthrough-demo_logs-source)
- [Adding a New Source](#adding-a-new-source)
- [Key Files](#key-files)

---

## SourceConfig Trait

Every source must implement `SourceConfig`, defined at [`src/config/source.rs:86`](../src/config/source.rs#L86):

```rust
#[async_trait]
#[typetag::serde(tag = "type")]
pub trait SourceConfig: DynClone + NamedComponent + Debug + Send + Sync {
    /// Build the source runtime from this configuration.
    async fn build(&self, cx: SourceContext) -> crate::Result<Source>;

    /// Declare the outputs this source produces and their schemas.
    fn outputs(&self, global_log_namespace: LogNamespace) -> Vec<SourceOutput>;

    /// Declare system resources this source needs (e.g., TCP port bindings).
    fn resources(&self) -> Vec<Resource> { Vec::new() }

    /// Whether this source supports end-to-end acknowledgements.
    fn can_acknowledge(&self) -> bool;

    /// Optional send timeout for SourceSender.
    fn send_timeout(&self) -> Option<Duration> { None }
}
```

Key points:

- **`build()`** receives a `SourceContext` and returns a `Source`, which is a `BoxFuture<'static, Result<(), ()>>` — an async task that runs until the source shuts down.
- **`outputs()`** declares what event types this source produces and their schemas, used for topology validation and schema propagation.
- **`can_acknowledge()`** indicates whether the source uses end-to-end acknowledgements (e.g., Kafka commits offsets only after confirmed delivery).
- **`#[typetag::serde(name = "my_source")]`** registers the source for dynamic deserialization from config files.

The configuration struct itself uses `#[configurable_component(source("name", "description"))]` for schema generation and documentation.

---

## SourceContext

Provided to `build()`, defined at [`src/config/source.rs:134`](../src/config/source.rs#L134):

```rust
pub struct SourceContext {
    pub key: ComponentKey,
    pub globals: GlobalOptions,
    pub shutdown: ShutdownSignal,
    pub out: SourceSender,
    pub proxy: ProxyConfig,
    pub acknowledgements: bool,
    pub schema_definitions: HashMap<Option<String>, schema::Definition>,
    pub schema: schema::Options,
    pub extra_context: ExtraContext,
    // ...
}
```

| Field | Purpose |
|-------|---------|
| `key` | Unique identifier for this source instance |
| `shutdown` | Signal to listen for graceful shutdown (select on this in your main loop) |
| `out` | `SourceSender` channel to emit events into the pipeline |
| `acknowledgements` | Whether acknowledgements are enabled for this source |
| `schema_definitions` | Output schema definitions for each named output |
| `proxy` | Proxy configuration for HTTP-based sources |

The source's main loop typically looks like:

```rust
async fn run(mut shutdown: ShutdownSignal, mut out: SourceSender) -> Result<(), ()> {
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            data = get_next_data() => {
                let events = parse_to_events(data);
                out.send_batch(events).await?;
            }
        }
    }
    Ok(())
}
```

---

## SourceSender

The channel interface for emitting events, defined at [`lib/vector-core/src/source_sender/sender.rs:93`](../lib/vector-core/src/source_sender/sender.rs#L93).

Key behaviors:

- **Batching:** Events are buffered and sent as `EventArray` chunks of `CHUNK_SIZE` (1000 events, defined at [`lib/vector-core/src/source_sender/mod.rs:20`](../lib/vector-core/src/source_sender/mod.rs#L20)). This reduces per-event overhead.
- **Backpressure:** `send_batch()` is async and will block when downstream buffers are full.
- **Multiple outputs:** Sources with multiple named outputs can use `SourceSender::add_outputs()` to create separate channels for each.
- **Send timeout:** Configurable via `SourceConfig::send_timeout()`, returns an error if the channel is blocked for too long.

Common send methods:

```rust
// Send a batch of events
out.send_batch(vec![event1, event2, event3]).await?;

// Send a single event
out.send_event(event).await?;

// Send to a named output (for sources with multiple outputs)
out.send_batch_named("errors", error_events).await?;
```

---

## Common Source Patterns

### Polling Sources

Sources that periodically fetch data (e.g., `internal_metrics`, `host_metrics`):

```rust
loop {
    tokio::select! {
        _ = &mut shutdown => break,
        _ = interval.tick() => {
            let events = collect_data();
            out.send_batch(events).await?;
        }
    }
}
```

### Listening Sources

Sources that accept incoming connections (e.g., `http_server`, `socket`):

```rust
let listener = TcpListener::bind(addr).await?;
loop {
    tokio::select! {
        _ = &mut shutdown => break,
        conn = listener.accept() => {
            let out = out.clone();
            tokio::spawn(handle_connection(conn, out));
        }
    }
}
```

### Tailing Sources

Sources that watch files for new data (e.g., `file`):

Uses the `file-source` library crate at [`lib/file-source/`](../lib/file-source/) which handles:
- File discovery via glob patterns
- Checkpointing (resume position after restart)
- File rotation detection
- Fingerprinting for file identity

---

## Decoder Integration

Many sources use codecs to parse raw bytes into events. The decoding pipeline:

1. **Framing** — Split a byte stream into individual messages (newline-delimited, length-prefixed, etc.).
2. **Decoding** — Parse each message into one or more `Event` values (JSON, Syslog, GELF, etc.).

Sources configure this via `DecodingConfig`:

```rust
#[configurable_component(source("my_source", "..."))]
pub struct MySourceConfig {
    #[configurable(derived)]
    #[serde(default)]
    decoding: DecodingConfig,

    #[configurable(derived)]
    #[serde(default)]
    framing: FramingConfig,
}
```

At build time, the decoder is constructed:

```rust
let decoder = DecodingConfig::new(framing, decoding, log_namespace).build()?;
```

The decoder is then used in the source's main loop to convert raw bytes into events.

See [Codecs](./onboarding-codecs.md) for details on available formats.

---

## Internal Events and Telemetry

Sources emit telemetry via the internal events system in [`src/internal_events/`](../src/internal_events/). The common pattern:

```rust
use vector_lib::internal_event::{
    BytesReceived, CountByteSize, EventsReceived, Protocol,
};

// Register event handles at source startup
let bytes_received = register!(BytesReceived::from(Protocol::HTTP));
let events_received = register!(EventsReceived);

// Emit events during processing
bytes_received.emit(ByteSize(raw_bytes.len()));
events_received.emit(CountByteSize(event_count, estimated_size));
```

Standard source telemetry:

| Event | When to Emit |
|-------|-------------|
| `BytesReceived` | Raw bytes received from the external system |
| `EventsReceived` | Events created from received data |
| `StreamClosedError` | Downstream channel closed unexpectedly |
| `ComponentEventsDropped` | Events intentionally dropped (e.g., decode error) |

See [`docs/specs/instrumentation.md`](../docs/specs/instrumentation.md) for naming conventions.

---

## Walkthrough: demo_logs Source

The `demo_logs` source at [`src/sources/demo_logs.rs:40`](../src/sources/demo_logs.rs#L40) is a good example of a simple polling source.

**Configuration:**

```rust
#[configurable_component(source("demo_logs", "Generate fake log events."))]
pub struct DemoLogsConfig {
    #[serde(default = "default_count")]
    pub count: usize,                // Number of log lines to generate

    #[serde(default = "default_interval")]
    pub interval: f64,               // Seconds between batches

    #[configurable(derived)]
    pub format: OutputFormat,        // Apache, Syslog, JSON, etc.

    #[configurable(derived)]
    pub decoding: DecodingConfig,    // How to decode generated lines

    pub log_namespace: Option<bool>, // Legacy or Vector namespace
}
```

**Build:**

```rust
impl SourceConfig for DemoLogsConfig {
    async fn build(&self, cx: SourceContext) -> crate::Result<Source> {
        let decoder = /* build decoder from config */;
        let log_namespace = /* resolve namespace */;

        Ok(demo_logs_source(
            self.interval,
            self.count,
            self.format,
            decoder,
            cx.shutdown,
            cx.out,
            log_namespace,
        ).boxed())
    }
}
```

**Runtime:**

The `demo_logs_source` function:

1. Registers `BytesReceived` and `EventsReceived` internal event handles.
2. Loops, generating log lines according to the configured format.
3. Decodes lines into events using the configured decoder.
4. Enriches events with source metadata (`source_type`, `service`, `host`) using `LogNamespace`.
5. Sends event batches via `SourceSender`.
6. Exits when `shutdown` signal fires or `count` is reached.

---

## Adding a New Source

Checklist for adding a new source:

1. **Create the module** under `src/sources/my_source.rs` (or `src/sources/my_source/` for complex sources).
2. **Define the config struct** with `#[configurable_component(source("my_source", "Description."))]`.
3. **Implement `SourceConfig`** with `#[typetag::serde(name = "my_source")]`.
4. **Add a feature flag** in `Cargo.toml` (e.g., `sources-my_source = []`).
5. **Register the module** in `src/sources/mod.rs` behind the feature flag.
6. **Add internal events** in `src/internal_events/` for telemetry.
7. **Write tests** — unit tests in the source file, integration tests if external services are involved.
8. **Run `make check-component-docs`** to verify documentation generation.

---

## Key Files

| File | Content |
|------|---------|
| [`src/config/source.rs`](../src/config/source.rs#L86) | `SourceConfig` trait, `SourceContext` |
| [`lib/vector-core/src/source_sender/sender.rs`](../lib/vector-core/src/source_sender/sender.rs#L93) | `SourceSender` implementation |
| [`lib/vector-core/src/source_sender/mod.rs`](../lib/vector-core/src/source_sender/mod.rs#L20) | `CHUNK_SIZE` constant |
| [`src/sources/mod.rs`](../src/sources/mod.rs) | Feature-gated source module registration |
| [`src/sources/demo_logs.rs`](../src/sources/demo_logs.rs#L40) | Example: polling source |
| [`src/sources/http_server.rs`](../src/sources/http_server.rs) | Example: listening source |
| [`src/sources/file.rs`](../src/sources/file.rs) | Example: tailing source |
| [`src/internal_events/`](../src/internal_events/) | Internal event definitions |
| [`docs/specs/component.md`](../docs/specs/component.md) | Component specification |

---

## Related Docs

- System overview: [`onboarding-system-overview.md`](./onboarding-system-overview.md)
- Event model (what sources produce): [`onboarding-event-model.md`](./onboarding-event-model.md)
- Codecs (decoding in sources): [`onboarding-codecs.md`](./onboarding-codecs.md)
- Buffers (downstream of sources): [`onboarding-buffers-backpressure.md`](./onboarding-buffers-backpressure.md)
