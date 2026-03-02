# Vector Onboarding Docs

This folder contains in-repo onboarding guides for engineers who need to understand and change Vector internals quickly.

Use this file as the entry point and reading plan.

If you encounter unfamiliar terms (Event, Topology, Fanout, VRL, etc.), see the [Glossary](./glossary.md).

## Existing Developer Docs (`docs/`)

Before diving into internals, review the existing developer documentation:

- [`docs/ARCHITECTURE.md`](../docs/ARCHITECTURE.md) — High-level system architecture and component wiring.
- [`docs/DEVELOPING.md`](../docs/DEVELOPING.md) — Development workflow, toolchain setup, and testing.
- [`CONTRIBUTING.md`](../CONTRIBUTING.md) — PR requirements, commit conventions, review process.
- [`STYLE.md`](../STYLE.md) — Code style rules (logging, formatting, error handling, metrics).
- [`AGENTS.md`](../AGENTS.md) — Quick reference for AI assistants and contributors.

Relationship to onboarding docs:

- `onboarding/` explains implementation internals and code navigation for contributors.
- `docs/` documents development workflow and high-level architecture.
- When changing behavior, update both when needed: `docs/` for workflow guidance, `onboarding/` for internal code navigation.

## Recommended Order

1. [`onboarding-system-overview.md`](./onboarding-system-overview.md)
2. [`onboarding-event-model.md`](./onboarding-event-model.md)
3. [`onboarding-config.md`](./onboarding-config.md)
4. [`onboarding-topology.md`](./onboarding-topology.md)
5. [`onboarding-sources.md`](./onboarding-sources.md)
6. [`onboarding-transforms.md`](./onboarding-transforms.md)
7. [`onboarding-sinks.md`](./onboarding-sinks.md)
8. [`onboarding-buffers-backpressure.md`](./onboarding-buffers-backpressure.md)
9. [`onboarding-codecs.md`](./onboarding-codecs.md)

Why this order:

- Start with the high-level architecture and data flow.
- Then learn the event model, since every component produces or consumes events.
- Then understand configuration and topology, since they wire everything together.
- Then drill into sources, transforms, and sinks to learn component development patterns.
- Finish with buffers/backpressure and codecs, which are cross-cutting libraries used by components.

## Role-Based Shortcuts

If you mostly work on:

- **Source development** (ingesting data from external systems):
  1. [`onboarding-system-overview.md`](./onboarding-system-overview.md)
  2. [`onboarding-event-model.md`](./onboarding-event-model.md)
  3. [`onboarding-sources.md`](./onboarding-sources.md)
  4. [`onboarding-codecs.md`](./onboarding-codecs.md)
  5. [`src/config/source.rs`](../src/config/source.rs#L86) — `SourceConfig` trait
  6. [`src/sources/demo_logs.rs`](../src/sources/demo_logs.rs#L40) — Example source

- **Transform development** (processing and routing events):
  1. [`onboarding-system-overview.md`](./onboarding-system-overview.md)
  2. [`onboarding-event-model.md`](./onboarding-event-model.md)
  3. [`onboarding-transforms.md`](./onboarding-transforms.md)
  4. [`src/config/transform.rs`](../src/config/transform.rs#L198) — `TransformConfig` trait
  5. [`src/transforms/filter.rs`](../src/transforms/filter.rs#L23) — Simple example
  6. [`src/transforms/remap.rs`](../src/transforms/remap.rs#L59) — Complex example (VRL)

- **Sink development** (sending data to external systems):
  1. [`onboarding-system-overview.md`](./onboarding-system-overview.md)
  2. [`onboarding-event-model.md`](./onboarding-event-model.md)
  3. [`onboarding-sinks.md`](./onboarding-sinks.md)
  4. [`onboarding-codecs.md`](./onboarding-codecs.md)
  5. [`src/config/sink.rs`](../src/config/sink.rs#L238) — `SinkConfig` trait
  6. [`src/sinks/console/config.rs`](../src/sinks/console/config.rs#L44) — Example sink

- **Topology / core runtime**:
  1. [`onboarding-system-overview.md`](./onboarding-system-overview.md)
  2. [`onboarding-topology.md`](./onboarding-topology.md)
  3. [`onboarding-buffers-backpressure.md`](./onboarding-buffers-backpressure.md)
  4. [`src/topology/builder.rs`](../src/topology/builder.rs#L77) — Topology construction
  5. [`src/topology/running.rs`](../src/topology/running.rs#L56) — Running topology management
  6. [`src/app.rs`](../src/app.rs#L54) — Application lifecycle

- **Configuration system**:
  1. [`onboarding-system-overview.md`](./onboarding-system-overview.md)
  2. [`onboarding-config.md`](./onboarding-config.md)
  3. [`src/config/mod.rs`](../src/config/mod.rs#L148) — `Config` struct
  4. [`src/config/builder.rs`](../src/config/builder.rs#L18) — `ConfigBuilder`
  5. [`src/config/loading/mod.rs`](../src/config/loading/mod.rs#L127) — Loading pipeline

## Local Dev Basics

Common commands and where they are defined:

- Build Vector:
  - `cargo build` or `make build`
- Format code:
  - `make fmt`
- Check formatting:
  - `make check-fmt`
- Run Clippy (linter):
  - `make check-clippy`
- Run unit tests:
  - `make test`
- Run integration tests:
  - `cargo vdev int show` (list available)
  - `cargo vdev int start <name>` then `cargo vdev int test <name>`
- Check component docs:
  - `make check-component-docs`
- Build with specific features only:
  - `cargo test --lib --no-default-features --features sinks-console sinks::console`

## What To Learn Next After These Docs

After finishing the onboarding docs, new engineers should:

1. **Trace a full data path from memory.**
   Pick a source (e.g., `demo_logs`), follow it through a transform (`remap`) to a sink (`console`), and identify every channel, buffer, and async boundary.

2. **Read the component specs.**
   [`docs/specs/component.md`](../docs/specs/component.md) defines naming, configuration, and health check requirements.
   [`docs/specs/instrumentation.md`](../docs/specs/instrumentation.md) defines internal event and metric naming.

3. **Run the test suite.**
   Execute `make test` and `make check-clippy` locally at least once.

4. **Build a toy component.**
   Create a minimal source or sink behind a feature flag, wire it into the config system, and write a unit test.

5. **Study VRL (Vector Remap Language).**
   VRL is the core transformation language. Start with the `remap` transform at [`src/transforms/remap.rs`](../src/transforms/remap.rs#L59).

## First-Week Learning Checklist

1. Draw the data flow from memory: Source → SourceSender → Buffer → Fanout → Transform → Buffer → Sink.
2. Explain the three transform types (Function, Sync, Task) and when to use each.
3. Explain how configuration reload works without data loss.
4. Find where backpressure is applied between components.
5. Run `make test` and `make check-clippy` locally.
6. Pick one existing source and trace the production code path from config parsing through event emission.
