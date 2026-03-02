# Vector Configuration System — Developer Onboarding Guide

This document explains how Vector's configuration system works: how config files are loaded, validated, and turned into a running pipeline.

It answers:

- How config files (YAML, JSON, TOML) are discovered and merged
- How `ConfigBuilder` constructs the `Config` struct
- How component registration works via `typetag` and feature flags
- How environment variable interpolation and secret backends work
- How the config watcher enables hot reload
- How schema generation via `#[configurable_component]` works
- How validation catches errors before the topology is built

For unfamiliar terms, see the [Glossary](./glossary.md).

## Table of Contents

- [Config Struct](#config-struct)
- [ConfigBuilder](#configbuilder)
- [Loading Pipeline](#loading-pipeline)
  - [File Discovery](#file-discovery)
  - [Parsing and Merging](#parsing-and-merging)
  - [Environment Variable Interpolation](#environment-variable-interpolation)
  - [Secret Backends](#secret-backends)
- [Component Registration](#component-registration)
- [Validation](#validation)
  - [Graph Validation](#graph-validation)
  - [Resource Conflict Detection](#resource-conflict-detection)
- [Schema Generation](#schema-generation)
- [Config Watcher](#config-watcher)
- [Config Diff](#config-diff)
- [Key Files](#key-files)

---

## Config Struct

The final validated configuration, defined at [`src/config/mod.rs:148`](../src/config/mod.rs#L148):

```rust
pub struct Config {
    pub global: GlobalOptions,
    sources: IndexMap<ComponentKey, SourceOuter>,
    transforms: IndexMap<ComponentKey, TransformOuter<OutputId>>,
    sinks: IndexMap<ComponentKey, SinkOuter<OutputId>>,
    pub enrichment_tables: IndexMap<ComponentKey, EnrichmentTableOuter<OutputId>>,
    tests: Vec<TestDefinition>,
    secret: IndexMap<ComponentKey, SecretBackends>,
    pub graceful_shutdown_duration: Option<Duration>,
    // ...
}
```

Wrapper types add per-component configuration:

- **`SourceOuter`** — Wraps a `BoxedSource` (dynamic `SourceConfig`) with proxy and graph config.
- **`TransformOuter`** — Wraps a `BoxedTransform` (dynamic `TransformConfig`) with input routing.
- **`SinkOuter`** — Wraps a `BoxedSink` (dynamic `SinkConfig`) with input routing, buffer config, healthcheck options, and proxy config.

`IndexMap` preserves insertion order, which matters for deterministic config diff computation.

---

## ConfigBuilder

The mutable builder used during config loading, defined at [`src/config/builder.rs:18`](../src/config/builder.rs#L18):

```rust
pub struct ConfigBuilder {
    pub global: GlobalOptions,
    pub sources: IndexMap<ComponentKey, SourceOuter>,
    pub transforms: IndexMap<ComponentKey, TransformOuter<String>>,
    pub sinks: IndexMap<ComponentKey, SinkOuter<String>>,
    pub enrichment_tables: IndexMap<ComponentKey, EnrichmentTableOuter<String>>,
    pub tests: Vec<TestDefinition>,
    pub secret: IndexMap<ComponentKey, SecretBackends>,
    // ...
}
```

Note: transforms and sinks use `String` for input references during building (before component key resolution). These are resolved to `OutputId` when converting to `Config`.

Multiple `ConfigBuilder` instances can be merged (for multi-file configs):

```rust
let mut builder = ConfigBuilder::default();
builder.append(other_builder)?;  // Merge another config file's builder
let config: Config = builder.build()?;
```

---

## Loading Pipeline

### File Discovery

Config files are specified via `--config` CLI flags. The loading pipeline at [`src/config/loading/mod.rs`](../src/config/loading/mod.rs):

1. **`process_paths()`** at [line 75](../src/config/loading/mod.rs#L75) — Expand directories and glob patterns into concrete file paths.
2. File format is detected by extension: `.yaml`/`.yml` → YAML, `.json` → JSON, `.toml` → TOML.
3. Multiple config files are loaded independently and merged.

### Parsing and Merging

[`load_from_paths()`](../src/config/loading/mod.rs#L127) orchestrates the full pipeline:

1. Read each file and deserialize into a `ConfigBuilder`.
2. Merge all builders via `ConfigBuilder::append()`.
3. Resolve secret backends.
4. Validate and convert to `Config`.

### Environment Variable Interpolation

Config values can reference environment variables:

```yaml
sources:
  my_source:
    type: http_server
    address: "${HTTP_BIND_ADDRESS:-0.0.0.0:8080}"
```

Syntax:
- `${VAR}` — Required variable (error if not set).
- `${VAR:-default}` — Variable with default value.
- `${VAR:?error message}` — Variable with custom error message.

Interpolation can be disabled via `--no-environment-variables` CLI flag.

### Secret Backends

Sensitive values (API keys, passwords) can be resolved from external secret stores:

```yaml
secret:
  my_backend:
    type: exec
    command: ["/path/to/secret-helper"]

sinks:
  my_sink:
    type: http
    auth:
      token: "SECRET[my_backend.api_token]"
```

Secret backends are resolved during config loading, before validation.

---

## Component Registration

Components are registered for dynamic deserialization using `typetag`:

```rust
#[async_trait]
#[typetag::serde(name = "demo_logs")]
impl SourceConfig for DemoLogsConfig { ... }
```

This allows the config system to deserialize `{ "type": "demo_logs", ... }` into the correct `DemoLogsConfig` struct without knowing all source types at compile time.

**Feature flags** gate which components are compiled:

```toml
# In Cargo.toml
[features]
sources-demo_logs = ["dep:fakedata"]
sinks-console = []
transforms-filter = []
```

Components are registered in their respective `mod.rs` files behind `#[cfg(feature = "...")]`:

```rust
// src/sources/mod.rs
#[cfg(feature = "sources-demo_logs")]
pub mod demo_logs;
```

---

## Validation

### Graph Validation

After building `Config`, the topology graph is validated in [`src/config/graph.rs`](../src/config/graph.rs):

- **No cycles** — The component graph must be a DAG.
- **Connected** — All component inputs must reference existing outputs.
- **Type compatibility** — Transform/sink inputs must be compatible with upstream output types (logs, metrics, traces).
- **No dangling** — Sources without downstream consumers generate warnings.

### Resource Conflict Detection

Components can declare system resources they need (e.g., TCP ports):

```rust
fn resources(&self) -> Vec<Resource> {
    vec![Resource::tcp(self.address)]
}
```

The config system checks for conflicts — two components cannot bind to the same port.

---

## Schema Generation

The `#[configurable_component]` macro from `vector-config` generates JSON Schema for component configs:

```rust
#[configurable_component(source("demo_logs", "Generate fake log events."))]
pub struct DemoLogsConfig {
    /// Number of log lines to generate.
    #[serde(default = "default_count")]
    pub count: usize,

    /// Seconds between log line batches.
    #[configurable(metadata(docs::examples = 1.0))]
    pub interval: f64,
}
```

This generates:
- JSON Schema for config validation.
- Documentation snippets for the Vector website.
- Default config examples via `GenerateConfig` trait.

Run `make check-component-docs` to verify schema generation is consistent.

---

## Config Watcher

When `--watch-config` is set, Vector monitors config files for changes:

- **Linux**: inotify (via `notify` crate).
- **macOS**: kqueue (via `notify` crate).
- **Fallback**: Polling at a configurable interval.

When changes are detected:
1. New config is loaded and validated.
2. A reload is triggered via the topology controller.
3. The topology performs a diff-based reload (see [Topology: Config Reload](./onboarding-topology.md#config-reload)).

Implemented in [`src/config/watcher.rs`](../src/config/watcher.rs).

---

## Config Diff

`ConfigDiff` at [`src/config/diff.rs:9`](../src/config/diff.rs#L9) computes the difference between two configs:

```rust
pub struct ConfigDiff {
    pub sources: Difference,
    pub transforms: Difference,
    pub sinks: Difference,
    pub enrichment_tables: Difference,
}
```

Each `Difference` contains `to_add`, `to_remove`, and `to_change` sets of `ComponentKey`. The topology builder only builds components in the diff, not the entire config, making reloads efficient.

A component is considered "changed" if its serialized config differs from the previous version. Unchanged components keep running without interruption.

---

## Key Files

| File | Content |
|------|---------|
| [`src/config/mod.rs`](../src/config/mod.rs#L148) | `Config` struct, `GlobalOptions` |
| [`src/config/builder.rs`](../src/config/builder.rs#L18) | `ConfigBuilder`, merging logic |
| [`src/config/loading/mod.rs`](../src/config/loading/mod.rs#L127) | File loading, parsing, env interpolation |
| [`src/config/source.rs`](../src/config/source.rs#L86) | `SourceConfig` trait, `SourceOuter` |
| [`src/config/transform.rs`](../src/config/transform.rs#L198) | `TransformConfig` trait, `TransformOuter` |
| [`src/config/sink.rs`](../src/config/sink.rs#L238) | `SinkConfig` trait, `SinkOuter` |
| [`src/config/diff.rs`](../src/config/diff.rs#L9) | `ConfigDiff`, `Difference` |
| [`src/config/graph.rs`](../src/config/graph.rs) | Graph validation (cycles, connectivity, types) |
| [`src/config/validation.rs`](../src/config/validation.rs) | Config-level validation |
| [`src/config/watcher.rs`](../src/config/watcher.rs) | File change detection |
| [`lib/vector-config/`](../lib/vector-config/) | `#[configurable_component]` macro, schema generation |

---

## Related Docs

- System overview: [`onboarding-system-overview.md`](./onboarding-system-overview.md)
- Topology (consumes config): [`onboarding-topology.md`](./onboarding-topology.md)
- Sources/Transforms/Sinks (registered in config): [`onboarding-sources.md`](./onboarding-sources.md), [`onboarding-transforms.md`](./onboarding-transforms.md), [`onboarding-sinks.md`](./onboarding-sinks.md)
