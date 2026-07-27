mod teamy_terminal_invoke_cli;

pub mod close;
pub mod get_content;
pub mod paste;

pub use get_content::GetContentArgs;
pub mod ping;
pub mod resize;
pub mod screenshot;
pub mod select;
pub mod send_keys;
pub mod send_text;
pub mod zoom;

pub use teamy_terminal_invoke_cli::{InvokeAction, InvokeArgs};
