//! Process-backed PowerShell puppet proof used by the terminal and Minecraft
//! integration plans.

#[cfg(windows)]
use std::time::{Duration, Instant};

#[cfg(windows)]
use teamy_terminal_core::{Color, TerminalLimits, TerminalSession};
#[cfg(windows)]
use teamy_terminal_pty::{PtyConfig, PtyEvent, PtySession};

/// Run the same bounded command sequence that the Minecraft terminal puppet
/// captures.  The PTY bytes are fed into the renderer-free core so this test
/// verifies both the process boundary and the retained ANSI colour metadata.
#[cfg(windows)]
#[test]
fn powershell_puppet_captures_range_and_cyan_write_host() {
    let session = PtySession::spawn(&PtyConfig::test()).expect("PowerShell should start in a test PTY");
    session
        .write_input(b"1..100; Write-Host -ForegroundColor Cyan \"hello, world!\"; exit\r")
        .expect("PowerShell should accept the puppet command");

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut output = Vec::new();
    let mut exit = None;
    while Instant::now() < deadline {
        let Some(event) = session
            .recv_timeout(Duration::from_millis(100))
            .expect("PTY event receive should remain healthy")
        else {
            continue;
        };
        match event {
            PtyEvent::Output(bytes) => output.extend(bytes),
            PtyEvent::Exited(status) => {
                exit = Some(status);
                break;
            }
            PtyEvent::Error(error) => panic!("test PTY failed: {error}"),
        }
    }

    assert_eq!(exit.map(|status| status.success), Some(true));
    let mut terminal = TerminalSession::new(
        TerminalLimits {
            max_columns: 120,
            max_rows: 120,
            max_scrollback_rows: 256,
        },
        120,
        120,
    );
    terminal.process_bytes(&output);
    let snapshot = terminal.snapshot();
    let rows = snapshot.visible_text();
    assert!(rows.lines().any(|line| line.trim() == "1"), "range output must contain 1:\n{rows}");
    assert!(rows.lines().any(|line| line.trim() == "100"), "range output must contain 100:\n{rows}");

    let (hello_row, hello_column) = rows
        .lines()
        .enumerate()
        .find_map(|(row, line)| line.find("hello, world!").map(|column| (row, column)))
        .expect("Write-Host output should be visible in the captured snapshot");
    let hello_color = snapshot
        .cell(hello_column as u16, hello_row as u16)
        .expect("hello cell should be inside the captured grid")
        .foreground;
    assert!(
        matches!(hello_color, Color::Indexed(6 | 14) | Color::Rgb(0, 255, 255)),
        "Write-Host Cyan should retain cyan ANSI metadata, got {hello_color:?}"
    );
}
