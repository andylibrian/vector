# Vector Buffers & Backpressure — Developer Onboarding Guide

This document explains how Vector manages data flow between components using buffers, and how backpressure propagates through the pipeline to prevent data loss and memory exhaustion.

It answers:

- What buffer types are available (memory, disk) and how they are configured
- How the `WhenFull` policy controls behavior when buffers are saturated
- How channels connect components and propagate backpressure
- How acknowledgement tracking works through buffers
- Key constants that govern batch sizes and buffer capacities

For unfamiliar terms, see the [Glossary](./glossary.md).

## Table of Contents

- [Overview](#overview)
- [Buffer Types](#buffer-types)
  - [Memory Buffer](#memory-buffer)
  - [Disk Buffer](#disk-buffer)
- [WhenFull Policy](#when-full-policy)
- [Channel Topology](#channel-topology)
- [Backpressure Propagation](#backpressure-propagation)
- [Acknowledgement Tracking](#acknowledgement-tracking)
- [Key Constants](#key-constants)
- [Configuration](#configuration)
- [Key Files](#key-files)

---

## Overview

Every sink in Vector has a buffer between it and its upstream components. Buffers serve two purposes:

1. **Absorb load spikes** — Temporary bursts of data are buffered so sinks can process at their own pace.
2. **Propagate backpressure** — When a buffer is full, upstream components slow down or drop events, preventing unbounded memory growth.

```
Transform ──▸ BufferSender ──▸ [ Buffer (memory or disk) ] ──▸ BufferReceiver ──▸ Sink
                    │                                                               │
                    ◂──────────── Backpressure (Block) ─────────────────────────────┘
```

Buffers are configured per-sink in the YAML config under the `buffer` key.

---

## Buffer Types

Defined at [`lib/vector-buffers/src/config.rs:216`](../lib/vector-buffers/src/config.rs#L216):

```rust
pub enum BufferType {
    Memory {
        max_events: NonZeroUsize,    // Maximum events in the buffer
        when_full: WhenFull,
    },
    DiskV2 {
        max_size: NonZeroU64,        // Maximum bytes on disk
        when_full: WhenFull,
    },
}
```

### Memory Buffer

- Stores events in an in-memory channel bounded by event count.
- Default capacity: 500 events.
- Fast and zero-copy — events stay as `EventArray` in memory.
- Lost on process crash (no durability).
- Best for: most use cases where durability isn't required.

### Disk Buffer

- Stores events on disk, bounded by total byte size.
- Survives process restarts — events are persisted and recovered.
- Higher latency than memory (disk I/O).
- Uses a write-ahead log format for sequential writes.
- Best for: critical pipelines where data loss is unacceptable.

Disk buffers are stored under Vector's data directory (`--data-dir` or `/var/lib/vector/` by default), in a subdirectory named after the sink component.

---

## WhenFull Policy

Defined at [`lib/vector-buffers/src/lib.rs:45`](../lib/vector-buffers/src/lib.rs#L45):

```rust
pub enum WhenFull {
    Block,
    DropNewest,
    Overflow,
}
```

| Policy | Behavior | Use When |
|--------|----------|----------|
| `Block` (default) | Upstream producers pause until space is available | Data loss is unacceptable; you prefer slowdown over dropping |
| `DropNewest` | New incoming events are dropped when the buffer is full | You prefer throughput over completeness; losing some data is acceptable |
| `Overflow` | Overflow to a secondary buffer (e.g., memory → disk) | You want memory speed with disk durability as a fallback |

`Block` is the safest default — it propagates backpressure upstream, slowing down sources when sinks can't keep up. This prevents data loss but can cause sources to buffer in their external systems (e.g., Kafka consumer lag).

`DropNewest` is useful for high-volume, best-effort pipelines where dropping some events is acceptable to maintain throughput.

---

## Channel Topology

Components are connected via async channels:

```
Source ──▸ SourceSender channel ──▸ Fanout ──▸ Buffer channel ──▸ Transform
                                          └──▸ Buffer channel ──▸ Sink
```

**SourceSender channel:**
- Connects a source to its `Fanout`.
- Bounded by `TOPOLOGY_BUFFER_SIZE` (100) event arrays.
- Batches events into arrays of `CHUNK_SIZE` (1000).

**Buffer channel:**
- Connects a Fanout output to a transform or sink input.
- Bounded by the sink's buffer config (default 500 events for memory).
- `BufferSender<EventArray>` / `BufferReceiver<EventArray>`.

**Fanout:**
- Does not buffer — it distributes events synchronously to all connected consumers.
- If any consumer's channel is full (and using `Block`), the Fanout blocks, which blocks the producer.

---

## Backpressure Propagation

With the `Block` policy, backpressure cascades upstream:

```
1. Sink is slow (e.g., Elasticsearch under load)
   ↓
2. Sink's buffer fills up
   ↓
3. BufferSender blocks (async await on send)
   ↓
4. Fanout blocks (waiting for slow consumer)
   ↓
5. Transform's output blocks
   ↓
6. Transform stops pulling from its input
   ↓
7. Upstream buffer fills up
   ↓
8. SourceSender blocks
   ↓
9. Source slows down (stops accepting new data)
```

This is the **intended behavior** — Vector slows down ingestion when the pipeline is saturated rather than consuming unbounded memory.

With `DropNewest`, step 3 drops the event instead of blocking, so backpressure does not propagate. The upstream components continue at full speed, but data is lost.

---

## Acknowledgement Tracking

Buffers participate in the acknowledgement system:

1. Events enter the buffer with their `EventFinalizers` intact.
2. The buffer preserves finalizers through storage (memory or disk).
3. When the sink reads events from the buffer, it extracts finalizers.
4. The sink reports delivery status via the finalizers.
5. The status propagates back to the source.

For disk buffers, finalizer metadata is serialized alongside event data, so acknowledgements survive process restarts.

---

## Key Constants

| Constant | Value | Location | Purpose |
|----------|-------|----------|---------|
| `CHUNK_SIZE` | 1000 | [`lib/vector-core/src/source_sender/mod.rs:20`](../lib/vector-core/src/source_sender/mod.rs#L20) | Events per batch in SourceSender |
| `TOPOLOGY_BUFFER_SIZE` | 100 | `src/topology/builder.rs` | EventArray capacity of inter-component channels |
| Default memory buffer | 500 events | `lib/vector-buffers/src/config.rs` | Default buffer size when not configured |

Effective capacity of the SourceSender channel: `TOPOLOGY_BUFFER_SIZE × CHUNK_SIZE` = 100,000 events.

---

## Configuration

Buffers are configured per-sink:

```yaml
sinks:
  my_sink:
    type: elasticsearch
    buffer:
      type: memory          # "memory" (default) or "disk"
      max_events: 1000      # For memory buffers
      when_full: block      # "block" (default) or "drop_newest"

  durable_sink:
    type: http
    buffer:
      type: disk
      max_size: 1073741824  # 1 GB max disk usage
      when_full: block
```

If no `buffer` section is specified, the default is a memory buffer with 500 events and `Block` policy.

---

## Key Files

| File | Content |
|------|---------|
| [`lib/vector-buffers/src/config.rs`](../lib/vector-buffers/src/config.rs#L216) | `BufferType` enum, buffer configuration |
| [`lib/vector-buffers/src/lib.rs`](../lib/vector-buffers/src/lib.rs#L45) | `WhenFull` enum |
| [`lib/vector-buffers/src/topology/`](../lib/vector-buffers/src/topology/) | Buffer topology building, ack tracking |
| [`lib/vector-core/src/source_sender/`](../lib/vector-core/src/source_sender/) | SourceSender channel, `CHUNK_SIZE` |
| [`lib/vector-core/src/fanout.rs`](../lib/vector-core/src/fanout.rs#L45) | Fanout distribution |
| [`src/topology/builder.rs`](../src/topology/builder.rs#L77) | Buffer creation during topology build |

---

## Related Docs

- System overview: [`onboarding-system-overview.md`](./onboarding-system-overview.md)
- Topology (creates buffers): [`onboarding-topology.md`](./onboarding-topology.md)
- Sources (upstream of buffers): [`onboarding-sources.md`](./onboarding-sources.md)
- Sinks (downstream of buffers): [`onboarding-sinks.md`](./onboarding-sinks.md)
