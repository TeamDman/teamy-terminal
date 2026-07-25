use arbitrary::Arbitrary;
use facet::Facet;
use figue::{self as args};

/// Arguments for `teamy-terminal invoke <id> send-keys`.
#[derive(Arbitrary, Debug, Facet, PartialEq, Eq)]
pub struct SendKeysArgs {
    /// Text or a named key/chord such as `enter`, `left`, or `ctrl+c`.
    #[facet(args::positional)]
    pub spec: String,
}

impl SendKeysArgs {
    pub fn into_control_action(self) -> eyre::Result<teamy_terminal_control::ControlAction> {
        Ok(teamy_terminal_control::ControlAction::SendKeys {
            keys: teamy_terminal_control::parse_key_sequence(&self.spec)?,
        })
    }
}
