//! Command-line interface definition for Vector.
//!
//! This module defines all CLI flags, options, and subcommands using the Clap derive macro system.
//! The CLI is the primary entry point for configuring and controlling Vector's runtime behavior.
//!
//! # Architecture Overview
//!
//! The CLI is structured as a two-level command hierarchy:
//! - **Root options**: Global flags that apply to the main `vector` run command (config paths,
//!   logging, threading, etc.)
//! - **Subcommands**: Specialized commands like `validate`, `test`, `graph`, `top`, etc.
//!
//! # Key Design Decisions
//!
//! ## Log Level Logic
//!
//! The `log_level()` method implements a nuanced approach to verbosity:
//! - For utility subcommands (validate, graph, list, test, etc.), we adjust the effective
//!   verbosity level to reduce noise - either adding 1 to quiet or subtracting 1 from verbose.
//! - This ensures these commands produce cleaner output by default.
//!
//! ## Configuration Path Flexibility
//!
//! Vector supports multiple ways to specify configuration:
//! - `--config`: Format auto-detected from file extension
//! - `--config-toml/json/yaml`: Explicit format specification
//! - `--config-dir`: Load all config files from a directory
//!
//! This flexibility is crucial for complex deployments where configs may be split across
//! multiple files or managed by external systems.

#![allow(missing_docs)]

use std::{num::NonZeroU64, path::PathBuf};

use clap::{ArgAction, CommandFactory, FromArgMatches, Parser};

#[cfg(windows)]
use crate::service;
#[cfg(feature = "api-client")]
use crate::tap;
#[cfg(feature = "top")]
use crate::top;

use crate::{
    completion, config, convert_config, generate, generate_schema, get_version, graph, list,
    signal, unit_test, validate,
};

#[derive(Parser, Debug)]
#[command(rename_all = "kebab-case")]
pub struct Opts {
    #[command(flatten)]
    pub root: RootOpts,

    #[command(subcommand)]
    pub sub_command: Option<SubCommand>,
}

impl Opts {
    pub fn get_matches() -> Result<Self, clap::Error> {
        let version = get_version();
        let app = Opts::command().version(version);
        Opts::from_arg_matches(&app.get_matches())
    }

    /// Determines the effective log level based on quiet/verbose flags.
    ///
    /// This implements a priority system where `--quiet` overrides `--verbose`.
    /// For utility subcommands (validate, graph, etc.), we shift the effective level
    /// to reduce log noise during their output.
    pub const fn log_level(&self) -> &'static str {
        let (quiet_level, verbose_level) = match self.sub_command {
            Some(SubCommand::Validate(_))
            | Some(SubCommand::Graph(_))
            | Some(SubCommand::Generate(_))
            | Some(SubCommand::ConvertConfig(_))
            | Some(SubCommand::List(_))
            | Some(SubCommand::Test(_)) => {
                if self.root.verbose == 0 {
                    (self.root.quiet + 1, self.root.verbose)
                } else {
                    (self.root.quiet, self.root.verbose - 1)
                }
            }
            _ => (self.root.quiet, self.root.verbose),
        };
        match quiet_level {
            0 => match verbose_level {
                0 => "info",
                1 => "debug",
                2..=255 => "trace",
            },
            1 => "warn",
            2 => "error",
            3..=255 => "off",
        }
    }
}

