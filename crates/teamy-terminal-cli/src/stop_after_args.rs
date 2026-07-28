use facet::Facet;
use figue as args;
use std::thread::JoinHandle;
use std::time::Duration;
use teamy_cancellation::CancellationToken;
use teamy_cancellation::StopAfterLayer;

/// Figue-compatible cancellation arguments for the Teamy Terminal CLI.
///
/// This remains local because the current published `teamy-cancellation`
/// revision pins its optional Figue dependency to an older Facet revision.
#[derive(arbitrary::Arbitrary, Clone, Debug, Default, Facet, PartialEq, Eq)]
#[facet(rename_all = "kebab-case")]
pub struct StopAfterArgs {
    /// Request graceful cancellation after the named tracing/Tracy span or log message is encountered.
    #[facet(rename = "stop-after", default, args::named)]
    #[arbitrary(default)]
    pub stop_after_span: Option<String>,

    /// Request graceful cancellation after a human-readable duration, such as `2s`, `500ms`, or `1m`.
    #[facet(default, args::named)]
    #[arbitrary(default)]
    pub stop_after_duration: Option<String>,
}

impl StopAfterArgs {
    /// Parse the configured stop-after duration.
    pub fn stop_after_duration(&self) -> eyre::Result<Option<Duration>> {
        self.stop_after_duration
            .as_deref()
            .map(humantime::parse_duration)
            .transpose()
            .map_err(Into::into)
    }

    #[must_use]
    pub fn stop_after_span_layer(
        &self,
        cancellation_token: CancellationToken,
    ) -> Option<StopAfterLayer> {
        self.stop_after_span
            .as_ref()
            .map(|stop_after| StopAfterLayer::new(stop_after, cancellation_token))
    }

    /// Start the configured stop-after-duration thread.
    pub fn start_stop_after_duration_thread(
        &self,
        cancellation_token: CancellationToken,
    ) -> eyre::Result<Option<JoinHandle<()>>> {
        let Some(duration) = self.stop_after_duration()? else {
            return Ok(None);
        };

        let duration_label = self
            .stop_after_duration
            .clone()
            .unwrap_or_else(|| humantime::format_duration(duration).to_string());
        std::thread::Builder::new()
            .name(String::from("teamy-terminal-stop-after-duration"))
            .spawn(move || {
                std::thread::sleep(duration);
                cancellation_token.request_cancel(format!(
                    "Operation cancelled after --stop-after-duration {duration_label}"
                ));
            })
            .map(Some)
            .map_err(Into::into)
    }
}
