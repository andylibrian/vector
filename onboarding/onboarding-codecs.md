# Vector Codecs — Developer Onboarding Guide

This document explains how Vector's codec system works: how sources decode raw bytes into events, and how sinks encode events into bytes for delivery.

It answers:

- How the two-layer architecture (framing + format) works
- What framing options are available (newline, length-prefixed, character-delimited, etc.)
- What format codecs are available (JSON, Protobuf, Syslog, GELF, Avro, etc.)
- How `DecodingConfig` and `EncodingConfigWithFraming` are used in sources and sinks
- How the `Transformer` applies pre-encoding mutations

For unfamiliar terms, see the [Glossary](./glossary.md).

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
| `SyslogDeserializer` | RFC 3164/5424 syslog | LogEvent with structured syslog fields |
| `GelfDeserializer` | GELF JSON | LogEvent with GELF fields |
| `ProtobufDeserializer` | Protocol Buffers | LogEvent from protobuf message |
| `OtlpDeserializer` | OpenTelemetry Protocol | Logs, Metrics, or Traces |
| `AvroDeserializer` | Apache Avro | LogEvent from Avro record |
| `CsvDeserializer` | CSV rows | LogEvent with header-keyed fields |

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
