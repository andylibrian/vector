# Vector Transforms — Developer Onboarding Guide

This document explains how transforms work in Vector: how they process events between sources and sinks, the three transform types, and how to build new ones.

It answers:

- What traits a transform must implement
- The three runtime variants (FunctionTransform, SyncTransform, TaskTransform) and when to use each
- How TransformContext provides schema and enrichment data
- How multi-output transforms work (OutputBuffer, TransformOutputsBuf)
- How concurrency is controlled
- Walkthroughs of `filter` (simple) and `remap` (complex) transforms

For unfamiliar terms, see the [Glossary](./glossary.md).

## What Problem Do Transforms Solve? (Conceptual Background)

### Processing data in flight

Raw observability data is rarely in the right shape for its destination. Log messages need parsing, sensitive fields need redacting, metrics need aggregating, and events need routing to different sinks based on their content. Doing this processing at the source (application code) couples your applications to your observability infrastructure. Doing it at the destination wastes network bandwidth shipping data that will be filtered or transformed.

Transforms sit between sources and sinks, processing data in flight:

```
Source: raw syslog lines
    │
    ▼
Transform (remap): parse syslog → structured fields, add geo-IP enrichment
    │
    ▼
Transform (filter): drop debug-level events
    │
    ▼
Transform (route): errors → PagerDuty, everything → Elasticsearch
    │
    ├──▸ Sink: PagerDuty (errors only)
    └──▸ Sink: Elasticsearch (all remaining)
```

This pipeline reduces Elasticsearch storage costs (debug events are dropped), adds enrichment data (geo-IP) without application changes, and routes alerts to PagerDuty — all configured in YAML, with zero code changes to the applications.

### Why three transform types?

Not all processing fits the same execution model. Consider three concrete examples:

**Example 1: Filtering events** — A `filter` transform evaluates a condition on each event and either passes it through or drops it. It needs no state, no async I/O, and no buffering. It processes one event at a time, synchronously. This is a `FunctionTransform`.

```
Input:  event{level="debug"} → evaluate condition → drop (no output)
Input:  event{level="error"} → evaluate condition → pass through
```

**Example 2: Routing to multiple outputs** — A `route` transform inspects each event and sends it to one of several named outputs based on conditions. It needs the ability to write to different output channels, but still processes one event at a time. This is a `SyncTransform`.

```
Input:  event{status=500} → condition matches "errors" → push to output "errors"
Input:  event{status=200} → no condition matches       → push to default output
```

**Example 3: Aggregating metrics over time** — An `aggregate` transform collects metrics over a time window and emits combined results periodically. It must buffer events internally, manage timers, and flush asynchronously. It can't process events one at a time — it needs the full input stream. This is a `TaskTransform`.

```
Input stream:  metric{name="requests", value=1}, metric{name="requests", value=1}, ...
               (accumulate for 10 seconds)
Output:        metric{name="requests", value=1547}  ← emitted every 10 seconds
```

The three trait variants (`FunctionTransform`, `SyncTransform`, `TaskTransform`) correspond directly to these execution models. Using the simplest variant that fits your use case gives the topology builder maximum flexibility — `FunctionTransform` can be inlined into the pipeline loop and parallelized, while `TaskTransform` must run as an independent Tokio task.

### How transforms fit in the pipeline

The topology builder wraps each transform variant differently:

```
FunctionTransform / SyncTransform:
  ┌──────────────────────────────────────────────────────┐
  │  loop {                                              │
  │      events = input_channel.recv()                   │
  │      for event in events {                           │
  │          transform.transform(&mut output, event)     │ ← Inlined in the loop
  │      }                                               │
  │      output_channel.send(output.drain())             │
  │  }                                                   │
  └──────────────────────────────────────────────────────┘
  (runs inside the pipeline's event-processing loop, not as a separate task)

TaskTransform:
  ┌──────────────────────────────────────────────────────┐
  │  output_stream = transform.transform(input_stream)   │ ← Separate Tokio task
  │  // The transform owns the stream and controls       │
  │  // when and how events are emitted                  │
  └──────────────────────────────────────────────────────┘
```

