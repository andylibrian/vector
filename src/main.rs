#![deny(warnings)]

//!
//! # Vector Binary Entry Point
//!
//! This is the main entry point for the Vector binary. The boot sequence is deliberately minimal:
//!
//! 1. **Allocation tracing** (optional, Unix-only): If enabled via feature flag, we initialize memory
//!    allocation tracking before any other code runs. This must happen first to capture all allocations.
//!
//! 2. **Delegation to Application**: The heavy lifting is done by `Application::run`, which:
//!    - Parses CLI arguments
//!    - Builds the Tokio runtime
//!    - Loads configuration
//!    - Starts the topology
//!    - Runs the main event loop
//!
//! ## Why is this file so small?
//!
//! The entry point is intentionally thin. All application logic lives in `src/app.rs` and the
//! `vector` library crate. This separation allows:
//! - Better testability (we can test `Application` without re-running the binary)
//! - Clear separation between "how to start" and "what to do"
//! - Reuse of the application logic in other contexts (e.g., Windows service wrapper)
//!
//! ## Platform Differences
//!
//! - **Unix**: Direct execution via `Application::run`
//! - **Windows**: First attempts to run as a Windows service; falls back to console mode if that fails.
//!   This allows the same binary to work both as a service and as a console application.
//!
//! See `src/app.rs` for the actual application lifecycle.

extern crate vector;
use std::process::ExitCode;

use vector::{app::Application, extra_context::ExtraContext};

#[cfg(unix)]
fn main() -> ExitCode {
    // Allocation tracing is a debugging/profiling feature that tracks memory allocations.
    // It must be initialized before ANY other code runs to capture all allocations.
    // This is controlled at compile time via the `allocation-tracing` feature flag.
    #[cfg(feature = "allocation-tracing")]
    {
        use std::sync::atomic::Ordering;

        use crate::vector::internal_telemetry::allocations::{
            REPORTING_INTERVAL_MS, TRACK_ALLOCATIONS, init_allocation_tracing,
        };

        // Parse CLI args early just to get allocation tracing settings.
        // We'll parse them again later in Application::run, but that's okay.
        let opts = vector::cli::Opts::get_matches()
            .map_err(|error| {
                // Printing to stdout/err can itself fail; ignore it.
                _ = error.print();
                exitcode::USAGE
            })
            .unwrap_or_else(|code| {
                std::process::exit(code);
            });
        let allocation_tracing = opts.root.allocation_tracing;

        // Store the reporting interval before starting tracing
        REPORTING_INTERVAL_MS.store(
            opts.root.allocation_tracing_reporting_interval_ms,
            Ordering::Relaxed,
        );
        drop(opts);

        // CRITICAL ASSUMPTION: At this point, we assume the heap contains no allocations
        // with a shorter lifetime than the program. Any such allocations would be missed
        // by the tracer. In practice, this is safe because we've only done minimal work above.
        if allocation_tracing {
            // Start tracking allocations - this hooks into the global allocator
            TRACK_ALLOCATIONS.store(true, Ordering::Relaxed);
            init_allocation_tracing();
        }
    }

    // Hand off to the Application struct which orchestrates the entire Vector lifecycle.
    // See src/app.rs for what happens next.
    let exit_code = Application::run(ExtraContext::default())
        .code()
        .unwrap_or(exitcode::UNAVAILABLE) as u8;
    ExitCode::from(exit_code)
}

#[cfg(windows)]
pub fn main() -> ExitCode {
    // Windows can run Vector either as a service or as a console application.
    // We first try the service mode; if that fails (e.g., not installed as service),
    // we fall back to interactive console mode.
    // See: https://docs.microsoft.com/en-us/dotnet/api/system.environment.userinteractive
    let exit_code = vector::vector_windows::run().unwrap_or_else(|_| {
        Application::run(ExtraContext::default())
            .code()
            .unwrap_or(exitcode::UNAVAILABLE)
    });
    ExitCode::from(exit_code as u8)
}