/// Root-level CLI options that control Vector's core behavior.
///
/// These options are available for the main `vector` run command and affect
/// how Vector loads configuration, manages logging, and handles the runtime.
#[derive(Parser, Debug)]
#[command(rename_all = "kebab-case")]
pub struct RootOpts {
    /// Read configuration from one or more files. Wildcard paths are supported.
    /// File format is detected from the file name.
    /// If zero files are specified, the deprecated default config path
    /// `/etc/vector/vector.yaml` is targeted.
    #[arg(
        id = "config",
        short,
        long,
        env = "VECTOR_CONFIG",
        value_delimiter(',')
    )]
    pub config_paths: Vec<PathBuf>,

    /// Read configuration from files in one or more directories.
    /// File format is detected from the file name.
    ///
    /// Files not ending in .toml, .json, .yaml, or .yml will be ignored.
    #[arg(
        id = "config-dir",
        short = 'C',
        long,
        env = "VECTOR_CONFIG_DIR",
        value_delimiter(',')
    )]
    pub config_dirs: Vec<PathBuf>,

    /// Read configuration from one or more files. Wildcard paths are supported.
    /// TOML file format is expected.
    #[arg(
        id = "config-toml",
        long,
        env = "VECTOR_CONFIG_TOML",
        value_delimiter(',')
    )]
    pub config_paths_toml: Vec<PathBuf>,

    /// Read configuration from one or more files. Wildcard paths are supported.
    /// JSON file format is expected.
    #[arg(
        id = "config-json",
        long,
        env = "VECTOR_CONFIG_JSON",
        value_delimiter(',')
    )]
    pub config_paths_json: Vec<PathBuf>,

    /// Read configuration from one or more files. Wildcard paths are supported.
    /// YAML file format is expected.
    #[arg(
        id = "config-yaml",
        long,
        env = "VECTOR_CONFIG_YAML",
        value_delimiter(',')
    )]
    pub config_paths_yaml: Vec<PathBuf>,

    /// Exit on startup if any sinks fail healthchecks.
    ///
    /// This is crucial for fail-fast behavior in production - if a sink's
    /// downstream service is unavailable, Vector will refuse to start rather
    /// than silently dropping data.
    #[arg(short, long, env = "VECTOR_REQUIRE_HEALTHY")]
    pub require_healthy: Option<bool>,

    /// Number of threads to use for processing (default is number of available cores).
    ///
    /// This controls the size of the Tokio runtime's thread pool. For I/O-bound
    /// workloads, fewer threads may be sufficient. For CPU-intensive transforms,
    /// matching core count is usually optimal.
    #[arg(short, long, env = "VECTOR_THREADS")]
    pub threads: Option<usize>,

    /// Enable more detailed internal logging. Repeat to increase level. Overridden by `--quiet`.
    #[arg(short, long, action = ArgAction::Count)]
    pub verbose: u8,

    /// Reduce detail of internal logging. Repeat to reduce further. Overrides `--verbose`.
    #[arg(short, long, action = ArgAction::Count)]
    pub quiet: u8,

    /// Disable interpolation of environment variables in configuration files.
    ///
    /// By default, Vector supports ${ENV_VAR} syntax in config files. This flag
    /// disables that for security or debugging purposes.
    #[arg(
        long,
        env = "VECTOR_DISABLE_ENV_VAR_INTERPOLATION",
        default_value = "false"
    )]
    pub disable_env_var_interpolation: bool,

    /// Set the logging format
    #[arg(long, default_value = "text", env = "VECTOR_LOG_FORMAT")]
    pub log_format: LogFormat,

    /// Control when ANSI terminal formatting is used.
    ///
    /// By default `vector` will try and detect if `stdout` is a terminal, if it is
    /// ANSI will be enabled. Otherwise it will be disabled. By providing this flag with
    /// the `--color always` option will always enable ANSI terminal formatting. `--color never`
    /// will disable all ANSI terminal formatting. `--color auto` will attempt
    /// to detect it automatically.
    #[arg(long, default_value = "auto", env = "VECTOR_COLOR")]
    pub color: Color,

    /// Watch for changes in configuration file, and reload accordingly.
    ///
    /// When enabled, Vector uses inotify (Linux), kqueue (macOS), or ReadDirectoryChangesW (Windows)
    /// to detect config changes and trigger a hot reload. This enables zero-downtime
    /// configuration updates.
    #[arg(short, long, env = "VECTOR_WATCH_CONFIG")]
    pub watch_config: bool,

    /// Method for configuration watching.
    ///
    /// By default, `vector` uses recommended watcher for host OS
    /// - `inotify` for Linux-based systems.
    /// - `kqueue` for unix/macos
    /// - `ReadDirectoryChangesWatcher` for windows
    ///
    /// The `poll` watcher can be used in cases where `inotify` doesn't work, e.g., when attaching the configuration via NFS.
    #[arg(
        long,
        default_value = "recommended",
        env = "VECTOR_WATCH_CONFIG_METHOD"
    )]
    pub watch_config_method: WatchConfigMethod,

    /// Poll for changes in the configuration file at the given interval.
    ///
    /// This setting is only applicable if `Poll` is set in `--watch-config-method`.
    #[arg(
        long,
        env = "VECTOR_WATCH_CONFIG_POLL_INTERVAL_SECONDS",
        default_value = "30"
    )]
    pub watch_config_poll_interval_seconds: NonZeroU64,

    /// Set the internal log rate limit in seconds.
    ///
    /// This controls the time window for rate limiting Vector's own internal logs.
    /// Within each time window, the first occurrence of a log is emitted, the second
    /// shows a suppression warning, and subsequent occurrences are silent until the
    /// window expires.
    ///
    /// Logs are grouped by their location in the code and the `component_id` field, so logs
    /// from different components are rate limited independently.
    ///
    /// Examples:
    /// - 1: Very verbose, logs can repeat every second
    /// - 10 (default): Logs can repeat every 10 seconds
    /// - 60: Less verbose, logs can repeat every minute
    #[arg(
        short,
        long,
        env = "VECTOR_INTERNAL_LOG_RATE_LIMIT",
        default_value = "10"
    )]
    pub internal_log_rate_limit: u64,

    /// Set the duration in seconds to wait for graceful shutdown after SIGINT or SIGTERM are
    /// received. After the duration has passed, Vector will force shutdown. To never force
    /// shutdown, use `--no-graceful-shutdown-limit`.
    ///
    /// During graceful shutdown, Vector:
    /// 1. Stops accepting new data from sources
    /// 2. Drains in-flight events through transforms
    /// 3. Flushes buffers to sinks
    /// 4. Waits for acknowledgements (if enabled)
    #[arg(
        long,
        default_value = "60",
        env = "VECTOR_GRACEFUL_SHUTDOWN_LIMIT_SECS",
        group = "graceful-shutdown-limit"
    )]
    pub graceful_shutdown_limit_secs: NonZeroU64,

    /// Never time out while waiting for graceful shutdown after SIGINT or SIGTERM received.
    /// This is useful when you would like for Vector to attempt to send data until terminated
    /// by a SIGKILL. Overrides/cannot be set with `--graceful-shutdown-limit-secs`.
    #[arg(
        long,
        default_value = "false",
        env = "VECTOR_NO_GRACEFUL_SHUTDOWN_LIMIT",
        group = "graceful-shutdown-limit"
    )]
    pub no_graceful_shutdown_limit: bool,

    /// Set runtime allocation tracing
    #[cfg(feature = "allocation-tracing")]
    #[arg(long, env = "ALLOCATION_TRACING", default_value = "false")]
    pub allocation_tracing: bool,

    /// Set allocation tracing reporting rate in milliseconds.
    #[cfg(feature = "allocation-tracing")]
    #[arg(
        long,
        env = "ALLOCATION_TRACING_REPORTING_INTERVAL_MS",
        default_value = "5000"
    )]
    pub allocation_tracing_reporting_interval_ms: u64,

    /// Disable probing and configuration of root certificate locations on the system for OpenSSL.
    ///
    /// The probe functionality manipulates the `SSL_CERT_FILE` and `SSL_CERT_DIR` environment variables
    /// in the Vector process. This behavior can be problematic for users of the `exec` source, which by
    /// default inherits the environment of the Vector process.
    #[arg(long, env = "VECTOR_OPENSSL_NO_PROBE", default_value = "false")]
    pub openssl_no_probe: bool,

    /// Allow the configuration to run without any components. This is useful for loading in an
    /// empty stub config that will later be replaced with actual components. Note that this is
    /// likely not useful without also watching for config file changes as described in
    /// `--watch-config`.
    #[arg(long, env = "VECTOR_ALLOW_EMPTY_CONFIG", default_value = "false")]
    pub allow_empty_config: bool,
}

