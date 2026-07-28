use crate::cli::output::CliOutput;
use crate::vox_server;
use arbitrary::Arbitrary;
use eyre::Context;
use facet::Facet;
use figue::{self as args};
use std::time::Duration;
use teamy_cancellation::CancellationToken;

/// Arguments for `teamy-terminal serve`.
#[derive(Arbitrary, Debug, Facet, Default, PartialEq, Eq)]
pub struct ServeArgs {
    /// TCP listen address; `:PORT` binds loopback and an omitted address uses
    /// the default development endpoint.
    #[facet(args::positional)]
    #[arbitrary(default)]
    pub address: Option<String>,
}

impl ServeArgs {
    pub fn invoke(self, cancellation_token: CancellationToken) -> eyre::Result<CliOutput> {
        let address = self
            .address
            .as_deref()
            .unwrap_or(vox_server::DEFAULT_LISTEN_ADDRESS);
        let address = vox_server::normalize_listen_address(address)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .wrap_err("failed to create the terminal server runtime")?;
        runtime.block_on(async move {
            tokio::select! {
                result = vox_server::serve(&address) => result?,
                () = wait_for_cancellation(cancellation_token) => {}
            }
            Ok(CliOutput::none())
        })
    }
}

async fn wait_for_cancellation(cancellation_token: CancellationToken) {
    while !cancellation_token.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
