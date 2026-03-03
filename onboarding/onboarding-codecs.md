# Vector Codecs — Developer Onboarding Guide

This document explains how Vector's codec system works: how sources decode raw bytes into events, and how sinks encode events into bytes for delivery.

It answers:

- How the two-layer architecture (framing + format) works
- What framing options are available (newline, length-prefixed, character-delimited, etc.)
- What format codecs are available (JSON, Protobuf, Syslog, GELF, Avro, etc.)
- How `DecodingConfig` and `EncodingConfigWithFraming` are used in sources and sinks
- How the `Transformer` applies pre-encoding mutations

For unfamiliar terms, see the [Glossary](./glossary.md).

## What Problem Does the Codec System Solve? (Conceptual Background)

### The format explosion problem

Vector has 47+ sources and 56+ sinks, each potentially dealing with different data formats. A syslog source receives RFC 5424 messages. An HTTP source might receive JSON, Protobuf, or plain text. A Kafka source can receive anything — the format depends on the producer.

Without a composable codec system, each source and sink would contain its own parsing and serialization code. Adding a new format would mean modifying every component. With 100+ components and 15+ formats, that's thousands of format-specific code paths.

### The two-layer insight: framing and format are independent

The key insight is that byte stream handling has two orthogonal concerns:

1. **Framing** — "Where does one message end and the next begin?"
2. **Format** — "What does each message mean?"

These are independent. Newline-delimited JSON and newline-delimited syslog use the same framing (split on `\n`) but different formats. Length-prefixed JSON and length-prefixed Protobuf use the same framing (read 4-byte length, then that many bytes) but different formats.

By separating them, Vector can mix and match:

```
Framing              Format              Result
───────              ──────              ──────
Newline-delimited  × JSON             = NDJSON (very common)
Newline-delimited  × Syslog           = syslog-over-TCP
Length-prefixed    × Protobuf         = gRPC-style binary protocol
Newline-delimited  × Bytes (raw)      = plain log lines
Octet-counting     × Syslog           = RFC 5425 syslog
Character('\0')    × JSON             = null-delimited JSON
```

Adding a new format (e.g., CBOR) means implementing one deserializer and one serializer. It immediately works with all 7 framing types, giving 7 new combinations for free. Adding a new framing type gives it access to all 15+ formats.

### How decoding works: a concrete example

Consider a TCP source receiving newline-delimited JSON logs:

```
Raw bytes on the wire:
  b'{"level":"info","msg":"request started"}\n{"level":"error","msg":"disk full"}\n'

Step 1 — Framing (NewlineDelimitedDecoder):
  Split on \n:
    message1: b'{"level":"info","msg":"request started"}'
    message2: b'{"level":"error","msg":"disk full"}'

Step 2 — Format (JsonDeserializer):
  Parse JSON into LogEvent:
    event1: LogEvent { "level": "info",  "msg": "request started" }
    event2: LogEvent { "level": "error", "msg": "disk full" }
```

Now consider the same source receiving length-prefixed Protobuf instead. The source code doesn't change — only the config:

```yaml
# Before                              # After
decoding:                              decoding:
  codec: json                            codec: protobuf
framing:                                 schema: my_schema.proto
  method: newline_delimited            framing:
                                         method: length_delimited
```

The source's main loop doesn't care — it calls `decoder.decode(bytes)` and gets `Event` values either way. The decoder is constructed from config at build time, and the source treats it as a black box.

### How encoding works: the three-stage pipeline

Sinks encode events to bytes through three stages, each handling a distinct concern:

```
Input event:
  { "message": "disk full", "host": "web-1", "password": "secret", "timestamp": "2024-01-15T10:30:00Z" }

Stage 1 — Transformer (configured by user):
  encoding.except_fields: ["password"]
  encoding.timestamp_format: unix
  Result: { "message": "disk full", "host": "web-1", "timestamp": 1705312200 }

Stage 2 — Serializer (JSON):
  Result: b'{"message":"disk full","host":"web-1","timestamp":1705312200}'

Stage 3 — Framer (newline-delimited):
  Result: b'{"message":"disk full","host":"web-1","timestamp":1705312200}\n'
```

The Transformer stage is crucial for data governance. It lets operators strip sensitive fields, control timestamp formats, and select specific fields — all without modifying application code or writing VRL transforms. A single `except_fields: ["password", "ssn", "credit_card"]` in the sink config prevents sensitive data from reaching the destination.

### Why `Decoder` implements `tokio_util::codec::Decoder`

Many sources read from TCP/UDP sockets using Tokio's `Framed` adapter, which wraps an `AsyncRead` stream with a codec. By implementing the standard `tokio_util::codec::Decoder` trait, Vector's `Decoder` plugs directly into Tokio's streaming infrastructure:

```rust
let framed = FramedRead::new(tcp_stream, decoder);
while let Some(result) = framed.next().await {
    let (events, byte_size) = result?;
    out.send_batch(events).await?;
}
```