For `FunctionTransform` with `enable_concurrency() = true`, the topology can spawn multiple worker tasks that pull from the same input and process events in parallel — useful for CPU-bound transforms where ordering doesn't matter.

### OutputBuffer vs TransformOutputsBuf

These two types control where processed events go:

```
FunctionTransform uses OutputBuffer:
  output.push(event)     ← Events go to the single default output

SyncTransform uses TransformOutputsBuf:
  output.push(None, event)            ← Events go to default output
  output.push(Some("errors"), event)  ← Events go to the named "errors" output
```

Named outputs create separate downstream paths in the topology graph. A transform with outputs `["default", "errors"]` creates two Fanouts, and downstream components can subscribe to either. This is how the `route` transform sends different events to different sinks without intermediate channels.

## Table of Contents

- [TransformConfig Trait](#transformconfig-trait)
- [TransformContext](#transformcontext)
- [Transform Enum](#transform-enum)
- [FunctionTransform](#functiontransform)
- [SyncTransform](#synctransform)
- [TaskTransform](#tasktransform)
- [Choosing a Transform Type](#choosing-a-transform-type)
- [Concurrency](#concurrency)
- [Walkthrough: filter Transform](#walkthrough-filter-transform)
- [Walkthrough: remap Transform](#walkthrough-remap-transform)
- [Adding a New Transform](#adding-a-new-transform)
- [Key Files](#key-files)

---

## TransformConfig Trait

Every transform must implement `TransformConfig`, defined at [`src/config/transform.rs:198`](../src/config/transform.rs#L198):

```rust
#[async_trait]
#[typetag::serde(tag = "type")]
pub trait TransformConfig: DynClone + NamedComponent + Debug + Send + Sync {
    /// Build the transform runtime from this configuration.
    async fn build(&self, context: &TransformContext) -> crate::Result<Transform>;

    /// Declare what event types this transform accepts.
    fn input(&self) -> Input;

    /// Declare the outputs this transform produces and their schemas.
    fn outputs(
        &self,
        context: &TransformContext,
        input_definitions: &[(OutputId, schema::Definition)],
    ) -> Vec<TransformOutput>;

    /// Whether this transform can run concurrently across multiple events.
    fn enable_concurrency(&self) -> bool { false }

    /// Whether this transform can be nested inside other transforms.
    fn nestable(&self, _parents: &HashSet<&'static str>) -> bool { true }

    /// Validate against the merged input schema definition.
    fn validate(&self, _merged_definition: &schema::Definition) -> Result<(), Vec<String>> { Ok(()) }
}
```

Key points:

- **`build()`** receives a `TransformContext` and returns a `Transform` enum variant.
- **`input()`** declares accepted event types (logs, metrics, traces, or combinations).
- **`outputs()`** declares output schemas, receiving input definitions for schema propagation.
- **`enable_concurrency()`** — if `true`, the topology spawns multiple worker tasks to process events in parallel. Suitable for CPU-light transforms.

---

## TransformContext

Provided to `build()`, defined at [`src/config/transform.rs:114`](../src/config/transform.rs#L114):

```rust
pub struct TransformContext {
    pub key: Option<ComponentKey>,
    pub globals: GlobalOptions,
    pub enrichment_tables: TableRegistry,
    pub merged_schema_definition: schema::Definition,
    pub schema: schema::Options,
    // ...
}
```

| Field | Purpose |
|-------|---------|
| `key` | Component identifier (for logging and metrics) |
| `globals` | Global config options (timezone, log schema, etc.) |
| `enrichment_tables` | Access to loaded enrichment tables (GeoIP, CSV lookups) |
| `merged_schema_definition` | Combined input schema from all upstream components |

---

## Transform Enum

The runtime representation, defined at [`lib/vector-core/src/transform/mod.rs:21`](../lib/vector-core/src/transform/mod.rs#L21):

```rust
pub enum Transform {
    Function(Box<dyn FunctionTransform>),
    Synchronous(Box<dyn SyncTransform>),
    Task(Box<dyn TaskTransform<EventArray>>),
}
```

Each variant corresponds to a different execution model. The topology builder wraps the chosen variant in the appropriate async runtime machinery.

---

## FunctionTransform

The simplest transform type. Defined at [`lib/vector-core/src/transform/mod.rs:97`](../lib/vector-core/src/transform/mod.rs#L97):

```rust
pub trait FunctionTransform: Send + DynClone + Sync {
    fn transform(&mut self, output: &mut OutputBuffer, event: Event);
}
```

- Receives one event at a time.
- Writes zero or more events to `OutputBuffer`.
- **No async** — purely synchronous processing.
- Can drop events (write nothing), pass through (write the same event), or expand (write multiple events).

Best for: stateless per-event operations (filtering, field manipulation, simple enrichment).

The topology pulls events from the input stream, calls `transform()` for each, and forwards whatever is in `OutputBuffer` downstream.

---

## SyncTransform

Multi-output capable transform. Defined at [`lib/vector-core/src/transform/mod.rs:137`](../lib/vector-core/src/transform/mod.rs#L137):

```rust
pub trait SyncTransform: Send + DynClone + Sync {
    fn transform(&mut self, event: Event, output: &mut TransformOutputsBuf);
}
```

- Like `FunctionTransform`, but receives `TransformOutputsBuf` instead of `OutputBuffer`.
- `TransformOutputsBuf` allows writing to **named outputs** (e.g., `output.push(Some("errors"), event)`).
- Used by routing transforms that send different events to different downstream paths.

Best for: `route`, `swimlanes`, or any transform that needs multiple output channels.

---

## TaskTransform

Async streaming transform. Defined at [`lib/vector-core/src/transform/mod.rs:111`](../lib/vector-core/src/transform/mod.rs#L111):

```rust
pub trait TaskTransform<T: EventContainer + 'static>: Send + 'static {
    fn transform(
        self: Box<Self>,
        task_transform_input: Pin<Box<dyn Stream<Item = T> + Send>>,
    ) -> Pin<Box<dyn Stream<Item = T> + Send>>;
}
```

- Receives the full input stream and returns a transformed output stream.
- Runs as an independent Tokio task.
- Can maintain internal state, perform async I/O, buffer events, or implement windowed aggregation.
- **Not cloneable** — runs as a single task.

Best for: aggregation, deduplication, windowed operations, or anything needing async coordination.

---

## Choosing a Transform Type

| Criterion | FunctionTransform | SyncTransform | TaskTransform |
|-----------|-------------------|---------------|---------------|
| Per-event, stateless | Yes | Yes | Overkill |
| Multiple outputs | No | Yes | No |
| Needs async I/O | No | No | Yes |
| Needs buffering/windowing | No | No | Yes |
| Concurrent execution | Yes (`enable_concurrency`) | Yes | No (single task) |
| Complexity | Low | Low | High |

**Default choice:** `FunctionTransform` for most transforms. Use `SyncTransform` only when you need named outputs. Use `TaskTransform` only when you need async coordination.

---

## Concurrency

When `enable_concurrency()` returns `true`, the topology builder spawns multiple worker tasks to process events in parallel. The concurrency level is based on the Tokio worker thread count.

This is suitable for CPU-light transforms where processing order doesn't matter (e.g., `filter`, simple field manipulation). Do not enable it for transforms that require ordering guarantees or shared mutable state.

---

## Walkthrough: filter Transform

The `filter` transform at [`src/transforms/filter.rs:23`](../src/transforms/filter.rs#L23) is the simplest possible transform.

**Configuration:**

```rust
#[configurable_component(transform("filter", "Filter events based on a set of conditions."))]
pub struct FilterConfig {
    #[configurable(derived)]
    condition: AnyCondition,
}
```

**Build:**

```rust
impl TransformConfig for FilterConfig {
    async fn build(&self, context: &TransformContext) -> crate::Result<Transform> {
        Ok(Transform::function(Filter::new(
            self.condition.build(&context.enrichment_tables, &context.metrics_storage)?,
        )))
    }

    fn input(&self) -> Input { Input::all() }

    fn enable_concurrency(&self) -> bool { true }  // CPU-light, safe to parallelize
}
```

**Runtime:**

```rust
pub struct Filter {
    condition: Condition,
    events_dropped: Registered<FilterEventsDropped>,
}

impl FunctionTransform for Filter {
    fn transform(&mut self, output: &mut OutputBuffer, event: Event) {
        let (result, event) = self.condition.check(event);
        if result {
            output.push(event);       // Pass through
        } else {
            self.events_dropped.emit(Count(1));  // Track dropped events
        }
    }
}
```

Key observations:

- Uses `FunctionTransform` — simplest variant.
- Evaluates a condition per event.
- Either passes the event through or drops it (with telemetry).
- Enables concurrency since it is stateless.

---

## Walkthrough: remap Transform

The `remap` transform at [`src/transforms/remap.rs:59`](../src/transforms/remap.rs#L59) is the most widely used transform, powered by VRL.

**Configuration:**

```rust
#[configurable_component(transform("remap", "Modify your observability data..."))]
pub struct RemapConfig {
    pub source: Option<String>,         // Inline VRL program
    pub file: Option<PathBuf>,          // VRL program from file
    pub files: Option<Vec<PathBuf>>,    // Multiple VRL files

    #[serde(default = "crate::serde::default_true")]
    pub drop_on_error: bool,            // Drop events on VRL error

    #[serde(default)]
    pub drop_on_abort: bool,            // Drop events on VRL abort

    #[serde(default)]
    pub reroute_dropped: bool,          // Send dropped events to "dropped" output
    // ...
}
```

Key features:

- **VRL compilation:** The VRL source is compiled into a `Program` at build time using schema information for type checking.
- **Multiple outputs:** When `reroute_dropped` is true, the transform has two outputs: the default output and a `"dropped"` output for events that failed VRL execution.
- **Caching:** Compiled VRL programs are cached by `(TableRegistry, Definition)` to avoid recompilation during reload.
- **Error handling:** Events that cause VRL errors can be dropped, rerouted, or passed through with error metadata.

---

## Adding a New Transform

Checklist for adding a new transform:

1. **Create the module** under `src/transforms/my_transform.rs`.
2. **Define the config struct** with `#[configurable_component(transform("my_transform", "Description."))]`.
3. **Implement `TransformConfig`** with `#[typetag::serde(name = "my_transform")]`.
4. **Implement the appropriate trait** (`FunctionTransform`, `SyncTransform`, or `TaskTransform`).
5. **Add a feature flag** in `Cargo.toml` (e.g., `transforms-my_transform = []`).
6. **Register the module** in `src/transforms/mod.rs` behind the feature flag.
7. **Add internal events** in `src/internal_events/` for telemetry.
8. **Write tests** — unit tests with `OutputBuffer` assertions.
9. **Run `make check-component-docs`** to verify documentation generation.

---

## Key Files

| File | Content |
|------|---------|
| [`src/config/transform.rs`](../src/config/transform.rs#L198) | `TransformConfig` trait, `TransformContext` |
| [`lib/vector-core/src/transform/mod.rs`](../lib/vector-core/src/transform/mod.rs#L21) | `Transform` enum, `FunctionTransform`, `SyncTransform`, `TaskTransform` |
| [`src/transforms/mod.rs`](../src/transforms/mod.rs) | Feature-gated transform module registration |
| [`src/transforms/filter.rs`](../src/transforms/filter.rs#L23) | Example: simple FunctionTransform |
| [`src/transforms/remap.rs`](../src/transforms/remap.rs#L59) | Example: complex VRL-powered transform |
| [`src/transforms/route.rs`](../src/transforms/route.rs) | Example: SyncTransform with multiple outputs |
| [`src/transforms/aggregate.rs`](../src/transforms/aggregate.rs) | Example: TaskTransform with windowing |

---

## Related Docs

- System overview: [`onboarding-system-overview.md`](./onboarding-system-overview.md)
- Event model (what transforms process): [`onboarding-event-model.md`](./onboarding-event-model.md)
- Sources (upstream of transforms): [`onboarding-sources.md`](./onboarding-sources.md)
- Sinks (downstream of transforms): [`onboarding-sinks.md`](./onboarding-sinks.md)
