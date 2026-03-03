# Vector Buffers & Backpressure — Developer Onboarding Guide

This document explains how Vector manages data flow between components using buffers, and how backpressure propagates through the pipeline to prevent data loss and memory exhaustion.

It answers:

- What buffer types are available (memory, disk) and how they are configured
- How the `WhenFull` policy controls behavior when buffers are saturated
- How channels connect components and propagate backpressure
- How acknowledgement tracking works through buffers
- Key constants that govern batch sizes and buffer capacities

For unfamiliar terms, see the [Glossary](./glossary.md).

## What Problem Do Buffers and Backpressure Solve? (Conceptual Background)

### The speed mismatch problem

In any pipeline, producers and consumers run at different speeds. A file source can read log lines at 500 MB/s, but an Elasticsearch sink might only ingest at 50 MB/s. Without buffering, either the source must slow down to match the sink (wasting source capacity), or events must be dropped (losing data).

Buffers absorb temporary speed differences:

```
Source: 100,000 events/sec ──▸ [ Buffer: 500 events ] ──▸ Sink: 80,000 events/sec

Second 1: buffer absorbs 20,000 excess events (20,000 in buffer)
Second 2: buffer absorbs 20,000 more (40,000 in buffer)
...
Second 25: buffer is full (500,000 events)  ← now what?
```

When the buffer fills, something must give. This is where backpressure policy matters.

### Backpressure: the domino effect

With the `Block` policy (the default), a full buffer causes the upstream producer to pause. This pause propagates backwards through the entire pipeline like a chain reaction:

```
Elasticsearch is slow (5,000 events/sec instead of 80,000)
    │
    ▼
Sink buffer fills up (500 events)
    │
    ▼
Fanout blocks trying to send to the full buffer
    │
    ▼
Transform can't emit output → stops pulling from its input
    │
    ▼
Source's SourceSender blocks
    │
    ▼
Source stops reading new data
    │
    ▼
External system sees slow consumption:
  - Kafka: consumer lag increases (messages queue in Kafka)
  - File: tail position falls behind (logs queue on disk)
  - HTTP: connection hangs (upstream retries or queues)
```

This is **intentional**. The alternative — continuing to consume at full speed — means either unbounded memory growth (eventually OOM-killed) or data loss (events dropped before delivery). Backpressure trades latency for reliability: events arrive late but they arrive.

### Why two buffer types?

**Memory buffers** store events as in-memory Rust objects. They're fast (no serialization, no disk I/O) but volatile — if Vector crashes, buffered events are lost:

```
Memory buffer:
  ┌──────────────────────────────────────┐
  │ EventArray EventArray EventArray ... │  ← Rust objects in heap memory
  └──────────────────────────────────────┘
  Capacity: 500 events (default)
  Latency: microseconds
  Durability: none (lost on crash)
```

**Disk buffers** serialize events to a write-ahead log on disk. They're slower but durable — events survive crashes and restarts:

```
Disk buffer:
  ┌──────────────────────────────────────┐
  │ /var/lib/vector/sinks/my_sink/       │
  │   segment_0001.dat (serialized)      │  ← Events written to disk
  │   segment_0002.dat                   │
  └──────────────────────────────────────┘
  Capacity: configurable bytes (e.g., 1 GB)
  Latency: milliseconds
  Durability: survives process crash
```

The choice depends on the use case. For metrics pipelines where losing a few data points is acceptable, memory buffers are ideal. For compliance logging where every event must be delivered, disk buffers provide the necessary durability.

### The Overflow strategy: best of both worlds

The `Overflow` policy chains two buffers — typically memory in front, disk behind:

```
Normal operation (sink keeping up):
  Source ──▸ [Memory buffer: 200/500] ──▸ Sink
              fast, low latency

Spike (sink falling behind):
  Source ──▸ [Memory buffer: 500/500 FULL] ──▸ overflow ──▸ [Disk buffer] ──▸ Sink
              memory exhausted                                durable, slower

Recovery (sink catches up):
  Disk buffer drains ──▸ memory buffer has space again ──▸ back to normal
```

