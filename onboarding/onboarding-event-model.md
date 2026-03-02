# Vector Event Model — Developer Onboarding Guide

This document explains Vector's internal event representation: how data is structured, stored, batched, and tracked as it flows through the pipeline.

It answers:

- How events are represented in memory (Log, Metric, Trace variants)
- How LogEvent fields are stored and accessed (Arc-based COW, Value type)
- How metrics carry typed values (Counter, Gauge, Distribution, etc.)
- How event metadata and finalizers enable acknowledgements
- How events are batched for efficient pipeline throughput
- How LogNamespace controls metadata placement

For unfamiliar terms, see the [Glossary](./glossary.md).

## Table of Contents

- [Event Enum](#event-enum)
- [LogEvent](#logevent)
  - [Inner Structure and COW Semantics](#inner-structure-and-cow-semantics)
  - [Field Access via Paths](#field-access-via-paths)
- [Metric](#metric)
  - [Metric Value Types](#metric-value-types)
- [TraceEvent](#traceevent)
- [EventArray](#eventarray)
- [EventMetadata](#eventmetadata)
  - [Finalizers and Acknowledgements](#finalizers-and-acknowledgements)
- [Key Traits](#key-traits)
- [LogNamespace](#log-namespace)
- [Key Files](#key-files)

---

## Event Enum

All data in Vector flows as `Event` instances. The enum is defined at [`lib/vector-core/src/event/mod.rs:50`](../lib/vector-core/src/event/mod.rs#L50):

```rust
pub enum Event {
    Log(LogEvent),
    Metric(Metric),
    Trace(TraceEvent),
}
```

Every component receives and emits `Event` values. Sources produce them, transforms process them, and sinks consume them. The variant determines what fields and operations are available.

`Event` implements key traits:

- `ByteSizeOf` — reports in-memory byte size for buffer accounting.
- `EstimatedJsonEncodedSizeOf` — estimates serialized size for throughput metrics.
- `Finalizable` — extracts finalizers for acknowledgement tracking.
- `EventDataEq` — compares event data, ignoring metadata (used in tests).

---

## LogEvent

The most common event type. A structured key-value map of field paths to values. Defined at [`lib/vector-core/src/event/log_event.rs:155`](../lib/vector-core/src/event/log_event.rs#L155):

```rust
pub struct LogEvent {
    inner: Arc<Inner>,
    // cached sizes for performance
}
```

### Inner Structure and COW Semantics

The actual data lives in [`Inner`](../lib/vector-core/src/event/log_event.rs#L51):

```rust
struct Inner {
    fields: Value,       // The event data (always Value::Object)
    metadata: EventMetadata,
}
```

`fields` is a `Value::Object` (a `BTreeMap<KeyString, Value>`) containing the user-visible event data.

**Copy-on-write (COW):** `LogEvent` wraps `Inner` in an `Arc`. When an event fans out to multiple sinks via `Fanout`, all copies share the same `Arc`. If a transform needs to modify the event, `Arc::make_mut` is called to create a private copy only when needed. This avoids unnecessary cloning for read-only paths.

### Field Access via Paths

Fields are accessed using the `path!()` macro for dot-separated or array-indexed paths:

```rust
// Insert a field
log.insert(path!("host", "name"), "my-server");

// Get a field
let host = log.get(path!("host", "name"));

// Remove a field
log.remove(path!("timestamp"));
```

The `Value` type supports: `Bytes` (strings), `Integer`, `Float`, `Boolean`, `Timestamp`, `Object` (nested map), `Array`, `Null`, and `Regex`.

---

## Metric

Defined at [`lib/vector-core/src/event/metric/mod.rs:58`](../lib/vector-core/src/event/metric/mod.rs#L58):

```rust
pub struct Metric {
    series: MetricSeries,    // name, namespace, tags
    data: MetricData,        // timestamp, kind, value
    metadata: EventMetadata,
}
```

A `MetricSeries` identifies the metric by name, namespace, and tags. `MetricData` holds the timestamp, kind (Incremental or Absolute), and the typed value.

### Metric Value Types

```rust
pub enum MetricValue {
    Counter { value: f64 },
    Gauge { value: f64 },
    Set { values: BTreeSet<String> },
    Distribution { samples: Vec<Sample>, statistic: StatisticKind },
    AggregatedHistogram { buckets: Vec<Bucket>, count: u64, sum: f64 },
    AggregatedSummary { quantiles: Vec<Quantile>, count: u64, sum: f64 },
    Sketch { sketch: MetricSketch },
}
```

- **Counter** — Monotonically increasing total.
- **Gauge** — Value that can go up or down.
- **Set** — Unique values (cardinality tracking).
- **Distribution** — Raw samples for percentile calculation.
- **AggregatedHistogram** — Pre-bucketed histogram.
- **AggregatedSummary** — Pre-calculated quantiles.
- **Sketch** — DDSketch for approximate percentiles.

Metrics have a `kind` field: `Incremental` (delta since last report) or `Absolute` (current total value).

---

## TraceEvent

A wrapper around `LogEvent` for distributed tracing spans. Uses the same field-based storage but carries trace-specific semantics (span ID, trace ID, parent span, etc.). Defined in [`lib/vector-core/src/event/trace.rs`](../lib/vector-core/src/event/trace.rs).

```rust
pub struct TraceEvent(LogEvent);
```

Trace events follow OpenTelemetry conventions for field names and structure.

---

## EventArray

For efficient batch processing, events are grouped into typed arrays. Defined at [`lib/vector-core/src/event/array.rs:134`](../lib/vector-core/src/event/array.rs#L134):

```rust
pub enum EventArray {
    Logs(Vec<LogEvent>),
    Metrics(Vec<Metric>),
    Traces(Vec<TraceEvent>),
}
```

`EventArray` is the primary unit of transfer between components. Sources emit arrays via `SourceSender`, which batches individual events into arrays of `CHUNK_SIZE` (1000). Transforms and sinks process arrays for throughput.

Key operations:

- `len()` — number of events in the array.
- `into_events()` — iterate over individual `Event` values.
- `byte_size()` — total memory usage.
- `estimated_json_encoded_size_of()` — estimated serialized size.

The `EventContainer` trait abstracts over both `Event` and `EventArray` for generic processing.

---

## EventMetadata

Defined at [`lib/vector-core/src/event/metadata.rs:28`](../lib/vector-core/src/event/metadata.rs#L28):

```rust
pub struct EventMetadata {
    value: Value,                    // Metadata fields (source type, etc.)
    secrets: Secrets,                // Sensitive values (tokens, passwords)
    finalizers: EventFinalizers,     // Delivery tracking
    source_id: Option<Arc<ComponentKey>>,
    upstream_id: Option<OutputId>,
    schema_definition: Arc<Definition>,
    // ...
}
```

Metadata is not part of the user-visible event data. It travels alongside the event for:

- **Routing** — `source_id` and `upstream_id` for lineage tracking.
- **Schema** — `schema_definition` for type information.
- **Secrets** — `secrets` for sensitive values that should not be logged.
- **Acknowledgements** — `finalizers` for delivery tracking (see below).

### Finalizers and Acknowledgements

`EventFinalizers` is a collection of `EventFinalizer` instances attached to events by sources. Each finalizer has a callback that reports delivery status:

```rust
pub enum EventStatus {
    Delivered,  // Successfully sent to destination
    Errored,    // Transient failure, may be retried
    Rejected,   // Permanently rejected by destination
    Dropped,    // Intentionally dropped (filtered out)
}
```

The lifecycle:

1. Source creates an `EventFinalizer` and attaches it to the event metadata.
2. Event flows through transforms (finalizers are preserved through `Arc` sharing).
3. Sink calls `event.take_finalizers()` before sending.
4. After write result: `finalizers.update_status(EventStatus::Delivered)` or `EventStatus::Errored`.
5. Source receives the status callback and acts (e.g., Kafka commits the offset).

When events are cloned for fanout, finalizers are shared — all copies must be delivered for the source to receive confirmation.

---

## Key Traits

| Trait | Purpose | Defined In |
|-------|---------|------------|
| `ByteSizeOf` | Reports in-memory byte size for buffer accounting | `vector-core` |
| `EstimatedJsonEncodedSizeOf` | Estimates serialized JSON size for throughput metrics | `vector-core` |
| `EventCount` | Returns event count (always 1 for `Event`, length for `EventArray`) | `vector-core` |
| `Finalizable` | Extracts `EventFinalizers` from an event | `vector-core` |
| `EventDataEq` | Compares event data ignoring metadata (for testing) | `vector-core` |
| `EventContainer` | Generic over `Event` and `EventArray` for batch processing | `vector-core` |

---

## Log Namespace

`LogNamespace` controls where source-injected metadata (source type, host, timestamp) is placed within log events:

- **`Legacy`** — Metadata fields are inserted at the event root (e.g., `source_type`, `host`). This is the Vector v1 behavior. Risk of collisions with user data.
- **`Vector`** — Metadata is stored in a separate namespace within `EventMetadata`, keeping user data clean. This is the recommended mode.

Sources use `LogNamespace` in their `outputs()` method to declare schema, and in their build function to insert fields correctly:

```rust
// Legacy namespace
log.insert(path!("source_type"), "demo_logs");

// Vector namespace
log_namespace.insert_source_metadata(
    DemoLogsConfig::NAME,
    &mut log,
    Some(LegacyKey::InsertIfEmpty(path!("source_type"))),
    path!("source_type"),
    "demo_logs",
);
```

The `insert_source_metadata` helper handles both namespaces transparently.

---

## Key Files

| File | Content |
|------|---------|
| [`lib/vector-core/src/event/mod.rs`](../lib/vector-core/src/event/mod.rs#L50) | `Event` enum definition |
| [`lib/vector-core/src/event/log_event.rs`](../lib/vector-core/src/event/log_event.rs#L155) | `LogEvent`, `Inner`, field access |
| [`lib/vector-core/src/event/metric/mod.rs`](../lib/vector-core/src/event/metric/mod.rs#L58) | `Metric`, `MetricValue`, `MetricSeries` |
| [`lib/vector-core/src/event/trace.rs`](../lib/vector-core/src/event/trace.rs) | `TraceEvent` |
| [`lib/vector-core/src/event/array.rs`](../lib/vector-core/src/event/array.rs#L134) | `EventArray`, `EventContainer` |
| [`lib/vector-core/src/event/metadata.rs`](../lib/vector-core/src/event/metadata.rs#L28) | `EventMetadata`, secrets, finalizers |
| [`lib/vector-core/src/event/finalization.rs`](../lib/vector-core/src/event/finalization.rs) | `EventFinalizer`, `EventStatus` |

---

## Related Docs

- System overview: [`onboarding-system-overview.md`](./onboarding-system-overview.md)
- Source development (event creation): [`onboarding-sources.md`](./onboarding-sources.md)
- Sink development (event consumption): [`onboarding-sinks.md`](./onboarding-sinks.md)
- Transform development (event processing): [`onboarding-transforms.md`](./onboarding-transforms.md)
