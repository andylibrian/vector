# Vector Transforms — Developer Onboarding Guide

This document explains how transforms work in Vector: how they process events between sources and sinks, the three transform types, and how to build new ones.

It answers:

- What traits a transform must implement
- The three runtime variants (FunctionTransform, SyncTransform, TaskTransform) and when to use each
- How TransformContext provides schema and enrichment data
- How multi-output transforms work (OutputBuffer, TransformOutputs)
- How concurrency is controlled
- Walkthroughs of `filter` (simple) and `remap` (complex) transforms

For unfamiliar terms, see the [Glossary](./glossary.md).

## Table of Contents

- [TransformConfig Trait](#transformconfig-trait)
- [TransformContext](#transformcontext)
- [Transform Enum](#transform-enum)
- [FunctionTransform](#function-transform)
- [SyncTransform](#sync-transform)
- [TaskTransform](#task-transform)
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
    fn transform(&mut self, output: &mut TransformOutputs, event: Event);
}
```

- Like `FunctionTransform`, but receives `TransformOutputs` instead of `OutputBuffer`.
- `TransformOutputs` allows writing to **named outputs** (e.g., `output.push_named("errors", event)`).
- Used by routing transforms that send different events to different downstream paths.

Best for: `route`, `swimlanes`, or any transform that needs multiple output channels.

---

## TaskTransform

Async streaming transform. Defined at [`lib/vector-core/src/transform/mod.rs:111`](../lib/vector-core/src/transform/mod.rs#L111):

```rust
pub trait TaskTransform<T: Send + 'static>: Send {
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
            self.condition.build(&context.enrichment_tables, context.merged_schema_definition.clone())?,
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
