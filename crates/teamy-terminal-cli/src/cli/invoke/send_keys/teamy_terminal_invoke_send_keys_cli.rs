use arbitrary::Arbitrary;
use facet::Facet;
use figue::{self as args};

/// Arguments for `teamy-terminal invoke <id> send-keys`.
#[derive(Arbitrary, Debug, Facet, PartialEq, Eq)]
pub struct SendKeysArgs {
    /// Explicit actions such as `down:ctrl wait:10 down:c wait:10 up:c
    /// wait:10 up:ctrl`. Use `send-text` for raw text.
    #[facet(args::positional)]
    pub spec: String,
}

impl SendKeysArgs {
    /// Parses the explicit key sequence into a control-plane action.
    ///
    /// # Errors
    ///
    /// Returns an error when `spec` contains an invalid key action or value.
    pub fn into_control_action(self) -> eyre::Result<teamy_terminal_control::ControlAction> {
        Ok(teamy_terminal_control::ControlAction::SendKeys {
            keys: teamy_terminal_control::parse_key_sequence(&self.spec)?,
        })
    }
}
