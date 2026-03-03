# Glossary

This glossary defines terms used across the onboarding docs that may not be familiar to all readers. Each entry is kept brief; follow the links for full context.

---

<a id="acknowledgement"></a>
**Acknowledgement** — A delivery-tracking mechanism where events carry `EventFinalizers` that report whether the event was successfully delivered (`Delivered`), failed (`Errored`), or rejected (`Rejected`). Sources that support acknowledgements (e.g., Kafka) use this feedback to commit offsets only after confirmed delivery. Configured per-sink via `acknowledgements.enabled`. See [Sinks: Acknowledgement Flow](./onboarding-sinks.md#acknowledgement-flow).

<a id="buffer"></a>
**Buffer** — An intermediate queue between components that absorbs load spikes and propagates backpressure. Vector supports two buffer types: in-memory (bounded by event count) and disk-based (bounded by byte size). Each sink has its own buffer configured via the `buffer` key. See [Buffers & Backpressure](./onboarding-buffers-backpressure.md).

<a id="chunk-size"></a>
**CHUNK_SIZE** — The batch size constant (1000 events) used by `SourceSender` when forwarding events downstream. Events are accumulated into arrays of this size before being sent through channels, reducing per-event overhead. Defined at [`lib/vector-core/src/source_sender/mod.rs:20`](../lib/vector-core/src/source_sender/mod.rs#L20).

<a id="codec"></a>
**Codec** — A paired encoder/decoder that serializes events to bytes (for sinks) or deserializes bytes to events (for sources). Vector ships codecs for JSON, Protobuf, Syslog, GELF, Avro, CSV, logfmt, and more. Each codec has a framing layer (newline-delimited, length-prefixed, etc.) and a format layer. See [Codecs](./onboarding-codecs.md).

<a id="component-key"></a>
**ComponentKey** — A unique string identifier for a component instance within a topology (e.g., `"my_source"`, `"filter_errors"`). Used as the key in config maps and for routing events between components. Derived from the user's config file keys.

<a id="condition"></a>
**Condition** — A boolean predicate evaluated against events, used by transforms like `filter` and `route`. Can be a VRL expression or a simpler check type. Defined via the `AnyCondition` enum in the config system.

<a id="config-diff"></a>
**ConfigDiff** — A struct that represents the difference between two configurations during a hot reload. Contains sets of added, removed, and changed component keys for sources, transforms, sinks, and enrichment tables. Used by the topology to determine which components to start, stop, or rewire. Defined at [`src/config/diff.rs:9`](../src/config/diff.rs#L9). See [Topology: Config Reload](./onboarding-topology.md#config-reload).

<a id="configurable-component"></a>
**`#[configurable_component]`** — A procedural macro from `vector-config` that generates JSON Schema metadata, documentation, and validation for component configuration structs. Applied to source, transform, and sink config types. Enables automatic documentation generation and config validation.

<a id="enrichment-table"></a>
**Enrichment table** — A data lookup table (CSV file, GeoIP database, etc.) loaded at startup and available to transforms for enriching events. Managed via `TableRegistry` and accessed through `TransformContext`. Defined in [`lib/enrichment/`](../lib/enrichment/).

<a id="event"></a>
**Event** — The fundamental data unit flowing through Vector's pipeline. An enum with three variants: `Log` (structured key-value data), `Metric` (counters, gauges, histograms, etc.), and `Trace` (distributed tracing spans). Defined at [`lib/vector-core/src/event/mod.rs:50`](../lib/vector-core/src/event/mod.rs#L50). See [Event Model](./onboarding-event-model.md).

<a id="event-array"></a>
**EventArray** — A batch container for events, with variants `Logs(Vec<LogEvent>)`, `Metrics(Vec<Metric>)`, and `Traces(Vec<TraceEvent>)`. Used for efficient batch processing through the pipeline rather than sending individual events. Defined at [`lib/vector-core/src/event/array.rs:134`](../lib/vector-core/src/event/array.rs#L134).

<a id="event-metadata"></a>
**EventMetadata** — Per-event metadata including the data type definition, secrets, source ID, upstream ID, and event finalizers. Not part of the user-visible event data but tracked internally for routing, acknowledgement, and schema enforcement. Defined at [`lib/vector-core/src/event/metadata.rs:28`](../lib/vector-core/src/event/metadata.rs#L28).

<a id="fanout"></a>
**Fanout** — A routing primitive that distributes events from one output to multiple downstream inputs. Each source output and transform output has a `Fanout` that manages connected consumers. Supports dynamic addition/removal of consumers via control channels during topology reload. Defined at [`lib/vector-core/src/fanout.rs:45`](../lib/vector-core/src/fanout.rs#L45). See [Topology: Fanout and Control Channels](./onboarding-topology.md#fanout-and-control-channels).

<a id="feature-flag"></a>
**Feature flag** — A Cargo feature that gates component compilation. Every source, transform, and sink must be behind a feature flag (e.g., `sources-file`, `sinks-console`, `transforms-remap`). This allows building Vector with only the components you need, reducing binary size and compile time. Feature definitions are in the root `Cargo.toml`.

<a id="finalizer"></a>
**Finalizer** — Part of the acknowledgement system. `EventFinalizer` instances are attached to events and report delivery status back to the source. When a sink successfully writes an event, it calls `finalizers.update_status(EventStatus::Delivered)`. See [Sinks: Acknowledgement Flow](./onboarding-sinks.md#acknowledgement-flow).

<a id="function-transform"></a>
**FunctionTransform** — The simplest transform trait. Processes one event at a time via `transform(&mut self, output: &mut OutputBuffer, event: Event)`. Suitable for stateless, per-event operations like filtering or field manipulation. Defined at [`lib/vector-core/src/transform/mod.rs:97`](../lib/vector-core/src/transform/mod.rs#L97). See [Transforms](./onboarding-transforms.md#functiontransform).

<a id="healthcheck"></a>
**Healthcheck** — An async function that validates a sink's configuration at startup (e.g., by pinging the destination endpoint). Returns `Ok(())` on success or an error. Can be required via `--require-healthy` CLI flag. Part of the `SinkConfig::build()` return value.

<a id="input"></a>
**Input** — A type constraint describing what event types a component accepts. Used by transforms and sinks to declare compatibility (e.g., logs only, metrics only, or all types). Checked during topology validation.

<a id="internal-event"></a>
**Internal event** — A telemetry event emitted by Vector components for observability. Uses the `register!()` and `emit!()` macros to track bytes received/sent, events processed/dropped, errors, and other operational metrics. Defined in [`src/internal_events/`](../src/internal_events/). See [Sources: Internal Events](./onboarding-sources.md#internal-events-and-telemetry).

<a id="log-event"></a>
**LogEvent** — A structured log entry stored as a map of field paths to values. Uses `Arc<Inner>` with copy-on-write semantics for efficient cloning when events fan out to multiple sinks. Defined at [`lib/vector-core/src/event/log_event.rs:155`](../lib/vector-core/src/event/log_event.rs#L155). See [Event Model: LogEvent](./onboarding-event-model.md#logevent).

<a id="log-namespace"></a>
**LogNamespace** — Controls where source metadata is placed within log events. `Legacy` mode puts metadata directly in the event root (Vector v1 behavior). `Vector` mode puts metadata under a separate namespace to avoid collisions with user data. See [Event Model: LogNamespace](./onboarding-event-model.md#log-namespace).

<a id="metric"></a>
**Metric** — An event variant for numerical measurements. Contains a name, namespace, tags, timestamp, and a value (Counter, Gauge, Set, Distribution, Histogram, Summary, or Sketch). Defined at [`lib/vector-core/src/event/metric/mod.rs:58`](../lib/vector-core/src/event/metric/mod.rs#L58). See [Event Model: Metric](./onboarding-event-model.md#metric).

<a id="output-id"></a>
**OutputId** — A compound identifier for a specific output of a component, combining the `ComponentKey` and an optional output name (for components with multiple named outputs, like the `route` transform). Used to wire inputs to outputs in the topology graph.

<a id="running-topology"></a>
**RunningTopology** — The live, executing pipeline state. Holds task handles for all running components, buffer senders for inputs, control channels for fanout management, and the shutdown coordinator. Supports config reload by computing diffs and rewiring components. Defined at [`src/topology/running.rs:56`](../src/topology/running.rs#L56). See [Topology](./onboarding-topology.md#runningtopology).

<a id="schema-definition"></a>
**SchemaDefinition** — A type-level description of an event's expected shape (field names, types, semantic meanings). Propagated through the topology from sources through transforms. Used for validation, documentation, and VRL type checking.

<a id="sink"></a>
**Sink** — A component that sends events to an external destination (Elasticsearch, S3, Kafka, stdout, etc.). Implements `SinkConfig` for configuration and either the `Sink` or `StreamSink` trait for runtime execution. See [Sinks](./onboarding-sinks.md).

<a id="source"></a>
**Source** — A component that ingests data from an external system and emits events into the pipeline. Implements `SourceConfig` for configuration and runs as an async task that sends events via `SourceSender`. See [Sources](./onboarding-sources.md).

<a id="source-sender"></a>
**SourceSender** — The channel interface that sources use to emit events into the pipeline. Batches events into `EventArray` chunks of `CHUNK_SIZE` (1000) for efficiency. Supports multiple named outputs. Provides backpressure when downstream consumers are slow. Defined at [`lib/vector-core/src/source_sender/sender.rs:93`](../lib/vector-core/src/source_sender/sender.rs#L93). See [Sources: SourceSender](./onboarding-sources.md#sourcesender).

<a id="stream-sink"></a>
**StreamSink** — A sink trait that receives a `BoxStream` of events and runs until the stream ends. The most common sink implementation pattern. Defined at [`lib/vector-core/src/sink.rs:92`](../lib/vector-core/src/sink.rs#L92). See [Sinks: StreamSink](./onboarding-sinks.md#streamsink).

<a id="sync-transform"></a>
**SyncTransform** — A transform trait that can write to multiple named outputs via `TransformOutputsBuf` (`transform(&mut self, event: Event, output: &mut TransformOutputsBuf)`). Used for routing transforms like `route` that send different events to different downstream paths. Defined at [`lib/vector-core/src/transform/mod.rs:137`](../lib/vector-core/src/transform/mod.rs#L137). See [Transforms](./onboarding-transforms.md#synctransform).

<a id="task-transform"></a>
**TaskTransform** — An async transform trait that receives and returns a pinned stream (`Pin<Box<dyn Stream<...>>>`). Suitable for transforms that need async coordination, internal buffering, or windowed aggregation. Runs as an independent Tokio task. Defined at [`lib/vector-core/src/transform/mod.rs:111`](../lib/vector-core/src/transform/mod.rs#L111). See [Transforms](./onboarding-transforms.md#tasktransform).

<a id="topology"></a>
**Topology** — The directed acyclic graph (DAG) of sources, transforms, and sinks that defines Vector's data pipeline. Built from configuration, managed at runtime, and supports zero-downtime reload. See [Topology](./onboarding-topology.md).

<a id="transform"></a>
**Transform** — A component that processes events between sources and sinks. Can filter, modify, aggregate, or route events. Comes in three runtime variants: `Function` (simple per-event), `Synchronous` (multi-output), and `Task` (async streaming). Defined at [`lib/vector-core/src/transform/mod.rs:21`](../lib/vector-core/src/transform/mod.rs#L21). See [Transforms](./onboarding-transforms.md).

<a id="vector-sink"></a>
**VectorSink** — An enum wrapping the two sink execution models: `Sink` (Tower-based futures sink) and `Stream` (stream-based `StreamSink`). Returned by `SinkConfig::build()`. Defined at [`lib/vector-core/src/sink.rs:10`](../lib/vector-core/src/sink.rs#L10). See [Sinks: VectorSink](./onboarding-sinks.md#vectorsink).

<a id="vrl"></a>
**VRL** (Vector Remap Language) — A purpose-built expression language for transforming observability data. Used primarily in the `remap` transform. Compiles to an AST that is evaluated per-event. Type-safe, with no I/O or side effects. See the `remap` transform at [`src/transforms/remap.rs:59`](../src/transforms/remap.rs#L59).

<a id="when-full"></a>
**WhenFull** — The backpressure policy for buffers. `Block` pauses the upstream producer until space is available (default). `DropNewest` discards incoming events when the buffer is full. `Overflow` passes events to the next stage in a chained buffer topology. Configured per-sink in the `buffer` section. Defined at [`lib/vector-buffers/src/lib.rs:45`](../lib/vector-buffers/src/lib.rs#L45). See [Buffers & Backpressure](./onboarding-buffers-backpressure.md#whenfull-policy).
