//! Source sender - the mechanism for getting events from sources into the topology.
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
//! A source creates a `SourceSender::Builder` to declare its outputs.
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

/// Number of events to batch together.
pub const CHUNK_SIZE: usize = 1000;

#[cfg(any(test, feature = "test"))]
const TEST_BUFFER_SIZE: usize = 100;

/// Name of the metric for tracking source lag time.
const LAG_TIME_NAME: &str = "source_lag_time_seconds";
