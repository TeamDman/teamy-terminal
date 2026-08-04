//! Global arguments that apply to all commands.

use crate::cli::output::OutputFormat;
use crate::stop_after_args::StopAfterArgs;
use arbitrary::Arbitrary;
use facet::Facet;
use figue::{self as args};

/// Global arguments that apply to all commands.
#[derive(Facet, Arbitrary, Debug, Default, PartialEq)]
#[facet(rename_all = "kebab-case")]
pub struct GlobalArgs {
    /// Enable debug logging, including backtraces on panics.
    #[facet(args::named, default)]
    pub debug: bool,

    /// Log level filter directive.
    #[facet(args::named)]
    #[arbitrary(default)]
    pub log_filter: Option<String>,

    /// Write structured ndjson logs.
    ///
    /// If a file path is provided, logs are written to that file. If a
    /// directory path is provided, a timestamped filename is generated.
    #[facet(args::named)]
    #[arbitrary(default)]
    pub log_file: Option<String>,

    /// Request graceful cancellation from tracing/Tracy spans, log messages, or elapsed time.
    #[facet(flatten, default)]
    #[arbitrary(default)]
    pub stop_after: StopAfterArgs,

    /// Render command output as `text`, `json`, or `csv`.
    #[facet(args::named)]
    #[arbitrary(default)]
    pub output_format: Option<OutputFormat>,

    /// Create the interactive Vulkan window hidden and without a taskbar entry.
    #[facet(args::named, default)]
    pub hidden: bool,
}
