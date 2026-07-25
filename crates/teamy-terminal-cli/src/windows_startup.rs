use eyre::Context;
use windows::Win32::Globalization::GetACP;
use windows::Win32::System::Console::CONSOLE_MODE;
use windows::Win32::System::Console::ENABLE_VIRTUAL_TERMINAL_PROCESSING;
use windows::Win32::System::Console::GetConsoleMode;
use windows::Win32::System::Console::GetStdHandle;
use windows::Win32::System::Console::STD_OUTPUT_HANDLE;
use windows::Win32::System::Console::SetConsoleMode;

const UTF8_CODEPAGE: u32 = 65_001;

pub fn enable_ansi_support() -> eyre::Result<()> {
    // SAFETY: `GetStdHandle` is called with the documented standard-output
    // handle selector and does not dereference a caller-provided pointer.
    let handle =
        unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }.wrap_err("failed to get stdout handle")?;
    if handle.is_invalid() {
        eyre::bail!("STD_OUTPUT_HANDLE is invalid");
    }

    let mut mode = CONSOLE_MODE::default();
    // SAFETY: `mode` is a valid writable `CONSOLE_MODE` value and `handle`
    // was obtained from the Windows standard-output handle API above.
    unsafe { GetConsoleMode(handle, &raw mut mode) }.wrap_err("failed to get console mode")?;
    // SAFETY: `handle` is the validated standard-output handle and `mode`
    // contains the console mode returned by `GetConsoleMode`.
    unsafe { SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) }
        .wrap_err("failed to set console mode")?;

    Ok(())
}

fn is_system_utf8() -> bool {
    // SAFETY: `GetACP` reads process-global Windows locale state and accepts
    // no caller-provided pointers.
    unsafe { GetACP() == UTF8_CODEPAGE }
}

pub fn warn_if_utf8_not_enabled() {
    if !is_system_utf8() {
        tracing::warn!(
            "The current system codepage is not UTF-8. This may cause replacement-character problems."
        );
        tracing::warn!(
            "See https://github.com/Azure/azure-cli/issues/22616#issuecomment-1147061949"
        );
        tracing::warn!(
            "Control panel -> Clock and Region -> Region -> Administrative -> Change system locale -> Check Beta: Use Unicode UTF-8 for worldwide language support."
        );
    }
}