impl RootOpts {
    /// Consolidates all config path specifications into a unified list.
    ///
    /// This merges paths from:
    /// - `--config` (format auto-detected)
    /// - `--config-toml/json/yaml` (explicit format)
    /// - `--config-dir` (directory scanning)
    ///
    /// The result is used by the config loading system to find and merge all
    /// configuration files.
    pub fn config_paths_with_formats(&self) -> Vec<config::ConfigPath> {
        config::merge_path_lists(vec![
            (&self.config_paths, None),
            (&self.config_paths_toml, Some(config::Format::Toml)),
            (&self.config_paths_json, Some(config::Format::Json)),
            (&self.config_paths_yaml, Some(config::Format::Yaml)),
        ])
        .map(|(path, hint)| config::ConfigPath::File(path, hint))
        .chain(
            self.config_dirs
                .iter()
                .map(|dir| config::ConfigPath::Dir(dir.to_path_buf())),
        )
        .collect()
    }

    /// Initializes global state that must be set up before the main application runs.
    ///
    /// This currently handles:
    /// - OpenSSL certificate path probing (unless disabled)
    /// - Metrics subsystem initialization
    ///
    /// The OpenSSL probing is necessary because many TLS libraries don't know where
    /// to find certificates on non-standard paths, especially in containers.
    pub fn init_global(&self) {
        if !self.openssl_no_probe {
            unsafe {
                openssl_probe::init_openssl_env_vars();
            }
        }

        crate::metrics::init_global().expect("metrics initialization failed");
    }
}

/// Vector subcommands for specialized operations.
///
/// Each subcommand is a self-contained operation that exits after completion.
/// They don't start the main Vector topology (except `run` which is the default
/// when no subcommand is specified).
#[derive(Parser, Debug)]
#[command(rename_all = "kebab-case")]
pub enum SubCommand {
    /// Validate the target config, then exit.
    Validate(validate::Opts),

    /// Convert a config file from one format to another.
    /// This command can also walk directories recursively and convert all config files that are discovered.
    /// Note that this is a best effort conversion due to the following reasons:
    /// * The comments from the original config file are not preserved.
    /// * Explicitly set default values in the original implementation might be omitted.
    /// * Depending on how each source/sink config struct configures serde, there might be entries with null values.
    ConvertConfig(convert_config::Opts),

