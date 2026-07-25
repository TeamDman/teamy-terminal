pub mod global_args;
pub mod invoke;
pub mod list;
pub mod output;

use crate::cli::global_args::GlobalArgs;
use crate::cli::output::CliOutput;
use arbitrary::Arbitrary;
use facet::Facet;
use figue::{self as args, FigueBuiltins};
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
    #[arbitrary(default)]
    pub command: Option<Command>,
}

/// Product subcommands.
#[derive(Facet, Arbitrary, Debug, PartialEq, Eq)]
#[repr(u8)]
#[facet(rename_all = "kebab-case")]
pub enum Command {
    /// List registered Teamy Terminal instances.
    List(list::ListArgs),
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
        match self.command {
            Some(command) => command.invoke(),
            None => {
                crate::interactive::run(cancellation_token)?;
                Ok(CliOutput::none())
            }
        }
    }
}

impl Command {
    fn invoke(self) -> eyre::Result<CliOutput> {
        match self {
            Self::List(args) => args.invoke(),
            Self::Invoke(args) => args.invoke(),
        }
    }
}
