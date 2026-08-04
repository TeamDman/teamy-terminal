use crate::cli::invoke::close::CloseArgs;
use crate::cli::invoke::get_content::GetContentArgs;
use crate::cli::invoke::paste::PasteArgs;
use crate::cli::invoke::ping::PingArgs;
use crate::cli::invoke::resize::ResizeArgs;
use crate::cli::invoke::screenshot::ScreenshotArgs;
use crate::cli::invoke::select::SelectArgs;
use crate::cli::invoke::send_keys::SendKeysArgs;
use crate::cli::invoke::send_text::SendTextArgs;
use crate::cli::invoke::zoom::ZoomArgs;
use crate::cli::output::CliOutput;
use arbitrary::Arbitrary;
use facet::Facet;
use figue::{self as args};
use teamy_terminal_control::ControlAction;

/// Arguments for `teamy-terminal invoke`.
#[derive(Arbitrary, Debug, Facet, PartialEq, Eq)]
#[facet(rename_all = "kebab-case")]
pub struct InvokeArgs {
    /// Numeric process identifier shown by `teamy-terminal list`.
    #[facet(args::positional)]
    pub instance_id: u32,

    /// Action to send to that terminal instance.
    #[facet(args::subcommand)]
    pub action: InvokeAction,
}

/// Actions supported by the terminal control plane.
#[derive(Arbitrary, Debug, Facet, PartialEq, Eq)]
#[repr(u8)]
#[facet(rename_all = "kebab-case")]
pub enum InvokeAction {
    Ping(PingArgs),
    SendKeys(SendKeysArgs),
    SendText(SendTextArgs),
    Paste(PasteArgs),
    Resize(ResizeArgs),
    Screenshot(ScreenshotArgs),
    /// Set the visible-grid selection without desktop mouse input.
    Select(SelectArgs),
    /// Copy the active selection and return the copied text.
    CopySelection(GetContentArgs),
    /// Adjust font size by bounded one-pixel steps.
    Zoom(ZoomArgs),
    /// Show the native window and restore its taskbar entry.
    Show(GetContentArgs),
    /// Hide the native window and remove its taskbar entry.
    Hide(GetContentArgs),
    Close(CloseArgs),
    GetContent(GetContentArgs),
}

impl InvokeArgs {
    /// Sends this invocation to the selected terminal instance.
    ///
    /// # Errors
    ///
    /// Returns an error when the instance cannot be found, the requested action
    /// is invalid, or the control request cannot be completed.
    pub fn invoke(self) -> eyre::Result<CliOutput> {
        let instance = teamy_terminal_control::find_instance(self.instance_id)?;
        let action = self.action.into_control_action()?;
        let response = teamy_terminal_control::invoke(&instance, action)?;
        Ok(CliOutput::facet(response))
    }
}

impl InvokeAction {
    fn into_control_action(self) -> eyre::Result<ControlAction> {
        match self {
            Self::Ping(_) => Ok(ControlAction::Ping),
            Self::SendKeys(args) => args.into_control_action(),
            Self::SendText(args) => Ok(ControlAction::SendText { text: args.text }),
            Self::Paste(args) => Ok(ControlAction::SendKeys {
                keys: vec![teamy_terminal_control::ControlKey::Paste { text: args.text }],
            }),
            Self::Resize(args) => Ok(ControlAction::Resize {
                width: args.width,
                height: args.height,
            }),
            Self::Screenshot(args) => Ok(ControlAction::Screenshot {
                path: args.resolve_path()?.to_string_lossy().into_owned(),
            }),
            Self::Select(args) => Ok(ControlAction::Select {
                anchor_column: args.anchor_column,
                anchor_row: args.anchor_row,
                focus_column: args.focus_column,
                focus_row: args.focus_row,
            }),
            Self::CopySelection(_) => Ok(ControlAction::CopySelection),
            Self::Zoom(args) => Ok(ControlAction::Zoom { steps: args.steps }),
            Self::Show(_) => Ok(ControlAction::SetVisible { visible: true }),
            Self::Hide(_) => Ok(ControlAction::SetVisible { visible: false }),
            Self::GetContent(_) => Ok(ControlAction::GetContent),
            Self::Close(_) => Ok(ControlAction::Close),
        }
    }
}