    /// Generate a Vector configuration containing a list of components.
    Generate(generate::Opts),

    /// Generate the configuration schema for this version of Vector. (experimental)
    ///
    /// A JSON Schema document will be generated that represents the valid schema for a
    /// Vector configuration. This schema is based on the "full" configuration, such that for usages
    /// where a configuration is split into multiple files, the schema would apply to those files
    /// only when concatenated together.
    ///
    /// By default all output is writen to stdout. The `output_path` option can be used to redirect to a file.
    GenerateSchema(generate_schema::Opts),

    /// Generate shell completion, then exit.
    #[command(hide = true)]
    Completion(completion::Opts),

    /// Output a provided Vector configuration file/dir as a single JSON object, useful for checking in to version control.
    #[command(hide = true)]
    Config(config::Opts),

    /// List available components, then exit.
    List(list::Opts),

    /// Run Vector config unit tests, then exit. This command is experimental and therefore subject to change.
    /// For guidance on how to write unit tests check out <https://vector.dev/guides/level-up/unit-testing/>.
    Test(unit_test::Opts),

    /// Output the topology as visual representation using the DOT language which can be rendered by GraphViz
    Graph(graph::Opts),

    /// Display topology and metrics in the console, for a local or remote Vector instance
    #[cfg(feature = "top")]
    Top(top::Opts),

    /// Observe output log events from source or transform components. Logs are sampled at a specified interval.
    #[cfg(feature = "api-client")]
    Tap(tap::Opts),

    /// Manage the vector service.
    #[cfg(windows)]
    Service(service::Opts),

    /// Vector Remap Language CLI
    Vrl(vrl::cli::Opts),
}

impl SubCommand {
    /// Executes the subcommand with the given signal handler and color settings.
    ///
    /// Each subcommand handles its own execution flow and returns an exit code.
    /// The signal pair allows subcommands like `tap` and `test` to respond to
    /// SIGINT/SIGTERM appropriately.
    pub async fn execute(
        &self,
        mut signals: signal::SignalPair,
        color: bool,
    ) -> exitcode::ExitCode {
        match self {
            Self::Completion(s) => completion::cmd(s),
            Self::Config(c) => config::cmd(c),
            Self::ConvertConfig(opts) => convert_config::cmd(opts),
            Self::Generate(g) => generate::cmd(g),
            Self::GenerateSchema(opts) => generate_schema::cmd(opts),
            Self::Graph(g) => graph::cmd(g),
            Self::List(l) => list::cmd(l),
            #[cfg(windows)]
            Self::Service(s) => service::cmd(s),
            #[cfg(feature = "api-client")]
            Self::Tap(t) => tap::cmd(t, signals.receiver).await,
            Self::Test(t) => unit_test::cmd(t, &mut signals.handler).await,
            #[cfg(feature = "top")]
            Self::Top(t) => top::cmd(t).await,
            Self::Validate(v) => validate::validate(v, color).await,
            Self::Vrl(s) => vrl::cli::cmd::cmd(s, vector_vrl_functions::all()),
        }
    }
}

/// Controls when ANSI color codes are used in terminal output.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Auto,
    Always,
    Never,
}

impl Color {
    /// Determines whether to actually use ANSI colors based on the setting
    /// and terminal capabilities.
    ///
    /// Note: On Windows, ANSI colors are disabled by default because cmd.exe
    /// doesn't support them (though modern Windows Terminal does).
    pub fn use_color(&self) -> bool {
        match self {
            #[cfg(unix)]
            Color::Auto => {
                use std::io::IsTerminal;
                std::io::stdout().is_terminal()
            }
            #[cfg(windows)]
            Color::Auto => false, // ANSI colors are not supported by cmd.exe
            Color::Always => true,
            Color::Never => false,
        }
    }
}

/// Output format for Vector's internal logs.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Text,
    Json,
}

/// Configuration file watching method.
///
/// The default (Recommended) uses OS-native file system notifications which
/// are efficient but may not work in all environments (e.g., network mounts).
/// Poll-based watching is slower but works everywhere.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchConfigMethod {
    /// Recommended watcher for the current OS, usually `inotify` for Linux-based systems.
    Recommended,
    /// Poll-based watcher, typically used for watching files on EFS/NFS-like network storage systems.
    /// The interval is determined by  [`RootOpts::watch_config_poll_interval_seconds`].
    Poll,
}

/// Handles configuration errors by logging them and returning the appropriate exit code.
///
/// This is used during startup to report config validation failures before
/// the main topology can be built.
pub fn handle_config_errors(errors: Vec<String>) -> exitcode::ExitCode {
    for error in errors {
        error!(message = "Configuration error.", %error, internal_log_rate_limit = false);
    }

    exitcode::CONFIG
}
