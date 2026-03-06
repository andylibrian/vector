//! Builder for constructing SourceSender instances.
//!
//! # Why Use a Builder?
//!
//! SourceSender needs to be configured with outputs before use. The builder
//! pattern provides a clean API for:
//!
//! 1. Setting buffer sizes
//! 2. Configuring timeouts
//! 3. Adding named outputs
//! 4. Building the final SourceSender
//!
//! # Usage Pattern
//!
//! The topology system uses this builder when setting up sources:
//!
//! ```ignore
//! let mut builder = SourceSender::builder()
//!     .with_buffer(1000)
//!     .with_timeout(Some(Duration::from_secs(30)));
//!
//! // Add an output for each declared SourceOutput
//! for output in source_config.outputs(log_namespace) {
//!     builder.add_source_output(output, component_key);
//! }
//!
//! let sender = builder.build();
//! ```

use std::{collections::HashMap, time::Duration};

use metrics::{Histogram, histogram};
use vector_buffers::topology::channel::LimitedReceiver;
use vector_common::internal_event::DEFAULT_OUTPUT;

use super::{CHUNK_SIZE, LAG_TIME_NAME, Output, SourceSender, SourceSenderItem};
use crate::config::{ComponentKey, OutputId, SourceOutput};

/// Builder for constructing SourceSender instances.
///
/// # Fields
///
/// - `buf_size`: Maximum events in the buffer before backpressure kicks in
/// - `default_output`: The unnamed output (most sources use this)
/// - `named_outputs`: Named outputs for multi-output sources
/// - `lag_time`: Histogram for tracking event lag
/// - `timeout`: Optional send timeout
pub struct Builder {
    buf_size: usize,
    default_output: Option<Output>,
    named_outputs: HashMap<String, Output>,
    lag_time: Option<Histogram>,
    timeout: Option<Duration>,
    ewma_half_life_seconds: Option<f64>,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            // Default buffer size matches CHUNK_SIZE for optimal batching
            buf_size: CHUNK_SIZE,
            default_output: None,
            named_outputs: Default::default(),
            // Enable lag time tracking by default
            lag_time: Some(histogram!(LAG_TIME_NAME)),
            timeout: None,
            ewma_half_life_seconds: None,
        }
    }
}

impl Builder {
    /// Set a custom buffer size.
    ///
    /// # When to Increase Buffer Size
    ///
    /// Increase the buffer size when:
    /// - The source produces bursts of events
    /// - Downstream processing has variable latency
    /// - You want to absorb temporary backpressure
    ///
    /// # Trade-offs
    ///
    /// Larger buffers:
    /// - Use more memory
    /// - Increase event latency during backpressure
    /// - Allow sources to continue longer during downstream slowdowns
    #[must_use]
    pub fn with_buffer(mut self, n: usize) -> Self {
        self.buf_size = n;
        self
    }

    /// Set a send timeout.
    ///
    /// # When to Use Timeouts
    ///
    /// Timeouts prevent sources from blocking indefinitely when downstream
    /// is slow or stuck. This is useful for:
    /// - Sources that need to remain responsive (e.g., HTTP servers)
    /// - Preventing memory exhaustion from buffered events
    /// - Failing fast during downstream outages
    ///
    /// When a timeout fires:
    /// - The source receives `SendError::Timeout`
    /// - Events are counted as "timed out" in telemetry
    /// - The source can decide whether to retry or drop
    #[must_use]
    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the EWMA half-life used for buffer utilization metrics.
    ///
    /// This controls smoothing for the `source_buffer_utilization_mean` gauge.
    #[must_use]
    pub fn with_ewma_half_life_seconds(mut self, half_life_seconds: Option<f64>) -> Self {
        self.ewma_half_life_seconds = half_life_seconds;
        self
    }

    /// Add a source output, creating the channel for it.
    ///
    /// # How This Works
    ///
    /// When the topology is being built, it calls this method for each
    /// output declared by the source. This creates:
    ///
    /// 1. A bounded channel with backpressure
    /// 2. An `Output` wrapper (stored in the builder)
    /// 3. A `LimitedReceiver` (returned to the topology)
    ///
    /// The receiver is connected to the "output pump" task, which reads
    /// events and fans them out to downstream components.
    ///
    /// # Named vs Default Outputs
    ///
    /// - If `output.port` is `None`: Creates the default output
    /// - If `output.port` is `Some(name)`: Creates a named output
    ///
    /// Most sources only have a default output. Multi-output sources
    /// (like `opentelemetry`) have multiple named outputs.
    pub fn add_source_output(
        &mut self,
        output: SourceOutput,
        component_key: ComponentKey,
    ) -> LimitedReceiver<SourceSenderItem> {
        let lag_time = self.lag_time.clone();
        let log_definition = output.schema_definition.clone();
        let output_id = OutputId {
            component: component_key,
            port: output.port.clone(),
        };
        match output.port {
            // Default output (unnamed) - used by most sources
            None => {
                let (output, rx) = Output::new_with_buffer(
                    self.buf_size,
                    DEFAULT_OUTPUT.to_owned(),
                    lag_time,
                    log_definition,
                    output_id,
                    self.timeout,
                    self.ewma_half_life_seconds,
                );
                self.default_output = Some(output);
                rx
            }
            // Named output - for multi-output sources
            Some(name) => {
                let (output, rx) = Output::new_with_buffer(
                    self.buf_size,
                    name.clone(),
                    lag_time,
                    log_definition,
                    output_id,
                    self.timeout,
                    self.ewma_half_life_seconds,
                );
                self.named_outputs.insert(name, output);
                rx
            }
        }
    }

    /// Build the final SourceSender.
    ///
    /// After adding all outputs, call this to create the SourceSender
    /// that will be passed to the source's `build()` function.
    pub fn build(self) -> SourceSender {
        SourceSender {
            default_output: self.default_output,
            named_outputs: self.named_outputs,
        }
    }
}