This gives you memory-speed performance during normal operation and disk durability during spikes. The events in the disk overflow are not lost if Vector crashes — they're recovered on restart.

### Concrete capacity math

Understanding the constants helps size buffers correctly:

```
SourceSender channel:
  SOURCE_SENDER_BUFFER_SIZE = TRANSFORM_CONCURRENCY_LIMIT × CHUNK_SIZE
  (capacity is in EventArray items, and depends on runtime worker thread count)

Transform input channel:
  TOPOLOGY_BUFFER_SIZE = 100 EventArray items

Default memory buffer:
  500 events

Typical event size:
  ~500 bytes (structured log with 10 fields)

So a default memory buffer holds:
  500 events × 500 bytes = ~250 KB

With a disk buffer at 1 GB:
  1,073,741,824 bytes / 500 bytes = ~2 million events

Example (8 worker threads):
  SOURCE_SENDER_BUFFER_SIZE = 8 × 1000 = 8000 EventArray items
  Worst-case queued events in that channel = 8000 × 1000 = 8,000,000
```

This back-of-the-envelope math helps operators choose buffer sizes: if your Elasticsearch cluster goes down for 10 minutes and you ingest 100,000 events/sec, you need `100,000 × 600 × 500 bytes ≈ 30 GB` of disk buffer to avoid data loss. A 500-event memory sink buffer would fill in 5 milliseconds.

## Table of Contents

- [Overview](#overview)
- [Buffer Types](#buffer-types)
  - [Memory Buffer](#memory-buffer)
  - [Disk Buffer](#disk-buffer)
- [WhenFull Policy](#whenfull-policy)
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
- Bounded by `SOURCE_SENDER_BUFFER_SIZE` event arrays, where `SOURCE_SENDER_BUFFER_SIZE = TRANSFORM_CONCURRENCY_LIMIT × CHUNK_SIZE`.
- Batches events into arrays of `CHUNK_SIZE` (1000).

**Buffer channel:**
- Connects a Fanout output to a downstream transform or sink.
- Transform input channels are bounded by `TOPOLOGY_BUFFER_SIZE` (100) event arrays.
- Sink input channels are bounded by the sink's configured `buffer` settings (default memory: 500 events).
- Uses `BufferSender<EventArray>` / `BufferReceiver<EventArray>`.

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
2. Memory buffers preserve finalizers directly in memory.
3. Disk buffers persist events and use internal batch notifiers/finalizers to track when records can be acknowledged and deleted.
4. When the sink reads events from the buffer, it extracts finalizers and reports delivery status.
5. The status propagates back upstream.

For disk buffers, acknowledgement state is tracked by the buffer subsystem; original in-memory finalizer callbacks are not serialized as part of event payloads.

---

## Key Constants

| Constant | Value | Location | Purpose |
|----------|-------|----------|---------|
| `CHUNK_SIZE` | 1000 | [`lib/vector-core/src/source_sender/mod.rs:20`](../lib/vector-core/src/source_sender/mod.rs#L20) | Events per batch in SourceSender |
| `SOURCE_SENDER_BUFFER_SIZE` | `TRANSFORM_CONCURRENCY_LIMIT × CHUNK_SIZE` | [`src/topology/builder.rs:62`](../src/topology/builder.rs#L62) | SourceSender channel capacity (EventArray items) |
| `TOPOLOGY_BUFFER_SIZE` | 100 | [`src/topology/builder.rs:66`](../src/topology/builder.rs#L66) | Transform input channel capacity (EventArray items) |
| Default memory buffer | 500 events | `lib/vector-buffers/src/config.rs` | Default buffer size when not configured |

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

  overflow_sink:
    type: kafka
    buffer:
      - type: memory
        max_events: 1000
        when_full: overflow
      - type: disk
        max_size: 1073741824
        when_full: block
```

If no `buffer` section is specified, the default is a memory buffer with 500 events and `Block` policy.
`when_full: overflow` is only valid when the buffer is chained and not on the final stage.

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
