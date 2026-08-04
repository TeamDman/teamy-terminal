use crate::cli::output::CliOutput;
use arbitrary::Arbitrary;
use facet::Facet;
use teamy_terminal_control::InstanceInfo;

/// Arguments for `teamy-terminal list`.
#[derive(Arbitrary, Debug, Facet, Default, PartialEq, Eq)]
pub struct ListArgs;

/// Registered live terminal instances.
#[derive(Debug, Facet, PartialEq, Eq)]
pub struct ListReport {
    pub instances: Vec<InstanceInfo>,
}

impl ListArgs {
    /// Lists terminal instances registered with the control plane.
    ///
    /// # Errors
    ///
    /// Returns an error when the instance registry cannot be read.
    pub fn invoke(self) -> eyre::Result<CliOutput> {
        Ok(CliOutput::facet(ListReport {
            instances: teamy_terminal_control::list_instances()?,
        }))
    }
}
