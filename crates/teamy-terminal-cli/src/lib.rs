#![deny(clippy::disallowed_methods)]
#![deny(clippy::disallowed_macros)]

pub mod cli;
mod interactive;
pub mod logging_init;
#[cfg(windows)]
mod windows_startup;

use crate::cli::Cli;
use chrono::DateTime;
use chrono::Local;
use chrono::Utc;
use teamy_cancellation::CtrlCHandler;

/// Version string combining package version, git repository metadata, and build time.
fn version() -> String {
    let built_at = option_env!("BUILD_TIMESTAMP_UNIX")
        .and_then(|value| value.parse::<i64>().ok())
        .and_then(|timestamp| DateTime::<Utc>::from_timestamp(timestamp, 0))
        .map_or_else(
            || "unknown build time".to_string(),
            |timestamp| {
                timestamp
                    .with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M:%S %Z")
                    .to_string()
            },
        );

    format!(
        "{} (repo {}, branch {}, rev {}, worktree {}, built {})",
        env!("CARGO_PKG_VERSION"),
        env!("GIT_REPOSITORY_URL"),
        env!("GIT_BRANCH"),
        env!("GIT_REVISION"),
        env!("GIT_WORKTREE_STATUS"),
        built_at,
    )
}

/// Entrypoint for the program.
///
/// # Errors
///
/// This function will return an error if color-eyre installation, CLI parsing,
/// logging initialization, cancellation, or output rendering fails.
///
/// # Panics
///
/// This function panics if the Figue schema for the CLI cannot be constructed
/// or if Figue fails to produce the parsed CLI value. Those are static
/// programming errors in the CLI definition.
pub fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    let cancellation_token = CtrlCHandler::default().install()?;

    #[cfg(windows)]
    {
        let _ = windows_startup::enable_ansi_support();
        windows_startup::warn_if_utf8_not_enabled();
    };

    let version = version();
    let cli: Cli = figue::Driver::new(
        figue::builder::<Cli>()
            .expect("schema should be valid")
            .cli(move |cli| cli.args_os(std::env::args_os().skip(1)).strict())
            .help(move |help| {
                help.version(version)
                    .include_implementation_source_file(true)
                    .include_implementation_github_url(
                        "TeamDman/teamy-terminal",
                        env!("GIT_REVISION"),
                    )
            })
            .build(),
    )
    .run()
    .unwrap();

    let _stop_after_duration_thread = cli
        .global_args
        .stop_after
        .start_stop_after_duration_thread(cancellation_token.clone())?;

    logging_init::init_logging(&cli.global_args, cancellation_token.clone())?;
    let requested_output_format = cli.global_args.output_format;
    let output = cli.invoke(&cancellation_token)?;
    if cancellation_token.is_cancelled() {
        return Ok(());
    }
    cancellation_token.bail_if_cancelled()?;
    output.emit(requested_output_format)?;
    cancellation_token.bail_if_cancelled()?;
    Ok(())
}