This single pattern handles all framing and format combinations. The `Framed` adapter manages the internal byte buffer, calling the decoder whenever new bytes arrive. The decoder extracts one message via the framing layer, parses it via the format layer, and returns the resulting events. No source needs to manage byte buffers or partial reads manually.

## Table of Contents

- [Overview](#overview)
- [Two-Layer Architecture](#two-layer-architecture)
- [Framing Layer](#framing-layer)
  - [Framing Decoders](#framing-decoders)
  - [Framing Encoders](#framing-encoders)
- [Format Layer](#format-layer)
  - [Deserializers (Sources)](#deserializers-sources)
  - [Serializers (Sinks)](#serializers-sinks)
- [Decoder (Sources)](#decoder-sources)
- [Encoder (Sinks)](#encoder-sinks)
- [Transformer](#transformer)
- [Usage in Sources](#usage-in-sources)
- [Usage in Sinks](#usage-in-sinks)
- [Key Files](#key-files)

---

## Overview

Vector's codec library at [`lib/codecs/`](../lib/codecs/) handles the conversion between raw bytes and structured events. It is used by sources (bytes → events) and sinks (events → bytes).

The codec system is **composable**: framing and format are configured independently, so you can combine any framing with any format (e.g., length-prefixed Protobuf, newline-delimited JSON, etc.).

---

## Two-Layer Architecture

```
                     DECODING (Sources)
Raw Bytes ──▸ [ Framing Decoder ] ──▸ Message Bytes ──▸ [ Format Deserializer ] ──▸ Event

                     ENCODING (Sinks)
Event ──▸ [ Transformer ] ──▸ [ Format Serializer ] ──▸ Message Bytes ──▸ [ Framing Encoder ] ──▸ Raw Bytes
```

1. **Framing** — Splits a byte stream into individual messages (decoding) or wraps messages with delimiters/lengths (encoding).
2. **Format** — Converts between a single message's bytes and one or more `Event` values.

For sinks, there is an additional **Transformer** stage that applies field filtering and timestamp formatting before serialization.

---

## Framing Layer

### Framing Decoders

Split a continuous byte stream into discrete messages. Located in [`lib/codecs/src/decoding/framing/`](../lib/codecs/src/decoding/framing/):

| Decoder | Description | Use Case |
|---------|-------------|----------|
| `NewlineDelimitedDecoder` | Split on `\n` | Log lines, NDJSON |
| `CharacterDelimitedDecoder` | Split on a configurable character | Custom delimiters |
| `LengthDelimitedDecoder` | Read 4-byte big-endian length prefix, then that many bytes | Binary protocols |
| `OctetCountingDecoder` | Syslog octet counting framing (RFC 5425) | Syslog over TCP |
| `BytesDecoder` | Pass through raw bytes as-is | Single-message protocols |
| `ChunkedGelfDecoder` | GELF chunked UDP reassembly | Graylog sources |
| `VarintLengthDelimitedDecoder` | Varint-encoded length prefix | Protobuf streams |

### Framing Encoders

Wrap serialized messages for output. Located in [`lib/codecs/src/encoding/framing/`](../lib/codecs/src/encoding/framing/):

| Encoder | Description |
|---------|-------------|
| `NewlineDelimitedEncoder` | Append `\n` after each message |
| `CharacterDelimitedEncoder` | Append a configurable character |
| `LengthDelimitedEncoder` | Prepend 4-byte big-endian length |
| `BytesEncoder` | No framing (raw bytes) |
| `VarintLengthDelimitedEncoder` | Prepend varint-encoded length |

---

## Format Layer

### Deserializers (Sources)

Convert message bytes to events. Located in [`lib/codecs/src/decoding/format/`](../lib/codecs/src/decoding/format/):

| Deserializer | Input Format | Output |
|-------------|-------------|--------|
| `BytesDeserializer` | Raw bytes | Single LogEvent with `message` field |
| `JsonDeserializer` | JSON object | LogEvent with parsed fields |
| `NativeJsonDeserializer` | Vector's native JSON format | Preserves Event type (Log/Metric/Trace) |
| `SyslogDeserializer`* | RFC 3164/5424 syslog | LogEvent with structured syslog fields |
| `GelfDeserializer` | GELF JSON | LogEvent with GELF fields |
| `ProtobufDeserializer` | Protocol Buffers | LogEvent from protobuf message |
| `OtlpDeserializer`* | OpenTelemetry Protocol | Logs, Metrics, or Traces |
| `AvroDeserializer` | Apache Avro | LogEvent from Avro record |
| `InfluxdbDeserializer` | InfluxDB line protocol | Metric events |
| `VrlDeserializer` | VRL program output | LogEvent values derived by VRL |

\* Feature-gated (`syslog` / `opentelemetry`).

### Serializers (Sinks)

Convert events to message bytes. Located in [`lib/codecs/src/encoding/format/`](../lib/codecs/src/encoding/format/):

| Serializer | Output Format |
|-----------|---------------|
| `JsonSerializer` | JSON object |
| `TextSerializer` | Plain text (event's `message` field) |
| `NativeJsonSerializer` | Vector's native JSON (preserves type) |
| `ProtobufSerializer` | Protocol Buffers |
| `SyslogSerializer` | RFC 5424 syslog |
| `GelfSerializer` | GELF JSON |
| `OtlpSerializer` | OpenTelemetry Protocol |
| `AvroSerializer` | Apache Avro |
| `CsvSerializer` | CSV rows |
| `LogfmtSerializer` | logfmt key=value pairs |
| `NativeSerializer` | Vector's native binary format |
| `CefSerializer` | Common Event Format |
| `RawMessageSerializer` | Raw bytes from event |

---

## Decoder (Sources)

The combined framing + format decoder, defined at [`lib/codecs/src/decoding/decoder.rs:19`](../lib/codecs/src/decoding/decoder.rs#L19):

```rust
pub struct Decoder {
    framer: Framer,
    deserializer: Deserializer,
    log_namespace: LogNamespace,
}
```

The decoder is typically used as a `tokio_util::codec::Decoder` for streaming byte sources, or called directly for message-at-a-time sources.

It implements `tokio_util::codec::Decoder<Item = (SmallVec<[Event; 1]>, usize)>`:
- `Item` is a vector of events (some formats produce multiple events per message) and the byte count consumed.

---

## Encoder (Sinks)

The combined framing + format encoder, defined at [`lib/codecs/src/encoding/encoder.rs:83`](../lib/codecs/src/encoding/encoder.rs#L83):

```rust
pub struct Encoder<Framer> {
    framer: Framer,
    serializer: Serializer,
}
```

Usage:

```rust
let mut bytes = BytesMut::new();
encoder.encode(event, &mut bytes)?;
// `bytes` now contains the framed, serialized event
```

There is also a `BatchEncoder` at [line 23](../lib/codecs/src/encoding/encoder.rs#L23) for encoding multiple events into a single byte buffer (used by batch-oriented sinks).

---

## Transformer

Pre-encoding event mutation, applied before serialization in sinks. Configured via the `encoding` section:

```yaml
sinks:
  my_sink:
    encoding:
      codec: json
      only_fields: ["message", "host", "timestamp"]    # Include only these fields
      except_fields: ["password", "secret"]              # Exclude these fields
      timestamp_format: rfc3339                          # Timestamp rendering
```

The `Transformer` struct applies these mutations to each event before it reaches the serializer.

---

## Usage in Sources

Sources that accept arbitrary input formats use `DecodingConfig`:

```rust
#[configurable_component(source("my_source", "..."))]
pub struct MySourceConfig {
    #[configurable(derived)]
    #[serde(default)]
    decoding: DecodingConfig,     // Format: json, bytes, syslog, etc.

    #[configurable(derived)]
    #[serde(default)]
    framing: FramingConfig,       // Framing: newline, length-delimited, etc.
}
```

At build time:

```rust
let decoder = DecodingConfig::new(framing, decoding, log_namespace).build()?;
```

The decoder is then used in the source's main loop to convert incoming bytes to events.

---

## Usage in Sinks

Sinks use `EncodingConfigWithFraming`:

```rust
#[configurable_component(sink("my_sink", "..."))]
pub struct MySinkConfig {
    #[serde(flatten)]
    pub encoding: EncodingConfigWithFraming,
}
```

At build time:

```rust
let transformer = self.encoding.transformer();
let (framer, serializer) = self.encoding.build(SinkType::StreamBased)?;
let encoder = Encoder::<Framer>::new(framer, serializer);
```

During event processing:

```rust
transformer.transform(&mut event);
let mut bytes = BytesMut::new();
encoder.encode(event, &mut bytes)?;
// Send `bytes` to the destination
```

---

## Key Files

| File | Content |
|------|---------|
| [`lib/codecs/src/decoding/decoder.rs`](../lib/codecs/src/decoding/decoder.rs#L19) | `Decoder` struct |
| [`lib/codecs/src/encoding/encoder.rs`](../lib/codecs/src/encoding/encoder.rs#L83) | `Encoder`, `BatchEncoder` |
| [`lib/codecs/src/decoding/framing/`](../lib/codecs/src/decoding/framing/) | Framing decoders |
| [`lib/codecs/src/encoding/framing/`](../lib/codecs/src/encoding/framing/) | Framing encoders |
| [`lib/codecs/src/decoding/format/`](../lib/codecs/src/decoding/format/) | Format deserializers |
| [`lib/codecs/src/encoding/format/`](../lib/codecs/src/encoding/format/) | Format serializers |
| [`lib/codecs/src/decoding/config.rs`](../lib/codecs/src/decoding/config.rs) | `DecodingConfig` |
| [`lib/codecs/src/encoding/config.rs`](../lib/codecs/src/encoding/config.rs) | `EncodingConfigWithFraming` |

---

## Related Docs

- Sources (use decoders): [`onboarding-sources.md`](./onboarding-sources.md)
- Sinks (use encoders): [`onboarding-sinks.md`](./onboarding-sinks.md)
- Event model (what codecs produce/consume): [`onboarding-event-model.md`](./onboarding-event-model.md)
