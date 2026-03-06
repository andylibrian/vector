//! Source sender - the mechanism for getting events from sources into the topology.
//!
//! # The Problem SourceSender Solves
//!
//! Without an abstraction, sources would need to understand:
//! - How to send events to multiple downstream consumers
//! - How to handle backpressure when buffers are full
//! - How to track event delivery for acknowledgements
//! - How to batch events for efficiency
//!
//! SourceSender encapsulates all of this, providing a simple interface for sources.
//!
//! # How It Works
//!
//! When a source produces events, it sends them through a `SourceSender`.
//! This sender handles:
//!
//! - **Batching**: Groups events into chunks for efficiency
//! - **Backpressure**: Propagates backpressure from downstream components
//! - **Latency tracking**: Tracks time from source to transform
//! - **Schema management**: Tracks schema definitions for events
//!
//! # Architecture
//!
//! ```
//! External System
//!      |
//!      v
//! +-------------+
//! |   Source    | produces events one at a time
//! +-------------+
//!      |
//!      | send_event() / send_batch()
//!      v
//! +-----------------------------------------------------+
//! |                   SourceSender                      |
//! |  Handles output routing, lag tracking, and optional |
//! |  chunking for batch/stream sends                    |
//! +-----------------------------------------------------+
//!      |
//!      | sends `SourceSenderItem` batches to output pumps
//!      v
//! +-----------------+
//! | SourceOutputPump| reads from sender
//! +-----------------+
//!      |
//!      v
//! +---------+
//! | Fanout  | distributes to transforms/sinks
//! +---------+
//! ```
//!
//! # Why 1000 Events Per Chunk?
//!
//! Channel operations have overhead: acquiring locks, waking receivers, context switches.
//! For high-throughput sources using `send_batch()` or `send_event_stream()`,
//! chunking reduces the number of channel operations significantly.
//!
//! By sending chunks up to 1000 events (`CHUNK_SIZE`) at once:
//! - Fewer channel operations
//! - Better cache locality
//! - Reduced lock contention
//!
//! Sources control this tradeoff by choosing between:
//! - `send_event()` for single events
//! - `send_batch()` / `send_event_stream()` for chunked sends
//!
//! # Multiple Outputs
//!
//! Some sources produce multiple types of output (e.g., OpenTelemetry produces
//! logs, metrics, and traces). SourceSender supports this via named outputs:
//!
//! ```ignore
//! // Source declares outputs in outputs()
//! vec![
//!     SourceOutput::new_maybe_logs(DataType::Log, schema),
//!     SourceOutput::new_maybe_metrics(DataType::Metric, schema),
//! ]
//!
//! // Source sends to named outputs
//! out.send_batch_named("logs", log_events).await?;
//! out.send_batch_named("metrics", metric_events).await?;
//! ```
//!
//! # The Builder Pattern
//!
//! A source uses `SourceSender::builder()` to declare its outputs.
//! Each output gets a channel that events are sent to. The builder
//! pattern allows sources to have multiple named outputs.
//!
//! # Event Flow
//!
//! ```
//! Source -> SourceSender -> SourceOutputPump -> Fanout -> Transforms/Sinks
//! ```
//!
//! The pump task reads from the sender and fans out to downstream consumers.

#![allow(
    missing_docs,
    clippy::missing_errors_doc,
    clippy::doc_markdown,
    clippy::missing_panics_doc
)]
mod builder;
mod errors;
mod output;
mod sender;
#[cfg(test)]
mod tests;

pub use builder::Builder;
pub use errors::SendError;
use output::Output;
pub use sender::{SourceSender, SourceSenderItem};

/// Number of events to batch together before sending through the channel.
///
/// # Why 1000?
///
/// This value was chosen through empirical testing to balance:
/// - **Throughput**: Larger batches reduce channel overhead
/// - **Latency**: Smaller batches mean events flow through faster
/// - **Memory**: Large batches use more memory in buffers
///
/// At 1000 events:
/// - High-throughput batch/stream sends can greatly reduce send overhead
/// - Each chunk can still be processed incrementally downstream
/// - Memory overhead remains practical for typical event sizes
///
/// This is a tuning parameter - increasing it may help very high-throughput
/// sources, but most workloads won't notice the difference.
pub const CHUNK_SIZE: usize = 1000;

#[cfg(any(test, feature = "test"))]
const TEST_BUFFER_SIZE: usize = 100;

/// Name of the metric for tracking source lag time.
const LAG_TIME_NAME: &str = "source_lag_time_seconds";
