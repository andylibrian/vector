# Vector System Overview — Developer Onboarding Guide

This document is the high-level map of Vector internals. It is meant to be read before deep dives into event model, topology, component development, and cross-cutting concerns.

It answers:

- Which crates, modules, and traits own which responsibilities
- How data flows through the pipeline from ingestion to delivery
- Where startup, routing, flow control, and telemetry are implemented
- Which files to open first when changing a specific subsystem

For unfamiliar terms (Event, Topology, Fanout, VRL, etc.), see the [Glossary](./glossary.md).

## Table of Contents

- [Overview](#overview)
- [Core Components](#core-components)
- [Data Flow](#data-flow)
- [Application Lifecycle](#application-lifecycle)
  - [Startup](#startup)
  - [Main Event Loop](#main-event-loop)
  - [Graceful Shutdown](#graceful-shutdown)
- [Configuration and Hot Reload](#configuration-and-hot-reload)
- [Cross-Cutting Concerns](#cross-cutting-concerns)
  - [Backpressure](#backpressure)
  - [Acknowledgements](#acknowledgements)
  - [Schema Propagation](#schema-propagation)
  - [Telemetry and Internal Events](#telemetry-and-internal-events)
  - [Feature Flags](#feature-flags)
- [Change Map](#change-map)
- [Related Onboarding Docs](#related-onboarding-docs)

---

## Overview

Vector is a single binary that runs a directed acyclic graph (DAG) of **sources**, **transforms**, and **sinks**. Data enters through sources, is processed by transforms, and exits through sinks. Components are connected via async channels with configurable buffering.

The top-level application logic lives in `src/` (binary entry point, CLI, config loading, topology management). Core abstractions (events, traits, buffers, codecs) live in `lib/` as independent Rust crates. Component implementations (individual sources, transforms, sinks) live under `src/sources/`, `src/transforms/`, and `src/sinks/`.

All I/O is async, powered by the Tokio runtime. Components run as isolated async tasks managed by `RunningTopology`.

---

## Core Components

| Component | Responsibility | Key Entry Points |
|-----------|----------------|------------------|
| `src/app.rs` | Application lifecycle: build runtime, load config, start topology, handle signals | [`Application`](../src/app.rs#L54), [`StartedApplication::run`](../src/app.rs#L291) |
| `src/config/` | Configuration loading, validation, schema generation, hot reload watcher | [`Config`](../src/config/mod.rs#L148), [`ConfigBuilder`](../src/config/builder.rs#L18), [`load_from_paths`](../src/config/loading/mod.rs#L127) |
| `src/topology/` | Component graph construction, live management, zero-downtime reload | [`Builder`](../src/topology/builder.rs#L77), [`RunningTopology`](../src/topology/running.rs#L56), [`ReloadOutcome`](../src/topology/controller.rs#L56) |
| `src/sources/` | 47+ data ingestion components | [`SourceConfig`](../src/config/source.rs#L86), [`SourceContext`](../src/config/source.rs#L134) |
| `src/transforms/` | 20+ data processing components | [`TransformConfig`](../src/config/transform.rs#L198), [`Transform`](../lib/vector-core/src/transform/mod.rs#L21) |
| `src/sinks/` | 56+ data output components | [`SinkConfig`](../src/config/sink.rs#L238), [`VectorSink`](../lib/vector-core/src/sink.rs#L10) |
| `lib/vector-core/src/event/` | Event types (Log, Metric, Trace), metadata, finalization | [`Event`](../lib/vector-core/src/event/mod.rs#L50), [`LogEvent`](../lib/vector-core/src/event/log_event.rs#L155) |
| `lib/vector-core/src/source_sender/` | Source-to-pipeline channel with batching | [`SourceSender`](../lib/vector-core/src/source_sender/sender.rs#L93), [`CHUNK_SIZE`](../lib/vector-core/src/source_sender/mod.rs#L20) |
| `lib/vector-core/src/fanout.rs` | Multi-consumer event distribution | [`Fanout`](../lib/vector-core/src/fanout.rs#L45) |
| `lib/vector-buffers/` | Memory and disk buffering with backpressure | [`BufferType`](../lib/vector-buffers/src/config.rs#L216), [`WhenFull`](../lib/vector-buffers/src/lib.rs#L45) |
| `lib/codecs/` | Encoding/decoding for 15+ formats | [`Decoder`](../lib/codecs/src/decoding/decoder.rs#L19), [`Encoder`](../lib/codecs/src/encoding/encoder.rs#L83) |
| `src/api/` | GraphQL management and monitoring API | Enabled via `--api` flag |
| `src/internal_events/` | Telemetry events for metrics and tracing | [`VectorStarted`](../src/internal_events/process.rs#L8), [`VectorStopped`](../src/internal_events/process.rs#L42) |

---

## Data Flow

```
┌──────────┐     ┌──────────────┐     ┌────────┐     ┌────────┐     ┌──────────┐     ┌────────┐     ┌──────┐
│  Source   │────▸│ SourceSender │────▸│ Buffer │────▸│ Fanout │────▸│Transform │────▸│ Buffer │────▸│ Sink │
│ (async   │     │ (batches to  │     │(memory │     │(routes │     │(Function │     │(memory │     │(runs │
│  task)   │     │  EventArray) │     │or disk)│     │ to N   │     │ Sync, or │     │or disk)│     │async)│
│          │     │              │     │        │     │outputs)│     │  Task)   │     │        │     │      │
└──────────┘     └──────────────┘     └────────┘     └────────┘     └──────────┘     └────────┘     └──────┘
                       │                                                                                │
                       │                    Backpressure propagates upstream ◂───────────────────────────│
                       │                                                                                │
                       └──── Finalizers track delivery status ─────────────────────────────────────────▸ │
```

Step by step:

1. **Source** runs as an async task. It ingests data from an external system (files, HTTP, Kafka, etc.) and sends events via `SourceSender`.
2. **SourceSender** batches events into `EventArray` chunks of 1000 (`CHUNK_SIZE`) and pushes them through a channel.
3. **Buffer** (per-component) holds events in memory or on disk, applying backpressure when full.
4. **Fanout** distributes events from one output to multiple downstream consumers (transforms or sinks).
5. **Transform** processes events — filtering, modifying, aggregating, or routing — and outputs to the next buffer.
6. **Sink** receives events from its buffer and writes them to the external destination (Elasticsearch, S3, stdout, etc.).
7. **Acknowledgements** flow backward: sinks report delivery status via event finalizers, which sources use for at-least-once guarantees (e.g., Kafka offset commits).

---

## Application Lifecycle

### Startup

Entry point: [`src/main.rs:48`](../src/main.rs#L48) → [`Application::run`](../src/app.rs#L171)

1. Parse CLI arguments via `clap` in [`src/cli.rs`](../src/cli.rs).
2. Build Tokio runtime with configurable worker threads in [`Application::prepare_from_opts`](../src/app.rs#L195).
3. Initialize logging and telemetry.
4. Load configuration from files/directories via [`load_from_paths`](../src/config/loading/mod.rs#L127).
5. Validate config and build topology via [`RunningTopology::start_init_validated`](../src/topology/running.rs#L1263).
6. Start API server (if enabled via `--api`).
7. Transition to `StartedApplication` and enter main event loop.

### Main Event Loop

In [`StartedApplication::run`](../src/app.rs#L291):

- Listen for OS signals: `SIGTERM`/`SIGINT` → graceful shutdown, `SIGHUP` → config reload.
- Watch config files for changes (if `--watch-config` is set).
- Handle topology reload requests.
- Monitor for component crashes and trigger recovery or shutdown.

### Graceful Shutdown

1. Stop accepting new data in sources (via `SourceShutdownCoordinator`).
2. Drain in-flight events through transforms and sinks.
3. Wait up to the graceful shutdown timeout (default 60 seconds, configurable via `--graceful-shutdown-limit-secs`).
4. Close all components and exit.

---

## Configuration and Hot Reload

**Loading pipeline:**

1. Config files (YAML, JSON, or TOML) are discovered via `--config` paths.
2. Files are parsed and merged into a [`ConfigBuilder`](../src/config/builder.rs#L18) via [`load_from_paths`](../src/config/loading/mod.rs#L127).
3. Environment variables are interpolated (unless disabled).
4. Secret backends are resolved.
5. `ConfigBuilder` is validated and converted to [`Config`](../src/config/mod.rs#L148).
6. Topology graph is validated for cycles, type compatibility, and resource conflicts.

**Hot reload:**

1. Config watcher detects file changes (inotify on Linux, kqueue on macOS, or polling).
2. New config is loaded and validated.
3. A [`ConfigDiff`](../src/config/diff.rs#L9) is computed: which components were added, removed, or changed.
4. [`RunningTopology::reload_config_and_respawn`](../src/topology/running.rs#L286) applies the diff:
   - New components are built and started.
   - Removed components are shut down.
   - Changed components are stopped and rebuilt.
   - Unchanged components keep running.
5. If the reload fails, it is rolled back to the previous config ([`ReloadOutcome::RolledBack`](../src/topology/controller.rs#L56)).

See [Configuration deep dive](./onboarding-config.md) and [Topology deep dive](./onboarding-topology.md).

---

## Cross-Cutting Concerns

### Backpressure

Backpressure propagates upstream through async channels:

- When a sink's buffer is full, the transform writing to it blocks (or drops events, depending on `WhenFull` policy).
- When a transform blocks, the source feeding it blocks on `SourceSender`.
- This chain reaction slows the entire pipeline gracefully without data loss (in `Block` mode).

Buffer policies are configurable per-sink: `Block` (default) or `DropNewest`.

See [Buffers & Backpressure](./onboarding-buffers-backpressure.md).

### Acknowledgements

End-to-end acknowledgements allow sources to confirm delivery:

1. Source attaches `EventFinalizer` to each event.
2. Event flows through transforms (finalizers are preserved).
3. Sink calls `finalizers.update_status(EventStatus::Delivered)` on success.
4. Source receives confirmation and takes action (e.g., commit Kafka offset).

Not all sources support acknowledgements — check `can_acknowledge()` on the source config.

See [Sinks: Acknowledgement Flow](./onboarding-sinks.md#acknowledgement-flow).

### Schema Propagation

Event schemas flow through the topology:

1. Sources declare their output schemas via `outputs()`.
2. Transforms receive input schemas and produce output schemas via `outputs()`.
3. Sinks validate that incoming events match expected schemas.
4. VRL programs in `remap` transforms use schema information for type checking.

Managed by [`src/topology/schema.rs`](../src/topology/schema.rs).

### Telemetry and Internal Events

Vector instruments itself via internal events:

- **Internal events** (`src/internal_events/`) emit metrics and logs for component operations.
- **`register!()` macro** — creates a registered event handle at component startup.
- **`emit!()` macro** — emits an event instance (increments counters, records histograms).
- Common events: `BytesReceived`, `BytesSent`, `EventsReceived`, `EventsSent`, `ComponentEventsDropped`.
- Process-level events: [`VectorStarted`](../src/internal_events/process.rs#L8), [`VectorStopped`](../src/internal_events/process.rs#L42), [`VectorReloaded`](../src/internal_events/process.rs#L25).

### Feature Flags

Every component is gated behind a Cargo feature flag:

- Sources: `sources-file`, `sources-kafka`, `sources-http_server`, etc.
- Transforms: `transforms-remap`, `transforms-filter`, `transforms-route`, etc.
- Sinks: `sinks-console`, `sinks-elasticsearch`, `sinks-aws_s3`, etc.

Build with specific features for faster iteration:

```bash
cargo test --lib --no-default-features --features sinks-console sinks::console
```

Feature definitions are in the root [`Cargo.toml`](../Cargo.toml).

---

## Change Map

If you need to change:

- **Public-facing CLI behavior:**
  - [`src/cli.rs`](../src/cli.rs) — argument parsing
  - [`src/app.rs`](../src/app.rs#L54) — application lifecycle

- **Configuration loading or validation:**
  - [`src/config/loading/mod.rs`](../src/config/loading/mod.rs#L127) — file discovery and parsing
  - [`src/config/builder.rs`](../src/config/builder.rs#L18) — config construction
  - [`src/config/mod.rs`](../src/config/mod.rs#L148) — `Config` struct and validation

- **Topology building, reload, or shutdown:**
  - [`src/topology/builder.rs`](../src/topology/builder.rs#L77) — component construction
  - [`src/topology/running.rs`](../src/topology/running.rs#L56) — live topology management
  - [`src/topology/controller.rs`](../src/topology/controller.rs#L56) — reload orchestration

- **Event types or metadata:**
  - [`lib/vector-core/src/event/`](../lib/vector-core/src/event/) — event definitions
  - [`lib/vector-core/src/event/metadata.rs`](../lib/vector-core/src/event/metadata.rs#L28) — metadata structure

- **Source, transform, or sink traits:**
  - [`src/config/source.rs`](../src/config/source.rs#L86) — `SourceConfig` trait
  - [`src/config/transform.rs`](../src/config/transform.rs#L198) — `TransformConfig` trait
  - [`src/config/sink.rs`](../src/config/sink.rs#L238) — `SinkConfig` trait

- **Buffering or backpressure:**
  - [`lib/vector-buffers/`](../lib/vector-buffers/) — buffer implementations
  - [`lib/vector-core/src/source_sender/`](../lib/vector-core/src/source_sender/) — source channel

- **Encoding or decoding:**
  - [`lib/codecs/`](../lib/codecs/) — codec implementations

- **Internal telemetry:**
  - [`src/internal_events/`](../src/internal_events/) — event definitions
  - [`docs/specs/instrumentation.md`](../docs/specs/instrumentation.md) — naming conventions

---

## Related Onboarding Docs

- Event model deep dive: [`onboarding-event-model.md`](./onboarding-event-model.md)
- Configuration deep dive: [`onboarding-config.md`](./onboarding-config.md)
- Topology deep dive: [`onboarding-topology.md`](./onboarding-topology.md)
- Source development: [`onboarding-sources.md`](./onboarding-sources.md)
- Transform development: [`onboarding-transforms.md`](./onboarding-transforms.md)
- Sink development: [`onboarding-sinks.md`](./onboarding-sinks.md)
- Buffers and backpressure: [`onboarding-buffers-backpressure.md`](./onboarding-buffers-backpressure.md)
- Codecs: [`onboarding-codecs.md`](./onboarding-codecs.md)

Suggested reading order for new engineers:

1. This file (`onboarding-system-overview.md`)
2. `onboarding-event-model.md`
3. `onboarding-config.md`
4. `onboarding-topology.md`
5. `onboarding-sources.md`
6. `onboarding-transforms.md`
7. `onboarding-sinks.md`
8. `onboarding-buffers-backpressure.md`
9. `onboarding-codecs.md`
