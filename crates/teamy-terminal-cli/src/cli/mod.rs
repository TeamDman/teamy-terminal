pub mod global_args;
pub mod invoke;
pub mod list;
pub mod output;
pub mod render_fixture;
pub mod serve;

use crate::cli::global_args::GlobalArgs;
use crate::cli::output::CliOutput;
use arbitrary::Arbitrary;
use facet::Facet;
use figue::FigueBuiltins;
use figue::{self as args};
use teamy_cancellation::CancellationToken;

/// Teamy Terminal command line utility.
///
#[derive(Facet, Arbitrary, Debug)]
pub struct Cli {
    /// Global arguments (`debug`, `log_filter`, `log_file`).
    #[facet(flatten)]
    pub global_args: GlobalArgs,

    /// Standard CLI options (help, version, completions).
    #[facet(flatten)]
    #[arbitrary(default)]
    pub builtins: FigueBuiltins,

    /// Control an already-running terminal instance, or launch interactively
    /// when omitted.
    #[facet(args::subcommand)]
    pub command: Option<Command>,
}

/// Product subcommands.
#[derive(Facet, Arbitrary, Debug, PartialEq, Eq)]
#[repr(u8)]
#[facet(rename_all = "kebab-case")]
pub enum Command {
    /// List registered Teamy Terminal instances.
    List(list::ListArgs),
    /// Render a deterministic terminal frame fixture to a portable artifact.
    RenderFixture(render_fixture::RenderFixtureArgs),
    /// Serve the Rust-authoritative terminal endpoint over TCP.
    Serve(serve::ServeArgs),
    /// Invoke an action on a registered Teamy Terminal instance.
    Invoke(invoke::InvokeArgs),
}

impl PartialEq for Cli {
    fn eq(&self, other: &Self) -> bool {
        self.global_args == other.global_args && self.command == other.command
    }
}

impl Cli {
    /// Run the interactive terminal when no built-in command exits first.
    ///
    /// # Errors
    ///
    /// This function returns an error if cancellation has already been requested.
    pub fn invoke(self, cancellation_token: &CancellationToken) -> eyre::Result<CliOutput> {
        cancellation_token.bail_if_cancelled()?;
        let initially_hidden = self.global_args.hidden;
        if let Some(command) = self.command {
            return command.invoke(cancellation_token.clone());
        }

        crate::interactive::run(cancellation_token, initially_hidden)?;
        Ok(CliOutput::none())
    }
}

impl Command {
    fn invoke(self, cancellation_token: CancellationToken) -> eyre::Result<CliOutput> {
        match self {
            Self::List(args) => args.invoke(),
            Self::RenderFixture(args) => args.invoke(),
            Self::Serve(args) => args.invoke(cancellation_token),
            Self::Invoke(args) => args.invoke(),
        }
    }
}
