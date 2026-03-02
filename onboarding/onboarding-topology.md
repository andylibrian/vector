# Vector Topology — Developer Onboarding Guide

This document explains how Vector's topology system works: how configuration is turned into a running pipeline of async tasks, how components are wired together, and how the topology supports zero-downtime reload and graceful shutdown.

It answers:

- How `TopologyPiecesBuilder` constructs sources, transforms, and sinks from config
- How `RunningTopology` manages live tasks, inputs, outputs, and shutdown
- How Fanout distributes events to multiple consumers
- How config reload works via `ConfigDiff` and component rewiring
- How graceful shutdown coordinates draining in-flight events
- How signals (SIGTERM, SIGHUP) are handled

For unfamiliar terms, see the [Glossary](./glossary.md).

## Table of Contents

- [Overview](#overview)
- [Topology Building](#topology-building)
  - [Builder Struct](#builder-struct)
  - [Build Phases](#build-phases)
  - [TopologyPieces](#topologypieces)
- [RunningTopology](#running-topology)
  - [Task Management](#task-management)
  - [Inputs and Outputs](#inputs-and-outputs)
- [Fanout and Control Channels](#fanout-and-control-channels)
- [Config Reload](#config-reload)
  - [ConfigDiff](#configdiff)
  - [Reload Process](#reload-process)
  - [Rollback on Failure](#rollback-on-failure)
- [Graceful Shutdown](#graceful-shutdown)
  - [SourceShutdownCoordinator](#sourceshutdowncoordinator)
  - [Drain Sequence](#drain-sequence)
- [Signal Handling](#signal-handling)
- [Key Files](#key-files)

---

## Overview

The topology is Vector's runtime orchestrator. It takes a validated `Config`, builds all components as async tasks, wires them together via channels and buffers, and manages their lifecycle (start, reload, shutdown).

```
Config ──▸ TopologyPiecesBuilder ──▸ TopologyPieces ──▸ RunningTopology
                                                              │
                                                    ┌─────────┼──────────┐
                                                    │         │          │
                                               Source     Transform    Sink
                                               Tasks      Tasks       Tasks
                                                    │         │          │
                                                    └─────────┼──────────┘
                                                              │
                                                     Signal Loop (app.rs)
                                                     - SIGTERM → shutdown
                                                     - SIGHUP → reload
```

---

## Topology Building

### Builder Struct

The internal `Builder` at [`src/topology/builder.rs:77`](../src/topology/builder.rs#L77) manages the construction process:

```rust
struct Builder<'a> {
    config: &'a super::Config,
    diff: &'a ConfigDiff,
    shutdown_coordinator: SourceShutdownCoordinator,
    errors: Vec<String>,
    outputs: HashMap<OutputId, UnboundedSender<fanout::ControlMessage>>,
    tasks: HashMap<ComponentKey, Task>,
    buffers: HashMap<ComponentKey, BuiltBuffer>,
    inputs: HashMap<ComponentKey, (BufferSender<EventArray>, Inputs<OutputId>)>,
    healthchecks: HashMap<ComponentKey, Task>,
    detach_triggers: HashMap<ComponentKey, Trigger>,
    extra_context: ExtraContext,
    // ...
}
```

### Build Phases

The public entry point is [`TopologyPiecesBuilder::build()`](../src/topology/builder.rs#L1022):

1. **Load enrichment tables** — Initialize GeoIP databases, CSV files, etc.
2. **Build sources** — For each source in the diff:
   - Create `SourceContext` with shutdown signal, `SourceSender`, enrichment tables.
   - Call `SourceConfig::build()` to get the async source task.
   - Create `Fanout` for each source output.
   - Spawn pump tasks that forward events from `SourceSender` to `Fanout`.
3. **Build transforms** — For each transform in the diff:
   - Create `TransformContext` with enrichment tables, schema definitions.
   - Call `TransformConfig::build()` to get the `Transform` variant.
   - Wrap in appropriate runtime: inline (Function/Sync) or spawned task (Task).
   - Create output `Fanout` for downstream routing.
4. **Build sinks** — For each sink in the diff:
   - Create `SinkContext` with healthcheck options, proxy config.
   - Create the buffer (memory or disk) per sink config.
   - Call `SinkConfig::build()` to get `(VectorSink, Healthcheck)`.
   - Wrap sink with buffer receiver.
5. **Finalize outputs** — Wire component outputs to their downstream inputs.

### TopologyPieces

The build result, defined at [`src/topology/builder.rs:957`](../src/topology/builder.rs#L957):

```rust
pub struct TopologyPieces {
    pub inputs: HashMap<ComponentKey, (BufferSender<EventArray>, Inputs<OutputId>)>,
    pub outputs: HashMap<OutputId, UnboundedSender<fanout::ControlMessage>>,
    pub tasks: HashMap<ComponentKey, Task>,
    pub source_tasks: HashMap<ComponentKey, Task>,
    pub healthchecks: HashMap<ComponentKey, Task>,
    pub shutdown_coordinator: SourceShutdownCoordinator,
    pub detach_triggers: HashMap<ComponentKey, Trigger>,
}
```

This struct contains everything needed to start or update the running topology.

---

## RunningTopology

The live pipeline, defined at [`src/topology/running.rs:56`](../src/topology/running.rs#L56):

```rust
pub struct RunningTopology {
    inputs: HashMap<ComponentKey, BufferSender<EventArray>>,
    outputs: HashMap<OutputId, ControlChannel>,
    source_tasks: HashMap<ComponentKey, TaskHandle>,
    tasks: HashMap<ComponentKey, TaskHandle>,
    shutdown_coordinator: SourceShutdownCoordinator,
    pub config: Config,
    pub running: Arc<AtomicBool>,
    graceful_shutdown_duration: Option<Duration>,
    // ...
}
```

### Task Management

Every component runs as a Tokio task (`TaskHandle = JoinHandle<TaskResult>`):

- **Source tasks** — The main source async function, plus pump tasks that forward events.
- **Transform tasks** — For `TaskTransform`, the transform runs as its own task. For `FunctionTransform`/`SyncTransform`, processing is inlined into the input stream handler.
- **Sink tasks** — The sink's `run()` method, wrapped with buffer reading.

Tasks are spawned during topology startup and joined during shutdown.

### Inputs and Outputs

- **Inputs** — `HashMap<ComponentKey, BufferSender<EventArray>>`: the write side of each component's input buffer. Used to send events to a specific component.
- **Outputs** — `HashMap<OutputId, ControlChannel>`: control channels for each output's `Fanout`. Used to dynamically add/remove downstream consumers during reload.

---

## Fanout and Control Channels

`Fanout` at [`lib/vector-core/src/fanout.rs:45`](../lib/vector-core/src/fanout.rs#L45) distributes events from one output to multiple downstream inputs:

```
Source Output ──▸ Fanout ──▸ Transform A Input
                        ├──▸ Transform B Input
                        └──▸ Sink C Input
```

Each `Fanout` has an associated control channel that accepts `ControlMessage` variants:

- **Add** — Connect a new downstream consumer (during reload).
- **Remove** — Disconnect a downstream consumer (during reload).
- **Replace** — Swap a consumer (for seamless component replacement).

This enables zero-downtime reload: new components are connected via `Add`, and removed components are disconnected via `Remove`, all while the source keeps running.

---

## Config Reload

### ConfigDiff

Defined at [`src/config/diff.rs:9`](../src/config/diff.rs#L9):

```rust
pub struct ConfigDiff {
    pub sources: Difference,
    pub transforms: Difference,
    pub sinks: Difference,
    pub enrichment_tables: Difference,
}

pub struct Difference {
    pub to_remove: HashSet<ComponentKey>,
    pub to_change: HashSet<ComponentKey>,
    pub to_add: HashSet<ComponentKey>,
}
```

The diff is computed by comparing the old and new `Config`:

- **to_add** — Components present in the new config but not the old.
- **to_remove** — Components present in the old config but not the new.
- **to_change** — Components present in both but with different configuration.

### Reload Process

Triggered by SIGHUP signal or config file watcher. Orchestrated by [`RunningTopology::reload_config_and_respawn`](../src/topology/running.rs#L286):

1. **Validate** the new configuration (parsing, schema, graph validation).
2. **Compute** `ConfigDiff` between old and new configs.
3. **Build** new components for `to_add` and `to_change` sets via `TopologyPiecesBuilder`.
4. **Stop** removed and changed components:
   - Signal source shutdown.
   - Wait for sources to drain.
   - Disconnect from fanouts.
5. **Start** new and changed components:
   - Spawn new tasks.
   - Wire into existing fanouts via control channels.
6. **Update** the `RunningTopology` state (inputs, outputs, tasks, config).

### Rollback on Failure

If the new topology fails to build (e.g., invalid config, build error), the reload is rolled back:

- No running components are stopped.
- The old config remains active.
- A [`VectorReloadError`](../src/internal_events/process.rs#L68) event is emitted.
- The result is [`ReloadOutcome::RolledBack`](../src/topology/controller.rs#L56).

---

## Graceful Shutdown

### SourceShutdownCoordinator

Manages orderly shutdown of all sources. Each source receives a `ShutdownSignal` that it must listen for in its main loop.

When shutdown is triggered:

1. All source shutdown signals are fired.
2. Sources finish current work and exit.
3. `SourceSender` channels are closed, which propagates through the topology.

### Drain Sequence

```
1. Signal all sources to stop
2. Sources finish and close their SourceSender channels
3. Buffers drain remaining events to transforms
4. Transforms process remaining events and close their output
5. Sink buffers drain remaining events
6. Sinks finish writing and report final acknowledgements
7. All tasks complete
```

The drain has a timeout (default 60 seconds, configurable via `--graceful-shutdown-limit-secs`). If tasks don't complete within the timeout, they are forcefully dropped.

---

## Signal Handling

Handled in [`StartedApplication::run`](../src/app.rs#L291):

| Signal | Action |
|--------|--------|
| `SIGTERM` / `SIGINT` | Initiate graceful shutdown |
| `SIGHUP` | Trigger config reload |

On Windows, `Ctrl+C` is equivalent to `SIGTERM`.

The main event loop also watches for:

- Config file changes (if `--watch-config` is enabled).
- Component crashes (task panics or unexpected exits).
- API reload requests (if the API is enabled).

---

## Key Files

| File | Content |
|------|---------|
| [`src/topology/builder.rs`](../src/topology/builder.rs#L77) | `Builder`, `TopologyPiecesBuilder`, `TopologyPieces` |
| [`src/topology/running.rs`](../src/topology/running.rs#L56) | `RunningTopology`, reload, shutdown |
| [`src/topology/controller.rs`](../src/topology/controller.rs#L56) | `ReloadOutcome`, high-level reload orchestration |
| [`src/topology/schema.rs`](../src/topology/schema.rs) | Schema tracking and propagation |
| [`lib/vector-core/src/fanout.rs`](../lib/vector-core/src/fanout.rs#L45) | `Fanout`, `ControlMessage` |
| [`src/config/diff.rs`](../src/config/diff.rs#L9) | `ConfigDiff`, `Difference` |
| [`src/app.rs`](../src/app.rs#L291) | Signal handling, main event loop |

---

## Related Docs

- System overview: [`onboarding-system-overview.md`](./onboarding-system-overview.md)
- Configuration (input to topology): [`onboarding-config.md`](./onboarding-config.md)
- Buffers (managed by topology): [`onboarding-buffers-backpressure.md`](./onboarding-buffers-backpressure.md)
- Sources/Transforms/Sinks (built by topology): [`onboarding-sources.md`](./onboarding-sources.md), [`onboarding-transforms.md`](./onboarding-transforms.md), [`onboarding-sinks.md`](./onboarding-sinks.md)
