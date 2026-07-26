#![cfg(windows)]

use eyre::Result;
use portable_pty::CommandBuilder;
use portable_pty::PtySize;
use portable_pty::native_pty_system;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;
use teamy_terminal_core::TerminalLimits;
use teamy_terminal_core::TerminalSession;

fn ratatui_key_debug_executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("RATATUI_KEY_DEBUG_EXE") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }

    let repos_root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3)?;
    ["debug", "release"].into_iter().find_map(|profile| {
        let path = repos_root
            .join("ratatui-key-debug")
            .join("target")
            .join(profile)
            .join("ratatui_key_debug.exe");
        path.is_file().then_some(path)
    })
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn receive_output(
    receiver: &mpsc::Receiver<Vec<u8>>,
    raw_output: &mut Vec<u8>,
    terminal: &mut TerminalSession,
) {
    while let Ok(bytes) = receiver.try_recv() {
        terminal.process_bytes(&bytes);
        raw_output.extend(bytes);
    }
}

fn wait_for_output(
    receiver: &mpsc::Receiver<Vec<u8>>,
    raw_output: &mut Vec<u8>,
    terminal: &mut TerminalSession,
    needle: &[u8],
    description: &str,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !contains_bytes(raw_output, needle) {
        if Instant::now() >= deadline {
            eyre::bail!(
                "timed out waiting for {description}; captured {} bytes",
                raw_output.len()
            );
        }
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(bytes) => {
                terminal.process_bytes(&bytes);
                raw_output.extend(bytes);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                eyre::bail!("PTY output reader disconnected before {description}")
            }
        }
    }
    Ok(())
}

fn visible_text(terminal: &TerminalSession) -> String {
    let snapshot = terminal.snapshot();
    snapshot
        .cells
        .chunks(usize::from(snapshot.columns))
        .map(|row| row.iter().map(|cell| cell.character).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn cmd_runs_ratatui_key_debug_and_restores_the_primary_screen() -> Result<()> {
    let Some(executable) = ratatui_key_debug_executable() else {
        eprintln!("skipping PTY regression: build ratatui-key-debug or set RATATUI_KEY_DEBUG_EXE");
        return Ok(());
    };

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 160,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| {
            eyre::eyre!("failed to allocate the ratatui-key-debug regression PTY: {error}")
        })?;
    let mut command = CommandBuilder::new("cmd.exe");
    command.arg("/d");
    command.arg("/q");
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    let mut child = pair.slave.spawn_command(command).map_err(|error| {
        eyre::eyre!("failed to spawn cmd.exe for the ratatui-key-debug regression: {error}")
    })?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| eyre::eyre!("failed to clone the regression PTY reader: {error}"))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|error| eyre::eyre!("failed to open the regression PTY writer: {error}"))?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(length) => {
                    if sender.send(buffer[..length].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut terminal = TerminalSession::new(
        TerminalLimits {
            max_columns: 200,
            max_rows: 40,
            max_scrollback_rows: 128,
        },
        160,
        24,
    );
    let mut raw_output = Vec::new();
    let result = (|| -> Result<()> {
        write!(
            writer,
            "echo teamy-primary-marker & \"{}\"\r\n",
            executable.display()
        )?;
        writer.flush()?;

        wait_for_output(
            &receiver,
            &mut raw_output,
            &mut terminal,
            b"\x1b[?1049h",
            "ratatui alternate-screen entry",
        )?;

        writer.write_all(&[0x01])?;
        writer.flush()?;
        wait_for_output(
            &receiver,
            &mut raw_output,
            &mut terminal,
            b"Char('a')",
            "Ctrl+A in ratatui-key-debug",
        )?;

        writer.write_all(b"\x1b[1;6D")?;
        writer.flush()?;
        wait_for_output(
            &receiver,
            &mut raw_output,
            &mut terminal,
            b"Left",
            "Ctrl+Shift+Left in ratatui-key-debug",
        )?;

        for _ in 0..3 {
            writer.write_all(&[0x1b])?;
            writer.flush()?;
            std::thread::sleep(Duration::from_millis(100));
        }
        wait_for_output(
            &receiver,
            &mut raw_output,
            &mut terminal,
            b"\x1b[?1049l",
            "ratatui alternate-screen exit",
        )?;

        writer.write_all(b"exit\r\n")?;
        writer.flush()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if child
                .try_wait()
                .map_err(|error| eyre::eyre!("failed to poll cmd.exe: {error}"))?
                .is_some()
            {
                break;
            }
            if Instant::now() >= deadline {
                eyre::bail!("cmd.exe did not exit after the PTY regression")
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        receive_output(&receiver, &mut raw_output, &mut terminal);
        Ok(())
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    result?;

    assert!(contains_bytes(&raw_output, b"\x1b[?1049h"));
    assert!(contains_bytes(&raw_output, b"\x1b[?1049l"));
    let restored_text = visible_text(&terminal);
    assert!(
        restored_text.contains("teamy-primary-marker"),
        "primary screen was not restored; visible text was:\n{restored_text}"
    );
    assert!(
        !restored_text.contains("Key Events"),
        "alternate-screen UI leaked into the primary screen:\n{restored_text}"
    );
    Ok(())
}
